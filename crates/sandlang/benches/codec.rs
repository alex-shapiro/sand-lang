//! Benchmarks for the `json` module and native parameters of type `Encoded<N>`.
//!
//! `cargo bench --bench codec` reports the fastest time per operation across
//! `BENCH_RUNS` engine runs. The default is 3 runs. Each operation runs in its
//! own engine instance. Each run executes the operation `BENCH_REPS` times and
//! keeps the fastest repetition. The default is 5 repetitions. The test
//! document contains `BENCH_USERS` users, which defaults to 5000. Set
//! `BENCH_OPS=encode,decode` to filter operations. Set `BENCH_FUEL=1` to print
//! the fuel of one call of each operation, in total and per item, instead of
//! its time.
//!
//! `cargo test --benches` runs each benchmark once and verifies its output.

use sandlang::engine::{Config, Engine, Error, HostModules, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::ty::Encoded;
use sandlang::stdlib::json::Json;
use sandlang::value::Value;

/// Declares the benchmark document types and `make(n)`, which builds a document
/// with `n` users.
const PRELUDE: &str = r#"import json
import time
import host
enum Status { Active, Suspended(reason: str), Banned(int, str) }
struct Address { street: str, city: str, zip: str? }
struct User { id: int, name: str, email: str, score: float, active: bool, tags: [str], address: Address, status: Status, logins: [int], meta: [str: int] }
struct Doc { users: [User] }
struct Email {
    address: str

    fn init(address: str) -> Result<Email> {
        if not address.contains("@") {
            return Err(Error("invalid email", kind: "invalid"))
        }
        Ok(Self(address: address))
    }
}
impl Encode for Email {
    fn encode(self, e: Encoder) {
        e.encode(self.address)
    }
}
impl Decode for Email {
    fn decode(d: Decoder) -> Result<Email> {
        Email(d.decode()?)
    }
}
struct Contact { id: int, name: str, email: Email, tags: [str] }
struct Page<T> { items: [T], next: str? }
var stamp_counter = 0
fn stamp() -> int {
    stamp_counter += 1
    stamp_counter
}
struct Stamped { id: int, stamp: int = stamp() }
fn load<T: Decode>(text: str) -> Result<T> { json.decode(text) }
fn contacts(n: int) -> [Contact] {
    let out: [Contact] = []
    for i in 0..n {
        out.push(Contact(id: i, name: "c \(i)", email: Email("c\(i)@example.com").unwrap(), tags: ["x", "y\(i % 3)"]))
    }
    out
}
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
    /// Sand statements that initialize the inputs. `operation_source` replaces
    /// `{n}` with the user count.
    inputs: &'static str,
    /// Sand closure of type `fn() -> int` that executes the operation.
    closure: &'static str,
    /// Iterations per call of `closure`, or `None` if `closure` processes a
    /// full document once.
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
    Operation { name: "encode_custom", inputs: "let cs = contacts({n})\n", closure: "() => json.encode(cs).len()", runs_per_call: None },
    Operation {
        name: "decode_custom",
        inputs: "let ctext = json.encode(contacts({n}))\n",
        closure: r#"() => {
    let cs: [Contact] = json.decode(ctext).unwrap()
    cs[cs.len() - 1].id
}"#,
        runs_per_call: None,
    },
    Operation {
        name: "decode_generic",
        inputs: DOC_TEXT,
        closure: r#"() => {
    let d: Doc = load(text).unwrap()
    d.users[d.users.len() - 1].id
}"#,
        runs_per_call: None,
    },
    Operation {
        name: "decode_page",
        inputs: "let ptext = json.encode(Page(items: make({n}).users, next: None))\n",
        closure: r#"() => {
    let p: Page<User> = json.decode(ptext).unwrap()
    p.items[p.items.len() - 1].id
}"#,
        runs_per_call: None,
    },
    Operation {
        name: "decode_ints",
        inputs: "let itext = json.encode((0..({n} * 10)).to_list())\n",
        closure: r#"() => {
    let xs: [int] = json.decode(itext).unwrap()
    xs[xs.len() - 1]
}"#,
        runs_per_call: None,
    },
    Operation {
        name: "decode_ints_generic",
        inputs: "let itext = json.encode((0..({n} * 10)).to_list())\n",
        closure: r#"() => {
    let xs: [int] = load(itext).unwrap()
    xs[xs.len() - 1]
}"#,
        runs_per_call: None,
    },
    Operation {
        name: "small_decode_default",
        inputs: "let stext = \"{\\\"id\\\": 7}\"\n",
        closure: r#"() => {
    var t = 0
    for _ in 0..20000 {
        let s: Stamped = json.decode(stext).unwrap()
        t += s.id
    }
    t
}"#,
        runs_per_call: Some(20000),
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

/// Prints the fuel that one call of the closure `operation` uses.
const FUEL: &str = r#"
let fuel_before = host.fuel()
let check = operation()
print("\(host.fuel() - fuel_before) \(check)")
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

/// Builds the `host` module with helper functions for `Encoded<N>`, fuel, and
/// `Json` conversion benchmarks.
fn host_modules() -> HostModules {
    let mut host = NativeModule::new("host");
    host.source("pub native fn copied_fields<T: Encode>(value: T) -> int");
    host.function("copied_fields", |value: Encoded<0>| -> i64 {
        match value.value() {
            Value::Struct(value) => value.fields.len() as i64,
            _ => 0,
        }
    });
    host.source("pub native fn fuel() -> int");
    host.function("fuel", |vm: &mut sandlang::vm::Vm| vm.meter.used() as i64);
    host.function("json_echo", |value: Json| -> Json { value });
    host.function("json_members", |value: Json| -> i64 {
        match value {
            Json::Obj(members) => members.len() as i64,
            _ => 0,
        }
    });
    HostModules::new([host]).unwrap()
}

/// Builds the Sand source for `op` with `users` users.
fn operation_source(op: &Operation, users: usize) -> String {
    format!("{PRELUDE}{}let operation = {}\n", op.inputs.replace("{n}", &users.to_string()), op.closure)
}

/// Returns the operations selected by `BENCH_OPS`, or all operations if
/// `BENCH_OPS` is unset.
fn selected_operations() -> impl Iterator<Item = &'static Operation> {
    let names = std::env::var("BENCH_OPS").unwrap_or_default();
    let names: Vec<String> = names.split(',').filter(|name| !name.is_empty()).map(str::to_string).collect();
    OPERATIONS.iter().filter(move |op| names.is_empty() || names.iter().any(|name| name == op.name))
}

fn run_source(src: &str, limits: Limits) -> Result<String, Error> {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().limits(limits).host_modules(host_modules()).stdout(out.clone()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("bench.sand", src))?;
    Ok(out.contents())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|n| n.parse().ok()).unwrap_or(default).max(1)
}

fn main() {
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let (users, runs, reps) =
        if quick { (200, 1, 1) } else { (env_usize("BENCH_USERS", 5000), env_usize("BENCH_RUNS", 3), env_usize("BENCH_REPS", 5)) };
    let checks = CHECKS.replace("{n}", &users.to_string());
    let checks = run_source(&format!("{PRELUDE}{checks}"), Limits::default()).unwrap_or_else(|e| panic!("checks: {e}"));
    assert!(!checks.is_empty(), "expected check output from benchmark prelude");
    for check in checks.lines() {
        assert!(check.starts_with("check ") && !check.ends_with("false"), "{check}");
    }
    println!("best ns per item ({users} users, {runs} runs of {reps} repetitions)");
    println!("{:<14} {:>10}", "operation", "ns");
    for op in selected_operations() {
        let items = op.runs_per_call.unwrap_or(users);
        if std::env::var("BENCH_FUEL").is_ok() {
            let out = run_source(&format!("{}{FUEL}", operation_source(op, users)), Limits::default())
                .unwrap_or_else(|e| panic!("{}: {e}", op.name));
            let fuel: u64 = out.split(' ').next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{}: {out:?}", op.name));
            println!("{:<20} {:>10} {:>10.1}", op.name, fuel, fuel as f64 / items as f64);
            continue;
        }
        let timing = TIMING.replace("{reps}", &reps.to_string()).replace("{items}", &items.to_string());
        let src = format!("{}{timing}", operation_source(op, users));
        let ns_per_item = |_| {
            let out = run_source(&src, Limits::default()).unwrap_or_else(|e| panic!("{}: {e}", op.name));
            let ns = out.split(' ').next().and_then(|ns| ns.parse::<u64>().ok());
            ns.unwrap_or_else(|| panic!("{}: unexpected output {out:?}", op.name))
        };
        println!("{:<14} {:>10}", op.name, (0..runs).map(ns_per_item).min().unwrap_or_default());
    }
    print!("{checks}");
}
