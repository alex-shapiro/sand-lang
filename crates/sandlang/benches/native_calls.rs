//! Benchmarks for async native functions that call Sand functions.
//!
//! A prototype codec walker encodes `BENCH_ROWS` rows (default 5000) with a
//! type schema. Each row has one `Point` field. The walker runs in four forms:
//!
//! - `sync`: a recursive native walker, as on `main`.
//! - `async_derived`: an async native call that walks derived subtrees with
//!   the synchronous walker inside one `NativeContext::with_vm` call.
//! - `async_per_node`: an async walker with one `with_vm` call and one boxed
//!   future for each node, without the derived-subtree shortcut.
//! - `async_custom`: the async walker with a custom `Point` codec in Sand.
//!
//! `sand_hook` times the custom `Point` codec when Sand code calls it
//! directly. `native_hop` times one call of an identity closure from an async
//! native loop, and `sand_hop` times the same call from a Sand loop. Each operation reports the fastest time per row across
//! `BENCH_RUNS` engines (default 3) of `BENCH_REPS` repetitions (default 5).
//!
//! `cargo test --benches` runs each benchmark once and checks that every form
//! produces the same text.

use sandlang::engine::{Config, Engine, Error, HostModules, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::context::{NativeContext, RootedValue};
use sandlang::native::func::Later;
use sandlang::native::ty::{Param, VmVal};
use sandlang::vm::value::{Obj, VmValue};
use sandlang::vm::{Fail, Vm, WaitKind};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const PRELUDE: &str = r#"import time
import walk
struct Point { x: int, y: int }
struct Row { id: int, name: str, tags: [str], point: Point }
fn make(n: int) -> [Row] {
    let rows: [Row] = []
    for i in 0..n {
        rows.push(Row(id: i, name: "row \(i)", tags: ["a", "b\(i % 5)"], point: Point(x: i, y: i * 2)))
    }
    rows
}
fn point_text(p: Point) -> str { "{\(p.x),\(p.y)}" }
let rows = make({n})
"#;

/// Times the closure `operation` and prints `ns_per_row check`.
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
print("\(best / rows.len()) \(check)")
"#;

const OPERATIONS: &[(&str, &str)] = &[
    ("sync", "() => walk.encode_sync(rows).len()"),
    ("async_derived", "() => walk.encode_async(rows).len()"),
    ("async_per_node", "() => walk.encode_per_node(rows).len()"),
    ("async_custom", "() => walk.encode_custom(rows, point_text).len()"),
    (
        "sand_hook",
        r#"() => {
    var t = 0
    for r in rows { t += point_text(r.point).len() }
    t
}"#,
    ),
    ("native_hop", "() => walk.call_n((x) => x, 1, rows.len())"),
    (
        "sand_hop",
        r#"() => {
    let f = (x) => x
    var t = 0
    for _ in 0..rows.len() { t += f(1) }
    t
}"#,
    ),
];

/// Prints a `check` line for each property that the benchmark relies on.
const CHECKS: &str = r#"
let text = walk.encode_sync(rows)
print("check async_derived: \(walk.encode_async(rows) == text)")
print("check async_per_node: \(walk.encode_per_node(rows) == text)")
print("check async_custom: \(walk.encode_custom(rows, point_text) == text)")
print("check text length: \(text.len())")
"#;

/// Type schema of a value that the walker encodes.
enum Schema {
    Int,
    Str,
    List(Box<Schema>),
    Struct(Vec<Schema>),
    /// Encoded by the Sand hook.
    Custom,
}

impl Schema {
    /// The schema of the benchmark rows.
    fn rows(custom_point: bool) -> Schema {
        let point = if custom_point { Schema::Custom } else { Schema::Struct(vec![Schema::Int, Schema::Int]) };
        Schema::List(Box::new(Schema::Struct(vec![Schema::Int, Schema::Str, Schema::List(Box::new(Schema::Str)), point])))
    }

    /// Whether the schema has no custom codec.
    fn is_derived(&self) -> bool {
        match self {
            Schema::Int | Schema::Str => true,
            Schema::List(item) => item.is_derived(),
            Schema::Struct(fields) => fields.iter().all(Schema::is_derived),
            Schema::Custom => false,
        }
    }
}

fn list_items(vm: &Vm, v: VmValue) -> &[VmValue] {
    match vm.heap.get(v) {
        Obj::List(items) => items,
        _ => &[],
    }
}

/// Encodes the derived value `v` into `out`.
fn encode_derived(vm: &Vm, v: VmValue, schema: &Schema, out: &mut String) -> Result<(), Fail> {
    match schema {
        Schema::Int => out.push_str(itoa(v).as_str()),
        Schema::Str => {
            out.push('"');
            out.push_str(vm.str(v));
            out.push('"');
        }
        Schema::List(item) => {
            out.push('[');
            for (i, &x) in list_items(vm, v).iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_derived(vm, x, item, out)?;
            }
            out.push(']');
        }
        Schema::Struct(fields) => {
            out.push('{');
            for (i, field) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_derived(vm, vm.heap.field(v, i).unwrap_or(VmValue::Unit), field, out)?;
            }
            out.push('}');
        }
        Schema::Custom => return Err(Fail::internal("a custom codec in a derived walk")),
    }
    Ok(())
}

fn itoa(v: VmValue) -> String {
    match v {
        VmValue::Int(i) => i.to_string(),
        _ => String::new(),
    }
}

/// Encodes the rooted value `v` into `out`, calling `hook` for custom values.
/// Walks a derived subtree inside one `with_vm` call unless `per_node`.
fn encode_async<'a>(
    cx: &'a NativeContext,
    v: RootedValue,
    schema: &'a Schema,
    hook: Option<RootedValue>,
    per_node: bool,
    out: &'a mut String,
) -> Pin<Box<dyn Future<Output = Result<(), Fail>> + Send + 'a>> {
    Box::pin(async move {
        if !per_node && schema.is_derived() || matches!(schema, Schema::Int | Schema::Str) {
            return cx.with_vm(|vm| encode_derived(vm, cx.value(v), schema, out));
        }
        match schema {
            Schema::Custom => {
                let hook = hook.ok_or_else(|| Fail::internal("no hook"))?;
                let text = cx.call(hook, &[v]).await;
                cx.with_vm(|vm| out.push_str(vm.str(cx.value(text))));
            }
            Schema::List(item) => {
                let items: Vec<RootedValue> = cx.with_vm(|vm| list_items(vm, cx.value(v)).iter().map(|&x| cx.root(x)).collect());
                out.push('[');
                for (i, x) in items.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    encode_async(cx, x, item, hook, per_node, out).await?;
                }
                out.push(']');
            }
            Schema::Struct(fields) => {
                out.push('{');
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let x = cx.with_vm(|vm| cx.root(vm.heap.field(cx.value(v), i).unwrap_or(VmValue::Unit)));
                    encode_async(cx, x, field, hook, per_node, out).await?;
                }
                out.push('}');
            }
            Schema::Int | Schema::Str => unreachable!("encoded above"),
        }
        Ok(())
    })
}

/// Returns a `Later` that encodes `rows` with `schema` on an async walker.
fn encode_later(rows: VmValue, schema: Schema, hook: Option<VmValue>, per_node: bool) -> Result<Later<String, Fail>, Fail> {
    let schema = Arc::new(schema);
    Ok(Later::new(WaitKind::Io, move |cx| {
        let rows = cx.root(rows);
        let hook = hook.map(|hook| cx.root(hook));
        async move {
            let mut out = String::new();
            encode_async(&cx, rows, &schema, hook, per_node, &mut out).await?;
            Ok(out)
        }
    }))
}

fn host_modules() -> HostModules {
    let mut walk = NativeModule::new("walk");
    walk.source(
        "pub native fn encode_sync<T>(rows: T) -> str\n\
         pub native fn encode_async<T>(rows: T) -> str\n\
         pub native fn encode_per_node<T>(rows: T) -> str\n\
         pub native fn encode_custom<T, P>(rows: T, hook: fn(P) -> str) -> str\n\
         pub native fn call_n(f: fn(int) -> int, x: int, n: int) -> int",
    );
    walk.function("encode_sync", |vm: &mut Vm, rows: Param<0>| -> Result<String, Fail> {
        let mut out = String::new();
        encode_derived(vm, rows.raw(), &Schema::rows(false), &mut out)?;
        Ok(out)
    });
    walk.function("encode_async", |rows: Param<0>| encode_later(rows.raw(), Schema::rows(false), None, false));
    walk.function("encode_per_node", |rows: Param<0>| encode_later(rows.raw(), Schema::rows(false), None, true));
    walk.function("encode_custom", |rows: Param<0>, hook: VmVal<fn(Param<1>) -> String>| {
        encode_later(rows.raw(), Schema::rows(true), Some(hook.raw()), false)
    });
    walk.function("call_n", |f: VmVal<fn(i64) -> i64>, x: i64, n: i64| -> Result<Later<i64, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, move |cx| {
            let (f, x) = (cx.root(f.raw()), cx.root(VmValue::Int(x)));
            async move {
                let mut sum = 0;
                for _ in 0..n {
                    let y = cx.call(f, &[x]).await;
                    if let VmValue::Int(y) = cx.value(y) {
                        sum += y;
                    }
                }
                Ok(sum)
            }
        }))
    });
    HostModules::new([walk]).unwrap()
}

fn run_source(src: &str) -> Result<String, Error> {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().host_modules(host_modules()).stdout(out.clone()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("bench.sand", src))?;
    Ok(out.contents())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|n| n.parse().ok()).unwrap_or(default).max(1)
}

fn main() {
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let (rows, runs, reps) =
        if quick { (200, 1, 1) } else { (env_usize("BENCH_ROWS", 5000), env_usize("BENCH_RUNS", 3), env_usize("BENCH_REPS", 5)) };
    let prelude = PRELUDE.replace("{n}", &rows.to_string());
    let checks = run_source(&format!("{prelude}{CHECKS}")).unwrap_or_else(|e| panic!("checks: {e}"));
    for check in checks.lines() {
        assert!(check.starts_with("check ") && !check.ends_with("false"), "{check}");
    }
    println!("best ns per row ({rows} rows, {runs} runs of {reps} repetitions)");
    println!("{:<16} {:>8}", "operation", "ns");
    for (name, closure) in OPERATIONS {
        let timing = TIMING.replace("{reps}", &reps.to_string());
        let src = format!("{prelude}let operation = {closure}\n{timing}");
        let ns_per_row = |_| {
            let out = run_source(&src).unwrap_or_else(|e| panic!("{name}: {e}"));
            let ns = out.split(' ').next().and_then(|ns| ns.parse::<u64>().ok());
            ns.unwrap_or_else(|| panic!("{name}: unexpected output {out:?}"))
        };
        println!("{:<16} {:>8}", name, (0..runs).map(ns_per_row).min().unwrap_or_default());
    }
    print!("{checks}");
}
