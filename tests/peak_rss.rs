//! Checks that the peak resident set size (RSS) of a process running a
//! script stays within a small factor of the script's `max_memory`, for
//! several ways of growing data.
//!
//! Peak RSS is measured per process, so the test runs its own binary once
//! per program, selecting the program with an environment variable, and
//! each run reports its peak.
#![cfg(unix)]

mod common;

use sandlang::{Config, Engine, Error, LimitKind, Limits, MemorySink};
use std::process::Command;

const LIMIT: usize = 128 << 20;

/// The largest allowed ratio of peak RSS, minus the peak RSS of an empty
/// program, to the limit. It is above 1 because the limit counts what the
/// heap holds, not the memory the allocator keeps in addition.
const FACTOR: f64 = 1.25;

const PROGRAMS: &[(&str, &str)] = &[
    ("empty", "print(1)"),
    ("channel of lists", "import task\nlet ch = task.channel(0)\nwhile true { ch.send([1, 2, 3]) }"),
    ("list of lists", "let xs = []\nwhile true { xs.push([1, 2, 3]) }"),
    ("map growth", "let m: [int: [int]] = [:]\nvar i = 0\nwhile true {\n    m[i] = [i]\n    i += 1\n}"),
    ("set growth", "let s: Set<str> = set()\nvar i = 0\nwhile true {\n    s.add(\"k\\(i)\")\n    i += 1\n}"),
    ("string building", "var s = \"\"\nwhile true { s += \"abcdefgh\" }"),
    ("list doubling", "let xs = []\nwhile true { xs.push(1) }"),
    (
        "type descriptions",
        "import json\nstruct P<A, B> { a: A, b: B }\nfn make<reflect A: Encode, reflect B: Encode>(n: int, x: [A], y: [B]) -> fn() -> bool {\n    \
         if n == 0 {\n        return () => {\n            let r: Result<[(A, B)]> = json.decode(\"[]\")\n            r.is_ok()\n        }\n    }\n    \
         let w: [P<A, B>] = []\n    make(n - 1, w, w)\n}\nlet keep = []\nlet w: [int] = []\nwhile true { keep.push(make(100, w, w)) }",
    ),
    (
        "decoded computed defaults",
        "import json\nfn one() -> int { 1 }\nstruct P { a: int = one(), b: int = one() }\nlet text = \"[\" + \"{},\".repeat(100000) + \"{}]\"\n\
         let keep = []\nwhile true {\n    let r: Result<[P]> = json.decode(text)\n    keep.push(r)\n}",
    ),
    (
        "decoded constant defaults",
        "import json\nstruct P { a: int = 1, b: int = 2 }\nlet text = \"[\" + \"{},\".repeat(100000) + \"{}]\"\n\
         let keep = []\nwhile true {\n    let r: Result<[P]> = json.decode(text)\n    keep.push(r)\n}",
    ),
];

const CASE: &str = "SAND_PEAK_RSS_CASE";

fn peak_rss() -> usize {
    let max = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF).unwrap().max_rss() as usize;
    if cfg!(target_vendor = "apple") { max } else { max << 10 }
}

/// Runs the program `name` in this process and prints the peak RSS.
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
    println!("peak rss: {}", peak_rss());
}

fn peak_rss_of(name: &str) -> usize {
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args(["peak_rss_stays_near_the_memory_limit", "--exact", "--nocapture", "--test-threads=1"])
        .env(CASE, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rss = stdout.lines().find_map(|l| Some(l.split_once("peak rss: ")?.1));
    match rss {
        Some(n) if out.status.success() => n.parse().unwrap(),
        _ => panic!("{name}: {}\n{stdout}\n{}", out.status, String::from_utf8_lossy(&out.stderr)),
    }
}

#[test]
fn peak_rss_stays_near_the_memory_limit() {
    if let Ok(name) = std::env::var(CASE) {
        return run_case(&name);
    }
    let base = peak_rss_of("empty");
    let mut report = Vec::new();
    for (name, _) in &PROGRAMS[1..] {
        let ratio = peak_rss_of(name).saturating_sub(base) as f64 / LIMIT as f64;
        report.push(format!("{name}: {ratio:.2}"));
        assert!(ratio <= FACTOR, "{name}: peak RSS is {ratio:.2} times the limit ({})", report.join(", "));
    }
    println!("peak RSS beyond an empty program's {} MiB, in limits: {}", base >> 20, report.join(", "));
}
