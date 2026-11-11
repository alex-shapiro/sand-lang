//! Acceptance tests for the embedding API, sandbox policy, and resource limits.
// Helpers of the tests that need the host (feature `os`) are unused without it.
#![cfg_attr(not(feature = "os"), allow(unused_imports, dead_code))]

mod common;

use common::{TempDir, literal_path, rt, run_with};
use jiff::civil::Date;
use parking_lot::Mutex;
use sandlang::engine::{CompiledProgram, Config, Engine, Error, HostModules, InterruptHandle, LimitKind, Limits, MemorySink};
use sandlang::files::mem::MemFs;
use sandlang::native::NativeModule;
use sandlang::native::convert::{Conversion, FromVmValue, IntoVmValue};
use sandlang::native::ty::{SandType, Type};
use sandlang::policy::rules::{PathPattern, RulePolicy};
use sandlang::policy::{Capability, Decision, PromptAnswer};
use sandlang::session::Session;
use sandlang::stdlib::log::{LogLevel, LogRecord, LogSink};
use sandlang::stdlib::time::TimeZone;
use sandlang::vm::value::{ValueType, VmValue};
use sandlang::vm::{Fail, Vm};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn engine_with(config: Config) -> (Engine, MemorySink, MemorySink) {
    let out = MemorySink::new();
    let err = MemorySink::new();
    let engine = Engine::new(config.stdout(out.clone()).stderr(err.clone()));
    (engine, out, err)
}

fn run_src(config: Config, src: &str) -> (Result<sandlang::engine::Outcome, Error>, String, String) {
    let (out, err) = (MemorySink::new(), MemorySink::new());
    let mut engine = Engine::new(config.stdout(out.clone()).stderr(err.clone()));
    let r = rt().block_on(engine.run_source("main.sand", src));
    (r, out.contents(), err.contents())
}

/// Returns the compile error messages for `modules`.
fn host_module_errors(modules: impl IntoIterator<Item = NativeModule>) -> Vec<String> {
    match HostModules::new(modules) {
        Err(Error::Compile(ds)) => ds.into_iter().map(|d| d.message).collect(),
        Err(e) => panic!("expected a compile error, got {e:?}"),
        Ok(_) => panic!("expected a compile error"),
    }
}

// ---------------------------------------------------------------------------
// Basic embedding
// ---------------------------------------------------------------------------

#[test]
fn engine_and_run_future_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let mut engine = Engine::new(Config::new());
    assert_send(&engine);
    let fut = engine.run_source("main.sand", "print(1)");
    assert_send(&fut);
}

#[test]
fn run_source_and_exit_code() {
    let (r, out, _) = run_src(Config::new(), "print(\"hi\")\nexit(4)");
    assert_eq!(r.unwrap().exit_code, 4);
    assert_eq!(out, "hi\n");
    let (r, _, _) = run_src(Config::new(), "let x = 1");
    assert_eq!(r.unwrap().exit_code, 0);
}

#[test]
fn compile_errors_are_reported_as_diagnostics() {
    let (r, _, _) = run_src(Config::new(), "let x: int = \"s\"\nlet y = nope");
    match r {
        Err(Error::Compile(diags)) => {
            assert_eq!(diags.len(), 2);
            assert_eq!(diags[0].line, 1);
            assert_eq!(diags[0].file, "main.sand");
            assert!(diags[0].message.contains("type mismatch"));
            let rendered = diags[0].render();
            assert!(rendered.contains("--> main.sand:1:14"), "{rendered}");
            assert!(rendered.contains("^^^"), "{rendered}");
        }
        other => panic!("expected compile error, got {other:?}"),
    }
}

#[test]
fn a_failed_call_reports_nothing_about_the_lambdas_after_it() {
    let src = "let counts = [\"a\": 2]\nlet top = counts\n    .map((k, v) => (k, v))\n    .sorted_by(t => (-t.1, t.0))\n    .take(10)";
    match Engine::new(Config::new()).check_source("main.sand", src) {
        Err(Error::Compile(diags)) => {
            let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
            assert_eq!(messages, ["no method `map` on type [str: int]; call it on `entries()`, a list of (key, value) pairs"]);
        }
        other => panic!("expected compile error, got {other:?}"),
    }
}

#[test]
fn an_undefined_name_reports_nothing_about_the_types_it_hides() {
    let src = "fn f() -> Result<int> {\n    let p = jsn.parse(\"[]\")?\n    let Some(xs) = p.get(\"a\").and_then(x => x.as_list()) else {\n        return Ok(0)\n    }\n    for x in xs {\n        print(x)\n    }\n    Ok(1)\n}\nlet rs = nope([1])\nfor r in rs {\n    print(r?)\n}";
    match Engine::new(Config::new()).check_source("main.sand", src) {
        Err(Error::Compile(diags)) => {
            let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
            assert_eq!(messages, ["undefined name `nope`", "undefined name `jsn`"]);
        }
        other => panic!("expected compile error, got {other:?}"),
    }
}

#[test]
fn check_source_does_not_run() {
    let engine = Engine::new(Config::new());
    engine.check_source("main.sand", "print(1)\nlet x: int = 2").unwrap();
    let err = engine.check_source("main.sand", "let x: int = true").unwrap_err();
    assert!(matches!(err, Error::Compile(_)));
}

#[test]
fn deferred_code_copies_are_bounded() {
    // Each `defer` is compiled into every exit of its block: 1500 defers and
    // 1500 returns would be millions of copies, so compilation stops early.
    let mut src = String::from("fn f(n: int) -> int {\n");
    for i in 0..1500 {
        src.push_str(&format!("    defer print(\"{i}\")\n    if n == {i} {{ return {i} }}\n"));
    }
    src.push_str("    0\n}\nprint(f(3))\n");
    let start = std::time::Instant::now();
    let (r, _, _) = run_src(Config::new(), &src);
    match r {
        Err(Error::Compile(d)) => assert!(d.iter().any(|d| d.message.contains("too much deferred code")), "{d:?}"),
        other => panic!("expected a compile error, got {other:?}"),
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn json_decode_defaults_appear_in_stack_traces() {
    let src =
        "import json\nfn boom() -> int { fail(\"boom\") }\nstruct S {\n    a: int = boom()\n}\nlet s: Result<S> = json.decode(\"{}\")\n";
    let (r, _, _) = run_src(Config::new(), src);
    let Err(Error::Runtime { trace, .. }) = r else { panic!("expected a runtime error, got {r:?}") };
    let frames: Vec<(&str, usize)> = trace.iter().map(|f| (f.function.as_str(), f.line)).collect();
    assert_eq!(frames[..2], [("boom", 2), ("S.a", 4)], "{trace:?}");
    assert_eq!(frames[2].0, "decode", "{trace:?}");
    assert_eq!(frames[3], ("<top level>", 6), "{trace:?}");
    assert_eq!(frames.len(), 4, "{trace:?}");
}

#[test]
fn json_decode_of_deep_options_runs_on_a_small_stack() {
    let deep = format!("int{}", "?".repeat(9000));
    let src = format!(
        "import json\nlet a: {deep} = json.decode(\"1\").unwrap()\nprint(json.encode(a))\nlet b: Result<[{deep}]> = json.decode(\"[{{}}]\")\n\
         print(b.unwrap_err().message)\nlet c: Result<[{deep}: str]> = json.decode(\"{{\\\"1\\\": \\\"a\\\"}}\")\nprint(c.unwrap_err().message)\n"
    );
    let run = move || run_src(Config::new(), &src);
    let (r, out, err) = std::thread::Builder::new().stack_size(512 << 10).spawn(run).unwrap().join().unwrap();
    assert!(r.is_ok(), "{r:?} {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[..2], ["1", "$[0]: expected int"]);
    assert!(lines[2].starts_with("$: cannot decode map keys of type int??") && lines[2].ends_with("?…"), "{}", lines[2]);
    assert!(lines[2].len() < 300, "{}", lines[2].len());
}

#[test]
fn required_traits_of_deep_types_are_checked_on_a_small_stack() {
    let deep = format!("(fn() -> int){}", "?".repeat(9000));
    let src = format!(
        "let a: {deep} = None\nprint(a == a)\nfn same<T: Hash>(x: T) -> T {{ x }}\nlet b: int{} = None\nprint(same(b) == b)\n",
        "?".repeat(9000)
    );
    let run = move || run_src(Config::new(), &src);
    let (r, _, _) = std::thread::Builder::new().stack_size(512 << 10).spawn(run).unwrap().join().unwrap();
    let Err(Error::Compile(ds)) = r else { panic!("{r:?}") };
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert!(ds[0].message.contains("does not implement `Eq`"), "{}", ds[0].message);
}

#[test]
fn runtime_errors_have_stack_traces() {
    // Non-tail calls, so every frame appears in the trace.
    let src = "fn a() -> int { fail(\"boom\") }\nfn b() -> int { a() + 1 }\nprint(b())\n";
    let (r, _, _) = run_src(Config::new(), src);
    match r {
        Err(Error::Runtime { message, trace }) => {
            assert!(message.contains("boom"));
            let lines: Vec<usize> = trace.iter().map(|f| f.line).collect();
            assert_eq!(lines, vec![1, 2, 3], "{trace:?}");
            assert_eq!(trace[0].function, "a");
            assert_eq!(trace[0].file, "main.sand");
        }
        other => panic!("expected runtime error, got {other:?}"),
    }
}

#[test]
fn init_frames_are_named_after_their_type() {
    let src = "struct Pos {\n    x: int\n    fn init(x: int) -> Pos {\n        if x < 0 { fail(\"negative\") }\n        Pos(x: x)\n    }\n}\nfn make(x: int) -> Pos {\n    let p = Pos(x)\n    p\n}\nprint(make(1))\nprint(make(-1))\n";
    for optimize in [true, false] {
        let (r, _, _) = run_src(Config::new().optimize(optimize), src);
        match r {
            Err(Error::Runtime { trace, .. }) => {
                let names: Vec<&str> = trace.iter().map(|f| f.function.as_str()).collect();
                assert_eq!(names, ["Pos", "make", "<top level>"], "{trace:?}");
            }
            other => panic!("expected runtime error, got {other:?}"),
        }
    }
}

#[test]
fn engine_can_run_multiple_sources_independently() {
    let (mut engine, out, _) = engine_with(Config::new());
    let rt = rt();
    rt.block_on(engine.run_source("a.sand", "var x = 1\nprint(x)")).unwrap();
    rt.block_on(engine.run_source("b.sand", "print(2)")).unwrap();
    assert_eq!(out.contents(), "1\n2\n");
}

#[test]
fn script_args_and_env_are_virtual() {
    let config = Config::new()
        .policy(sandlang::policy::rules::RulePolicy::allow_all())
        .args(vec!["a".into(), "b".into()])
        .env(vec![("ONLY".into(), "this".into())]);
    let (r, out, _) = run_src(config, "import env\nprint(env.args())\nprint(env.get(\"ONLY\"))\nprint(env.get(\"PATH\"))");
    r.unwrap();
    assert_eq!(out, "[\"a\", \"b\"]\nSome(\"this\")\nNone\n");
}

// ---------------------------------------------------------------------------
// Native modules
// ---------------------------------------------------------------------------

fn geo_module() -> NativeModule {
    let mut m = NativeModule::new("geo");
    m.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() }).params(&["a", "b"]).doc("Euclidean length of (a, b).");
    m.function("count", |s: &str| -> i64 { s.chars().count() as i64 });
    m.function("split", |s: String| -> Vec<String> { s.split(',').map(String::from).collect() });
    m.function("find", |xs: Vec<i64>, t: i64| -> Option<i64> { xs.iter().position(|&x| x == t).map(|i| i as i64) });
    m.function("checked", |a: i64| -> Result<i64, String> {
        if a >= 0 { Ok(a) } else { Err(format!("negative: {a}")) }
    });
    m.function("tally", |words: Vec<String>| -> HashMap<String, i64> {
        let mut m = HashMap::new();
        for w in words {
            *m.entry(w).or_insert(0) += 1;
        }
        m
    });
    m.function("pair", |a: i64, s: String| -> (i64, String) { (a * 2, s.to_uppercase()) });
    m.function("raw", |b: Vec<u8>| -> Vec<u8> { b.into_iter().rev().collect() });
    m.function("flag", |b: bool| -> bool { !b });
    m
}

#[test]
fn native_module_typed_functions() {
    let config = Config::new().host_modules(HostModules::new([geo_module()]).unwrap());
    let src = r#"
import geo
print(geo.dist(3.0, 4.0))
print(geo.count("héllo"))
print(geo.split("a,b"))
print(geo.find([5, 6, 7], 7))
print(geo.find([5], 1))
print(geo.checked(3))
print(geo.checked(-1).unwrap_err().message)
print(geo.tally(["a", "b", "a"])["a"])
print(geo.pair(21, "x"))
print(geo.raw(b"abc"))
print(geo.flag(false))
help(geo.dist)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(
        out,
        "5.0\n5\n[\"a\", \"b\"]\nSome(2)\nNone\nOk(3)\nnegative: -1\n2\n(42, \"X\")\nb\"cba\"\ntrue\nfn geo.dist(a: float, b: float) -> float\n\nEuclidean length of (a, b).\n"
    );
}

/// Cent count converted to and from a Sand `int`.
struct Cents(i64);

impl SandType for Cents {
    fn sand_type() -> Type {
        Type::Int
    }
}

impl FromVmValue for Cents {
    type Item<'a> = Cents;
    fn from_vm_value(vm: &Vm, value: VmValue, conv: &mut Conversion) -> Result<Cents, Fail> {
        i64::from_vm_value(vm, value, conv).map(Cents)
    }
}

impl IntoVmValue for Cents {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        self.0.into_vm_value(vm)
    }
}

#[test]
fn host_type_implements_vm_value_conversion() {
    let mut module = NativeModule::new("till");
    module.function("double", |cents: Cents| Cents(cents.0 * 2));
    module.function("refund", |cents: Cents| async move { Cents(-cents.0) });
    let config = Config::new().host_modules(HostModules::new([module]).unwrap());
    let (result, out, _) = run_src(config, "import till\nprint(till.double(21))\nprint(till.refund(5))");
    result.unwrap();
    assert_eq!(out, "42\n-5\n");
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, sandlang::SandValue)]
#[sand(module = "bank")]
struct Money(i64);

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

impl ValueType for Money {
    const ORDER: Option<fn(&Self, &Self) -> std::cmp::Ordering> = Some(Ord::cmp);
    fn parse(text: &str) -> Result<Self, String> {
        let (whole, cents) = text.split_once('.').ok_or("expected 0.00")?;
        Ok(Money(whole.parse::<i64>().map_err(|e| e.to_string())? * 100 + cents.parse::<i64>().map_err(|e| e.to_string())?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, sandlang::SandValue)]
#[sand(module = "bank")]
struct Cell(i64, i64);

impl std::fmt::Display for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0, self.1)
    }
}

impl ValueType for Cell {
    fn parse(text: &str) -> Result<Self, String> {
        let (a, b) = text.split_once(':').ok_or("expected a:b")?;
        Ok(Cell(a.parse().map_err(|_| "bad row")?, b.parse().map_err(|_| "bad column")?))
    }
}

fn bank_module() -> NativeModule {
    let mut m = NativeModule::new("bank");
    m.bind::<Money>().bind::<Cell>();
    m.function("cents", Money);
    m.function("sum", |xs: Vec<Money>| Money(xs.iter().map(|m| m.0).sum()));
    m.function("cell", |a: i64, b: i64| Cell(a, b));
    m.function("due", |d: Date, days: i64| d.checked_add(jiff::Span::new().days(days)).unwrap_or(d)).params(&["start", "days"]);
    m.function("opened", || jiff::Timestamp::UNIX_EPOCH);
    m.function("zone_of", |z: sandlang::stdlib::time::ZonedDateTime| z.zoned().time_zone().iana_name().map(String::from));
    m
}

#[test]
fn host_value_types_compare_hash_print_and_encode_by_value() {
    let config = Config::new().host_modules(HostModules::new([bank_module()]).unwrap());
    let src = r#"
import bank
import json
let xs = [bank.cents(250), bank.cents(1999), bank.cents(5)]
print(xs.sorted())
print(bank.sum(xs))
print(bank.cents(5) == bank.cents(5))
let seen = [bank.cents(5): "five"]
print(seen[bank.cents(5)])
print(json.encode(xs))
let back: [bank.Money] = json.decode("[\"1.50\"]")?
print(back)
let bad: Result<[bank.Money]> = json.decode("[\"x\"]")
print(bad.unwrap_err().message)
print(Set([bank.cell(1, 2), bank.cell(1, 2)]).len())
help(bank.Money)
help(bank.Cell)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(
        out,
        "[0.05, 2.50, 19.99]\n22.54\ntrue\nfive\n[\"2.50\",\"19.99\",\"0.05\"]\n[1.50]\n$[0]: expected 0.00\n1\ntype bank.Money\n\nImplements:\n    Eq\n    Ord\n    Hash\n    Encode\n    Decode\ntype bank.Cell\n\nImplements:\n    Eq\n    Hash\n    Encode\n    Decode\n"
    );
}

#[test]
fn host_functions_take_and_return_values_of_other_modules() {
    let config = Config::new().host_modules(HostModules::new([bank_module()]).unwrap());
    let src = r#"
import bank
import time
print(bank.due(time.PlainDate(2024, 2, 28)?, 2))
print(bank.opened())
print(bank.zone_of(time.now_zoned(time.TimeZone("Asia/Tokyo")?)))
help(bank.due)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "2024-03-01\n1970-01-01T00:00:00Z\nSome(\"Asia/Tokyo\")\nfn bank.due(start: PlainDate, days: int) -> PlainDate\n");
}

#[test]
fn host_functions_take_value_types_of_modules_added_later() {
    let mut ledger = NativeModule::new("ledger");
    ledger.function("fee", || Money(150));
    let (r, out, _) =
        run_src(Config::new().host_modules(HostModules::new([ledger, bank_module()]).unwrap()), "import ledger\nprint(ledger.fee())");
    r.unwrap();
    assert_eq!(out, "1.50\n");
}

#[test]
#[should_panic(expected = "belongs to module `bank`")]
fn value_types_belong_to_their_module() {
    NativeModule::new("other").bind::<Money>();
}

#[test]
fn host_sources_with_crlf_lines_import_referenced_modules() {
    let mut m = NativeModule::new("geo");
    m.source("//! Geometry.\r\n//! é\r\n");
    m.function("start", || Date::constant(2024, 1, 1));
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), "import geo\nprint(geo.start())\nhelp(geo)");
    r.unwrap();
    assert!(out.starts_with("2024-01-01\nmodule geo\n\nGeometry.\né\n"), "{out}");
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, sandlang::SandValue)]
#[sand(module = "bank", name = "Money")]
struct Impostor;

impl std::fmt::Display for Impostor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("0.00")
    }
}

impl ValueType for Impostor {
    fn parse(_: &str) -> Result<Self, String> {
        Ok(Impostor)
    }
}

#[test]
fn value_types_in_signatures_must_be_registered() {
    let mut m = NativeModule::new("bank");
    m.function("zero", || Money(0));
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("native `bank.zero` uses `bank.Money`, which module `bank` does not declare"), "{errors:?}");

    let mut m = NativeModule::new("ledger");
    m.function("fee", || Money(150));
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["the Rust function of native `ledger.fee` uses `bank.Money`, and module `bank` is not added to the engine"]);
}

#[test]
fn value_types_in_signatures_must_be_the_registered_rust_type() {
    let mut m = bank_module();
    m.function("fake", || Impostor);
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("uses a Rust type that is not the one bound to `bank.Money`"), "{errors:?}");
}

/// A plane figure.
#[derive(Clone, Copy, Debug, PartialEq, sandlang::SandEnum)]
#[sand(module = "shapes")]
enum Shape {
    /// Round.
    Circle,
    Square,
    /// Three-sided.
    Triangle,
}

fn shapes_module() -> NativeModule {
    let mut m = NativeModule::new("shapes");
    m.bind::<Shape>();
    m.function("sides", |s: Shape| match s {
        Shape::Circle => 0,
        Shape::Square => 4,
        Shape::Triangle => 3,
    });
    m.function("with_sides", |n: i64| match n {
        0 => Some(Shape::Circle),
        3 => Some(Shape::Triangle),
        4 => Some(Shape::Square),
        _ => None,
    });
    m.function("count", |shapes: Vec<Shape>, of: Shape| shapes.iter().filter(|&&s| s == of).count() as i64).params(&["shapes", "of"]);
    m
}

#[test]
fn host_functions_take_and_return_enums() {
    let src = r#"
import shapes
print(shapes.sides(shapes.Shape.Square))
print(shapes.with_sides(3))
print(shapes.with_sides(5))
print(shapes.count([shapes.Shape.Circle, shapes.Shape.Square, shapes.Shape.Circle], of: shapes.Shape.Circle))
match shapes.with_sides(0) {
    Some(Circle) => print("round")
    _ => print("other")
}
help(shapes.count)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([shapes_module()]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "4\nSome(Triangle)\nNone\n2\nround\nfn shapes.count(shapes: [Shape], of: Shape) -> int\n");
}

#[test]
fn host_enums_take_their_docs_from_rust() {
    let (r, out, _) =
        run_src(Config::new().host_modules(HostModules::new([shapes_module()]).unwrap()), "import shapes\nhelp(shapes.Shape)");
    r.unwrap();
    assert_eq!(
        out,
        "enum shapes.Shape\n\nA plane figure.\n\nVariants:\n    Circle\n        Round.\n    Square\n    Triangle\n        Three-sided.\n\nImplements:\n    Eq\n    Ord\n    Hash\n    Encode\n    Decode\n"
    );
}

#[test]
fn host_functions_take_and_return_enums_of_standard_modules() {
    use sandlang::stdlib::time::enums::Unit;
    let mut m = NativeModule::new("units");
    m.function("smaller", |u: Unit| -> Option<Unit> {
        let all = [Unit::Year, Unit::Month, Unit::Week, Unit::Day, Unit::Hour];
        all.iter().position(|&x| x == u).and_then(|i| all.get(i + 1)).copied()
    });
    m.function("is_warning", |l: LogLevel| l >= LogLevel::Warn);
    let src = "import units\nimport time\nimport log\nprint(units.smaller(time.Unit.Week))\nprint(units.is_warning(log.Level.Error))\nhelp(units.smaller)";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "Some(Day)\ntrue\nfn units.smaller(arg1: Unit) -> Unit?\n");
}

#[test]
fn registered_types_cannot_be_declared_in_the_module_source() {
    let cases = [
        (shapes_module(), "pub enum Shape { Circle, Square, Triangle }"),
        (shapes_module(), "pub struct Shape { sides: int }"),
        (bank_module(), "pub enum Money { Cents }"),
        (bank_module(), "pub type Money = int"),
    ];
    for (mut m, decl) in cases {
        m.source(decl);
        let errors = host_module_errors([m]);
        assert!(errors[0].ends_with("is already defined"), "{decl}: {errors:?}");
    }
}

#[test]
fn native_types_cannot_be_declared_in_sand() {
    let mut m = NativeModule::new("bank");
    m.source("pub native type Money");
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("native types are declared by registering Rust types"), "{errors:?}");
}

#[test]
fn host_function_declarations_must_have_the_rust_types() {
    let cases = [
        ("pub native fn sides(s: str) -> int", "is declared `fn(str) -> int`, but its Rust function is `fn(Shape) -> int`"),
        ("pub native fn with_sides(n: int) -> Shape", "is declared `fn(int) -> Shape`, but its Rust function is `fn(int) -> Shape?`"),
        (
            "pub native fn count(shapes: [Shape]) -> int",
            "is declared `fn([Shape]) -> int`, but its Rust function is `fn([Shape], Shape) -> int`",
        ),
    ];
    for (decl, msg) in cases {
        let mut m = shapes_module();
        m.source(decl);
        let r = HostModules::new([m]);
        let Err(Error::Compile(ds)) = r else { panic!("{decl}: expected a compile error, got {r:?}") };
        assert!(ds[0].message.contains(msg), "{decl}: {ds:?}");
    }
    let mut m = shapes_module();
    m.source("pub native fn sides(shape: Shape = Shape.Square) -> int");
    let (r, out, _) = run_src(
        Config::new().host_modules(HostModules::new([m]).unwrap()),
        "import shapes\nprint(shapes.sides())\nprint(shapes.sides(shape: shapes.Shape.Circle))",
    );
    r.unwrap();
    assert_eq!(out, "4\n0\n");
}

#[test]
fn registered_methods_are_declared_from_their_rust_types() {
    let mut m = shapes_module();
    m.function("Shape.rank", |s: Shape, base: i64| base + s as i64).params(&["base"]);
    m.function("Shape.first", || Shape::Circle);
    let src = "import shapes\nprint(shapes.Shape.Square.rank(base: 10))\nprint(shapes.Shape.first())";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "11\nCircle\n");

    let mut m = shapes_module();
    m.function("Ghost.rank", |s: Shape| s as i64);
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["native `shapes.Ghost.rank` is a method of `Ghost`, which is not a type of module `shapes`"]);

    let mut m = shapes_module();
    m.source("pub struct Box<T> {\n    pub item: T\n}");
    m.function("Box.size", || 1i64);
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["native `shapes.Box.size` is a method of the generic type `Box`, so the module must declare it with `native fn`"]);
}

#[test]
fn registered_functions_cannot_share_names_with_sand_items() {
    let mut m = NativeModule::new("tally");
    m.source("pub fn count() -> int { 0 }");
    m.function("count", || 1i64);
    let r = HostModules::new([m]);
    let Err(Error::Compile(ds)) = r else { panic!("expected a compile error, got {r:?}") };
    let msg = "`count` is also the name of the registered native `tally.count`; declare the native with `native fn count`";
    assert_eq!((ds[0].message.as_str(), ds[0].source_line.as_deref()), (msg, Some("pub fn count() -> int { 0 }")), "{ds:?}");
}

#[test]
fn derived_natives_take_named_arguments() {
    let (r, out, _) =
        run_src(Config::new().host_modules(HostModules::new([geo_module()]).unwrap()), "import geo\nprint(geo.dist(b: 4.0, a: 3.0))");
    r.unwrap();
    assert_eq!(out, "5.0\n");
}

#[test]
fn public_functions_of_host_modules_use_public_types() {
    let cases = [
        ("enum Color { Red }\npub native fn color() -> Color", "public function `color` uses the private type `Color`"),
        ("struct Hidden { x: int }\npub native fn color(h: [Hidden]) -> int", "public function `color` uses the private type `Hidden`"),
    ];
    for (src, msg) in cases {
        let mut m = shapes_module();
        m.source(src);
        m.function("color", || 0i64);
        let errors = host_module_errors([m]);
        assert!(errors.iter().any(|e| e.contains(msg)), "{src}: {errors:?}");
    }
}

#[test]
fn host_modules_can_take_the_names_of_prelude_types() {
    let mut m = NativeModule::new("str");
    m.function("shout", |s: String| s.to_uppercase());
    let (r, out, _) =
        run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), "import str\nprint(str.shout(\"hi\"))\nprint(\"hi\".len())");
    r.unwrap();
    assert_eq!(out, "HI\n2\n");
}

#[test]
fn natives_declared_with_other_enums_do_not_check() {
    let mut m = shapes_module();
    m.source("pub enum Color { Red, Green, Blue }\npub native fn sides_of(c: Color) -> int");
    m.function("sides_of", |s: Shape| s as i64);
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("is declared `fn(Color) -> int`, but its Rust function is `fn(Shape) -> int`"), "{errors:?}");
}

#[test]
fn enums_in_signatures_must_be_registered() {
    let mut m = NativeModule::new("shapes");
    m.function("square", || Shape::Square);
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("uses `shapes.Shape`, which module `shapes` does not declare"), "{errors:?}");
}

#[test]
fn unordered_value_types_cannot_be_sorted() {
    let config = Config::new().host_modules(HostModules::new([bank_module()]).unwrap());
    let (r, _, _) = run_src(config, "import bank\nprint([bank.cell(1, 2)].sorted())");
    let Err(Error::Compile(ds)) = r else { panic!("expected a compile error, got {r:?}") };
    assert!(ds.iter().any(|d| d.message.contains("Ord")), "{ds:?}");
}

#[test]
fn local_time_zone_needs_the_capability() {
    assert!(TimeZone::get("Mars/Olympus_Mons").is_none());
    let tokyo = || Config::new().time_zone(TimeZone::get("asia/tokyo").unwrap());
    let src = "import time\nprint(time.local_zone())";
    let (r, out, _) = run_src(tokyo(), src);
    r.unwrap();
    assert_eq!(out, "UTC\n", "denied by default");
    let (r, out, _) = run_src(tokyo().policy(sandlang::policy::rules::RulePolicy::standard().allow_timezone()), src);
    r.unwrap();
    assert_eq!(out, "Asia/Tokyo\n");
    let (r, out, _) = run_src(Config::new().policy(sandlang::policy::rules::RulePolicy::allow_all()), src);
    r.unwrap();
    assert_eq!(out, "UTC\n", "no zone configured");
    let deny = sandlang::policy::rules::RulePolicy::standard().allow_timezone().deny_timezone();
    let (r, out, _) = run_src(tokyo().policy(deny), src);
    r.unwrap();
    assert_eq!(out, "UTC\n", "deny wins a tie");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let prompter = move |cap: Capability| {
        seen.lock().push(cap.to_string());
        std::future::ready(PromptAnswer::AllowOnce)
    };
    let prompting = |cap: &Capability| if *cap == Capability::TimeZone { Decision::Prompt } else { Decision::Allow };
    let (r, out, _) = run_src(tokyo().custom_policy(prompting).prompter(prompter), src);
    r.unwrap();
    assert_eq!(out, "Asia/Tokyo\n");
    assert_eq!(*asked.lock(), ["read the local time zone"]);
}

#[test]
fn native_module_conversions() {
    let mut m = NativeModule::new("conv");
    m.function("grid", |n: i64| -> Vec<Vec<i64>> { (0..n).map(|i| (0..i).collect()).collect() });
    m.function("pairs", |xs: Vec<(String, i64)>| -> Vec<(i64, String)> { xs.into_iter().map(|(s, i)| (i, s)).collect() });
    m.function("somes", |xs: Vec<Option<i64>>| -> i64 { xs.iter().flatten().sum() });
    m.function("words", |text: &str, sep: &str| -> Vec<String> { text.split(sep).map(String::from).collect() });
    m.function("first", |b: &[u8]| -> Option<i64> { b.first().map(|&x| x as i64) });
    m.function("lookup", |key: &str| -> Result<i64, sandlang::native::convert::HostError> {
        key.parse().map_err(|_| sandlang::native::convert::HostError::new("not_found", format!("no key {key}")))
    })
    .params(&["key"]);
    m.function("many", |n: i64| -> Vec<String> { (0..n).map(|i| "x".repeat(1000) + &i.to_string()).collect() });
    m.function("write", |vm: &mut Vm, path: String, data: Vec<u8>| -> Result<i64, sandlang::native::convert::HostError> {
        vm.require(&Capability::Custom { name: "write".into(), detail: format!("{path} {}", data.len()) })?;
        Ok((path.len() + data.len()) as i64)
    });
    let src = r#"
import conv
print(conv.grid(3))
print(conv.pairs([("a", 1), ("b", 2)]))
print(conv.somes([Some(1), None, Some(5)]))
print(conv.words("a b c", " "))
print(conv.first(b"xyz"))
print(conv.lookup(key: "12"))
print(conv.lookup("x"))
help(conv.lookup)
help(conv.grid)
print(conv.write("f", b"abc"))
print(conv.write("g", b""))
"#;
    let policy = |cap: &Capability| match cap {
        Capability::Custom { detail, .. } if detail == "f 3" => Decision::Allow,
        Capability::Custom { .. } => Decision::Deny,
        _ => Decision::Allow,
    };
    let (r, out, _) = run_src(Config::new().custom_policy(policy).host_modules(HostModules::new([m.clone()]).unwrap()), src);
    r.unwrap();
    assert_eq!(
        out,
        "[[], [0], [0, 1]]\n[(1, \"a\"), (2, \"b\")]\n6\n[\"a\", \"b\", \"c\"]\nSome(120)\nOk(12)\nErr(Error(message: \"no key x\", kind: \"not_found\"))\nfn conv.lookup(key: str) -> Result<int, Error>\nfn conv.grid(arg1: int) -> [[int]]\nOk(4)\nErr(Error(message: \"permission denied: write g 0\", kind: \"permission_denied\"))\n"
    );
    // Results are built within the memory limit.
    let limits = Limits { max_memory: Some(16 << 20), ..Limits::default() };
    let (r, _, _) =
        run_src(Config::new().limits(limits).host_modules(HostModules::new([m]).unwrap()), "import conv\nprint(conv.many(100000).len())");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

#[test]
#[should_panic(expected = "name of a standard module")]
fn host_modules_cannot_replace_standard_modules() {
    NativeModule::new("json");
}

#[test]
#[should_panic(expected = "added twice")]
fn host_module_names_are_unique() {
    let _ = Config::new().host_modules(HostModules::new([NativeModule::new("a"), NativeModule::new("a")]).unwrap());
}

#[test]
#[should_panic(expected = "registered twice")]
fn host_function_names_are_unique() {
    NativeModule::new("a").function("f", || -> i64 { 1 }).function("f", || -> i64 { 2 });
}

#[test]
#[should_panic(expected = "is given twice")]
fn parameter_names_are_unique() {
    NativeModule::new("a").function("f", |a: i64, b: i64| a + b).params(&["x", "x"]);
}

#[test]
#[should_panic(expected = "is not an identifier")]
fn parameter_names_are_identifiers() {
    NativeModule::new("a").function("f", |a: i64| a).params(&["fn"]);
}

#[test]
fn declared_natives_take_names_and_docs_from_the_declaration() {
    let mut m = NativeModule::new("a");
    m.source("pub native fn f(x: int) -> int");
    m.function("f", |a: i64| a).params(&["y"]);
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["native `a.f` has `params` or `doc`, which apply only to a native that the module does not declare"]);
}

#[test]
fn native_module_signatures_are_type_checked() {
    let config = Config::new().host_modules(HostModules::new([geo_module()]).unwrap());
    let (r, _, _) = run_src(config, "import geo\nprint(geo.dist(1, 2))");
    match r {
        Err(Error::Compile(d)) => {
            assert!(d[0].message.contains("expected float, found int"), "{d:?}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn natives_that_return_futures_suspend_only_their_task() {
    let mut m = NativeModule::new("slow");
    m.function("echo", |s: String, ms: i64| async move {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        s
    });
    let src = r#"
import slow
let a = spawn slow.echo("a", 40)
let b = spawn slow.echo("b", 5)
print(task.race([a, b]))
print(a.wait())
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    match r {
        // `a` was cancelled by race, so waiting on it fails.
        Err(Error::Runtime { message, .. }) => assert!(message.contains("task cancelled")),
        other => panic!("{other:?}"),
    }
    assert_eq!(out, "b\n");
}

#[test]
fn native_module_with_sand_source() {
    let mut m = NativeModule::new("shapes");
    m.source(
        r#"
/// A square.
pub struct Square { pub side: float }
pub fn area(s: Square) -> float { s.side * s.side }
pub native fn scale(x: float, k: float) -> float
"#,
    );
    m.function("scale", |x: f64, k: f64| -> f64 { x * k });
    let src = "import shapes\nlet s = shapes.Square(side: shapes.scale(2.0, 1.5))\nprint(shapes.area(s))";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "9.0\n");
}

#[test]
fn native_module_declares_traits_for_builtin_types() {
    let mut m = NativeModule::new("units");
    m.source(
        r#"
/// Values with a unit.
pub trait Unit {
    /// The unit's symbol.
    fn symbol() -> str
    fn show(self) -> str { "\(self.magnitude())\(Self.symbol())" }
    fn magnitude(self) -> float
}
impl Unit for duration {
    fn symbol() -> str { "s" }
    fn magnitude(self) -> float { self.secs() }
}
impl<T: Unit> Unit for [T] {
    fn symbol() -> str { T.symbol() }
    fn magnitude(self) -> float { self.map(x => x.magnitude()).sum() }
}
pub fn total<T: Unit>(xs: [T]) -> str { xs.show() }
"#,
    );
    let src = r#"
import units
struct Metres { m: float }
impl units.Unit for Metres {
    fn symbol() -> str { "m" }
    fn magnitude(self) -> float { self.m }
}
print(1500ms.show())
print(units.total([1s, 2s]))
print(units.total([Metres(m: 2.0), Metres(m: 0.5)]))
help(units)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(
        out,
        "1.5s\n3.0s\n2.5m\nmodule units\n\nTraits:\n    trait Unit\n        Values with a unit.\n\nFunctions:\n    fn total<T: Unit>(xs: [T]) -> str\n"
    );
}

#[test]
fn help_describes_host_modules_that_are_not_imported() {
    let mut shapes = NativeModule::new("shapes");
    shapes.source(
        "//! Shapes.\n/// A square.\npub struct Square { pub side: float }\n/// Area of a square.\npub fn area(s: Square) -> float { s.side * s.side }\n",
    );
    let config = || Config::new().host_modules(HostModules::new([shapes.clone()]).unwrap());
    let (r, out, _) = run_src(config(), "help(shapes)\nprint(members(shapes))\nhelp(\"shapes.Square\")");
    r.unwrap();
    assert_eq!(
        out,
        "module shapes\n\nShapes.\n\nTypes:\n    struct Square\n        A square.\n\nFunctions:\n    fn area(s: Square) -> float\n        Area of a square.\n[\"Square\", \"area\"]\nstruct shapes.Square\n\nA square.\n\nFields:\n    side: float\n\nImplements:\n    Eq\n    Ord\n    Encode\n    Decode\n"
    );
    let (r, out, _) = run_src(config(), "help()");
    r.unwrap();
    assert!(out.contains("    shapes      Shapes.\n"), "{out}");
}

#[test]
fn host_modules_cannot_import_each_other_in_a_cycle() {
    let (mut a, mut b) = (NativeModule::new("ring_a"), NativeModule::new("ring_b"));
    a.source("import ring_b\npub fn one() -> int { 1 }");
    b.source("import ring_a\npub fn two() -> int { 2 }");
    let errors = host_module_errors([a, b]);
    assert_eq!(errors, ["import cycle: ring_a → ring_b → ring_a"]);
}

#[derive(Clone, Copy, sandlang::SandEnum)]
#[sand(module = "ring_a")]
enum RingA {
    One,
}

#[derive(Clone, Copy, sandlang::SandEnum)]
#[sand(module = "ring_b")]
enum RingB {
    Two,
}

#[test]
fn host_modules_cannot_use_each_others_types_in_a_cycle() {
    let (mut a, mut b) = (NativeModule::new("ring_a"), NativeModule::new("ring_b"));
    a.bind::<RingA>().function("next", |_: RingB| RingA::One);
    b.bind::<RingB>().function("next", |_: RingA| RingB::Two);
    let errors = host_module_errors([a, b]);
    assert_eq!(errors, ["import cycle: ring_a → ring_b → ring_a"]);
}

#[test]
fn host_modules_new_type_checks_module_source() {
    let mut broken = NativeModule::new("broken");
    broken.source("pub fn bad() -> int { \"not an int\" }\n");
    let errors = host_module_errors([broken]);
    assert!(errors[0].contains("type mismatch"), "{errors:?}");
}

/// A tally that functions of `stats` update.
#[derive(sandlang::SandObject)]
#[sand(module = "stats")]
struct Counter(AtomicUsize);

impl sandlang::native::NativeObject for Counter {}

#[derive(sandlang::SandObject)]
#[sand(module = "stats", private)]
struct Scratch;

impl sandlang::native::NativeObject for Scratch {}

fn stats_module() -> NativeModule {
    let mut m = NativeModule::new("stats");
    m.bind::<Counter>();
    m.function("counter", || Counter(AtomicUsize::new(0)));
    m.function("total", |c: &Counter| c.0.load(Ordering::Relaxed) as i64);
    m.function("Counter.add", |c: &Counter, n: i64| (c.0.fetch_add(n as usize, Ordering::Relaxed) + n as usize) as i64)
        .params(&["n"])
        .doc("Adds `n` and returns the total.");
    m
}

#[test]
fn host_functions_take_and_return_objects() {
    let src = r#"
import stats
let c = stats.counter()
let d = stats.counter()
print(c.add(2) + c.add(3))
print("\(stats.total(c)) \(stats.total(d))")
print("\(c == c) \(c == d) \(c)")
help(stats.Counter)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([stats_module()]).unwrap()), src);
    r.unwrap();
    assert_eq!(
        out,
        "7\n5 0\ntrue false <Counter>\ntype stats.Counter\n\nA tally that functions of `stats` update.\n\nMethods:\n    fn add(self, n: int) -> int\n        Adds `n` and returns the total.\n\nImplements:\n    Eq\n"
    );
}

#[test]
fn natives_must_be_declared_with_their_object_types() {
    let mut m = stats_module();
    m.bind::<Scratch>();
    m.function("scratch", || Scratch);
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("public function `scratch` uses the private type `Scratch`"), "{errors:?}");

    let mut m = stats_module();
    m.source("pub struct Box { pub n: int }\npub native fn peek(b: Box) -> int");
    m.function("peek", |c: &Counter| c.0.load(Ordering::Relaxed) as i64);
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("is declared `fn(Box) -> int`, but its Rust function is `fn(Counter) -> int`"), "{errors:?}");
}

#[test]
fn natives_cannot_use_private_types_of_other_modules() {
    let mut stats = stats_module();
    stats.bind::<Scratch>();
    let mut m = NativeModule::new("peek");
    m.function("scratch", |_: &Scratch| 0i64);
    let errors = host_module_errors([stats, m]);
    assert!(errors[0].contains("native `peek.scratch` uses `stats.Scratch`, which is private"), "{errors:?}");
}

/// A point in the plane.
#[derive(Clone, Copy, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "plane")]
struct Point {
    /// Horizontal coordinate.
    x: f64,
    /// Vertical coordinate.
    y: f64,
}

#[derive(Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "plane")]
struct Route {
    label: Option<String>,
    points: Vec<Point>,
    lengths: HashMap<String, i64>,
    end: Shape,
    #[sand(private, var)]
    visits: i64,
}

fn plane_module() -> NativeModule {
    let mut m = NativeModule::new("plane");
    m.bind::<Point>().bind::<Route>();
    m.function("mid", |a: Point, b: Point| Point { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0 });
    m.function("Point.norm", |p: Point| (p.x * p.x + p.y * p.y).sqrt());
    m.function("route", |label: Option<String>, points: Vec<Point>| {
        let lengths = HashMap::from([("points".to_string(), points.len() as i64)]);
        Route { label, points, lengths, end: Shape::Triangle, visits: 0 }
    })
    .params(&["label", "points"]);
    m.function("Route.visit_count", |r: Route| r.visits);
    m.source("extend Route {\n    /// Counts a visit.\n    pub fn visit(self) {\n        self.visits += 1\n    }\n}");
    m
}

#[test]
fn host_functions_take_and_return_structs() {
    let src = r#"
import plane
import shapes
let a = plane.Point(x: 1.0, y: 2.0)
print(plane.mid(a, plane.Point(x: 3.0, y: 4.0)))
print(plane.Point(x: 3.0, y: 4.0).norm())
let r = plane.route(label: Some("home"), points: [a, a])
print(r)
r.visit()
r.visit()
print(r.visit_count())
print(r.end == shapes.Shape.Triangle)
help(plane.Point)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([shapes_module(), plane_module()]).unwrap()), src);
    r.unwrap();
    let route = "Route(label: Some(\"home\"), points: [Point(x: 1.0, y: 2.0), Point(x: 1.0, y: 2.0)], lengths: [\"points\": 2], end: Triangle, visits: 0)";
    let help = "struct plane.Point\n\nA point in the plane.\n\nFields:\n    x: float\n        Horizontal coordinate.\n    y: float\n        Vertical coordinate.\n\nMethods:\n    fn norm(self) -> float\n\nImplements:\n    Eq\n    Ord\n    Encode\n    Decode\n";
    assert_eq!(out, format!("Point(x: 2.0, y: 3.0)\n5.0\n{route}\n2\ntrue\n{help}"));
}

#[test]
fn private_struct_fields_stay_in_their_module() {
    let config = || Config::new().host_modules(HostModules::new([shapes_module(), plane_module()]).unwrap());
    let cases = [
        "let r = plane.route(label: None, points: [])\nprint(r.visits)",
        "let r = plane.route(label: None, points: [])\nr.visits = 3",
        "print(plane.Route(label: None, points: [], lengths: [:], end: shapes.Shape.Circle, visits: 0))",
    ];
    for src in cases {
        let (r, _, _) = run_src(config(), &format!("import plane\nimport shapes\n{src}"));
        let Err(Error::Compile(ds)) = r else { panic!("{src}: expected a compile error, got {r:?}") };
        assert!(ds[0].message.contains("private"), "{src}: {ds:?}");
    }
}

#[test]
fn struct_fields_must_use_registered_public_types() {
    let errors = host_module_errors([plane_module()]);
    let msg = "the Rust field `end` of `plane.Route` uses `shapes.Shape`, and module `shapes` is not added to the engine";
    assert!(errors.iter().any(|e| e == msg), "{errors:?}");

    #[derive(Clone, Copy, sandlang::SandEnum)]
    #[sand(module = "lab", private)]
    enum Secret {
        Code,
    }
    #[derive(sandlang::SandStruct)]
    #[sand(module = "lab")]
    struct Report {
        secret: Secret,
    }
    let mut m = NativeModule::new("lab");
    m.bind::<Secret>().bind::<Report>();
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["public field `secret` uses the private type `Secret`; make `Secret` public"]);
}

#[test]
#[should_panic(expected = "field `fn` of `Keyword` is not an identifier, or is a keyword")]
fn struct_fields_must_be_sand_identifiers() {
    use sandlang::native::Binding;
    use sandlang::native::ty::Type;
    struct Keyword;
    impl sandlang::native::NamedType for Keyword {
        const MODULE: &'static str = "lab";
        const NAME: &'static str = "Keyword";
        const DOC: &'static str = "";
        const PUBLIC: bool = true;
        fn binding() -> Binding<Self> {
            Binding::of_struct()
        }
    }
    impl sandlang::native::ty::SandType for Keyword {
        fn sand_type() -> Type {
            Type::bound::<Self>(Vec::new())
        }
    }
    impl sandlang::native::StructType for Keyword {
        const FIELDS: &'static [sandlang::native::StructField] = &[sandlang::native::StructField {
            name: "fn",
            doc: "",
            public: true,
            var: false,
            ty: <i64 as sandlang::native::ty::SandType>::sand_type,
            default: None,
        }];
    }
    NativeModule::new("lab").bind::<Keyword>();
}

#[test]
fn struct_fields_can_take_another_sand_name() {
    #[derive(sandlang::SandStruct)]
    #[sand(module = "lab")]
    struct Sample {
        #[sand(name = "kind")]
        r#type: String,
        #[sand(name = "variance", var)]
        var: i64,
    }
    let mut m = NativeModule::new("lab");
    m.bind::<Sample>();
    m.function("grow", |s: Sample| Sample { r#type: format!("{}!", s.r#type), var: s.var * 2 });
    let src = "import lab\nvar s = lab.grow(lab.Sample(kind: \"a\", variance: 2))\ns.variance += 1\nprint(s)";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "Sample(kind: \"a!\", variance: 5)\n");
}

#[test]
#[should_panic(expected = "type `Point` is registered twice in module `plane`")]
fn structs_bind_once() {
    NativeModule::new("plane").bind::<Point>().bind::<Point>();
}

#[derive(sandlang::SandStruct)]
#[sand(module = "tree")]
struct Node {
    children: Vec<Node>,
}

#[test]
fn recursive_structs_convert_to_a_bounded_depth() {
    fn size(n: &Node) -> i64 {
        1 + n.children.iter().map(size).sum::<i64>()
    }
    let run = |src: &str| {
        let mut m = NativeModule::new("tree");
        m.bind::<Node>();
        m.function("size", |n: Node| size(&n));
        m.function("echo", |n: Node| n);
        m.function("chain", |n: i64| (0..n).fold(Node { children: Vec::new() }, |n, _| Node { children: vec![n] }));
        run_src(
            Config::new().host_modules(HostModules::new([m]).unwrap()),
            &format!("import tree\nlet leaf = tree.Node(children: [])\n{src}"),
        )
    };
    let (r, out, _) = run("print(tree.size(tree.echo(tree.Node(children: [leaf, tree.Node(children: [leaf])]))))");
    r.unwrap();
    assert_eq!(out, "4\n");
    let (r, out, _) =
        run("var n = leaf\nfor _ in 0..255 {\n    n = tree.Node(children: [n])\n}\nprint(tree.size(n))\nprint(tree.size(tree.chain(255)))");
    r.unwrap();
    assert_eq!(out, "256\n256\n");
    for src in [
        "print(tree.chain(256))",
        "print(tree.chain(5000))",
        "var n = leaf\nfor _ in 0..256 {\n    n = tree.Node(children: [n])\n}\nprint(tree.size(n))",
        "var n = leaf\nfor _ in 0..1000000 {\n    n = tree.Node(children: [n])\n}\nprint(tree.size(n))",
        "leaf.children.push(leaf)\nprint(tree.size(leaf))",
    ] {
        let (r, _, _) = run(src);
        let Err(Error::Runtime { message, .. }) = r else { panic!("{src}: expected a runtime error, got {r:?}") };
        assert!(message.contains("value too deeply nested to convert"), "{src}: {message}");
    }
}

#[derive(sandlang::SandStruct)]
#[sand(module = "chain")]
struct Link {
    n: i64,
    next: Option<Box<Link>>,
}

#[test]
fn boxed_structs_convert_to_a_bounded_depth() {
    fn sum(l: &Link) -> i64 {
        l.n + l.next.as_deref().map_or(0, sum)
    }
    let run = |src: &str| {
        let mut m = NativeModule::new("chain");
        m.bind::<Link>();
        m.function("sum", |l: Link| sum(&l));
        m.function("range", |n: i64| (0..n).fold(None, |next, i| Some(Box::new(Link { n: i, next }))));
        run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), &format!("import chain\n{src}"))
    };
    let (r, out, _) = run("print(chain.sum(chain.Link(n: 1, next: Some(chain.Link(n: 2, next: None)))))\nprint(chain.range(3))");
    r.unwrap();
    assert_eq!(out, "3\nSome(Link(n: 2, next: Some(Link(n: 1, next: Some(Link(n: 0, next: None))))))\n");
    let (r, out, _) = run("print(chain.sum(chain.range(256)?))");
    r.unwrap();
    assert_eq!(out, "32640\n");
    for src in [
        "print(chain.range(257))",
        "var l = chain.Link(n: 0, next: None)\nfor i in 0..256 {\n    l = chain.Link(n: i, next: Some(l))\n}\nprint(chain.sum(l))",
    ] {
        let (r, _, _) = run(src);
        let Err(Error::Runtime { message, .. }) = r else { panic!("{src}: expected a runtime error, got {r:?}") };
        assert!(message.contains("value too deeply nested to convert"), "{src}: {message}");
    }
}

/// A figure in the plane.
#[derive(Clone, Debug, PartialEq, sandlang::SandEnum)]
#[sand(module = "draw")]
enum Figure {
    /// Nothing at all.
    Empty,
    /// A circle with its radius.
    Circle(f64),
    /// A rectangle.
    Rect { w: f64, h: f64 },
    /// A figure with a label.
    Labeled(String, Box<Figure>),
    /// Several figures.
    Group(Vec<Figure>),
}

fn draw_module() -> NativeModule {
    let mut m = NativeModule::new("draw");
    m.bind::<Figure>();
    fn area(f: &Figure) -> f64 {
        match f {
            Figure::Empty => 0.0,
            Figure::Circle(r) => 3.0 * r * r,
            Figure::Rect { w, h } => w * h,
            Figure::Labeled(_, f) => area(f),
            Figure::Group(fs) => fs.iter().map(area).sum(),
        }
    }
    m.function("area", |f: Figure| area(&f));
    m.function("echo", |f: Figure| f);
    m.function("square", |side: f64| Figure::Labeled("square".into(), Box::new(Figure::Rect { w: side, h: side })));
    m.function("nest", |n: i64| (0..n).fold(Figure::Empty, |f, _| Figure::Group(vec![f])));
    m
}

#[test]
fn host_functions_take_and_return_enums_with_payloads() {
    let src = r#"
import draw
let g = draw.Figure.Group([draw.Figure.Circle(1.0), draw.Figure.Rect(w: 2.0, h: 3.0), draw.Figure.Empty])
print(draw.area(g))
print(draw.echo(g) == g)
let s = draw.square(2.0)
print(s)
match s {
    Labeled(name, Rect(w, h)) => print("\(name): \(w) by \(h)")
    _ => print("other")
}
print(draw.area(draw.Figure.Labeled("x", draw.Figure.Circle(2.0))))
help(draw.Figure)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([draw_module()]).unwrap()), src);
    r.unwrap();
    let help = "enum draw.Figure\n\nA figure in the plane.\n\nVariants:\n    Empty\n        Nothing at all.\n    Circle(float)\n        A circle with its radius.\n    Rect(w: float, h: float)\n        A rectangle.\n    Labeled(str, Figure)\n        A figure with a label.\n    Group([Figure])\n        Several figures.\n\nImplements:\n    Eq\n    Ord\n    Encode\n    Decode\n";
    assert_eq!(out, format!("9.0\ntrue\nLabeled(\"square\", Rect(w: 2.0, h: 2.0))\nsquare: 2.0 by 2.0\n12.0\n{help}"));
}

#[test]
fn enums_with_payloads_convert_to_a_bounded_depth() {
    let run = |src: &str| run_src(Config::new().host_modules(HostModules::new([draw_module()]).unwrap()), &format!("import draw\n{src}"));
    let (r, out, _) = run("print(draw.area(draw.nest(256)))");
    r.unwrap();
    assert_eq!(out, "0.0\n");
    for src in
        ["print(draw.nest(257))", "var f = draw.Figure.Empty\nfor _ in 0..257 {\n    f = draw.Figure.Group([f])\n}\nprint(draw.area(f))"]
    {
        let (r, _, _) = run(src);
        let Err(Error::Runtime { message, .. }) = r else { panic!("{src}: expected a runtime error, got {r:?}") };
        assert!(message.contains("value too deeply nested to convert"), "{src}: {message}");
    }
}

#[test]
#[should_panic(expected = "field `type` of `Shape.Named` is not an identifier, or is a keyword")]
fn enum_payload_fields_must_be_sand_identifiers() {
    use sandlang::native::ty::Type;
    use sandlang::native::{Binding, EnumVariant, VariantField};
    struct Shape;
    impl sandlang::native::NamedType for Shape {
        const MODULE: &'static str = "lab";
        const NAME: &'static str = "Shape";
        const DOC: &'static str = "";
        const PUBLIC: bool = true;
        fn binding() -> Binding<Self> {
            Binding::of_enum()
        }
    }
    impl sandlang::native::ty::SandType for Shape {
        fn sand_type() -> Type {
            Type::bound::<Self>(Vec::new())
        }
    }
    impl sandlang::native::EnumType for Shape {
        const VARIANTS: &'static [EnumVariant] = &[EnumVariant {
            name: "Named",
            doc: "",
            fields: &[VariantField { name: Some("type"), ty: <i64 as sandlang::native::ty::SandType>::sand_type }],
        }];
        fn index(&self) -> usize {
            0
        }
    }
    NativeModule::new("lab").bind::<Shape>();
}

/// A value with a label.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "boxes")]
struct Labeled<T> {
    label: String,
    /// The labeled value.
    value: T,
    /// Earlier values.
    #[sand(default)]
    history: Vec<T>,
}

/// A left or right value.
#[derive(Clone, Debug, PartialEq, sandlang::SandEnum)]
#[sand(module = "boxes")]
enum Either<L, R> {
    Left(L),
    Right(R),
}

fn boxes_module() -> NativeModule {
    use sandlang::native::ty::{Param, VmVal};
    let mut m = NativeModule::new("boxes");
    m.bind::<Labeled<()>>().bind::<Either<(), ()>>();
    m.function("count", |n: i64| Labeled { label: "count".into(), value: n, history: (0..n).collect() });
    m.function("names", || Labeled { label: "names".into(), value: vec!["a".to_string()], history: Vec::new() });
    m.function("split", |x: i64| if x < 0 { Either::Left(x.to_string()) } else { Either::Right(x as f64) });
    m.function("total", |l: Labeled<i64>| l.value + l.history.iter().sum::<i64>());
    m.function(
        "relabel",
        |_: &mut sandlang::vm::Vm, l: Labeled<Param<0>>, label: String| -> Result<Labeled<Param<0>>, sandlang::vm::Fail> {
            Ok(Labeled { label, ..l })
        },
    );
    m.function("unwrap", |vm: &mut sandlang::vm::Vm, e: VmVal<Either<Param<0>, Param<0>>>| -> Result<Param<0>, sandlang::vm::Fail> {
        Ok(Param::from_raw(vm.heap.field(e.raw(), 0).unwrap()))
    });
    m.source("pub native fn relabel<T>(l: Labeled<T>, label: str) -> Labeled<T>\npub native fn unwrap<T>(e: Either<T, T>) -> T");
    m
}

/// A generic recursive enum whose named payload has a renamed field.
#[derive(Clone, Debug, PartialEq, sandlang::SandEnum)]
#[sand(module = "cons")]
enum Chain<T> {
    End,
    Link {
        #[sand(name = "item")]
        r#type: T,
        next: Box<Chain<T>>,
    },
}

#[test]
fn generic_recursive_enums_bind_with_renamed_payloads() {
    let mut m = NativeModule::new("cons");
    m.bind::<Chain<()>>();
    m.function("words", |n: i64| (0..n).fold(Chain::End, |next, i| Chain::Link { r#type: format!("w{i}"), next: Box::new(next) }));
    m.function("sum", |c: Chain<i64>| {
        let (mut total, mut c) = (0, c);
        while let Chain::Link { r#type, next } = c {
            total += r#type;
            c = *next;
        }
        total
    });
    let src = r#"
import cons
let w = cons.words(2)
print(w)
match w {
    Link(first, _) => print(first)
    End => print("end")
}
print(cons.sum(cons.Chain.Link(item: 4, next: cons.Chain.Link(item: 5, next: cons.Chain.End))))
help(cons.Chain)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    let help = "enum cons.Chain<T>\n\nA generic recursive enum whose named payload has a renamed field.\n\nVariants:\n    End\n    Link(item: T, next: Chain<T>)\n\nImplements:\n    Eq where T: Eq\n    Ord where T: Ord\n    Hash where T: Hash\n    Encode where T: Encode\n    Decode where T: Decode\n";
    assert_eq!(out, format!("Link(item: \"w1\", next: Link(item: \"w0\", next: End))\nw1\n9\n{help}"));
}

#[test]
fn generic_host_types_can_require_traits_of_their_type_parameters() {
    #[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
    #[sand(module = "sorted")]
    struct Sorted<T: Ord> {
        #[sand(default)]
        items: Vec<T>,
    }
    let mut m = NativeModule::new("sorted");
    m.bind::<Sorted<i64>>();
    m.function("sort", |mut s: Sorted<i64>| {
        s.items.sort();
        s
    });
    let src = "import sorted\nprint(sorted.sort(sorted.Sorted(items: [3, 1, 2])))\nlet e: sorted.Sorted<str> = sorted.Sorted()\nprint(e)";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "Sorted(items: [1, 2, 3])\nSorted(items: [])\n");
}

#[test]
fn generic_host_types_bind_type_parameters() {
    let src = r#"
import boxes
let c = boxes.count(3)
print(c)
print(boxes.total(c))
print(boxes.names().value)
print(boxes.split(-4))
print(boxes.split(4))
let l = boxes.Labeled(label: "x", value: (1, "a"))
print(boxes.relabel(l, "y"))
let s: str = boxes.unwrap(boxes.Either.Left("left"))
print(s)
let e: boxes.Either<int, str> = boxes.Either.Right("r")
match e {
    Left(n) => print(n + 1)
    Right(t) => print(t.len())
}
help(boxes.Labeled)
help(boxes.Either)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([boxes_module()]).unwrap()), src);
    r.unwrap();
    let help = "struct boxes.Labeled<T>\n\nA value with a label.\n\nFields:\n    label: str\n    value: T\n        The labeled value.\n    history: [T] = []\n        Earlier values.\n\nImplements:\n    Eq where T: Eq\n    Ord where T: Ord\n    Encode where T: Encode\n    Decode where T: Decode\n\
                enum boxes.Either<L, R>\n\nA left or right value.\n\nVariants:\n    Left(L)\n    Right(R)\n\nImplements:\n    Eq where L: Eq, R: Eq\n    Ord where L: Ord, R: Ord\n    Hash where L: Hash, R: Hash\n    Encode where L: Encode, R: Encode\n    Decode where L: Decode, R: Decode\n";
    let expected = [
        "Labeled(label: \"count\", value: 3, history: [0, 1, 2])",
        "6",
        "[\"a\"]",
        "Left(\"-4\")",
        "Right(4.0)",
        "Labeled(label: \"y\", value: (1, \"a\"), history: [])",
        "left",
        "1",
    ];
    assert_eq!(out, format!("{}\n{help}", expected.join("\n")));
    let (r, _, _) = run_src(
        Config::new().host_modules(HostModules::new([boxes_module()]).unwrap()),
        "import boxes\nlet l: boxes.Labeled<str> = boxes.count(1)",
    );
    let Err(Error::Compile(ds)) = r else { panic!("expected a type error, got {r:?}") };
    assert!(ds[0].message.contains("expected Labeled<str>, found Labeled<int>"), "{ds:?}");
}

#[test]
fn generic_host_types_bind_once_for_every_instance() {
    let errors =
        std::panic::catch_unwind(|| NativeModule::new("boxes").bind::<Labeled<i64>>().bind::<Labeled<String>>().name().to_string());
    let msg = errors.unwrap_err();
    let msg = msg.downcast_ref::<String>().unwrap();
    assert!(msg.contains("type `Labeled` is registered twice in module `boxes`"), "{msg}");
}

#[test]
fn generic_host_types_refer_to_their_own_type_as_self() {
    use sandlang::native::ty::Param;
    #[derive(Clone, Debug, PartialEq, sandlang::SandEnum)]
    #[sand(module = "trees")]
    enum Tree<T> {
        Leaf(T),
        Node(Vec<Self>),
    }
    #[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
    #[sand(module = "trees")]
    struct Chain<T> {
        value: T,
        next: Option<Box<Self>>,
    }
    let mut m = NativeModule::new("trees");
    m.bind::<Tree<()>>().bind::<Chain<()>>();
    m.function("tree", |t: Tree<Param<0>>| t);
    m.function("chain", |c: Chain<Param<0>>| c);
    m.source("pub native fn tree<T>(t: Tree<T>) -> Tree<T>\npub native fn chain<T>(c: Chain<T>) -> Chain<T>");
    let src = "import trees\nlet t = trees.Tree.Node([trees.Tree.Leaf(\"a\")])\nprint(trees.tree(t) == t)\n\
               let c = trees.Chain(value: 1, next: Some(trees.Chain(value: 2, next: None)))\nprint(trees.chain(c) == c)\n\
               help(trees.Tree)";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(
        out,
        "true\ntrue\nenum trees.Tree<T>\n\nVariants:\n    Leaf(T)\n    Node([Tree<T>])\n\nImplements:\n    Eq where T: Eq\n    Ord where T: Ord\n    Encode where T: Encode\n    Decode where T: Decode\n"
    );
}

/// Settings with defaults.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "conf")]
struct Settings {
    name: String,
    #[sand(default = 8080)]
    port: i64,
    #[sand(default)]
    verbose: bool,
    #[sand(default = "info")]
    level: String,
    #[sand(default = jiff::SignedDuration::from_secs(30))]
    timeout: jiff::SignedDuration,
    #[sand(default)]
    tags: HashMap<String, String>,
    #[sand(default)]
    parent: Option<Box<Settings>>,
    #[sand(default = -0.5)]
    ratio: f64,
    #[sand(default = LogLevel::Warn)]
    log: LogLevel,
    #[sand(default)]
    key: Vec<u8>,
}

#[test]
fn rust_field_defaults_fill_constructions_and_decoded_json() {
    let mut m = NativeModule::new("conf");
    m.bind::<Settings>();
    m.function("port", |s: Settings| s.port);
    m.function("key", |s: Settings| s.key);
    let src = r#"
import conf
import json
let s = conf.Settings(name: "a")
print(s)
print(conf.port(conf.Settings(name: "b", port: 1)))
print(conf.key(s))
let d: conf.Settings = json.decode("{\"name\": \"c\", \"verbose\": true}")?
print(d)
help(conf.Settings)
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    let s = "Settings(name: \"a\", port: 8080, verbose: false, level: \"info\", timeout: 30s, tags: [:], parent: None, ratio: -0.5, log: Warn, key: b\"\")";
    let d = "Settings(name: \"c\", port: 8080, verbose: true, level: \"info\", timeout: 30s, tags: [:], parent: None, ratio: -0.5, log: Warn, key: b\"\")";
    let help = "struct conf.Settings\n\nSettings with defaults.\n\nFields:\n    name: str\n    port: int = 8080\n    verbose: bool = false\n    level: str = \"info\"\n    timeout: duration = 30s\n    tags: [str: str] = [:]\n    parent: Settings? = None\n    ratio: float = -0.5\n    log: Level = Level.Warn\n    key: bytes = b\"\"\n\nImplements:\n    Eq\n    Encode\n    Decode\n";
    assert_eq!(out, format!("{s}\n1\nb\"\"\n{d}\n{help}"));
}

fn default_doc() -> sandlang::stdlib::json::Json {
    use sandlang::stdlib::json::Json;
    Json::Obj(
        [("on".to_string(), Json::Arr(vec![Json::Bool(true), Json::Int(-1), Json::Float(0.5), Json::Str("x".into()), Json::Null]))]
            .into_iter()
            .collect(),
    )
}

#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "conf")]
struct Doc {
    #[sand(default = default_doc())]
    body: sandlang::stdlib::json::Json,
}

#[test]
fn rust_json_field_defaults_are_constants() {
    let mut m = NativeModule::new("conf");
    m.bind::<Doc>();
    m.function("same", |d: Doc| d.body == default_doc());
    let src = "import conf\nimport json\nlet d = conf.Doc()\nprint(json.encode(d.body))\nprint(conf.same(d))\nlet e: conf.Doc = json.decode(\"{}\")?\nprint(conf.same(e))\n";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "{\"on\":[true,-1,0.5,\"x\",null]}\ntrue\ntrue\n");
}

#[test]
#[should_panic(expected = "the default of field `at` of `Bad` is not a Sand constant")]
fn rust_field_defaults_must_be_sand_constants() {
    #[derive(sandlang::SandStruct)]
    #[sand(module = "lab")]
    struct Bad {
        #[sand(default = jiff::Timestamp::UNIX_EPOCH)]
        at: jiff::Timestamp,
    }
    NativeModule::new("lab").bind::<Bad>();
}

#[test]
#[should_panic(expected = "the default of field `table` of `Big` is not a Sand constant, or has more than 65536 parts or 64 levels")]
fn rust_field_defaults_are_bounded() {
    #[derive(sandlang::SandStruct)]
    #[sand(module = "lab")]
    struct Big {
        #[sand(default = vec![0; 1 << 16])]
        table: Vec<i64>,
    }
    NativeModule::new("lab").bind::<Big>();
}

#[test]
fn help_cuts_long_rust_field_defaults_as_it_cuts_sand_ones() {
    #[derive(sandlang::SandStruct)]
    #[sand(module = "lab")]
    struct Big {
        #[sand(default = vec![7; 60000])]
        table: Vec<i64>,
    }
    let mut m = NativeModule::new("lab");
    m.bind::<Big>();
    let src = format!("import lab\nstruct Mine {{ table: [int] = [{}] }}\nhelp(lab.Big)\nhelp(Mine)", vec!["7"; 100].join(", "));
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), &src);
    r.unwrap();
    let line = format!("    table: [int] = [{}7,...", "7, ".repeat(19));
    assert_eq!(out.lines().filter(|l| l.starts_with("    table")).collect::<Vec<_>>(), [&line, &line], "{}", &out[..out.len().min(500)]);
}

#[test]
fn rust_field_defaults_can_be_any_constant() {
    #[derive(Debug, PartialEq, sandlang::SandStruct)]
    #[sand(module = "draw")]
    struct Frame {
        #[sand(default = 1.5)]
        width: f64,
        #[sand(default)]
        labels: Vec<String>,
    }
    #[derive(Debug, PartialEq, sandlang::SandStruct)]
    #[sand(module = "draw")]
    struct Sketch {
        #[sand(default = Figure::Empty)]
        figure: Figure,
        #[sand(default)]
        backdrop: Option<Figure>,
        #[sand(default = Some(Figure::Labeled("a\"b".into(), Box::new(Figure::Rect { w: 1.0, h: -2.0 }))))]
        frame: Option<Figure>,
        #[sand(default = Some(2))]
        layers: Option<i64>,
        #[sand(default = vec![Figure::Circle(1.0), Figure::Group(Vec::new())])]
        parts: Vec<Figure>,
        #[sand(default = (3, "x".to_string()))]
        pair: (i64, String),
        #[sand(default = std::collections::BTreeMap::from([(2, vec![true])]))]
        flags: std::collections::BTreeMap<i64, Vec<bool>>,
        #[sand(default = Frame { width: 2.0, labels: vec!["l".into()] })]
        border: Frame,
        #[sand(default = std::collections::BTreeSet::from(["b".to_string(), "a".to_string()]))]
        seen: std::collections::BTreeSet<String>,
        #[sand(default)]
        none: std::collections::HashSet<i64>,
    }
    let mut m = draw_module();
    m.bind::<Frame>().bind::<Sketch>();
    m.function("figure", |s: Sketch| s.figure);
    m.function("keep", |s: Sketch| s);
    let src = "import draw\nimport json\nprint(draw.figure(draw.Sketch()))\nlet s: draw.Sketch = json.decode(\"{}\")?\nprint(s)\n\
               print(s == draw.Sketch())\nprint(draw.keep(s) == s)\nprint(draw.Frame())\n\
               struct Scene { sketch: draw.Sketch = draw.Sketch(pair: (4, \"y\")) }\nlet scene: Scene = json.decode(\"{}\")?\n\
               print(scene == Scene() and scene.sketch.pair == (4, \"y\") and scene.sketch.border == draw.Frame(width: 2.0, labels: [\"l\"]))\n\
               help(draw.Sketch)";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(
        out,
        "Empty\nSketch(figure: Empty, backdrop: None, frame: Some(Labeled(\"a\\\"b\", Rect(w: 1.0, h: -2.0))), layers: Some(2), \
         parts: [Circle(1.0), Group([])], pair: (3, \"x\"), flags: [2: [true]], border: Frame(width: 2.0, labels: [\"l\"]), \
         seen: Set([\"a\", \"b\"]), none: Set([]))\n\
         true\ntrue\nFrame(width: 1.5, labels: [])\ntrue\nstruct draw.Sketch\n\nFields:\n    figure: Figure = Figure.Empty\n    \
         backdrop: Figure? = None\n    frame: Figure? = Some(Figure.Labeled(\"a\\\"b\", Figure.Rect(w: 1.0, h: -2.0)))\n    \
         layers: int? = Some(2)\n    parts: [Figure] = [Figure.Circle(1.0), Figure.Group([])]\n    pair: (int, str) = (3, \"x\")\n    \
         flags: [int: [bool]] = [2: [true]]\n    border: Frame = Frame(width: 2.0, labels: [\"l\"])\n    \
         seen: Set<str> = Set([\"a\", \"b\"])\n    none: Set<int> = Set()\n\nImplements:\n    Eq\n    Encode\n    Decode\n"
    );
}

#[test]
fn natives_take_num_evidence() {
    use sandlang::native::ty::{Num, Param, VmVal};
    use sandlang::types::traits;
    use sandlang::vm::{Ctl, Fail, Vm};
    let module = |decl: &str| {
        let mut m = NativeModule::new("nums");
        m.function("total", |vm: &mut Vm, num: Num<0>, xs: VmVal<Vec<Param<0, traits::Num>>>| -> Result<Param<0>, Ctl> {
            let mut acc = num.zero(vm)?.raw();
            for &x in xs.items(&vm.heap) {
                acc = match (acc, x) {
                    (sandlang::vm::value::VmValue::Int(a), sandlang::vm::value::VmValue::Int(b)) => {
                        sandlang::vm::value::VmValue::Int(a + b)
                    }
                    (sandlang::vm::value::VmValue::Float(a), sandlang::vm::value::VmValue::Float(b)) => {
                        sandlang::vm::value::VmValue::Float(a + b)
                    }
                    _ => return Err(Fail::internal("not a number").into()),
                };
            }
            Ok(Param::from_raw(acc))
        });
        m.function("unit", |vm: &mut Vm, num: Num<1>, _: Param<0>, _: Param<1>| num.one(vm));
        m.source(decl);
        m
    };
    let decls = "pub native fn total<T: Num>(xs: [T]) -> T\npub native fn unit<A, B: Num>(a: A, b: B) -> B";
    let src = r#"
import nums
print(nums.total([1, 2, 3]))
print(nums.total([0.5, 0.25]))
let empty: [float] = []
print(nums.total(empty))
fn twice<T: Num>(xs: [T]) -> T {
    nums.total(xs) + nums.total(xs)
}
print(twice([2, 3]))
print(twice([1.5]))
print(nums.unit("a", 7.0))
print(nums.unit(true, 7))
let f: fn([int]) -> int = nums.total
print(f([4, 5]))
print([1.5, 2.5].sum())
print([1, 2, 3].product())
let none: [float] = []
print(none.product())
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([module(decls)]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "6\n0.75\n0.0\n10\n3.0\n1.0\n1\n9\n4.0\n6\n1.0\n");

    let cases = [
        (
            "pub native fn total<T>(xs: [T]) -> T\npub native fn unit<A, B: Num>(a: A, b: B) -> B",
            "native function `total` is declared `fn([T]) -> T`, but its Rust function is `fn(<Num evidence of T>, [T]) -> T`",
        ),
        (
            "pub native fn total<T: Num>(xs: [T]) -> T\npub native fn unit<A, B: Ord>(a: A, b: B) -> B",
            "native function `unit` is declared `fn(A, B) -> B`, but its Rust function is `fn(<Num evidence of B>, A, B) -> B`",
        ),
    ];
    for (decl, msg) in cases {
        let errors = host_module_errors([module(decl)]);
        assert!(errors.iter().any(|e| e == msg), "{decl}: {errors:?}");
    }

    let mut m = module("pub native fn total<T: Num>(xs: [T]) -> T\npub native fn unit<A: Num, B: Num>(a: A, b: B) -> B");
    m.function("first", |xs: Vec<Param<0, traits::Num>>| xs[0]);
    m.source("pub native fn first<T: Num>(xs: [T]) -> T");
    let src = "import nums\nprint(nums.first([2.5]))\nprint(nums.unit(1, 2))\nlet f: fn([int]) -> int = nums.first\nprint(f([7]))";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "2.5\n1\n7\n");

    let mut m = NativeModule::new("nums");
    m.function("first", |xs: Vec<Param<0, traits::Num>>| xs[0]);
    m.source("pub native fn first<T: Ord>(xs: [T]) -> T");
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["native function `first` must declare `T: Num`, as its Rust function requires `T: Num`"]);

    let mut m = NativeModule::new("nums");
    m.function("List.go", |vm: &mut Vm, num: Num<0>, _: VmVal<Vec<Param<0, traits::Num>>>| num.one(vm));
    m.source("pub trait Go { fn go(self) -> int }\nimpl<T: Num> Go for [T] { native fn go(self) -> T }");
    let errors = host_module_errors([m]);
    assert!(errors.iter().any(|e| e == "the Rust function of a native impl method cannot take `Num` evidence"), "{errors:?}");
}

#[test]
fn natives_can_require_encode() {
    use sandlang::native::ty::Param;
    use sandlang::types::traits;
    let module = |decl: &str| {
        let mut m = NativeModule::new("store");
        m.function("keep", |x: Param<0, traits::Encode>| x);
        m.source(decl);
        m
    };
    let run = |src: &str| {
        run_src(
            Config::new().host_modules(HostModules::new([module("pub native fn keep<T: Encode>(x: T) -> T")]).unwrap()),
            &format!("import store\n{src}"),
        )
    };
    let (r, out, _) = run("print(store.keep((1, [\"a\"])))\nfn pass<T: Num>(x: T) -> T {\n    store.keep(x)\n}\nprint(pass(2))");
    r.unwrap();
    assert_eq!(out, "(1, [\"a\"])\n2\n");
    let cases = [
        ("store.keep(() => 1)", "fn() -> int does not implement `Encode`"),
        ("fn pass<T>(x: T) -> T {\n    store.keep(x)\n}", "type parameter `T` does not implement `Encode`; add `T: Encode`"),
        ("let f: fn(fn() -> int) -> fn() -> int = store.keep", "fn() -> int does not implement `Encode`"),
    ];
    for (src, msg) in cases {
        let (r, _, _) = run(src);
        let Err(Error::Compile(ds)) = r else { panic!("{src}: expected a compile error, got {r:?}") };
        assert_eq!(ds[0].message, msg, "{src}");
    }
    let errors = host_module_errors([module("pub native fn keep<T>(x: T) -> T")]);
    assert_eq!(errors, ["native function `keep` must declare `T: Encode`, as its Rust function requires `T: Encode`"]);
}

#[test]
fn json_rejects_unrepresentable_types_at_compile_time() {
    let cases = vec![
        ("json.encode((x: int) => x)", "fn(int) -> int does not implement `Encode`"),
        ("json.encode([spawn 1])", "[Task<int>] does not implement `Encode` because the elements of `[Task<int>]` have type `Task<int>`"),
        ("let c: Channel<int> = Channel()\njson.to_json(c)", "Channel<int> does not implement `Encode`"),
        (
            "let f: fn((int, fn() -> int), int) -> str = json.encode",
            "(int, fn() -> int) does not implement `Encode` because element 1 of `(int, fn() -> int)` has type `fn() -> int`",
        ),
        (
            "struct S { f: fn() -> int }\njson.encode(S(f: () => 1))",
            "S does not implement `Encode` because field `f` of `S` has type `fn() -> int`",
        ),
        ("fn dump<T>(x: T) -> str {\n    json.encode(x)\n}", "type parameter `T` does not implement `Encode`; add `T: Encode`"),
    ];
    let http = ("import http\nlet r = http.post_json(\"http://h/\", (x: int) => x)", "fn(int) -> int does not implement `Encode`");
    for (src, msg) in cases.into_iter().chain(cfg!(feature = "os").then_some(http)) {
        let (r, _, _) = run_src(Config::new(), &format!("import json\n{src}"));
        let Err(Error::Compile(ds)) = r else { panic!("{src}: expected a compile error, got {r:?}") };
        assert_eq!(ds[0].message, msg, "{src}");
    }
    let src = "import json\nprint(json.encode(1..4))\nprint(json.to_json([Some(1), None]))\nfn dump<T: Encode>(x: T) -> str {\n    json.encode(x)\n}\nprint(dump([1]))";
    let (r, out, _) = run_src(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "{\"start\":1,\"end\":4}\nArr([Int(1), Null])\n[1]\n");
}

#[test]
fn host_functions_take_and_return_json_values() {
    use sandlang::stdlib::json::Json;
    let mut m = NativeModule::new("doc");
    m.function("keys", |j: Json| match j {
        Json::Obj(fields) => fields.keys().cloned().collect(),
        _ => Vec::new(),
    });
    m.function("make", || {
        let mut fields = indexmap::IndexMap::new();
        fields.insert("z".to_string(), Json::Int(1));
        fields.insert("a".to_string(), Json::Arr(vec![Json::Null, Json::Bool(true), Json::Float(1.5), Json::Str("s".into())]));
        Json::Obj(fields)
    });
    m.function("same", |a: Json, b: Json| a == b);
    let src = r#"
import doc
import json
print(doc.keys(json.parse("{\"b\": 1, \"a\": [2], \"c\": null}")?))
let made = doc.make()
print(json.encode(made))
print(doc.same(made, json.parse(json.encode(made))?))
print(made.get("a")?.at(2)?.as_float())
print(doc.keys(json.Json.Null))
"#;
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "[\"b\", \"a\", \"c\"]\n{\"z\":1,\"a\":[null,true,1.5,\"s\"]}\ntrue\nSome(1.5)\n[]\n");
}

#[test]
fn convert_deep_json_values_on_small_stack() {
    use sandlang::stdlib::json::Json;
    fn nest(n: usize) -> Json {
        (0..n).fold(Json::Str("leaf".into()), |j, i| match i % 2 {
            0 => Json::Arr(vec![Json::Int(i as i64), j, Json::Null]),
            _ => Json::Obj([("a".to_string(), Json::Bool(true)), ("k".to_string(), j)].into_iter().collect()),
        })
    }
    let run = |src: &'static str| {
        let run = move || {
            let mut m = NativeModule::new("doc");
            m.function("make", |n: i64| nest(n as usize));
            m.function("check", |j: Json, n: i64| j == nest(n as usize));
            run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), &format!("import doc\nimport json\n{src}"))
        };
        std::thread::Builder::new().stack_size(512 << 10).spawn(run).unwrap().join().unwrap()
    };
    let (r, out, _) = run("let text = json.encode(doc.make(512))\nlet parsed = json.parse(text)?\nprint(doc.check(parsed, 512))\n\
         print(json.encode(parsed) == text)\nprint(doc.check(json.Json.Obj([\"k\": doc.make(511)]), 512))");
    r.unwrap();
    assert_eq!(out, "true\ntrue\nfalse\n");
    for src in [
        "print(doc.make(513))",
        "print(doc.check(json.Json.Arr([doc.make(512)]), 513))",
        "var j = json.Json.Null\nfor _ in 0..100000 {\n    j = json.Json.Arr([j])\n}\nprint(doc.check(j, 0))",
    ] {
        let (r, _, _) = run(src);
        let Err(Error::Runtime { message, .. }) = r else { panic!("{src}: expected a runtime error, got {r:?}") };
        assert_eq!(message, "limit: value too deeply nested to convert", "{src}");
    }
}

#[derive(Clone, Copy, sandlang::SandEnum)]
#[sand(module = "zoo")]
enum Zebra {
    Stripe,
}

#[doc = include_str!("include/apple.md")]
#[derive(sandlang::SandStruct)]
#[sand(module = "zoo")]
struct Apple {
    #[doc = include_str!("include/apple.md")]
    grams: i64,
}

#[test]
fn help_lists_rust_bound_items_in_registration_order() {
    let mut m = NativeModule::new("zoo");
    m.bind::<Zebra>().bind::<Apple>();
    m.function("zz", || 1i64).doc("Last letter.");
    m.function("aa", || Apple { grams: 1 }).doc("First letter.");
    m.source("/// From source.\npub fn mm() {}");
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), "import zoo\nhelp(zoo)\nhelp(zoo.Apple)");
    r.unwrap();
    let order: Vec<usize> =
        ["enum Zebra", "struct Apple", "fn zz()", "fn aa()", "fn mm()"].iter().map(|s| out.find(s).unwrap_or(usize::MAX)).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{out}");
    assert!(out.ends_with("struct zoo.Apple\n\nA fruit, documented in a file.\n\nIt has one field.\n\nFields:\n    grams: int\n        A fruit, documented in a file.\n\nImplements:\n    Eq\n    Ord\n    Hash\n    Encode\n    Decode\n"), "{out}");
}

#[test]
fn native_declarations_must_require_the_traits_of_their_rust_functions() {
    use sandlang::native::ty::Param;
    use sandlang::types::traits;
    let module = |decl: &str| {
        let mut m = NativeModule::new("dedupe");
        m.function("uniq", |xs: Vec<Param<0, traits::Hash>>| xs);
        m.function("top", |xs: Vec<Param<0, traits::Ord>>, _key: Param<1, (traits::Ord, traits::Hash)>| xs);
        m.source(decl);
        m
    };
    let decls = "pub native fn uniq<T: Hash>(xs: [T]) -> [T]\npub native fn top<T, K: Hash>(xs: [T], key: K) -> [T] where T: Ord, K: Ord";
    let (r, out, _) = run_src(
        Config::new().host_modules(HostModules::new([module(decls)]).unwrap()),
        "import dedupe\nprint(dedupe.uniq([1, 2]))\nprint(dedupe.top([3], 1))",
    );
    r.unwrap();
    assert_eq!(out, "[1, 2]\n[3]\n");

    let cases = [
        ("pub native fn uniq<T>(xs: [T]) -> [T]", "native function `uniq` must declare `T: Hash`, as its Rust function requires `T: Hash`"),
        (
            "pub native fn uniq<T: Eq>(xs: [T]) -> [T]",
            "native function `uniq` must declare `T: Hash`, as its Rust function requires `T: Hash`",
        ),
        (
            "pub native fn top<T, K: Hash>(xs: [T], key: K) -> [T]",
            "native function `top` must declare `T: Ord`, as its Rust function requires `T: Ord`",
        ),
        (
            "pub native fn top<T: Ord, K>(xs: [T], key: K) -> [T]",
            "native function `top` must declare `K: Ord + Hash`, as its Rust function requires `K: Ord + Hash`",
        ),
    ];
    for (decl, msg) in cases {
        let errors = host_module_errors([module(decl)]);
        assert!(errors.iter().any(|e| e == msg), "{decl}: {errors:?}");
    }
}

#[test]
fn generic_natives_take_type_parameters() {
    use sandlang::native::ty::{Param, Reflect, VmVal};
    use sandlang::vm::{Fail, Vm};
    let module = |src: &str| {
        let mut m = NativeModule::new("gen");
        m.source(src);
        m
    };
    let mut m = module("pub native fn second<T>(xs: [T]) -> T?\npub native fn swap<A, B>(p: (A, B)) -> (B, A)");
    m.function("second", |vm: &mut Vm, xs: VmVal<Vec<Param<0>>>| xs.items(&vm.heap).get(1).copied().map(Param::<0>::from_raw));
    m.function("swap", |vm: &mut Vm, p: VmVal<(Param<0>, Param<1>)>| -> Result<(Param<1>, Param<0>), Fail> {
        let (a, b) = (vm.heap.field(p.raw(), 0), vm.heap.field(p.raw(), 1));
        Ok((Param::from_raw(b.unwrap()), Param::from_raw(a.unwrap())))
    });
    let (r, out, _) = run_src(
        Config::new().host_modules(HostModules::new([m]).unwrap()),
        "import gen\nprint(gen.second([\"a\", \"b\"]))\nprint(gen.swap((1, \"x\")))",
    );
    r.unwrap();
    assert_eq!(out, "Some(\"b\")\n(\"x\", 1)\n");

    type Register = fn(&mut NativeModule);
    let cases: [(&str, Register); 4] = [
        ("is declared `fn([T]) -> T?`, but its Rust function is `fn([T]) -> T`", |m| {
            m.function("second", |_: &mut Vm, xs: VmVal<Vec<Param<0>>>| -> Result<Param<0>, Fail> { Ok(Param::from_raw(xs.raw())) });
        }),
        ("uses type parameter 1, which its declaration does not have", |m| {
            m.function("second", |_: &mut Vm, _: VmVal<Vec<Param<1>>>| -> Result<Option<Param<1>>, Fail> { Ok(None) });
        }),
        ("is declared `fn([T]) -> T?`, but its Rust function is `fn(<description of T>, [T]) -> T?`", |m| {
            m.function("second", |_: &mut Vm, _: Reflect<0>, _: VmVal<Vec<Param<0>>>| -> Result<Option<Param<0>>, Fail> { Ok(None) });
        }),
        ("is declared `fn([T]) -> T?`, but its Rust function is `fn([int]) -> int?`", |m| {
            m.function("second", |xs: Vec<i64>| xs.get(1).copied());
        }),
    ];
    for (msg, register) in cases {
        let mut m = module("pub native fn second<T>(xs: [T]) -> T?");
        register(&mut m);
        let errors = host_module_errors([m]);
        assert!(errors[0].contains(msg), "{msg}: {errors:?}");
    }

    let mut m = NativeModule::new("gen");
    m.function("second", |_: &mut Vm, _: VmVal<Vec<Param<0>>>| -> Result<Option<Param<0>>, Fail> { Ok(None) });
    let errors = host_module_errors([m]);
    assert!(errors[0].contains("native `gen.second` is generic, so the module must declare it with `native fn`"), "{errors:?}");
}

#[test]
fn reflecting_natives_take_type_descriptions() {
    use sandlang::native::ty::{Param, Reflect};
    use sandlang::vm::Fail;
    use sandlang::vm::Vm;
    let mut m = NativeModule::new("desc");
    m.source("pub native fn describe<reflect T>(x: T) -> str");
    m.function("describe", |vm: &mut Vm, t: Reflect<0>, _: Param<0>| -> Result<String, Fail> { Ok(format!("{:?}", t.desc(&vm.heap)?)) });
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), "import desc\nprint(desc.describe([1]))");
    r.unwrap();
    assert_eq!(out, "List(Int)\n");
}

#[test]
fn host_functions_take_and_return_durations_and_never() {
    use jiff::SignedDuration;
    use sandlang::native::ty::Never;
    use sandlang::vm::{Fail, Vm};
    let mut m = NativeModule::new("clock");
    m.function("double", |d: SignedDuration| d * 2);
    m.function("forever", || SignedDuration::MAX);
    m.function("halt", |_: &mut Vm, code: i64| -> Result<Never, Fail> { Err(Fail::exit(code as i32)) });
    let (r, out, _) = run_src(
        Config::new().host_modules(HostModules::new([m.clone()]).unwrap()),
        "import clock\nprint(clock.double(1500ms))\nlet n: int = clock.halt(3)",
    );
    assert_eq!((r.unwrap().exit_code, out.as_str()), (3, "3s\n"));
    let (r, _, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), "import clock\nprint(clock.forever())");
    assert!(matches!(&r, Err(Error::Runtime { message, .. }) if message.contains("the duration is out of range")), "{r:?}");
}

fn quit_module() -> HostModules {
    let mut module = NativeModule::new("quit");
    module.function("now", |code: i64| async move { Err::<(), sandlang::vm::Fail>(sandlang::vm::Fail::exit(code as i32)) });
    HostModules::new([module]).unwrap()
}

fn script_with_deferred_task(deferred: &str) -> String {
    format!("import quit\nlet c: Channel<int> = Channel()\nspawn {{\n    defer {deferred}\n    c.recv()\n}};\ntask.yield_now()")
}

#[cfg(feature = "os")]
#[test]
fn exit_from_a_finisher_ends_a_test_like_a_script_exit() {
    let dir = TempDir::new("exit-tests");
    let main = dir.write("t.sand", "import quit\ntest \"zero\" { quit.now(0) }\ntest \"one\" { quit.now(1) }\ntest \"plain\" { exit(0) }");
    let mut engine = Engine::new(Config::new().host_modules(quit_module()));
    let report = rt().block_on(engine.run_tests(&main)).unwrap();
    let results: Vec<_> = report.results.iter().map(|r| (r.name.as_str(), r.passed, r.message.as_deref())).collect();
    assert_eq!(results, [("zero", true, None), ("one", false, Some("exited with code 1")), ("plain", true, None)]);
}

#[cfg(feature = "os")]
#[test]
fn exit_from_a_finisher_in_module_init_fails_each_test_like_a_script_exit() {
    for code in [0, 4] {
        let dir = TempDir::new("exit-init");
        let main = dir.write("t.sand", &format!("import quit\nquit.now({code})\ntest \"t\" {{ }}"));
        let mut engine = Engine::new(Config::new().host_modules(quit_module()));
        let report = rt().block_on(engine.run_tests(&main)).unwrap();
        assert_eq!(report.results[0].message.as_deref(), Some(format!("module exited with code {code}").as_str()));
    }
}

#[test]
fn exit_from_a_finisher_in_deferred_code_sets_the_exit_code() {
    for deferred in ["quit.now(5)", "exit(5)"] {
        let (outcome, _, _) = run_src(Config::new().host_modules(quit_module()), &script_with_deferred_task(deferred));
        assert_eq!(outcome.unwrap().exit_code, 5, "{deferred}");
    }
}

#[derive(Clone)]
struct BacklogSink {
    waits: Arc<AtomicUsize>,
    interrupt_on_wait: Option<InterruptHandle>,
}

impl sandlang::vm::OutputSink for BacklogSink {
    fn write(&self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }

    fn wait_backlog(&self, _: usize, _: Duration) -> bool {
        self.waits.fetch_add(1, Ordering::SeqCst);
        if let Some(interrupt) = &self.interrupt_on_wait {
            interrupt.interrupt();
        }
        self.interrupt_on_wait.is_none()
    }
}

#[test]
fn ending_tasks_drains_output_after_an_exit_or_a_trap() {
    for deferred in ["exit(5)", "quit.now(5)", "fail(\"error\", \"boom\")"] {
        let sink = BacklogSink { waits: Arc::new(AtomicUsize::new(0)), interrupt_on_wait: None };
        let config = Config::new().host_modules(quit_module()).stdout(sink.clone());
        let mut engine = Engine::new(config);
        let _ = rt().block_on(engine.run_source("main.sand", &script_with_deferred_task(deferred)));
        assert_eq!(sink.waits.load(Ordering::SeqCst), 2, "{deferred}");
    }
}

#[test]
fn a_failed_drain_replaces_an_exit_but_not_a_trap() {
    let run = |src: &str| {
        let interrupt = InterruptHandle::new();
        let sink = BacklogSink { waits: Arc::new(AtomicUsize::new(0)), interrupt_on_wait: Some(interrupt.clone()) };
        let mut engine = Engine::new(Config::new().host_modules(quit_module()).stdout(sink).interrupt(interrupt));
        rt().block_on(engine.run_source("main.sand", src))
    };
    for src in ["exit(3)", "import quit\nquit.now(3)"] {
        assert!(matches!(run(src), Err(Error::Interrupted)), "{src}");
    }
    assert!(matches!(run("fail(\"error\", \"boom\")"), Err(Error::Runtime { .. })));
}

#[test]
fn public_fields_of_public_host_types_use_public_types() {
    let mut m = NativeModule::new("shapes");
    m.source("struct Hidden { x: int }\npub struct Shown { pub h: Hidden }\npub struct Fine { h: Hidden }");
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["public field `h` uses the private type `Hidden`; make `Hidden` public"]);
}

#[test]
fn native_module_custom_capability() {
    let mut m = NativeModule::new("door");
    m.function("open", |vm: &mut Vm, name: String| -> Result<String, sandlang::native::convert::HostError> {
        vm.require(&Capability::Custom { name: "door".into(), detail: name.clone() })?;
        Ok(format!("opened {name}"))
    });
    let src = "import door\nprint(door.open(\"front\"))\nprint(door.open(\"back\"))";
    let policy = |cap: &Capability| match cap {
        Capability::Custom { detail, .. } if detail == "front" => Decision::Allow,
        Capability::Custom { .. } => Decision::Deny,
        _ => Decision::Allow,
    };
    let (r, out, _) = run_src(Config::new().custom_policy(policy).host_modules(HostModules::new([m]).unwrap()), src);
    r.unwrap();
    assert_eq!(out, "Ok(\"opened front\")\nErr(Error(message: \"permission denied: door back\", kind: \"permission_denied\"))\n");
}

#[test]
fn native_module_capability_prompts() {
    let opened = Arc::new(AtomicUsize::new(0));
    let o = opened.clone();
    let mut m = NativeModule::new("door");
    m.function("open", move |vm: &mut Vm, name: String| -> Result<String, sandlang::native::convert::HostError> {
        vm.require(&Capability::Custom { name: "door".into(), detail: name.clone() })?;
        o.fetch_add(1, Ordering::SeqCst);
        Ok(format!("opened {name}"))
    });
    let prompter = ScriptedPrompter::new(&[PromptAnswer::AllowOnce, PromptAnswer::DenyOnce]);
    let policy = |cap: &Capability| if matches!(cap, Capability::Custom { .. }) { Decision::Prompt } else { Decision::Allow };
    let out = MemorySink::new();
    let mut engine = Engine::new(prompter.config(policy).host_modules(HostModules::new([m]).unwrap()).stdout(out.clone()));
    rt().block_on(engine.run_source("main.sand", "import door\nprint(door.open(\"front\"))\nprint(door.open(\"back\"))")).unwrap();
    assert_eq!(
        out.contents(),
        "Ok(\"opened front\")\nErr(Error(message: \"permission denied: door back\", kind: \"permission_denied\"))\n"
    );
    let door = |detail: &str| Capability::Custom { name: "door".into(), detail: detail.into() };
    assert_eq!(prompter.asked(), vec![door("front"), door("back")]);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Compiled programs and host modules
// ---------------------------------------------------------------------------

/// Runs `program` on a new engine with `config` and returns its outcome and stdout.
fn run_program_with(config: Config, program: &CompiledProgram) -> (Result<sandlang::engine::Outcome, Error>, String) {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone()));
    let r = rt().block_on(engine.run_program(program));
    (r, out.contents())
}

#[test]
fn compiled_programs_use_their_compiled_host_modules() {
    let host_modules = HostModules::new([geo_module()]).unwrap();
    let compiler = Engine::new(Config::new().host_modules(host_modules));
    let src = "import geo\nvar runs = 0\nruns += 1\nprint(\"\\(geo.dist(3.0, 4.0)) \\(runs)\")";
    let program = rt().block_on(compiler.compile_source("main.sand", src)).unwrap();
    for _ in 0..2 {
        let (r, out) = run_program_with(Config::new(), &program);
        r.unwrap();
        assert_eq!(out, "5.0 1\n");
    }
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().stdout(out.clone()));
    rt().block_on(engine.run_program(&program)).unwrap();
    rt().block_on(engine.run_program(&program)).unwrap();
    assert_eq!(out.contents(), "5.0 1\n5.0 1\n");
}

#[test]
fn compile_methods_check_types_and_import_policies() {
    let engine = Engine::new(Config::new());
    let r = rt().block_on(engine.compile_source("main.sand", "let x: int = \"text\""));
    assert!(matches!(r, Err(Error::Compile(_))), "{r:?}");
    let r = rt().block_on(engine.compile_source("main.sand", "import geo"));
    assert!(matches!(r, Err(Error::Compile(_))), "{r:?}");

    let fs = MemFs::new();
    fs.write("/app/main.sand", "import \"lib\"\nprint(lib.value())").unwrap();
    fs.write("/app/lib.sand", "pub fn value() -> int { 1 }").unwrap();
    let r = rt().block_on(Engine::new(Config::new().memory_fs(fs.clone())).compile_file("/app/main.sand"));
    let Err(Error::Compile(ds)) = r else { panic!("expected a denied import, got {r:?}") };
    assert!(ds[0].message.contains("permission denied"), "{ds:?}");

    let policy = RulePolicy::standard().allow_import(PathPattern::literal("/app").unwrap());
    let compiler = Engine::new(Config::new().policy(policy).memory_fs(fs.clone()));
    let program = rt().block_on(compiler.compile_file("/app/main.sand")).unwrap();
    // `run_program` uses the modules loaded during compilation.
    fs.write("/app/lib.sand", "pub fn value() -> int { 2 }").unwrap();
    let (r, out) = run_program_with(Config::new().memory_fs(fs), &program);
    r.unwrap();
    assert_eq!(out, "1\n");
}

#[test]
fn run_program_uses_the_running_engine_working_directory() {
    let src = "import fs\nprint(fs.read_text(\"in.txt\")?)";
    let fs = MemFs::new();
    fs.write("/app/main.sand", src).unwrap();
    fs.write("/in.txt", "root").unwrap();
    fs.write("/app/in.txt", "app").unwrap();
    fs.write("/work/in.txt", "work").unwrap();
    let compiler = Engine::new(Config::new().memory_fs(fs.clone()).module_root("/app").cwd("/work"));
    let programs = [
        rt().block_on(compiler.compile_file("/app/main.sand")).unwrap(),
        rt().block_on(compiler.compile_source("main.sand", src)).unwrap(),
        rt().block_on(Engine::new(Config::new()).compile_source("main.sand", src)).unwrap(),
    ];
    let policy = RulePolicy::standard().allow_read("/".parse().unwrap());
    let config = || Config::new().policy(policy.clone()).memory_fs(fs.clone());
    for program in &programs {
        for (config, expected) in [(config(), "root\n"), (config().module_root("/app"), "app\n"), (config().cwd("/work"), "work\n")] {
            let (r, out) = run_program_with(config, program);
            r.unwrap();
            assert_eq!(out, expected);
        }
    }
}

#[test]
fn run_program_measures_limits_per_run() {
    let program = rt().block_on(Engine::new(Config::new()).compile_source("main.sand", "var n = 0\nfor i in 0..20000 { n += i }")).unwrap();
    let limited = |max: u64| Config::new().limits(Limits { max_instructions: Some(max), ..Limits::default() });
    let fits = |max: u64| rt().block_on(Engine::new(limited(max)).run_program(&program)).is_ok();
    assert!(!fits(1_000));
    let mut max = 1_000;
    while !fits(max) {
        max *= 2;
    }
    // Each run executes more than `max / 2` instructions.
    let mut engine = Engine::new(limited(max));
    for _ in 0..3 {
        rt().block_on(engine.run_program(&program)).unwrap();
    }
}

#[test]
fn host_modules_and_programs_are_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    let host_modules = HostModules::new([geo_module()]).unwrap();
    assert_send_sync(&host_modules);
    let compiler = Engine::new(Config::new().host_modules(host_modules));
    let program = rt().block_on(compiler.compile_source("main.sand", "import geo\nprint(geo.dist(6.0, 8.0))")).unwrap();
    assert_send_sync(&program);
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let program = program.clone();
            std::thread::spawn(move || run_program_with(Config::new(), &program))
        })
        .collect();
    for h in handles {
        let (r, out) = h.join().unwrap();
        r.unwrap();
        assert_eq!(out, "10.0\n");
    }
}

// ---------------------------------------------------------------------------
// Sandbox policy
// ---------------------------------------------------------------------------

#[cfg(feature = "os")]
#[test]
fn deny_all_blocks_io_but_allows_output() {
    let dir = TempDir::new("deny");
    dir.write("secret.txt", "s3cret");
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard()).cwd(dir.path());
    let src = r#"
import fs
import env
import net
let r = fs.read_text("secret.txt")
print(r.unwrap_err().kind)
print(fs.write_text("out.txt", "x").unwrap_err().kind)
print(env.get("HOME"))
print(net.connect("127.0.0.1", 1).unwrap_err().kind)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "permission_denied\npermission_denied\nNone\npermission_denied\n");
    assert!(!dir.path().join("out.txt").exists());
}

#[test]
fn stdout_can_be_denied() {
    let policy = |cap: &Capability| if matches!(cap, Capability::Stdout) { Decision::Deny } else { Decision::Allow };
    let (r, out, _) = run_src(Config::new().custom_policy(policy), "print(\"x\")");
    match r {
        Err(Error::Runtime { message, .. }) => {
            assert!(message.contains("permission denied: stdout"))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out, "");
}

#[test]
fn log_records_go_to_the_host_sink() {
    let records = Arc::new(Mutex::new(Vec::<LogRecord>::new()));
    let r2 = records.clone();
    let policy = |cap: &Capability| if matches!(cap, Capability::Stderr) { Decision::Deny } else { Decision::Allow };
    let config = Config::new().custom_policy(policy).log_sink(move |r: &LogRecord| r2.lock().push(r.clone()));
    let src = r#"
import log
fn start(user: str) {
    log.info("started", fields: ["user": user, "note": "a b"])
}
log.debug("hidden")
log.set_level(log.Level.Debug)
log.debug("details")
start("ann")
log.set_level(log.Level.Error)
log.warn("hidden")
log.error("failed")
"#;
    let (r, _, err) = run_src(config, src);
    r.unwrap();
    assert_eq!(err, "");
    let records = records.lock();
    let summary: Vec<_> =
        records.iter().map(|r| (r.level, r.message.as_str(), r.location.as_ref().map(|l| (l.function.as_str(), l.line)))).collect();
    assert_eq!(
        summary,
        [
            (LogLevel::Debug, "details", Some(("<top level>", 8))),
            (LogLevel::Info, "started", Some(("start", 4))),
            (LogLevel::Error, "failed", Some(("<top level>", 12))),
        ]
    );
    assert_eq!(records[1].fields, [("user".to_string(), "ann".to_string()), ("note".to_string(), "a b".to_string())]);
}

#[test]
fn log_sinks_select_levels() {
    struct WarnSink(Arc<Mutex<Vec<String>>>);
    impl LogSink for WarnSink {
        fn enabled(&self, level: LogLevel) -> bool {
            level >= LogLevel::Warn
        }
        fn log(&self, record: &LogRecord) {
            self.0.lock().push(record.message.clone());
        }
    }
    let messages = Arc::new(Mutex::new(Vec::new()));
    let config = Config::new().log_sink(WarnSink(messages.clone()));
    let src = "import log\nlog.debug(\"d\")\nlog.info(\"i\")\nlog.warn(\"w\")\nlog.disable()\nlog.error(\"e\")";
    let (r, _, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(*messages.lock(), ["w"]);
}

#[test]
fn hosts_set_the_starting_log_level() {
    let src = "import log\nlog.debug(\"d\")\nlog.set_level(log.Level.Info)\nlog.debug(\"hidden\")";
    let (r, _, err) = run_src(Config::new().policy(sandlang::policy::rules::RulePolicy::allow_all()).log_level(LogLevel::Debug), src);
    r.unwrap();
    assert_eq!(err, "DEBUG d\n");
}

#[test]
fn log_records_are_dropped_if_stderr_is_denied() {
    let policy = |cap: &Capability| if matches!(cap, Capability::Stderr) { Decision::Deny } else { Decision::Allow };
    let (r, out, err) = run_src(Config::new().custom_policy(policy), "import log\nlog.error(\"failed\")\nprint(\"done\")");
    r.unwrap();
    assert_eq!((out.as_str(), err.as_str()), ("done\n", ""));
}

#[test]
fn log_records_count_against_the_output_limit() {
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    let config = Config::new()
        .limits(Limits { max_output_bytes: Some(1000), ..Limits::default() })
        .log_sink(move |_: &LogRecord| _ = c2.fetch_add(1, Ordering::Relaxed));
    let (r, _, _) = run_src(config, "import log\nfor i in 0..1000 { log.info(\"record\", fields: [\"i\": \"\\(i)\"]) }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Output))), "{r:?}");
    let n = count.load(Ordering::Relaxed);
    assert!(n > 50 && n < 125, "{n}");
}

#[test]
fn stdin_requires_permission() {
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard()).stdin_bytes(b"line\n".to_vec());
    let (r, out, _) = run_src(config, "import io\nprint(io.stdin.read_line().unwrap_err().kind)");
    r.unwrap();
    assert_eq!(out, "permission_denied\n");
}

#[cfg(feature = "os")]
#[test]
fn rule_policy_confines_paths() {
    let dir = TempDir::new("rules");
    dir.write("public/a.txt", "A");
    dir.write("private/b.txt", "B");
    std::fs::create_dir_all(dir.path().join("public/sub")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("private/b.txt"), dir.path().join("public/link.txt")).unwrap();

    let policy = sandlang::policy::rules::RulePolicy::standard()
        .allow_read(literal_path(dir.path().join("public")))
        .allow_write(literal_path(dir.path().join("public/sub")));
    let config = Config::new().policy(policy).cwd(dir.path());
    let src = r#"
import fs
print(fs.read_text("public/a.txt"))
print(fs.read_text("private/b.txt").unwrap_err().kind)
print(fs.read_text("public/../private/b.txt").unwrap_err().kind)
print(fs.read_text("public/sub/../../private/b.txt").unwrap_err().kind)
print(fs.read_text("public/link.txt").unwrap_err().kind)
print(fs.write_text("public/sub/new.txt", "n").is_ok())
print(fs.write_text("public/new.txt", "n").unwrap_err().kind)
print(fs.list_dir("public").is_ok())
print(fs.list_dir(".").unwrap_err().kind)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    let expected_link = if cfg!(unix) { "permission_denied" } else { "not_found" };
    assert_eq!(
        out,
        format!(
            "Ok(\"A\")\npermission_denied\npermission_denied\npermission_denied\n{expected_link}\ntrue\npermission_denied\ntrue\npermission_denied\n"
        )
    );
}

#[cfg(feature = "os")]
#[test]
fn rule_policy_deny_overrides_allow() {
    let dir = TempDir::new("denyover");
    dir.write("data/ok.txt", "ok");
    dir.write("data/secret/key.txt", "key");
    let policy = sandlang::policy::rules::RulePolicy::standard()
        .allow_read(literal_path(dir.path().join("data")))
        .deny_read(literal_path(dir.path().join("data/secret")));
    let config = Config::new().policy(policy).cwd(dir.path());
    let (r, out, _) =
        run_src(config, "import fs\nprint(fs.read_text(\"data/ok.txt\").is_ok())\nprint(fs.read_text(\"data/secret/key.txt\").is_ok())");
    r.unwrap();
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn typed_rule_builders_round_trip_through_policy_text() {
    use sandlang::policy::file::PolicyFile;
    use sandlang::policy::rules::{HttpMethod, Methods, NamePattern, RulePolicy, Transports};
    let methods = Methods::only("get".parse().unwrap(), ["M-SEARCH".parse().unwrap(), "GET".parse().unwrap()]);
    let built = RulePolicy::new()
        .allow_read("/data".parse().unwrap())
        .deny_read("/data/**/.env".parse().unwrap())
        .allow_write(PathPattern::literal("/out/*").unwrap())
        .allow_net(Transports::Plaintext, "localhost:8080".parse().unwrap())
        .deny_net(Transports::Both, "10.0.0.0/8".parse().unwrap())
        .allow_http(Transports::Tls, Methods::all(), "api.example.com/v1/**".parse().unwrap())
        .prompt_http(Transports::Both, Methods::all(), "*.example.org/admin".parse().unwrap())
        .deny_http(Transports::Plaintext, methods, "api.example.com/v2".parse().unwrap())
        .deny_listen("localhost:22".parse().unwrap())
        .allow_env("AWS_*".parse().unwrap())
        .allow_env(NamePattern::exact("A*B"));
    let text = "allow read /data\ndeny read /data/**/.env\nallow write \"/out/*\"\nallow net plaintext localhost:8080\ndeny net 10.0.0.0/8\nallow http api.example.com/v1\nprompt http *.example.org/admin\ndeny http plaintext GET M-SEARCH api.example.com/v2\ndeny listen localhost:22\nallow env AWS_*\nallow env \"A*B\"\n";
    assert_eq!(built.to_string(), text);
    assert_eq!(PolicyFile::parse(text, std::path::Path::new("/"), None).unwrap().rules.to_string(), text);
    let repeated = PolicyFile::parse("deny http get GET a.example", std::path::Path::new("/"), None).unwrap();
    assert_eq!(repeated.rules.to_string(), "deny http GET a.example\n");
    assert_eq!("get".parse::<HttpMethod>().unwrap(), HttpMethod::GET);
    assert_eq!("M-SEARCH".parse::<HttpMethod>().unwrap().as_str(), "M-SEARCH");
    for word in ["TLS", "HTTP", "get-x", ""] {
        assert!(word.parse::<HttpMethod>().is_err(), "{word}");
    }
}

#[test]
fn rule_policy_network_hosts() {
    let p = sandlang::policy::rules::RulePolicy::standard()
        .allow_net(sandlang::policy::rules::Transports::Tls, "example.com".parse().unwrap())
        .allow_net(sandlang::policy::rules::Transports::Tls, "*.example.org".parse().unwrap())
        .allow_net(sandlang::policy::rules::Transports::Tls, "localhost:8080".parse().unwrap());
    let check = |host: &str, port: u16| p.check(&Capability::Net { host: host.into(), port, tls: true });
    assert_eq!(check("example.com", 443), Decision::Allow);
    assert_eq!(check("example.com", 80), Decision::Allow);
    // Unmatched requests return `Decision::Prompt`.
    assert_eq!(check("evil.com", 80), Decision::Prompt);
    assert_eq!(check("api.example.org", 443), Decision::Allow);
    assert_eq!(check("example.org", 443), Decision::Prompt);
    assert_eq!(check("localhost", 8080), Decision::Allow);
    assert_eq!(check("localhost", 8081), Decision::Prompt);
    let prompting = sandlang::policy::rules::RulePolicy::standard();
    assert_eq!(prompting.check(&Capability::Env("X".into())), Decision::Prompt);
    assert_eq!(prompting.check(&Capability::Stdout), Decision::Allow);
}

#[test]
fn rule_policy_env_and_listen() {
    let p = sandlang::policy::rules::RulePolicy::standard()
        .allow_env("HOME".parse().unwrap())
        .allow_listen("127.0.0.1:*".parse().unwrap())
        .allow_listen("[::1]:0".parse().unwrap())
        .allow_stdin();
    assert_eq!(p.check(&Capability::Env("HOME".into())), Decision::Allow);
    assert_eq!(p.check(&Capability::Env("SECRET".into())), Decision::Prompt);
    let listen = |ip: &str, port| Capability::NetListen(std::net::SocketAddr::new(ip.parse().unwrap(), port));
    assert_eq!(p.check(&listen("127.0.0.1", 9000)), Decision::Allow);
    assert_eq!(p.check(&listen("::ffff:127.0.0.1", 9000)), Decision::Allow);
    assert_eq!(p.check(&listen("0.0.0.0", 9000)), Decision::Prompt);
    assert_eq!(p.check(&listen("::1", 0)), Decision::Allow);
    assert_eq!(p.check(&listen("::1", 9000)), Decision::Prompt, "port 0 allows only an ephemeral port");
    assert_eq!(listen("::1", 0).to_string(), "listen on an ephemeral port at ::1");
    assert_eq!(p.check(&Capability::Stdin), Decision::Allow);

    let p = sandlang::policy::rules::RulePolicy::standard()
        .allow_listen("LocalHost".parse().unwrap())
        .allow_listen("[::]:80".parse().unwrap())
        .allow_listen("*:8080".parse().unwrap())
        .deny_listen("localhost:22".parse().unwrap());
    assert_eq!(p.check(&listen("127.0.0.2", 3000)), Decision::Allow);
    assert_eq!(p.check(&listen("::1", 3000)), Decision::Allow);
    assert_eq!(p.check(&listen("127.0.0.1", 22)), Decision::Deny);
    assert_eq!(p.check(&listen("::ffff:127.0.0.1", 22)), Decision::Deny);
    assert_eq!(p.check(&listen("0:0::0", 80)), Decision::Allow);
    assert_eq!(p.check(&listen("::", 81)), Decision::Prompt);
    assert_eq!(p.check(&listen("10.0.0.1", 8080)), Decision::Allow);
    assert_eq!(p.check(&listen("10.0.0.1", 8081)), Decision::Prompt);
    for invalid in [
        "[::1",
        "[::1]x",
        "[localhost]:80",
        "[127.0.0.1]:80",
        "127.0.0.1:http",
        "127.0.0.1:+80",
        "example.com",
        "example.com:80",
        "127.0.0.1:65536",
    ] {
        assert!(invalid.parse::<sandlang::policy::net::ListenPattern>().is_err(), "{invalid}");
    }
}

#[cfg(feature = "os")]
#[test]
fn denied_import_is_a_compile_error() {
    let dir = TempDir::new("imp");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let policy = |cap: &Capability| if matches!(cap, Capability::Import(_)) { Decision::Deny } else { Decision::Allow };
    let mut engine = Engine::new(Config::new().custom_policy(policy));
    match rt().block_on(engine.run_file(&main)) {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("permission denied"), "{d:?}"),
        other => panic!("{other:?}"),
    }
}

#[cfg(feature = "os")]
#[test]
fn unallowed_import_reports_the_missing_rule() {
    let dir = TempDir::new("imp-default");
    dir.write("lib/util.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib/util.sand\"\nprint(util.X)");
    let lib = sandlang::policy::normalize_path(&dir.path().join("lib"));
    let mut engine = Engine::new(Config::new());
    match rt().block_on(engine.run_file(&main)) {
        Err(Error::Compile(d)) => assert_eq!(
            d[0].message,
            format!(
                "cannot import `./lib/util.sand`: permission denied; unmatched import requires a prompt, and a prompter is not configured; add `allow import {}` to allow it",
                lib.display()
            )
        ),
        other => panic!("{other:?}"),
    }
    let mut engine = Engine::new(Config::new().policy(sandlang::policy::rules::RulePolicy::standard().allow_import(literal_path(&lib))));
    rt().block_on(engine.run_file(&main)).unwrap();
    let util = lib.join("util.sand");
    let policy = sandlang::policy::rules::RulePolicy::standard().allow_import(literal_path(&lib)).prompt_import(literal_path(&util));
    let mut engine = Engine::new(Config::new().policy(policy));
    match rt().block_on(engine.run_file(&main)) {
        Err(Error::Compile(d)) => assert_eq!(
            d[0].message,
            format!(
                "cannot import `./lib/util.sand`: permission denied; the rule `prompt import {}` requires a prompt, and a prompter is not configured",
                util.display()
            )
        ),
        other => panic!("{other:?}"),
    }
    let engine = Engine::new(Config::new().prompter(|_: Capability| async { PromptAnswer::AllowOnce }));
    match engine.check_file(&main) {
        Err(Error::Compile(d)) => {
            assert!(d[0].message.contains("unmatched import requires a prompt, and cannot prompt during check"), "{}", d[0].message)
        }
        other => panic!("{other:?}"),
    }
}

#[cfg(feature = "os")]
#[test]
fn prompt_answers() {
    let dir = TempDir::new("prompt");
    dir.write("a.txt", "A");
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let answers = Arc::new(Mutex::new(vec![PromptAnswer::AllowOnce, PromptAnswer::DenyOnce, PromptAnswer::AllowAlways]));
    let asked2 = asked.clone();
    let answers2 = answers.clone();
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard()).cwd(dir.path()).prompter(move |cap: Capability| {
        asked2.lock().push(cap.to_string());
        let answer = answers2.lock().remove(0);
        async move { answer }
    });
    let src = r#"
import fs
print(fs.read_text("a.txt").is_ok())
print(fs.read_text("a.txt").is_ok())
print(fs.read_text("a.txt").is_ok())
print(fs.read_text("a.txt").is_ok())
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "true\nfalse\ntrue\ntrue\n");
    let asked = asked.lock();
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert!(asked[0].contains("read") && asked[0].contains("a.txt"), "{asked:?}");
}

#[test]
fn prompt_without_prompter_denies() {
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard());
    let (r, out, _) = run_src(config, "import env\nprint(env.get(\"X\"))");
    r.unwrap();
    assert_eq!(out, "None\n");
}

#[test]
fn prompts_suspend_all_execution() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let stable = Arc::new(AtomicBool::new(false));
    let mut m = NativeModule::new("probe");
    let t = ticks.clone();
    m.function("tick", move || {
        t.fetch_add(1, Ordering::SeqCst);
    });
    let (t2, s2) = (ticks.clone(), stable.clone());
    let config = Config::new()
        .host_modules(HostModules::new([m]).unwrap())
        .policy(sandlang::policy::rules::RulePolicy::standard())
        .prompter(move |_cap: Capability| {
            let (t, s) = (t2.clone(), s2.clone());
            async move {
                let before = t.load(Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                let after = t.load(Ordering::SeqCst);
                s.store(before > 0 && before == after, Ordering::SeqCst);
                PromptAnswer::AllowOnce
            }
        });
    let src = r#"
import probe
import env
let ticker = spawn {
    loop {
        probe.tick()
        task.sleep(1ms)
    }
}
task.sleep(20ms)
print(env.get("ANY").is_some() or true)
"#;
    let (r, _, _) = run_src(config, src);
    r.unwrap();
    assert!(stable.load(Ordering::SeqCst), "ticker ran while the prompt was pending");
}

// ---------------------------------------------------------------------------
// Prompts for imports (checked at compile time)
// ---------------------------------------------------------------------------

/// A prompter answering from a script (then `DenyOnce`), recording what it was asked.
#[derive(Clone, Default)]
struct ScriptedPrompter {
    asked: Arc<Mutex<Vec<Capability>>>,
    answers: Arc<Mutex<Vec<PromptAnswer>>>,
}

impl ScriptedPrompter {
    fn new(answers: &[PromptAnswer]) -> ScriptedPrompter {
        ScriptedPrompter { asked: Arc::default(), answers: Arc::new(Mutex::new(answers.to_vec())) }
    }
    fn asked(&self) -> Vec<Capability> {
        std::mem::take(&mut *self.asked.lock())
    }
    fn config(&self, policy: impl sandlang::policy::CustomPolicy + 'static) -> Config {
        self.base().custom_policy(policy)
    }
    fn base(&self) -> Config {
        let p = self.clone();
        Config::new().stdout(MemorySink::new()).prompter(move |cap: Capability| {
            p.asked.lock().push(cap);
            let mut answers = p.answers.lock();
            let answer = if answers.is_empty() { PromptAnswer::DenyOnce } else { answers.remove(0) };
            async move { answer }
        })
    }
}

fn prompt_for_imports() -> impl sandlang::policy::CustomPolicy {
    |cap: &Capability| if matches!(cap, Capability::Import(_)) { Decision::Prompt } else { Decision::Allow }
}

fn import_cap(dir: &TempDir, rel: &str) -> Capability {
    Capability::Import(sandlang::policy::normalize_path(&rel.split('/').fold(dir.path().to_path_buf(), |p, part| p.join(part))))
}

fn assert_import_denied(r: Result<sandlang::engine::Outcome, Error>, import: &str) {
    match r {
        Err(Error::Compile(d)) => {
            assert!(d.iter().any(|d| d.message.starts_with(&format!("cannot import `{import}`: permission denied"))), "{d:?}")
        }
        other => panic!("expected a denied import, got {other:?}"),
    }
}

#[cfg(feature = "os")]
#[test]
fn import_prompt_allow_once_asks_on_every_compile() {
    let dir = TempDir::new("impallowonce");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let prompter = ScriptedPrompter::new(&[PromptAnswer::AllowOnce, PromptAnswer::AllowOnce]);
    let mut engine = Engine::new(prompter.config(prompt_for_imports()));
    rt().block_on(engine.run_file(&main)).unwrap();
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "lib.sand")]);
    rt().block_on(engine.run_file(&main)).unwrap();
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "lib.sand")]);
}

#[cfg(feature = "os")]
#[test]
fn import_prompt_allow_always_is_remembered_by_the_engine() {
    let dir = TempDir::new("impallowalways");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let prompter = ScriptedPrompter::new(&[PromptAnswer::AllowAlways]);
    let mut engine = Engine::new(prompter.config(prompt_for_imports()));
    rt().block_on(engine.run_file(&main)).unwrap();
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "lib.sand")]);
    // Not asked again for later compilations by the same engine...
    rt().block_on(engine.run_file(&main)).unwrap();
    engine.check_file(&main).unwrap();
    assert_eq!(prompter.asked(), vec![]);
    // ...but a different module is a different request.
    dir.write("other.sand", "pub let Y = 2");
    let main2 = dir.write("main2.sand", "import \"./other.sand\"\nimport \"./lib.sand\"\nprint(other.Y + lib.X)");
    assert_import_denied(rt().block_on(engine.run_file(&main2)), "./other.sand");
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "other.sand")]);
    // A fresh engine asks again.
    let mut fresh = Engine::new(prompter.config(prompt_for_imports()));
    assert_import_denied(rt().block_on(fresh.run_file(&main)), "./lib.sand");
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "lib.sand")]);
}

#[cfg(feature = "os")]
#[test]
fn import_prompt_deny_once_and_deny_always() {
    let dir = TempDir::new("impdeny");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let prompter = ScriptedPrompter::new(&[PromptAnswer::DenyOnce, PromptAnswer::DenyAlways, PromptAnswer::AllowAlways]);
    let mut engine = Engine::new(prompter.config(prompt_for_imports()));
    assert_import_denied(rt().block_on(engine.run_file(&main)), "./lib.sand");
    assert_eq!(prompter.asked().len(), 1);
    // `DenyOnce` is not remembered; `DenyAlways` is.
    assert_import_denied(rt().block_on(engine.run_file(&main)), "./lib.sand");
    assert_eq!(prompter.asked().len(), 1);
    assert_import_denied(rt().block_on(engine.run_file(&main)), "./lib.sand");
    assert!(engine.check_file(&main).is_err());
    assert_eq!(prompter.asked(), vec![]);
}

#[cfg(feature = "os")]
#[test]
fn import_prompt_without_prompter_denies() {
    let dir = TempDir::new("impnoprompter");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let mut engine = Engine::new(Config::new().custom_policy(prompt_for_imports()));
    assert_import_denied(rt().block_on(engine.run_file(&main)), "./lib.sand");
    // Standard modules are not script imports and are never prompted for.
    let prompter = ScriptedPrompter::new(&[]);
    let mut engine = Engine::new(prompter.config(prompt_for_imports()));
    rt().block_on(engine.run_source("main.sand", "import json\nprint(json.encode(json.Json.Null))")).unwrap();
    assert_eq!(prompter.asked(), vec![]);
}

#[cfg(feature = "os")]
#[test]
fn nested_imports_are_prompted_with_resolved_paths() {
    let dir = TempDir::new("impnested");
    dir.write("lib/a.sand", "import \"./b.sand\"\nfrom \"../c\" import Z\npub let X = b.Y + Z");
    dir.write("lib/b.sand", "pub let Y = 2");
    dir.write("c.sand", "pub let Z = 3");
    let main = dir.write("main.sand", "import \"lib/a.sand\"\nimport \"./c.sand\"\nprint(a.X + c.Z)");
    let prompter = ScriptedPrompter::new(&[PromptAnswer::AllowOnce; 3]);
    let out = MemorySink::new();
    let mut engine = Engine::new(prompter.config(prompt_for_imports()).stdout(out.clone()));
    rt().block_on(engine.run_file(&main)).unwrap();
    assert_eq!(out.contents(), "8\n");
    // Depth-first, each module once (c.sand is shared), with paths resolved
    // relative to the importing module.
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "lib/a.sand"), import_cap(&dir, "lib/b.sand"), import_cap(&dir, "c.sand")]);

    // Denying a nested import reports it in the importing module, and modules
    // after it are not loaded.
    let prompter = ScriptedPrompter::new(&[PromptAnswer::AllowOnce, PromptAnswer::DenyOnce]);
    let mut engine = Engine::new(prompter.config(prompt_for_imports()));
    match rt().block_on(engine.run_file(&main)) {
        Err(Error::Compile(d)) => {
            let denied: Vec<_> = d.iter().filter(|d| d.message.contains("permission denied")).collect();
            assert_eq!(denied.len(), 1, "{d:?}");
            assert_eq!(denied[0].message, "cannot import `./b.sand`: permission denied");
            assert_eq!((denied[0].file.replace('\\', "/").as_str(), denied[0].line), ("lib/a.sand", 1));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(prompter.asked(), vec![import_cap(&dir, "lib/a.sand"), import_cap(&dir, "lib/b.sand")]);
}

#[cfg(feature = "os")]
#[test]
fn runtime_always_answers_last_for_the_engine() {
    let dir = TempDir::new("alwaysruntime");
    dir.write("a.txt", "A");
    let prompter = ScriptedPrompter::new(&[PromptAnswer::AllowAlways, PromptAnswer::DenyAlways]);
    let out = MemorySink::new();
    let mut engine =
        Engine::new(prompter.base().policy(sandlang::policy::rules::RulePolicy::standard()).cwd(dir.path()).stdout(out.clone()));
    let src = "import fs\nimport env\nprint(fs.read_text(\"a.txt\").is_ok())\nprint(env.get(\"SAND_TEST_UNSET_VAR\"))";
    rt().block_on(engine.run_source("main.sand", src)).unwrap();
    assert_eq!(prompter.asked().len(), 2);
    rt().block_on(engine.run_source("main.sand", src)).unwrap();
    assert_eq!(prompter.asked(), vec![], "always answers must be remembered across runs of one engine");
    assert_eq!(out.contents(), "true\nNone\ntrue\nNone\n");
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

#[test]
fn instruction_limit() {
    let config = Config::new().limits(Limits { max_instructions: Some(100_000), ..Limits::default() });
    let (r, _, _) = run_src(config, "var i = 0\nwhile true { i += 1 }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
}

#[test]
fn infinite_iterators_run_out_of_fuel() {
    // Iterators are plain Sand code: every element costs calls, so endless
    // sources are stopped by the instruction limit (or the memory limit).
    for src in ["print(Iter.repeat(1).count())", "print(Iter.count_from(0).filter(x => x < 0).first())"] {
        let config = Config::new().limits(Limits { max_instructions: Some(100_000), ..Limits::default() });
        let (r, _, _) = run_src(config, src);
        assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{src}: {r:?}");
    }
    let config = Config::new().limits(Limits { max_memory: Some(8 << 20), ..Limits::default() });
    let (r, _, _) = run_src(config, "print(Iter.repeat(\"x\").map(s => s.repeat(100)).to_list().len())");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

#[test]
fn memory_limit() {
    let config = Config::new().limits(Limits { max_memory: Some(8 << 20), ..Limits::default() });
    let (r, _, _) = run_src(config, "let xs = []\nwhile true { xs.push(\"some string data\".repeat(10)) }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

#[test]
fn a_native_runs_once_when_its_result_needs_a_collection() {
    let pops = Arc::new(AtomicUsize::new(0));
    let p = pops.clone();
    let mut m = NativeModule::new("queue");
    m.function("pop", move || {
        p.fetch_add(1, Ordering::SeqCst);
        "x".repeat(700_000)
    });
    let config =
        Config::new().host_modules(HostModules::new([m]).unwrap()).limits(Limits { max_memory: Some(8 << 20), ..Limits::default() });
    let src = "import queue\nvar s = \"y\".repeat(3_900_000)\ns = \"y\".repeat(3_900_000)\nprint(queue.pop().len())";
    let (r, out, _) = run_src(config, src);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "700000\n");
    assert_eq!(pops.load(Ordering::SeqCst), 1);
}

#[test]
fn an_async_result_that_needs_a_collection_does_not_end_the_run() {
    let mut m = NativeModule::new("slow");
    m.function("read", || async { "x".repeat(700_000) });
    let config =
        Config::new().host_modules(HostModules::new([m]).unwrap()).limits(Limits { max_memory: Some(8 << 20), ..Limits::default() });
    let src = "import slow\nvar s = \"y\".repeat(3_900_000)\ns = \"y\".repeat(3_900_000)\nprint(slow.read().len())";
    let (r, out, _) = run_src(config, src);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "700000\n");
}

#[test]
fn memory_limit_counts_big_buffers() {
    let config = Config::new().limits(Limits { max_memory: Some(4 << 20), ..Limits::default() });
    let (r, _, _) = run_src(config, "let s = \"x\".repeat(100000000)\nprint(s.len())");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

/// Counts, sizes and indices past `u32::MAX` must keep their meaning on
/// 32-bit targets (where `usize` is 32 bits): rejected by the memory limit or
/// a bounds check, never wrapped to a small value.
#[test]
fn counts_past_u32_max_are_not_truncated() {
    const BIG: &str = "4294967297"; // 2^32 + 1: wraps to 1 on a 32-bit target
    let memory = Limits { max_memory: Some(4 << 20), ..Limits::default() };
    for src in [
        "print(\"x\".repeat(BIG).len())",
        "print(\"x\".pad_start(BIG).len())",
        "print(\"x\".pad_end(BIG, \"ab\").len())",
        "import random\nprint(random.bytes(BIG).len())",
        "print((0..BIG).to_list().len())",
        "print((0..BIG).reversed().len())",
        "print((0..BIG * 2).step_by(2).len())",
        "print([0; BIG].len())",
    ] {
        let src = src.replace("BIG", BIG);
        let (r, out, _) = run_src(Config::new().limits(memory.clone()), &src);
        assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{src}: {r:?} (stdout {out:?})");
    }
    let src = format!(
        "let xs = [1, 2, 3]\n\
         print(xs.get({BIG}))\n\
         print(\"abc\".char_at({BIG}))\n\
         print(\"abc\".bytes().get({BIG}))\n\
         print(xs.take({BIG}))\n\
         print(xs.skip({BIG}))\n\
         print(xs.chunks({BIG}))\n\
         print(xs.windows({BIG}))\n\
         print((0..10000000000).step_by(5000000000))\n\
         print((5000000000..5000000003).to_list())\n\
         let ch = Channel(capacity: {BIG})\n\
         ch.send(1)\n\
         print(ch.len())"
    );
    let (r, out, _) = run_src(Config::new(), &src);
    r.unwrap();
    assert_eq!(out, "None\nNone\nNone\n[1, 2, 3]\n[]\n[[1, 2, 3]]\n[]\n[0, 5000000000]\n[5000000000, 5000000001, 5000000002]\n1\n");
    for (src, message) in [
        ("let xs = [1, 2, 3]\nprint(xs[BIG])", "index out of bounds: index BIG, length 3"),
        ("var xs = [1, 2, 3]\nxs[BIG] = 4\nprint(xs)", "index out of bounds: index BIG, length 3"),
        ("var xs = [1, 2, 3]\nxs.insert(BIG, 4)", "index out of bounds: index BIG, length 3"),
        ("var xs = [1, 2, 3]\nxs.remove(BIG)", "index out of bounds: index BIG, length 3"),
        ("var xs = [1, 2, 3]\nxs.swap(0, BIG)", "index out of bounds: index BIG, length 3"),
        ("let xs = [1, 2, 3]\nprint(xs[1..BIG])", "out of range"),
    ] {
        let (src, message) = (src.replace("BIG", BIG), message.replace("BIG", BIG));
        let (r, out, _) = run_src(Config::new(), &src);
        match r {
            Err(Error::Runtime { message: m, .. }) => assert!(m.contains(&message), "{src}: {m:?}"),
            other => panic!("{src}: expected a panic, got {other:?} (stdout {out:?})"),
        }
    }
}

#[test]
fn garbage_is_collected_under_memory_limit() {
    let config = Config::new().limits(Limits { max_memory: Some(16 << 20), ..Limits::default() });
    let src = "var total = 0\nfor i in 0..200000 {\n    let tmp = [i, i + 1, i + 2].map(x => x.to_str())\n    total += tmp.len()\n}\nprint(total)";
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "600000\n");
}

#[test]
fn wall_time_limit() {
    let config = Config::new().limits(Limits { max_wall_time: Some(Duration::from_millis(100)), ..Limits::default() });
    let start = Instant::now();
    let (r, _, _) = run_src(config, "task.sleep(10s)");
    assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn cpu_time_limit_excludes_waiting() {
    let config = || Config::new().limits(Limits { max_cpu_time: Some(Duration::from_millis(200)), ..Limits::default() });
    let (r, out, _) = run_src(config(), "task.sleep(300ms)\nprint(\"slept\")");
    r.unwrap();
    assert_eq!(out, "slept\n");
    let (r, _, _) = run_src(config(), "var i = 0\nwhile true { i += 1 }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::CpuTime))), "{r:?}");
}

#[test]
fn call_depth_limit() {
    let config = Config::new().limits(Limits { max_call_depth: 100, ..Limits::default() });
    let (r, _, _) = run_src(config, "fn f(n: int) -> int { if n == 0 { 0 } else { 1 + f(n - 1) } }\nprint(f(50))\nprint(f(200))");
    match r {
        Err(Error::Runtime { message, .. }) => assert!(message.contains("stack overflow")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn returned_frames_do_not_keep_garbage_alive() {
    // Each call leaves a 10 MB string in a dead register at a different stack
    // depth; those slots must not be GC roots once the call has returned.
    let config = Config::new().limits(Limits { max_memory: Some(48 << 20), ..Limits::default() });
    let src = "fn alloc_at(depth: int) -> int {\n    if depth == 0 { \"x\".repeat(10000000).len() } else { alloc_at(depth - 1) + 0 }\n}\nvar t = 0\nfor d in 0..12 { t += alloc_at(11 - d) }\nprint(t)";
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "120000000\n");
}

#[test]
fn deep_recursion_then_garbage_churn() {
    // The stack grows for deep recursion and is shrunk again by the GC; values
    // in live frames must survive collections triggered deep in the recursion.
    let src = "fn deep(n: int, acc: [str]) -> int {\n    if n == 0 { return acc.len() }\n    let s = \"v\\(n)\"\n    let r = deep(n - 1, acc + [s])\n    if r % 1000 == 0 { let _junk = (0..1000).map(i => i.to_str()) }\n    r + s.len() - s.len()\n}\nvar total = 0\nfor round in 0..3 {\n    total += deep(3000, [])\n    var junk = 0\n    for i in 0..200000 { junk += [i].len() }\n    total += junk\n}\nprint(total)";
    let (r, out, _) = run_src(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "609000\n");
}

#[test]
fn objects_reachable_only_through_inline_variants_survive_gc() {
    // `Some(obj)`, `Ok(obj)` etc. are stored inline (no heap allocation of
    // their own); the payload objects must still be traced from every kind
    // of root and container.
    let src = r#"
struct Holder {
    v: str?
}
let in_list = [Some("list \(i)") for i in 0..1000]
let in_map: [int: Result<str, int>] = [:]
for i in 0..1000 { in_map[i] = Ok("map \(i)") }
let in_struct = Holder(v: Some("struct field"))
let captured = Some(["captured", "list"])
let closure = () => captured.unwrap().len()
let ch = Channel(capacity: 10)
ch.send(Some("queued"))
let t = spawn { Some("task result") }
t.wait()
var churn = 0
for i in 0..300000 { churn += [i, i + 1].len() }
let local = Some("local \(churn)")
for i in 0..300000 { churn += [i].len() }
var total = 0
for x in in_list { total += x.unwrap().len() }
for (k, v) in in_map { total += v.unwrap().len() }
print(total)
print(in_struct.v)
print(closure())
print(ch.recv())
print(t.wait())
print(local)
"#;
    let (r, out, _) = run_src(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "14780\nSome(\"struct field\")\n2\nSome(Some(\"queued\"))\nSome(\"task result\")\nSome(\"local 600000\")\n");
}

/// Under GC stress every container is promoted to the old generation almost
/// immediately, so each mutation below stores a young object into an old one
/// and relies on the write barrier.
const BARRIER_PROGRAM: &str = r#"
struct Node {
    name: str
    var next: Node?
}
let xs: [str] = []
let ys = ["", "", ""]
let m: [str: [int]] = [:]
let s: Set<str> = Set()
var head: Node? = None
let node = Node(name: "root", next: None)
let ch = Channel(capacity: 100)
var captured = "c0"
let bump = (i: int) => {
    captured = "c\(i)"
}
for i in 0..60 {
    xs.push("p\(i)")
    ys[i % 3] = "y\(i)"
    m["k\(i % 7)"] = [i, i * 2]
    s.add("s\(i % 11)")
    head = Some(Node(name: "n\(i)", next: head))
    node.next = Some(Node(name: "child\(i)", next: None))
    ch.send("msg\(i)")
    bump(i)
    xs.insert(0, "front\(i)")
    xs.extend(["e\(i)"])
    if i % 5 == 0 {
        xs.sort()
        xs.swap(0, xs.len() - 1)
        m.remove("k\((i + 3) % 7)")
        m.insert("ins\(i)", [i])
        let _ = m.get_or_insert("goi\(i)", [i, i])
    }
}
let t = spawn { ["task", "result"].map(x => x + "!") }
t.wait()
for i in 0..200 { let _ = [i].map(x => x.to_str()) }
var total = 0
for x in xs { total += x.len() }
for y in ys { total += y.len() }
for (k, v) in m { total += k.len() + v.len() }
for x in s { total += x.len() }
var n = head
while let Some(nd) = n {
    total += nd.name.len()
    n = nd.next
}
total += node.next.unwrap().name.len()
while let Some(msg) = ch.try_recv() { total += msg.len() }
total += captured.len()
print(total)
print(t.wait())
print("\(xs[0]) \(xs[xs.len() - 1]) \(ys) \(s.len())")
"#;

#[test]
fn write_barrier_under_gc_stress() {
    let (r, normal, _) = run_src(Config::new(), BARRIER_PROGRAM);
    r.unwrap();
    let (r, stressed, _) = run_src(Config::new().gc_stress(true), BARRIER_PROGRAM);
    r.unwrap();
    assert_eq!(normal, stressed);
}

#[test]
fn map_mutation_ignores_truncated_iterator_pins() {
    let src = r#"
let m: [int: int] = [:]
for i in 0..10 { m[i] = i }
fn churn() {
    var junk: [[int]] = []
    for i in 0..10000 { junk.push([i]) }
    for (k, v) in m { break }
}
for round in 0..20 {
    (spawn churn()).wait()
    var s = 0
    for i in 0..1000 { s += i }
    m[10 + round] = s
}
print(m.len())
"#;
    let (r, out, _) = run_src(Config::new().gc_stress(true), src);
    r.unwrap();
    assert_eq!(out, "30\n");
}

#[test]
fn decoding_many_structs_with_computed_defaults_fits_the_memory_limit() {
    let src = r#"
import json
fn one() -> int { 1 }
struct P {
    a: int = one(), b: int = one(), c: int = one(), d: int = one()
    e: int = one(), f: int = one(), g: int = one(), h: int = one()
    i: int = one(), j: int = one(), k: int = one(), l: int = one()
    m: int = one(), n: int = one(), o: int = one(), p: int = one()
}
let text = "[" + "{},".repeat(99999) + "{}]"
let ps: [P] = json.decode(text)?
print("\(ps.len()) \(ps[99999].p)")
"#;
    let limits = Limits { max_memory: Some(64 << 20), ..Limits::default() };
    let (r, out, _) = run_src(Config::new().limits(limits), src);
    r.unwrap();
    assert_eq!(out, "100000 1\n");
}

#[test]
fn old_lists_receiving_young_values_between_minor_collections() {
    // Large old lists get young strings stored at scattered indices (more
    // than the partial-remembering limit per collection) and pushed, with
    // enough allocation in between for several minor collections.
    const N: usize = 300_000;
    let src = format!(
        r#"
let xs: [str] = ["" for _ in 0..{N}]
let few: [str] = ["" for _ in 0..{N}]
let tail: [str] = []
for round in 0..6 {{
    for i in 0..{N} {{
        if i % 7 == round {{
            xs[i] = "r\(round)i\(i)"
        }}
    }}
    few[round * 1000] = "few\(round)"
    for i in 0..3000 {{ tail.push("t\(round).\(i)") }}
    var junk = 0
    for j in 0..150000 {{ junk += [j, j].len() }}
    if round == 3 {{
        tail.insert(0, "front")
    }}
}}
var total = 0
for x in xs {{ total += x.len() }}
for x in few {{ total += x.len() }}
for x in tail {{ total += x.len() }}
print(total)
print(tail[0])
print(few[5000])
"#
    );
    let digits = |i: usize| i.to_string().len();
    let xs: usize = (0..N).filter(|i| i % 7 < 6).map(|i| 3 + digits(i)).sum();
    let tail: usize = 5 + 6 * (0..3000).map(|i| 3 + digits(i)).sum::<usize>();
    let (mut engine, out, _) = engine_with(Config::new());
    rt().block_on(engine.run_source("main.sand", &src)).unwrap();
    assert_eq!(out.contents(), format!("{}\nfront\nfew5\n", xs + 6 * 4 + tail));
    let stats = engine.gc_stats();
    assert!(stats.collections - stats.majors >= 3, "{stats:?}");
}

#[test]
fn gc_stats_report_minor_and_major_collections() {
    // A large live structure plus allocation churn: most collections are
    // minor ones that do not scan the old generation.
    let src = "let live = [[i] for i in 0..300000]\nvar t = 0\nfor i in 0..2000000 { t += [i, i].len() }\nprint(t + live.len())";
    let (mut engine, out, _) = engine_with(Config::new());
    rt().block_on(engine.run_source("main.sand", src)).unwrap();
    assert_eq!(out.contents(), "4300000\n");
    let stats = engine.gc_stats();
    assert!(stats.collections > stats.majors, "{stats:?}");
    assert!(stats.majors >= 1, "{stats:?}");
}

#[test]
fn task_limit() {
    let config = Config::new().limits(Limits { max_tasks: 10, ..Limits::default() });
    let (r, _, _) = run_src(config, "let ts = (0..100).map(i => spawn { task.sleep(10ms) })");
    match r {
        Err(Error::Runtime { message, .. }) => assert!(message.contains("task limit exceeded")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn output_limit() {
    let config = Config::new().limits(Limits { max_output_bytes: Some(1000), ..Limits::default() });
    let (r, out, _) = run_src(config, "for i in 0..1000 { print(\"line \\(i)\") }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Output))), "{r:?}");
    assert!(out.len() <= 1000);
}

#[cfg(feature = "os")]
#[test]
fn open_resource_limit() {
    let config = Config::new().limits(Limits { max_open_resources: 2, ..Limits::default() });
    let src = r#"
import net
let a = net.listen("127.0.0.1", 0)?
let b = net.listen("127.0.0.1", 0)?
let c = net.listen("127.0.0.1", 0)
print(c.unwrap_err().kind)
a.close()
print(net.listen("127.0.0.1", 0).is_ok())
"#;
    let (r, out, _) = run_src(config.policy(sandlang::policy::rules::RulePolicy::allow_all()), src);
    r.unwrap();
    assert_eq!(out, "limit\ntrue\n");
}

#[test]
fn source_size_limit() {
    let config = Config::new().limits(Limits { max_source_bytes: 100, ..Limits::default() });
    let (r, _, _) = run_src(config, &"print(1)\n".repeat(50));
    match r {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("source too large")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn interrupt_from_another_thread() {
    let handle = InterruptHandle::new();
    let limits = Limits { max_wall_time: Some(Duration::from_secs(10)), ..Limits::default() };
    let mut engine = Engine::new(Config::new().limits(limits).interrupt(handle.clone()));
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        handle.interrupt();
    });
    let r = rt().block_on(engine.run_source("main.sand", "while true {}"));
    t.join().unwrap();
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
}

#[test]
fn interrupt_stops_all_engines_and_sessions_with_shared_handle() {
    let handle = InterruptHandle::new();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let config = || {
        let mut module = NativeModule::new("signal");
        let started_tx = started_tx.clone();
        module.function("started", move || started_tx.send(()).unwrap());
        let limits = Limits { max_wall_time: Some(Duration::from_secs(10)), ..Limits::default() };
        Config::new().limits(limits).host_modules(HostModules::new([module]).unwrap()).interrupt(handle.clone())
    };
    let source = "import signal\nsignal.started()\nwhile true {}";
    let mut engine = Engine::new(config());
    let mut session = Session::new(config());
    let runs = [
        std::thread::spawn(move || rt().block_on(engine.run_source("main.sand", source)).map(|_| ())),
        std::thread::spawn(move || rt().block_on(session.eval(source)).map(|_| ())),
    ];
    for _ in &runs {
        started_rx.recv().unwrap();
    }
    handle.interrupt();
    for run in runs {
        let result = run.join().unwrap();
        assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
    }
}

#[cfg(feature = "os")]
#[test]
fn interrupt_remains_set_across_concurrent_call_on_same_engine() {
    let dir = TempDir::new("interruptoverlap");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let asked_tx = Mutex::new(asked_tx);
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let handle = InterruptHandle::new();
    let config = Config::new().custom_policy(prompt_for_imports()).interrupt(handle.clone()).prompter(move |_cap: Capability| {
        let _ = asked_tx.lock().send(());
        let mut release = release_rx.clone();
        async move {
            let _ = release.wait_for(|released| *released).await;
            PromptAnswer::AllowOnce
        }
    });
    let engine = Engine::new(config);
    let result = std::thread::scope(|scope| {
        let compiling = scope.spawn(|| rt().block_on(engine.compile_file(&main)).map(|_| ()));
        asked_rx.recv_timeout(Duration::from_secs(10)).expect("the prompter was not asked");
        handle.interrupt();
        engine.check_source("main.sand", "print(1)").unwrap();
        release_tx.send(true).unwrap();
        compiling.join().unwrap()
    });
    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
}

/// Runs `call` with a pending import prompt, interrupts the call during the
/// prompt, and asserts that `call` returns `Err(Error::Interrupted)`.
fn assert_interrupt_stops_an_import_prompt(call: impl FnOnce(Config) -> Result<(), Error> + Send + 'static) {
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let asked_tx = Mutex::new(asked_tx);
    let handle = InterruptHandle::new();
    let config = Config::new().custom_policy(prompt_for_imports()).interrupt(handle.clone()).prompter(move |_cap: Capability| {
        let _ = asked_tx.lock().send(());
        std::future::pending::<PromptAnswer>()
    });
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(call(config));
    });
    asked_rx.recv_timeout(Duration::from_secs(10)).expect("the prompter was not asked");
    handle.interrupt();
    let result = done_rx.recv_timeout(Duration::from_secs(10)).expect("the interrupt did not stop the call");
    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
}

#[cfg(feature = "os")]
#[test]
fn interrupt_stops_a_compilation_waiting_for_an_import_prompt() {
    let dir = TempDir::new("interruptimport");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    assert_interrupt_stops_an_import_prompt(move |config| rt().block_on(Engine::new(config).run_file(&main)).map(|_| ()));
    let root = dir.path().to_path_buf();
    assert_interrupt_stops_an_import_prompt(move |config| {
        rt().block_on(Session::new(config.module_root(root)).eval("import \"./lib.sand\"")).map(|_| ())
    });
}

#[cfg(feature = "os")]
#[test]
fn interrupt_stops_engine_test_runs() {
    let dir = TempDir::new("interrupttests");
    let main = dir.write("main.sand", "import stop\ntest \"first\" { stop.interrupt()\nwhile true {} }\ntest \"second\" {}");
    let handle = InterruptHandle::new();
    let mut module = NativeModule::new("stop");
    let interrupter = handle.clone();
    module.function("interrupt", move || interrupter.interrupt());
    let limits = Limits { max_wall_time: Some(Duration::from_secs(10)), ..Limits::default() };
    let mut engine = Engine::new(Config::new().limits(limits).host_modules(HostModules::new([module]).unwrap()).interrupt(handle));
    let result = rt().block_on(engine.run_tests(&main));
    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
}

#[test]
fn engine_ignores_interrupt_before_call_starts() {
    let handle = InterruptHandle::new();
    handle.interrupt();
    let mut engine = Engine::new(Config::new().interrupt(handle.clone()));
    let result = rt().block_on(engine.run_source("main.sand", "print(1)"));
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn interrupt_stops_a_run_waiting_for_a_prompt() {
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let asked_tx = Mutex::new(asked_tx);
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard()).prompter(move |_cap: Capability| {
        let _ = asked_tx.lock().send(());
        std::future::pending::<PromptAnswer>()
    });
    let handle = InterruptHandle::new();
    let mut engine = Engine::new(config.interrupt(handle.clone()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = rt().block_on(engine.run_source("main.sand", "import env\nprint(env.get(\"HOME\"))"));
        let _ = done_tx.send(r);
    });
    asked_rx.recv_timeout(Duration::from_secs(10)).expect("the prompter was not asked");
    handle.interrupt();
    let r = done_rx.recv_timeout(Duration::from_secs(10)).expect("the run did not stop after the interrupt");
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
}

#[test]
fn limits_are_per_engine() {
    // One engine hitting its limit must not affect another.
    let limited = Config::new().limits(Limits { max_instructions: Some(10_000), ..Limits::default() });
    let (r1, _, _) = run_src(limited, "var i = 0\nwhile true { i += 1 }");
    assert!(matches!(r1, Err(Error::Limit(LimitKind::Instructions))));
    let (r2, out, _) = run_src(Config::new(), "var s = 0\nfor i in 0..100000 { s += i }\nprint(s)");
    r2.unwrap();
    assert_eq!(out, "4999950000\n");
}

// ---------------------------------------------------------------------------
// Concurrency of engines
// ---------------------------------------------------------------------------

const FIB: &str = "fn fib(n: int) -> int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\nprint(fib(22))";

#[test]
fn engines_run_concurrently_on_threads() {
    let handles: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let out = MemorySink::new();
                let mut engine = Engine::new(Config::new().stdout(out.clone()).seed(i));
                rt().block_on(engine.run_source("main.sand", FIB)).unwrap();
                out.contents()
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), "17711\n");
    }
}

#[test]
fn engines_run_concurrently_on_a_multithreaded_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    runtime.block_on(async {
        let mut tasks = Vec::new();
        for i in 0..16 {
            tasks.push(tokio::spawn(async move {
                let out = MemorySink::new();
                let mut engine = Engine::new(Config::new().stdout(out.clone()));
                let src = format!("task.sleep(10ms)\nprint({i} * 2)");
                engine.run_source("main.sand", &src).await.unwrap();
                out.contents()
            }));
        }
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), format!("{}\n", i * 2));
        }
    });
}

#[test]
fn engines_on_one_thread_interleave() {
    // A busy engine must yield to the host runtime so others make progress.
    let runtime = rt();
    runtime.block_on(async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let busy = tokio::task::spawn_local(async {
                    let mut e =
                        Engine::new(Config::new().limits(Limits { max_wall_time: Some(Duration::from_millis(500)), ..Limits::default() }));
                    e.run_source("busy.sand", "while true {}").await
                });
                let quick = tokio::task::spawn_local(async {
                    let out = MemorySink::new();
                    let mut e = Engine::new(Config::new().stdout(out.clone()));
                    let start = Instant::now();
                    e.run_source("quick.sand", "task.sleep(10ms)\nprint(\"done\")").await.unwrap();
                    (out.contents(), start.elapsed())
                });
                let (out, elapsed) = quick.await.unwrap();
                assert_eq!(out, "done\n");
                assert!(elapsed < Duration::from_millis(400), "quick engine starved: {elapsed:?}");
                assert!(matches!(busy.await.unwrap(), Err(Error::Limit(LimitKind::WallTime))));
            })
            .await;
    });
}

#[test]
fn seeded_engines_are_deterministic() {
    let src = "import random\nprint([random.int(0, 1000000) for _ in 0..5])";
    let a = run_src(Config::new().seed(5), src).1;
    let b = run_src(Config::new().seed(5), src).1;
    let c = run_src(Config::new().seed(6), src).1;
    assert_eq!(a, b);
    assert_ne!(a, c);
}

// ---------------------------------------------------------------------------
// Test runner API and file loading
// ---------------------------------------------------------------------------

#[cfg(feature = "os")]
#[test]
fn run_tests_api() {
    let dir = TempDir::new("tests");
    let main = dir.write("t.sand", "test \"a\" { assert(true) }\ntest \"b\" { assert(false, \"nope\") }");
    let mut engine = Engine::new(Config::new());
    let report = rt().block_on(engine.run_tests(&main)).unwrap();
    assert_eq!(report.passed(), 1);
    assert_eq!(report.failed(), 1);
    assert_eq!(report.results[1].name, "b");
    assert_eq!(report.results[1].message.as_deref(), Some("assert: nope"));
}

#[cfg(feature = "os")]
#[test]
fn run_file_with_imports_and_cwd() {
    let r = run_with("import \"./util.sand\"\nprint(util.X)", |c| c);
    // util.sand does not exist in the run() temp dir.
    assert!(matches!(r.result, Err(Error::Compile(_))));
}

// ---------------------------------------------------------------------------
// Source audit: the implementation must stay inside the sandbox rules.
// ---------------------------------------------------------------------------

#[test]
fn implementation_has_no_unsafe_or_process_access() {
    fn visit(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                visit(&p, files);
            } else if p.extension().is_some_and(|e| e == "rs") {
                files.push(p);
            }
        }
    }
    let mut files = Vec::new();
    visit(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(!files.is_empty());
    let forbidden =
        ["unsafe ", "unsafe{", "std::process::Command", "process::Command", "libc::", "memmap", "mmap(", "dlopen", "extern \"C\""];
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for bad in forbidden {
                // The CLI binary may exit the process; nothing else may touch processes.
                assert!(!code.contains(bad), "{}:{}: forbidden `{bad}`", f.display(), i + 1);
            }
        }
    }
    let lib = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(lib.contains("#![forbid(unsafe_code)]"));
}

// ---------------------------------------------------------------------------
// Hardening: script-controlled text never passes for an engine signal
// ---------------------------------------------------------------------------

#[test]
fn error_messages_cannot_impersonate_limits_or_interrupts() {
    let srcs = [
        r#"fail("\u{0}memory")"#,
        r#"fail("\u{0}interrupted")"#,
        r#"assert(false, "\u{0}instructions")"#,
        "let x: int? = None\nlet y = x.expect(\"\\u{0}cpu\")",
        "let t = spawn { fail(\"\\u{0}wall\") }\nt.wait()",
    ];
    for src in srcs {
        let (r, _, _) = run_src(Config::new(), src);
        match r {
            Err(Error::Runtime { message, .. }) => assert!(message.contains('\u{0}'), "{src}: {message:?}"),
            other => panic!("{src}: {other:?}"),
        }
    }
    // A failed top-level `?` in a session fails with the error's text.
    let mut session = Session::new(Config::new());
    let r = rt().block_on(session.eval("let r: Result<int> = Err(Error(\"\\u{0}memory\"))\nr?"));
    assert!(matches!(r, Err(Error::Runtime { .. })), "{r:?}");
}

#[cfg(feature = "os")]
#[test]
fn rust_panics_in_natives_do_not_unwind_into_the_host() {
    let module = || {
        let mut m = NativeModule::new("buggy");
        m.function("boom", |n: i64| -> i64 {
            if n > 2 {
                panic!("native bug {n}");
            }
            n
        });
        m
    };
    let src = "import buggy\nprint(buggy.boom(1))\nprint(buggy.boom(3))\nprint(\"unreachable\")";
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([module()]).unwrap()), src);
    match r {
        Err(Error::Internal { message, .. }) => assert!(message.contains("native bug 3"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(out, "1\n");
    // In a task, in tests, and in a session (which keeps working).
    let (r, _, _) = run_src(
        Config::new().host_modules(HostModules::new([module()]).unwrap()),
        "import buggy\nlet t = spawn { buggy.boom(5) }\nprint(t.wait())",
    );
    assert!(matches!(r, Err(Error::Internal { ref message, .. }) if message.contains("native bug 5")), "{r:?}");
    let dir = TempDir::new("panic-tests");
    dir.write("t.sand", "import buggy\ntest \"bad\" {\n    buggy.boom(9)\n}\ntest \"good\" {\n    assert(buggy.boom(2) == 2)\n}\n");
    let report = rt()
        .block_on(Engine::new(Config::new().host_modules(HostModules::new([module()]).unwrap())).run_tests(dir.path().join("t.sand")))
        .unwrap();
    assert_eq!((report.passed(), report.failed()), (1, 1));
    let mut session = Session::new(Config::new().host_modules(HostModules::new([module()]).unwrap()));
    assert!(rt().block_on(session.eval("import buggy\nbuggy.boom(7)")).is_err());
    assert!(session.was_reset());
    assert!(matches!(rt().block_on(session.eval("buggy.boom(2)")), Err(Error::Compile(_))));
    assert_eq!(rt().block_on(session.eval("import buggy\nbuggy.boom(2) + 1")).unwrap().as_deref(), Some("3"));
}

#[test]
fn rust_panics_in_host_code_become_internal_errors() {
    // A prompter is host code that runs inside the engine's future.
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard()).prompter(|_cap: Capability| async {
        panic!("prompter bug");
        #[allow(unreachable_code)]
        PromptAnswer::DenyOnce
    });
    let (r, _, _) = run_src(config, "import env\nprint(env.get(\"HOME\"))");
    assert!(matches!(r, Err(Error::Internal { ref message, .. }) if message.contains("prompter bug")), "{r:?}");
}

#[cfg(feature = "os")]
#[test]
fn module_discovery_is_bounded() {
    let dir = TempDir::new("discovery");
    // An import path that is too long is rejected before it is resolved.
    let main = dir.write("main.sand", &format!("import \"{}b\"\nprint(1)", "a/".repeat(400_000)));
    let start = Instant::now();
    let r = rt().block_on(Engine::new(Config::new()).run_file(&main));
    match r {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("too long"), "{d:?}"),
        other => panic!("{other:?}"),
    }
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    // Many failing imports: discovery stops early.
    let src: String = (0..20_000).map(|i| format!("import \"missing{i}\"\n")).collect();
    let main = dir.write("many.sand", &src);
    let r = rt().block_on(Engine::new(Config::new()).run_file(&main));
    match r {
        Err(Error::Compile(d)) => assert!(d.len() <= 100, "{}", d.len()),
        other => panic!("{other:?}"),
    }
    // And it can be interrupted.
    let mut engine = Engine::new(Config::new().limits(Limits { max_wall_time: Some(Duration::ZERO), ..Limits::default() }));
    let r = rt().block_on(engine.run_file(&main));
    assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");
}

#[cfg(feature = "os")]
#[test]
fn once_answers_apply_only_to_the_call_that_asked() {
    let dir = TempDir::new("once");
    dir.write("a.txt", "A");
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let answers =
        Arc::new(Mutex::new(vec![PromptAnswer::AllowOnce, PromptAnswer::AllowOnce, PromptAnswer::DenyOnce, PromptAnswer::DenyOnce]));
    let (asked2, answers2) = (asked.clone(), answers.clone());
    let config = Config::new().policy(sandlang::policy::rules::RulePolicy::standard()).cwd(dir.path()).prompter(move |cap: Capability| {
        asked2.lock().push(cap.to_string());
        let answer = answers2.lock().remove(0);
        async move { answer }
    });
    // `copy` asks for both capabilities; the first answer stays valid while
    // the call is retried for the second.
    let src = r#"
import fs
print(fs.copy("a.txt", "c.txt").is_ok())
print(fs.write_text("c.txt", "overwritten").is_ok())
print(fs.copy("a.txt", "d.txt").is_ok())
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "true\nfalse\nfalse\n");
    let asked = asked.lock();
    assert_eq!(asked.len(), 4, "{asked:?}");
    assert!(asked[0].starts_with("read") && asked[1].starts_with("write") && asked[2].starts_with("write"), "{asked:?}");
    assert!(asked[3].starts_with("read"), "{asked:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("c.txt")).unwrap(), "A");
}

#[cfg(feature = "os")]
#[cfg(unix)]
#[test]
fn a_symlinked_main_file_imports_from_its_own_directory() {
    let dir = TempDir::new("symlinked-main");
    dir.write("proj/lib.sand", "pub fn hello() -> str { \"hello from lib\" }");
    dir.write("proj/main.sand", "import \"./lib\"\nprint(lib.hello())");
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("proj/main.sand"), dir.path().join("bin/tool.sand")).unwrap();
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().policy(sandlang::policy::rules::RulePolicy::allow_all()).stdout(out.clone()));
    rt().block_on(engine.run_file(dir.path().join("bin/tool.sand"))).unwrap();
    assert_eq!(out.contents(), "hello from lib\n");
}

#[test]
fn host_modules_can_define_resources() {
    use sandlang::native::NativeObject;
    use sandlang::native::ty::VmVal;
    use sandlang::vm::iomem::ResourceSlot;
    use sandlang::vm::{Fail, Vm};

    /// A pooled connection.
    #[derive(sandlang::SandObject)]
    #[sand(module = "pool")]
    struct Conn {
        slot: Option<ResourceSlot>,
        closed: Arc<AtomicUsize>,
    }
    impl NativeObject for Conn {
        fn is_resource(&self) -> bool {
            self.slot.is_some()
        }
    }

    let closed = Arc::new(AtomicUsize::new(0));
    let mut m = NativeModule::new("pool");
    m.bind::<Conn>();
    m.source("impl Close for Conn {\n    native fn close(self)\n}");
    let c = closed.clone();
    m.function("connect", move |vm: &mut Vm| -> Result<Conn, sandlang::native::convert::HostError> {
        let slot = vm.resource_slot()?;
        Ok(Conn { slot: Some(slot), closed: c.clone() })
    });
    m.function("Conn.close", |vm: &mut Vm, conn: VmVal<Conn>| -> Result<(), Fail> {
        let conn = conn.get_mut(&mut vm.heap)?;
        if conn.slot.take().is_some() {
            conn.closed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    let src = "import pool\nfor _ in 0..5 {\n    use c = pool.connect()?\n}\nlet kept = [pool.connect()?, pool.connect()?]\nprint(pool.connect())\nhelp(pool.Conn)";
    let config = Config::new().host_modules(HostModules::new([m]).unwrap()).limits(Limits { max_open_resources: 2, ..Limits::default() });
    let (r, out, err) = run_src(config, src);
    assert!(r.is_ok(), "{r:?} {err}");
    assert!(out.contains("too many open resources"), "{out}");
    assert!(out.ends_with("type pool.Conn\n\nA pooled connection.\n\nImplements:\n    Eq\n    Close\n"), "{out}");
    assert_eq!(closed.load(Ordering::Relaxed), 5);
}

#[test]
fn host_errors_are_err_values_and_aborts_end_the_run() {
    use sandlang::native::convert::HostError;
    use sandlang::native::func::Later;
    use sandlang::vm::WaitKind;
    use sandlang::vm::{Fail, Vm};
    let mut m = NativeModule::new("errs");
    m.function("now", || -> Result<i64, HostError> { Err(HostError::new("invalid", "bad input")) });
    m.function("later", || -> Result<Later<i64, HostError>, HostError> {
        Ok(Later::new(WaitKind::Io, async { |_: &mut Vm| Err(HostError::new("io", "broken pipe")) }))
    });
    m.function("abort", || -> Result<i64, HostError> { Err(Fail::runtime_error("invalid", "no result").into()) });
    let run = |src: &str| run_src(Config::new().host_modules(HostModules::new([m.clone()]).unwrap()), &format!("import errs\n{src}"));
    let (r, out, _) = run("print(errs.now())\nprint(errs.later())");
    r.unwrap();
    assert_eq!(out, "Err(Error(message: \"bad input\", kind: \"invalid\"))\nErr(Error(message: \"broken pipe\", kind: \"io\"))\n");
    let (r, _, _) = run("print(errs.abort())");
    assert!(matches!(&r, Err(Error::Runtime { message, .. }) if message.contains("no result")), "{r:?}");
}

#[test]
fn natives_that_cannot_make_scheduler_requests_return_fail() {
    use sandlang::native::convert::HostError;
    use sandlang::native::func::Later;
    use sandlang::vm::{Fail, Vm, WaitKind};
    let mut m = NativeModule::new("fails");
    m.function("stop", || -> Result<i64, Fail> { Err(Fail::runtime_error("invalid", "no result")) });
    m.function("later_exit", || -> Result<Later<i64, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, async { |_: &mut Vm| Err(Fail::exit(7)) }))
    });
    m.function("async_ok", || async { Ok::<i64, HostError<Fail>>(4) });
    m.function("async_err", || async { Err::<i64, HostError<Fail>>(HostError::new("io", "broken pipe")) });
    m.function("async_stop", || async { Err::<i64, Fail>(Fail::runtime_error("invalid", "no result")) });
    m.function("async_exit", || async {
        Err(Fail::exit(3))?;
        Ok::<i64, HostError<Fail>>(4)
    });
    let run = |src: &str| run_src(Config::new().host_modules(HostModules::new([m.clone()]).unwrap()), &format!("import fails\n{src}"));

    let (r, out, _) = run("print(fails.async_ok())\nprint(fails.async_err())");
    r.unwrap();
    assert_eq!(out, "Ok(4)\nErr(Error(message: \"broken pipe\", kind: \"io\"))\n");

    for call in ["fails.stop()", "fails.async_stop()"] {
        let (r, _, _) = run(&format!("print({call})"));
        assert!(matches!(&r, Err(Error::Runtime { message, .. }) if message.contains("no result")), "{call}: {r:?}");
    }

    let (r, _, _) = run("print(fails.later_exit())");
    assert_eq!(r.unwrap().exit_code, 7);

    let (r, out, _) = run("print(fails.async_exit())");
    assert_eq!((r.unwrap().exit_code, out.as_str()), (3, ""));
}

#[test]
fn native_functions_can_be_fn_items_in_each_form() {
    fn borrowed(s: &str) -> i64 {
        s.len() as i64
    }
    fn with_vm(vm: &mut Vm, s: String) -> Result<i64, Fail> {
        vm.check_memory(s.len())?;
        Ok(s.len() as i64 * 10)
    }
    async fn waits(s: String) -> i64 {
        tokio::task::yield_now().await;
        s.len() as i64 * 100
    }
    let mut m = NativeModule::new("forms");
    m.function("borrowed", borrowed);
    m.function("with_vm", with_vm);
    m.function("waits", waits);
    let config = Config::new().host_modules(HostModules::new([m]).unwrap());
    let (r, out, _) =
        run_src(config, "import forms\nprint(forms.borrowed(\"ab\"))\nprint(forms.with_vm(\"abc\"))\nprint(forms.waits(\"abcd\"))");
    r.unwrap();
    assert_eq!(out, "2\n30\n400\n");
}

#[test]
fn native_functions_take_async_closures_and_twelve_parameters() {
    #[allow(clippy::too_many_arguments)]
    fn sum(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64, i: i64, j: i64, k: i64, l: i64) -> i64 {
        a + b + c + d + e + f + g + h + i + j + k + l
    }
    let mut m = NativeModule::new("wide");
    m.function("waits", async |s: String| s.len() as i64);
    m.function("params_only", sum);
    m.function("vm_first", |_: &mut Vm, a, b, c, d, e, f, g, h, i, j, k, l| sum(a, b, c, d, e, f, g, h, i, j, k, l));
    m.function("future", |a, b, c, d, e, f, g, h, i, j, k, l| async move { sum(a, b, c, d, e, f, g, h, i, j, k, l) });
    let args = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12";
    let src = format!(
        "import wide\nprint(wide.waits(\"abc\"))\nprint(wide.params_only({args}))\nprint(wide.vm_first({args}))\nprint(wide.future({args}))"
    );
    let (r, out, _) = run_src(Config::new().host_modules(HostModules::new([m]).unwrap()), &src);
    r.unwrap();
    assert_eq!(out, "3\n78\n78\n78\n");
}

#[test]
fn later_closures_return_new_handles() {
    use sandlang::native::func::Later;
    use sandlang::native::ty::VmVal;
    use sandlang::vm::WaitKind;
    let mut m = NativeModule::new("handles");
    m.function("after_wait", |_: &mut Vm, n: i64| -> Result<Later<VmVal<String>, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, async move {
            tokio::task::yield_now().await;
            move |vm: &mut Vm| {
                let v = format!("made {n}").into_vm_value(vm)?;
                Ok(VmVal::<String>::from_raw(v))
            }
        }))
    });
    m.function("now", |vm: &mut Vm, n: i64| -> Result<Later<VmVal<String>, Fail>, Fail> {
        let v = format!("ready {n}").into_vm_value(vm)?;
        Ok(Later::ready(Ok(VmVal::from_raw(v))))
    });
    let config = Config::new().gc_stress(true).host_modules(HostModules::new([m]).unwrap());
    let (r, out, _) = run_src(config, "import handles\nprint(handles.after_wait(1))\nprint(handles.now(2))");
    r.unwrap();
    assert_eq!(out, "made 1\nready 2\n");
}

#[test]
fn host_error_closures_infer_their_return_type() {
    use sandlang::native::convert::HostError;
    let mut m = NativeModule::new("inferred");
    m.function("positive", |n: i64| if n > 0 { Ok(n) } else { Err(HostError::new("invalid", "not positive")) });
    let config = Config::new().host_modules(HostModules::new([m]).unwrap());
    let (r, out, _) = run_src(config, "import inferred\nprint(inferred.positive(1))\nprint(inferred.positive(0))");
    r.unwrap();
    assert_eq!(out, "Ok(1)\nErr(Error(message: \"not positive\", kind: \"invalid\"))\n");
}

#[test]
fn engine_docs_include_prelude_std_and_host_modules() {
    use sandlang::docs::{ItemKind, MemberKind, ModuleKind};
    let mut geo = NativeModule::new("geo");
    geo.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() }).params(&["a", "b"]);
    let docs = Engine::new(Config::new().host_modules(HostModules::new([geo]).unwrap())).docs().unwrap();
    assert_eq!((docs[0].name.as_str(), docs[0].kind), ("prelude", ModuleKind::Prelude));
    let prelude = &docs[0];
    let str_type = prelude.items.iter().find(|i| i.name == "str").unwrap();
    assert_eq!((str_type.kind, str_type.decl.as_str()), (ItemKind::Builtin, "type str"));
    let len = str_type.members.iter().find(|m| m.name == "len").unwrap();
    assert_eq!((len.kind, len.decl.as_str()), (MemberKind::Method, "fn len(self) -> int"));
    assert!(prelude.items.iter().any(|i| i.name == "Iterator" && i.kind == ItemKind::Trait && i.related.contains(&"Iter<T>".to_string())));
    let close = prelude.items.iter().find(|i| i.name == "Close").unwrap();
    assert!(["Option<T>", "List<T>"].iter().all(|n| close.related.contains(&n.to_string())), "{:?}", close.related);

    let json = docs.iter().find(|m| m.name == "json").unwrap();
    assert_eq!(json.kind, ModuleKind::Std);
    assert!(json.doc.as_deref().unwrap().starts_with("JSON"));
    let parse = json.items.iter().find(|i| i.name == "parse").unwrap();
    assert_eq!(parse.decl, "fn parse(text: str) -> Result<Json, Error>");
    assert!(parse.doc.is_some());
    let value = json.items.iter().find(|i| i.name == "Json").unwrap();
    assert!(value.members.iter().any(|m| m.kind == MemberKind::Variant && m.decl == "Int(int)"));

    let regex = docs.iter().find(|m| m.name == "regex").unwrap();
    let r = regex.items.iter().find(|i| i.name == "Regex").unwrap();
    assert!(r.members.iter().all(|m| m.name != "capture_spans"));

    let host = docs.last().unwrap();
    assert_eq!((host.name.as_str(), host.kind), ("geo", ModuleKind::Host));
    assert_eq!(host.items[0].decl, "fn dist(a: float, b: float) -> float");
}

#[test]
fn retry_stress_fails_a_check_in_the_native_body() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let mut m = NativeModule::new("probe");
    m.function("touch", move |vm: &mut Vm, s: String| -> Result<i64, sandlang::vm::Fail> {
        c.fetch_add(1, Ordering::SeqCst);
        vm.check_memory(s.len())?;
        Ok(s.len() as i64)
    });
    let (r, out, _) =
        run_src(Config::new().retry_stress(true).host_modules(HostModules::new([m]).unwrap()), "import probe\nprint(probe.touch(\"abc\"))");
    r.unwrap();
    assert_eq!(out, "3\n");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn derived_impls_of_deep_field_types_are_checked_on_a_small_stack() {
    let (ok, bad) = (format!("int{}", "?".repeat(9000)), format!("float{}", "?".repeat(9000)));
    let src = format!(
        "struct S {{ f: {ok} }}\nstruct F {{ f: {bad} }}\nstruct B<T: Hash> {{ v: T }}\nprint(Set([S(f: None)]).len())\n\
         let b = B(v: S(f: None))\nlet c = B(v: F(f: None))\n"
    );
    let run = move || run_src(Config::new(), &src);
    let (r, _, _) = std::thread::Builder::new().stack_size(512 << 10).spawn(run).unwrap().join().unwrap();
    let Err(Error::Compile(ds)) = r else { panic!("{r:?}") };
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert!(ds[0].message.starts_with("F does not implement `Hash`"), "{}", ds[0].message);
}

#[test]
fn deep_set_elements_and_map_keys_of_fields_are_checked_on_a_small_stack() {
    let (ok, bad) = (format!("int{}", "?".repeat(9000)), format!("float{}", "?".repeat(9000)));
    let src = format!(
        "import json\nstruct K {{ m: [{ok}: int], s: Set<{ok}> }}\nstruct G {{ m: [{bad}: int] }}\n\
         let k: Result<K> = json.decode(\"{{}}\")\nprint(k.is_ok())\n"
    );
    let run = move || run_src(Config::new(), &src);
    let (r, _, _) = std::thread::Builder::new().stack_size(512 << 10).spawn(run).unwrap().join().unwrap();
    let Err(Error::Compile(ds)) = r else { panic!("{r:?}") };
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert_eq!(ds[0].line, 3, "{ds:?}");
    assert!(ds[0].message.starts_with(&format!("{bad} does not implement `Hash`")), "{}", ds[0].message);
}

#[test]
fn rust_bound_field_types_meet_the_requirements_of_their_constructors() {
    #[derive(sandlang::SandStruct)]
    #[sand(module = "lab")]
    struct Seen {
        rows: std::collections::HashSet<Vec<i64>>,
    }
    let mut m = NativeModule::new("lab");
    m.bind::<Seen>();
    let errors = host_module_errors([m]);
    assert_eq!(errors, ["field `rows` of `Seen`: [int] does not implement `Hash` (required for the elements of Set<[int]>)"]);
}
