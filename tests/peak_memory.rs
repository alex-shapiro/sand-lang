//! Checks that the peak memory of a process running a script stays within a
//! small factor of the script's `max_memory`, for several ways of growing
//! data.
//!
//! Peak memory is measured per process, so the test runs its own binary once
//! per program, selecting the program with an environment variable. On Apple
//! platforms, the test reads the peak physical footprint, which the system
//! uses to limit the memory of a process: the peak resident set size also
//! counts freed pages, which the kernel takes back only when it needs memory.
//! On other platforms, the test reads the peak resident set size.
#![cfg(unix)]

mod common;

use sandlang::{Config, Engine, Error, LimitKind, Limits, MemorySink};
use std::process::Command;

const LIMIT: usize = 128 << 20;

/// The largest allowed ratio of peak memory, minus the peak memory of an
/// empty program, to the limit. It is above 1 because the limit counts what
/// the engine holds, not the memory that the allocator keeps in addition.
const FACTOR: f64 = 1.25;

const PROGRAMS: &[(&str, &str)] = &[
    ("empty", "print(1)"),
    ("channel of lists", "import task\nlet ch = Channel(0)\nwhile true { ch.send([1, 2, 3]) }"),
    ("list of lists", "let xs = []\nwhile true { xs.push([1, 2, 3]) }"),
    ("map growth", "let m: [int: [int]] = [:]\nvar i = 0\nwhile true {\n    m[i] = [i]\n    i += 1\n}"),
    ("set growth", "let s: Set<str> = Set()\nvar i = 0\nwhile true {\n    s.add(\"k\\(i)\")\n    i += 1\n}"),
    ("string building", "var s = \"\"\nwhile true { s += \"abcdefgh\" }"),
    ("list doubling", "let xs = []\nwhile true { xs.push(1) }"),
    (
        "type descriptions",
        "import json\nstruct P<A, B> { a: A, b: B }\nfn make<reflect A: Decode, reflect B: Decode>(n: int, x: [A], y: [B]) -> fn() -> bool {\n    \
         if n == 0 {\n        return () => {\n            let r: Result<[(A, B)]> = json.decode(\"[]\")\n            r.is_ok()\n        }\n    }\n    \
         let w: [P<A, B>] = []\n    make(n - 1, w, w)\n}\nlet keep = []\nlet w: [int] = []\nwhile true { keep.push(make(100, w, w)) }",
    ),
    (
        "decoded computed defaults",
        "import json\nfn one() -> int { 1 }\nstruct P { a: int = one(), b: int = one() }\nlet text = \"[\" + \"{},\".repeat(100000) + \"{}]\"\n\
         let keep = []\nwhile true {\n    let r: Result<[P]> = json.decode(text)\n    keep.push(r)\n}",
    ),
    (
        "decoded wide computed defaults",
        "import json\nfn one() -> int { 1 }\nstruct P {\n    a: int = one(), b: int = one(), c: int = one(), d: int = one()\n    \
         e: int = one(), f: int = one(), g: int = one(), h: int = one()\n    i: int = one(), j: int = one(), k: int = one(), l: int = one()\n    \
         m: int = one(), n: int = one(), o: int = one(), p: int = one()\n}\nlet text = \"[\" + \"{},\".repeat(100000) + \"{}]\"\n\
         let keep = []\nwhile true {\n    let r: Result<[P]> = json.decode(text)\n    keep.push(r)\n}",
    ),
    (
        "decoded constant defaults",
        "import json\nstruct P { a: int = 1, b: int = 2 }\nlet text = \"[\" + \"{},\".repeat(100000) + \"{}]\"\n\
         let keep = []\nwhile true {\n    let r: Result<[P]> = json.decode(text)\n    keep.push(r)\n}",
    ),
];

const CASE: &str = "SAND_PEAK_MEMORY_CASE";
const TEST: &str = "peak_memory_stays_near_the_memory_limit";

/// Runs the program `name` in this process and prints the peak resident set
/// size.
fn run_case(name: &str) {
    let (_, src) = PROGRAMS.iter().find(|(n, _)| *n == name).unwrap();
    let limits = Limits { max_memory: Some(LIMIT), ..Default::default() };
    let mut engine = Engine::new(Config::new().limits(limits).stdout(MemorySink::new())).unwrap();
    let r = common::rt().block_on(engine.run_source("main.sand", src));
    if name == "empty" {
        r.unwrap();
    } else {
        assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{name}: {r:?}");
    }
    let max_rss = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF).unwrap().max_rss() as usize;
    println!("peak rss: {}", if cfg!(target_vendor = "apple") { max_rss } else { max_rss << 10 });
}

/// The peak memory, in bytes, of a process that runs the program `name`.
fn peak_memory_of(name: &str) -> usize {
    let exe = std::env::current_exe().unwrap();
    let args = [TEST, "--exact", "--nocapture", "--test-threads=1"];
    let apple = cfg!(target_vendor = "apple");
    let mut cmd = if apple { Command::new("/usr/bin/time") } else { Command::new(&exe) };
    if apple {
        cmd.arg("-l").arg(&exe);
    }
    let out = cmd.args(args).env(CASE, name).output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let peak = if apple {
        stderr.lines().find_map(|l| l.trim().strip_suffix("peak memory footprint")?.trim().parse().ok())
    } else {
        stdout.lines().find_map(|l| l.split_once("peak rss: ")?.1.parse().ok())
    };
    match peak {
        Some(n) if out.status.success() && stdout.contains("peak rss: ") => n,
        _ => panic!("{name}: {}\n{stdout}\n{stderr}", out.status),
    }
}

#[test]
fn peak_memory_stays_near_the_memory_limit() {
    if let Ok(name) = std::env::var(CASE) {
        return run_case(&name);
    }
    let base = peak_memory_of("empty");
    let mut report = Vec::new();
    for (name, _) in &PROGRAMS[1..] {
        let ratio = peak_memory_of(name).saturating_sub(base) as f64 / LIMIT as f64;
        report.push(format!("{name}: {ratio:.2}"));
        assert!(ratio <= FACTOR, "{name}: peak memory is {ratio:.2} times the limit ({})", report.join(", "));
    }
    println!("peak memory beyond an empty program's {} MiB, in limits: {}", base >> 20, report.join(", "));
}
