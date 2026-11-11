//! Benchmarks for the `json` module.
//!
//! `cargo bench --bench codec` reports the best time for each operation over
//! `BENCH_RUNS` engine runs (default 3). Each run executes each operation
//! `BENCH_REPS` times (default 5) and keeps the fastest. The document has
//! `BENCH_USERS` users (default 5000). `BENCH_OPS=encode,decode` limits the
//! timed operations.
//!
//! `cargo bench --bench codec -- --limits` reports the memory and fuel that the
//! engine charges for each operation on `BENCH_LIMIT_USERS` users (default
//! 1000). The harness finds the smallest `max_memory` and `max_instructions` at
//! which a program runs to the end, and subtracts the values of the same
//! program without the operation. The memory value is the increase in peak
//! memory.
//!
//! `cargo test --benches` runs each program once and checks its output.

use sandlang::engine::{Config, Engine, Error, Limits, MemorySink};

/// Sets up `doc`, a document of `{n}` users.
const PRELUDE: &str = r#"import json
import time
enum Status { Active, Suspended(reason: str), Banned(int, str) }
struct Address { street: str, city: str, zip: str? }
struct User { id: int, name: str, email: str, score: float, active: bool, tags: [str], address: Address, status: Status, logins: [int], meta: [str: int] }
struct Doc { users: [User] }
fn make(n: int) -> Doc {
    let users: [User] = []
    for i in 0..n {
        let status = if i % 3 == 0 { Status.Active } else if i % 3 == 1 { Status.Suspended(reason: "r\(i)") } else { Status.Banned(i, "b") }
        let zip = if i % 4 == 0 { None } else { Some("0\(i % 1000)") }
        let address = Address(street: "\(i) Main St", city: "Springfield", zip: zip)
        users.push(User(id: i, name: "user \(i)", email: "u\(i)@example.com", score: i.to_float() * 0.5, active: i % 2 == 0, tags: ["a", "b\(i % 5)"], address: address, status: status, logins: [i, i + 1, i + 2], meta: ["k": i, "j": i * 2]))
    }
    Doc(users: users)
}
let doc = make({n})
"#;

/// Sets up `text` and `value`, the JSON text and the `Json` value of `doc`.
const INPUTS: &str = "let text = json.encode(doc)\nlet value = json.to_json(doc)\n";

/// Times each operation and prints `name ns_per_item check` for each, then the
/// `check` lines.
const TIMING: &str = r#"
fn selected(name: str) -> bool {
    let names: [str] = [{ops}]
    names.is_empty() or names.contains(name)
}
fn bench(name: str, items: int, f: fn() -> int) {
    if not selected(name) {
        return
    }
    var best = -1
    var check = 0
    for _ in 0..{reps} {
        let sw = time.Stopwatch()
        check = f()
        let t = sw.elapsed().nanos()
        if best < 0 or t < best {
            best = t
        }
    }
    print("\(name) \(best / items) \(check)")
}
bench("encode", {n}, () => json.encode(doc).len())
bench("encode_indent", {n}, () => json.encode(doc, indent: 2).len())
bench("decode", {n}, () => {
    let d: Doc = json.decode(text).unwrap()
    d.users[d.users.len() - 1].id
})
bench("value", {n}, () => {
    let v = json.to_json(doc)
    [v].len()
})
bench("parse", {n}, () => {
    let p: json.Json = json.decode(text).unwrap()
    [p].len()
})
bench("encode_value", {n}, () => json.encode(value).len())
let addr = Address(street: "1 Main St", city: "Springfield", zip: Some("01234"))
let atext = json.encode(addr)
bench("small_encode", 20000, () => {
    var t = 0
    for _ in 0..20000 {
        t += json.encode(addr).len()
    }
    t
})
bench("small_decode", 20000, () => {
    var t = 0
    for _ in 0..20000 {
        let a: Address = json.decode(atext).unwrap()
        t += a.city.len()
    }
    t
})
var nested = json.Json.Int(1)
for _ in 0..400 {
    nested = json.Json.Arr([nested])
}
let deep = json.Json.Obj(["v": nested])
let dtext = json.encode(deep)
bench("deep_encode", 200, () => {
    var t = 0
    for _ in 0..200 {
        t += json.encode(deep).len()
    }
    t
})
bench("deep_decode", 200, () => {
    var t = 0
    for _ in 0..200 {
        let p: json.Json = json.decode(dtext).unwrap()
        t += [p].len()
    }
    t
})
let p: json.Json = json.decode(text).unwrap()
let t: Doc = json.decode(text).unwrap()
let pretty: Doc = json.decode(json.encode(doc, indent: 2)).unwrap()
print("check json.decode(text) == value: \(p == value)")
print("check json.decode(text) == doc: \(t == doc)")
print("check json.decode(indented text) == doc: \(pretty == doc)")
let dp: json.Json = json.decode(dtext).unwrap()
print("check json.decode(deep text) == deep: \(dp == deep)")
print("check text length: \(text.len())")
"#;

/// Operations that the `--limits` mode measures, as a name, the Sand statement
/// that makes the input of the operation, and the Sand statement of the
/// operation.
const LIMIT_OPS: &[(&str, &str, &str)] = &[
    ("encode", "", "let r = json.encode(doc)"),
    ("encode_indent", "", "let r = json.encode(doc, indent: 2)"),
    ("value", "", "let r = json.to_json(doc)"),
    ("decode", "let text = json.encode(doc)", "let r: Doc = json.decode(text).unwrap()"),
    ("parse", "let text = json.encode(doc)", "let r: json.Json = json.decode(text).unwrap()"),
    ("encode_value", "let value = json.to_json(doc)", "let r = json.encode(value)"),
];

fn fill(template: &str, users: usize, reps: usize) -> String {
    let ops = std::env::var("BENCH_OPS").unwrap_or_default();
    let ops: Vec<String> = ops.split(',').filter(|o| !o.is_empty()).map(|o| format!("{o:?}")).collect();
    template.replace("{ops}", &ops.join(", ")).replace("{n}", &users.to_string()).replace("{reps}", &reps.to_string())
}

fn run(src: &str, limits: Limits) -> Result<String, Error> {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().limits(limits).stdout(out.clone()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("bench.sand", src))?;
    Ok(out.contents())
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|n| n.parse().ok()).unwrap_or(default).max(1)
}

/// Returns the smallest value in `lo..=hi` for which `ok` is true, to a
/// precision of `step`. `ok` must be monotonic.
fn smallest(mut lo: u64, mut hi: u64, step: u64, ok: impl Fn(u64) -> bool) -> u64 {
    assert!(ok(hi), "the program fails at the upper bound {hi}");
    while hi - lo > step {
        let mid = lo + (hi - lo) / 2;
        if ok(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

/// Returns the smallest memory limit and fuel limit at which `src` runs to the end.
fn charged(src: &str) -> (u64, u64) {
    let memory = smallest(0, 1 << 31, 16 << 10, |b| run(src, Limits { max_memory: Some(b as usize), ..Default::default() }).is_ok());
    let fuel = smallest(0, 1 << 36, 1000, |f| run(src, Limits { max_instructions: Some(f), ..Default::default() }).is_ok());
    (memory, fuel)
}

fn limits_report() {
    let users = env("BENCH_LIMIT_USERS", 1000);
    println!("charged memory (KiB) and fuel for one operation on {users} users, beyond the setup");
    println!("{:<14} {:>12} {:>14}", "operation", "memory KiB", "fuel");
    let prelude = fill(PRELUDE, users, 1);
    for (op, input, stmt) in LIMIT_OPS {
        let setup = format!("{prelude}{input}\n");
        let (base_memory, base_fuel) = charged(&setup);
        let (memory, fuel) = charged(&format!("{setup}{stmt}\n"));
        println!("{op:<14} {:>12} {:>14}", memory.saturating_sub(base_memory) >> 10, fuel.saturating_sub(base_fuel));
    }
}

fn main() {
    if std::env::args().any(|a| a == "--limits") {
        return limits_report();
    }
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let (users, runs, reps) = if quick { (200, 1, 1) } else { (env("BENCH_USERS", 5000), env("BENCH_RUNS", 3), env("BENCH_REPS", 5)) };
    let src = format!("{}{INPUTS}{}", fill(PRELUDE, users, reps), fill(TIMING, users, reps));
    let mut table: Vec<(String, u64)> = Vec::new();
    let mut checks = Vec::new();
    for run_index in 0..runs {
        let out = run(&src, Limits::default()).unwrap_or_else(|e| panic!("json: {e}"));
        for line in out.lines() {
            if let Some(check) = line.strip_prefix("check ") {
                assert!(!check.ends_with("false"), "json: {check}");
                if run_index == 0 {
                    checks.push(check.to_string());
                }
                continue;
            }
            let mut parts = line.split(' ');
            let (Some(name), Some(ns), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
                panic!("json: unexpected output line {line:?}")
            };
            let ns: u64 = ns.parse().unwrap();
            match table.iter_mut().find(|(n, _)| n == name) {
                Some((_, best)) => *best = (*best).min(ns),
                None => table.push((name.to_string(), ns)),
            }
        }
    }
    assert!(!checks.is_empty(), "json: the program printed no checks");
    println!("best ns per item ({users} users, {runs} runs of {reps} repetitions)");
    println!("{:<14} {:>10}", "operation", "json");
    for (name, ns) in &table {
        println!("{name:<14} {ns:>10}");
    }
    for check in checks {
        println!("{check}");
    }
}
