//! Benchmarks for the `json` module and for [`Vm::to_value`].
//!
//! `cargo bench --bench codec` reports the best time for each operation over
//! `BENCH_RUNS` engine runs (default 3). Each operation runs in its own engine,
//! so one operation does not change the time of another. Each run executes the
//! operation `BENCH_REPS` times (default 5) and keeps the fastest. The document
//! has `BENCH_USERS` users (default 5000). `BENCH_OPS=encode,decode` limits the
//! operations.
//!
//! `cargo bench --bench codec -- --limits` reports the memory and fuel that the
//! engine charges for each operation on `BENCH_LIMIT_USERS` users (default
//! 1000). The harness finds the smallest `max_memory` and `max_instructions` at
//! which a program runs to the end, and subtracts the values of the same
//! program without the operation. The memory value is the increase in peak
//! memory.
//!
//! `cargo test --benches` runs each program once and checks its output.

use sandlang::engine::{Config, Engine, Error, HostModules, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::ty::Param;
use sandlang::stdlib::json::Json;
use sandlang::value::Value;
use sandlang::vm::{Fail, Vm};

/// Declares the types of a document and `make`, which returns a document of
/// `n` users.
const PRELUDE: &str = r#"import json
import time
import host
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
"#;

/// A benchmark operation.
struct Operation {
    name: &'static str,
    /// The Sand statements that make the inputs of the operation. `{n}` is the
    /// number of users.
    inputs: &'static str,
    /// The Sand closure of type `fn() -> int` that runs the operation.
    closure: &'static str,
    /// The number of times that one call of `closure` runs the operation, or
    /// `None` if one call runs the operation once on a document. The
    /// benchmark reports the time of one run, or of one user of the document.
    runs_per_call: Option<usize>,
}

const DOC: &str = "let doc = make({n})\n";
const DOC_TEXT: &str = "let text = json.encode(make({n}))\n";
const DOC_JSON: &str = "let value = json.to_json(make({n}))\n";
const ADDRESS: &str = "let addr = Address(street: \"1 Main St\", city: \"Springfield\", zip: Some(\"01234\"))\n";
const ADDRESS_TEXT: &str = "let atext = json.encode(Address(street: \"1 Main St\", city: \"Springfield\", zip: Some(\"01234\")))\n";
const DEEP: &str = r#"var nested = json.Json.Int(1)
for _ in 0..400 {
    nested = json.Json.Arr([nested])
}
let deep = json.Json.Obj(["v": nested])
"#;
const DEEP_TEXT: &str = r#"var nested = json.Json.Int(1)
for _ in 0..400 {
    nested = json.Json.Arr([nested])
}
let dtext = json.encode(json.Json.Obj(["v": nested]))
"#;

const OPERATIONS: &[Operation] = &[
    Operation { name: "encode", inputs: DOC, closure: "() => json.encode(doc).len()", runs_per_call: None },
    Operation { name: "encode_indent", inputs: DOC, closure: "() => json.encode(doc, indent: 2).len()", runs_per_call: None },
    Operation {
        name: "decode",
        inputs: DOC_TEXT,
        closure: r#"() => {
    let d: Doc = json.decode(text).unwrap()
    d.users[d.users.len() - 1].id
}"#,
        runs_per_call: None,
    },
    Operation { name: "to_json", inputs: DOC, closure: "() => [json.to_json(doc)].len()", runs_per_call: None },
    Operation {
        name: "decode_json",
        inputs: DOC_TEXT,
        closure: r#"() => {
    let p: json.Json = json.decode(text).unwrap()
    [p].len()
}"#,
        runs_per_call: None,
    },
    Operation { name: "encode_json", inputs: DOC_JSON, closure: "() => json.encode(value).len()", runs_per_call: None },
    Operation { name: "json_param", inputs: DOC_JSON, closure: "() => host.json_members(value)", runs_per_call: None },
    Operation { name: "json_echo", inputs: DOC_JSON, closure: "() => [host.json_echo(value)].len()", runs_per_call: None },
    Operation { name: "to_value", inputs: DOC, closure: "() => host.copied_fields(doc)", runs_per_call: None },
    Operation {
        name: "small_encode",
        inputs: ADDRESS,
        closure: r#"() => {
    var t = 0
    for _ in 0..20000 {
        t += json.encode(addr).len()
    }
    t
}"#,
        runs_per_call: Some(20000),
    },
    Operation {
        name: "small_to_json",
        inputs: ADDRESS,
        closure: r#"() => {
    var t = 0
    for _ in 0..20000 {
        t += [json.to_json(addr)].len()
    }
    t
}"#,
        runs_per_call: Some(20000),
    },
    Operation {
        name: "small_decode",
        inputs: ADDRESS_TEXT,
        closure: r#"() => {
    var t = 0
    for _ in 0..20000 {
        let a: Address = json.decode(atext).unwrap()
        t += a.city.len()
    }
    t
}"#,
        runs_per_call: Some(20000),
    },
    Operation {
        name: "small_to_value",
        inputs: ADDRESS,
        closure: r#"() => {
    var t = 0
    for _ in 0..20000 {
        t += host.copied_fields(addr)
    }
    t
}"#,
        runs_per_call: Some(20000),
    },
    Operation {
        name: "deep_encode",
        inputs: DEEP,
        closure: r#"() => {
    var t = 0
    for _ in 0..200 {
        t += json.encode(deep).len()
    }
    t
}"#,
        runs_per_call: Some(200),
    },
    Operation {
        name: "deep_decode",
        inputs: DEEP_TEXT,
        closure: r#"() => {
    var t = 0
    for _ in 0..200 {
        let p: json.Json = json.decode(dtext).unwrap()
        t += [p].len()
    }
    t
}"#,
        runs_per_call: Some(200),
    },
];

/// Times the closure `operation` and prints `ns_per_item check`.
const TIMING: &str = r#"
var best = -1
var check = 0
for _ in 0..{reps} {
    let sw = time.Stopwatch()
    check = operation()
    let t = sw.elapsed().nanos()
    if best < 0 or t < best {
        best = t
    }
}
print("\(best / {items}) \(check)")
"#;

/// Prints a `check` line for each property that the benchmark relies on.
const CHECKS: &str = r#"
let doc = make({n})
let text = json.encode(doc)
let p: json.Json = json.decode(text).unwrap()
let t: Doc = json.decode(text).unwrap()
let pretty: Doc = json.decode(json.encode(doc, indent: 2)).unwrap()
print("check json.decode(text) == json.to_json(doc): \(p == json.to_json(doc))")
print("check json.decode(text) == doc: \(t == doc)")
print("check json.decode(indented text) == doc: \(pretty == doc)")
print("check host.json_members(json.to_json(doc)) == 1: \(host.json_members(json.to_json(doc)) == 1)")
print("check host.copied_fields(doc) == 1: \(host.copied_fields(doc) == 1)")
print("check text length: \(text.len())")
"#;

/// Returns the module `host`. Its function `copied_fields` copies a struct out
/// of the VM with [`Vm::to_value`] and returns the number of fields of the
/// copy. Its function `json_members` takes a `Json` object as a Rust [`Json`]
/// and returns the number of its members. Its function `json_echo` takes a
/// `Json` value as a Rust [`Json`] and returns it as a Sand `Json` value.
fn host_modules() -> HostModules {
    let mut host = NativeModule::new("host");
    host.source("pub native fn copied_fields<T>(value: T) -> int");
    host.function("copied_fields", |vm: &mut Vm, value: Param<0>| -> Result<i64, Fail> {
        let fields = match vm.to_value(value.raw())? {
            Value::Struct(value) => value.fields.len(),
            _ => 0,
        };
        Ok(fields as i64)
    });
    host.function("json_echo", |value: Json| -> Json { value });
    host.function("json_members", |value: Json| -> i64 {
        match value {
            Json::Obj(members) => members.len() as i64,
            _ => 0,
        }
    });
    HostModules::new([host]).unwrap()
}

/// Returns the program that declares `operation`, the closure of `op`, for a
/// document of `users` users.
fn program(op: &Operation, users: usize) -> String {
    format!("{PRELUDE}{}let operation = {}\n", op.inputs.replace("{n}", &users.to_string()), op.closure)
}

/// Returns the operations that `BENCH_OPS` names, or all operations.
fn selected_operations() -> impl Iterator<Item = &'static Operation> {
    let names = std::env::var("BENCH_OPS").unwrap_or_default();
    let names: Vec<String> = names.split(',').filter(|name| !name.is_empty()).map(str::to_string).collect();
    OPERATIONS.iter().filter(move |op| names.is_empty() || names.iter().any(|name| name == op.name))
}

fn run(src: &str, limits: Limits) -> Result<String, Error> {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().limits(limits).host_modules(host_modules()).stdout(out.clone()));
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
    println!("charged memory (KiB) and fuel for one call of each operation on {users} users, beyond the setup");
    println!("{:<14} {:>12} {:>14}", "operation", "memory KiB", "fuel");
    for op in selected_operations() {
        let setup = program(op, users);
        let (base_memory, base_fuel) = charged(&setup);
        let (memory, fuel) = charged(&format!("{setup}let r = operation()\n"));
        println!("{:<14} {:>12} {:>14}", op.name, memory.saturating_sub(base_memory) >> 10, fuel.saturating_sub(base_fuel));
    }
}

fn main() {
    if std::env::args().any(|a| a == "--limits") {
        return limits_report();
    }
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let (users, runs, reps) = if quick { (200, 1, 1) } else { (env("BENCH_USERS", 5000), env("BENCH_RUNS", 3), env("BENCH_REPS", 5)) };
    let checks = CHECKS.replace("{n}", &users.to_string());
    let checks = run(&format!("{PRELUDE}{checks}"), Limits::default()).unwrap_or_else(|e| panic!("checks: {e}"));
    assert!(!checks.is_empty(), "the program printed no checks");
    for check in checks.lines() {
        assert!(check.starts_with("check ") && !check.ends_with("false"), "{check}");
    }
    println!("best ns per item ({users} users, {runs} runs of {reps} repetitions)");
    println!("{:<14} {:>10}", "operation", "ns");
    for op in selected_operations() {
        let items = op.runs_per_call.unwrap_or(users);
        let timing = TIMING.replace("{reps}", &reps.to_string()).replace("{items}", &items.to_string());
        let src = format!("{}{timing}", program(op, users));
        let ns_per_item = |_| {
            let out = run(&src, Limits::default()).unwrap_or_else(|e| panic!("{}: {e}", op.name));
            let ns = out.split(' ').next().and_then(|ns| ns.parse::<u64>().ok());
            ns.unwrap_or_else(|| panic!("{}: unexpected output {out:?}", op.name))
        };
        println!("{:<14} {:>10}", op.name, (0..runs).map(ns_per_item).min().unwrap_or_default());
    }
    print!("{checks}");
}
