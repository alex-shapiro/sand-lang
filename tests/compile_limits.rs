//! The compile budget: hostile programs must fail to compile, format, or lint
//! quickly instead of stalling or crashing the host.
// Helpers of the tests that need the host (feature `os`) are unused without it.
#![cfg_attr(not(feature = "os"), allow(unused_imports, dead_code))]

mod common;

use common::{TempDir, rt};
use sandlang::check::budget::CompileBudget;
use sandlang::{Config, Engine, Error, LimitKind, Limits, MemorySink, Policy};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Generous: each of these took minutes, hours or gigabytes before.
const FAST: Duration = Duration::from_secs(10);

fn check(config: Config, src: &str) -> (Result<(), Error>, Duration) {
    let mut engine = Engine::new(config.stdout(MemorySink::new()));
    let start = Instant::now();
    let r = engine.check_source("main.sand", src).map(|_| ());
    (r, start.elapsed())
}

/// Asserts that `src` fails to compile, quickly, with an error containing `msg`.
fn rejects(src: &str, msg: &str) {
    rejects_with(Config::new(), src, msg)
}

fn rejects_with(config: Config, src: &str, msg: &str) {
    let (r, took) = check(config, src);
    match r {
        Err(Error::Compile(ds)) => {
            assert!(ds.iter().any(|d| d.message.contains(msg)), "expected `{msg}`, got {ds:?}")
        }
        other => panic!("expected a compile error `{msg}`, got {other:?}"),
    }
    assert!(took < FAST, "took {took:?}");
}

fn limits(f: impl FnOnce(&mut Limits)) -> Config {
    let mut l = Limits::default();
    f(&mut l);
    Config::new().limits(l)
}

#[test]
fn or_patterns_over_tuples() {
    // Proving the second arm unreachable explores 2^30 combinations.
    let n = 30;
    let src = format!(
        "enum E {{ A, B, C }}\nfn f(t: ({})) -> int {{\n    match t {{\n        ({}) => 0\n        ({}) => 1\n    }}\n}}\n",
        vec!["E"; n].join(", "),
        vec!["_"; n].join(", "),
        vec!["A | B"; n].join(", "),
    );
    rejects(&src, "pattern too complex to check for exhaustiveness");
    // Also in irrefutability checks (`let`, `for`).
    let src = format!(
        "enum E {{ A, B }}\nfn f(t: ({})) -> int {{\n    let ({}) = t\n    1\n}}\n",
        vec!["E"; n].join(", "),
        vec!["A | B"; n].join(", "),
    );
    rejects(&src, "pattern too complex to check for exhaustiveness");
}

#[test]
fn large_matches_are_still_checked() {
    let n = 1000;
    let variants: Vec<String> = (0..n).map(|i| format!("V{i}")).collect();
    let arms: Vec<String> = (0..n).map(|i| format!("        V{i} => {i}")).collect();
    let src = format!(
        "enum E {{ {} }}\nfn f(e: E) -> int {{\n    match e {{\n{}\n    }}\n}}\nprint(f(E.V7))\n",
        variants.join(", "),
        arms.join("\n")
    );
    let (r, took) = check(Config::new(), &src);
    r.unwrap();
    assert!(took < FAST, "took {took:?}");
    // Exhaustiveness errors are still reported normally.
    let src = src.replace("        V999 => 999\n", "");
    rejects(&src, "non-exhaustive match: missing `V999`");
}

#[test]
fn types_that_double_in_size() {
    // Inference: each line doubles the type.
    let mut src = String::from("let a0 = 1\n");
    for i in 1..60 {
        src += &format!("let a{i} = (a{}, a{})\n", i - 1, i - 1);
    }
    rejects(&src, "type too large");
    // Aliases.
    let mut src = String::from("type T0 = int\n");
    for i in 1..60 {
        src += &format!("type T{i} = (T{}, T{})\n", i - 1, i - 1);
    }
    src += "fn f(x: T59) -> int { 1 }\n";
    rejects(&src, "type too large");
    // Generic instantiation.
    let src = format!("fn dup<T>(x: T) -> (T, T) {{ (x, x) }}\nlet a = {}1{}\n", "dup(".repeat(60), ")".repeat(60));
    rejects(&src, "type too large");
}

#[test]
fn bound_checks_on_structs_sharing_field_types() {
    let mut src = String::from("struct S0 { x: int }\n");
    for i in 1..60 {
        src += &format!("struct S{i} {{ a: S{}, b: S{} }}\n", i - 1, i - 1);
    }
    src += "fn same<T: Eq>(a: T, b: T) -> bool { a == b }\nfn f(x: S59) -> bool { same(x, x) }\n";
    rejects(&src, "program too complex to compile");
    // Runtime type descriptions (`json.decode`) share the descriptions of
    // such structs, so they stay small (see `reflected_types_are_shared`).
    let mut src = String::from("import json\nstruct S0 { x: int }\n");
    for i in 1..60 {
        src += &format!("struct S{i} {{ a: S{}, b: S{} }}\n", i - 1, i - 1);
    }
    src += "let r: Result<S59> = json.decode(\"1\")\n";
    let (r, took) = check(Config::new(), &src);
    r.unwrap();
    assert!(took < FAST, "took {took:?}");
}

#[test]
fn deeply_nested_string_interpolation() {
    // Used to overflow the host stack in the lexer (aborting the process).
    let n = 100_000;
    let src = format!("let s = {}1{}\n", "\"{".repeat(n), "}\"".repeat(n));
    rejects(&src, "strings nested too deeply");
    // Moderate nesting still works.
    let src = "let x = 1\nprint(\"a{\"b{\"c{x}\"}\"}\")\n";
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().stdout(out.clone()));
    rt().block_on(engine.run_source("main.sand", src)).unwrap();
    assert_eq!(out.contents(), "abc1\n");
}

/// A program that is slow to check, but only polynomially (so it finishes
/// eventually when unlimited): many matches near the pattern work limit.
fn slow_program() -> String {
    let n = 18;
    let m =
        format!("    match t {{\n        ({}) => 0\n        ({}) => 1\n    }}\n", vec!["_"; n].join(", "), vec!["A | B"; n].join(", "),);
    let mut src = String::from("enum E { A, B, C }\n");
    for i in 0..200 {
        src += &format!("fn f{i}(t: ({})) -> int {{\n{m}}}\n", vec!["E"; n].join(", "));
    }
    src
}

#[test]
fn compile_fuel_is_configurable() {
    let src = "fn f(x: int) -> int { x + 1 }\nprint(f(1))\n";
    rejects_with(limits(|l| l.max_compile_fuel = Some(100)), src, "program too complex to compile");
    let (r, _) = check(limits(|l| l.max_compile_fuel = None), src);
    r.unwrap();
    // The default stops a program whose checking is merely slow.
    rejects(&slow_program(), "program too complex to compile");
}

/// A program that is cheap to type check but expensive to compile: deferred
/// code is recompiled at every exit.
fn deferred_copies(functions: usize, returns: usize) -> String {
    let mut src = String::new();
    let sum = vec!["x"; 500].join(" + ");
    for i in 0..functions {
        src += &format!("fn f{i}(x: int) -> int {{\n    defer {{ print({sum}) }}\n");
        for r in 0..returns {
            src += &format!("    if x == {r} {{ return {r} }}\n");
        }
        src += "    0\n}\n";
    }
    src
}

#[test]
fn many_blocks_parse_quickly() {
    let src = format!("fn f(x: int) -> int {{\n{}    0\n}}\nprint(f(3))\n", "    if x == 1 { return 1 }\n".repeat(100_000));
    let (r, took) = check(Config::new(), &src);
    r.unwrap();
    assert!(took < FAST, "took {took:?}");
}

#[test]
fn code_generation_is_covered_by_the_compile_budget() {
    let src = deferred_copies(40, 900);
    let (r, took) = check(limits(|l| l.max_compile_fuel = None), &deferred_copies(1, 900));
    r.unwrap();
    assert!(took < FAST, "took {took:?}");
    rejects(&src, "program too complex to compile");
    let timed = limits(|l| {
        l.max_compile_fuel = None;
        l.max_wall_time = Some(Duration::from_millis(100));
    });
    let (r, took) = check(timed, &src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");
    assert!(took < FAST, "took {took:?}");
}

#[test]
fn the_formatter_is_bounded() {
    use sandlang::fmt::{format_source, format_source_with};
    let src = format!("let x = {}g(\n{}){}\n", "f(".repeat(200), "    1,\n".repeat(30_000), ")".repeat(200));
    let start = Instant::now();
    let e = format_source(&src).unwrap_err();
    assert!(e.contains("source too complex to format"), "{e}");
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
    let out = format_source_with(&src, &CompileBudget::new(Some(100_000_000), None, None)).unwrap();
    assert!(out.len() > 20 * src.len());
    assert_eq!(format_source("let  x = [1,2]\n").unwrap(), "let x = [1, 2]\n");
    let e = format_source_with(&src, &CompileBudget::new(None, Some(Instant::now()), None)).unwrap_err();
    assert!(e.starts_with("time limit exceeded"), "{e}");
    let interrupted = CompileBudget::new(None, None, Some(Arc::new(AtomicBool::new(true))));
    assert_eq!(format_source_with("let x = 1\n", &interrupted).unwrap_err(), "interrupted");
}

#[test]
fn the_linter_is_bounded() {
    let nested = |depth: usize| {
        let mut src = String::from("var x = 0\n");
        for i in 0..depth {
            src += &format!("@l{i} loop {{\n");
        }
        src += &"x = x + 1\n".repeat(20_000);
        for i in (0..depth).rev() {
            src += &format!("break @l{i}\n}}\nx = 2\n");
        }
        src
    };
    let start = Instant::now();
    let e = sandlang::lint::lint_source("main.sand", &nested(120)).unwrap_err();
    assert!(e.contains("source too complex to lint"), "{e}");
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
    assert_eq!(sandlang::lint::lint_source("main.sand", &nested(20)).unwrap(), vec![]);
    let e = sandlang::lint::lint_source("main.sand", &slow_program()).unwrap_err();
    assert!(e.contains("program too complex to compile"), "{e}");
    let lets: String = (0..20_000).map(|i| format!("let _v{i} = {i}; ")).collect();
    let src = format!("// \u{e9}\nfn f() {{ {lets}}}\nf()\n");
    let start = Instant::now();
    assert_eq!(sandlang::lint::lint_source("main.sand", &src).unwrap(), vec![]);
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
}

#[test]
fn wall_time_and_interrupts_stop_compilation() {
    let src = slow_program();
    let unlimited = || limits(|l| l.max_compile_fuel = None);
    let timed = limits(|l| {
        l.max_compile_fuel = None;
        l.max_wall_time = Some(Duration::from_millis(100));
    });
    let (r, took) = check(timed, &src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");
    assert!(took < FAST, "took {took:?}");

    let mut engine = Engine::new(unlimited().stdout(MemorySink::new()));
    let handle = engine.interrupt_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        handle.interrupt();
    });
    let start = Instant::now();
    let r = rt().block_on(engine.run_source("main.sand", &src));
    t.join().unwrap();
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
}

#[cfg(feature = "os")]
#[test]
fn source_size_limit_covers_imports() {
    let dir = TempDir::new("imports");
    let body = format!("pub fn f() -> int {{ 1 }}\n// {}\n", "x".repeat(200));
    dir.write("a.sand", &body);
    dir.write("b.sand", &body);
    let main = dir.write("main.sand", "import \"a\"\nimport \"b\"\nprint(a.f() + b.f())\n");
    let config = |max: usize| {
        Config::new()
            .policy(Policy::allow_all())
            .cwd(dir.path())
            .stdout(MemorySink::new())
            .limits(Limits { max_source_bytes: max, ..Limits::default() })
    };
    // Each file fits, but not all of them together.
    let mut engine = Engine::new(config(400));
    match rt().block_on(engine.run_file(&main)) {
        Err(Error::Compile(ds)) => assert!(ds.iter().any(|d| d.message.contains("source too large")), "{ds:?}"),
        other => panic!("{other:?}"),
    }
    let mut engine = Engine::new(config(1000));
    rt().block_on(engine.run_file(&main)).unwrap();
}

/// Runs the script `main` (with the script modules `files` beside it),
/// returning its output.
fn run_files(files: &[(&str, &str)], main: &str) -> Result<String, Error> {
    let dir = TempDir::new("reflect");
    for (name, body) in files {
        dir.write(name, body);
    }
    let main = dir.write("main.sand", main);
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().policy(Policy::allow_all()).cwd(dir.path()).stdout(out.clone()));
    rt().block_on(engine.run_file(&main))?;
    Ok(out.contents())
}

#[cfg(feature = "os")]
#[test]
fn reflected_types_are_described_once() {
    // Struct descriptions that double at each level, used by many call
    // sites: each struct is described once and shared, within a
    // description and by every call site (this took gigabytes).
    let mut src = String::from("import json\nstruct S0 { x: int }\n");
    for i in 1..40 {
        src += &format!("struct S{i} {{ a: S{}, b: S{} }}\n", i - 1, i - 1);
    }
    for i in 0..400 {
        src += &format!("let r{i}: Result<S39> = json.decode(\"1\")\n");
    }
    src += r#"let s: S2 = json.decode("\{\"a\": \{\"a\": \{\"x\": 1\}, \"b\": \{\"x\": 2\}\}, \"b\": \{\"a\": \{\"x\": 3\}, \"b\": \{\"x\": 4\}\}\}").unwrap()
print(s.a.b.x * 10 + s.b.a.x)
"#;
    let start = Instant::now();
    assert_eq!(run_files(&[], &src).unwrap(), "23\n");
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
}

#[cfg(feature = "os")]
#[test]
fn shared_reflected_types_with_back_references() {
    // Shared descriptions that refer back to an enclosing type, and
    // recursive enums.
    let src = r#"import json
struct S0 { x: int, up: Root? }
struct S1 { a: S0, b: S0 }
struct Root { a: S1, b: S1, n: int }
enum Tree { Leaf(int), Node(Tree, Tree) }
let leaf = "\{\"x\": 1\}"
let s1 = "\{\"a\": {leaf}, \"b\": {leaf}\}"
let inner = "\{\"a\": {s1}, \"b\": {s1}, \"n\": 7\}"
let s1b = "\{\"a\": {leaf}, \"b\": \{\"x\": 5, \"up\": {inner}\}\}"
let r: Root = json.decode("\{\"a\": {s1}, \"b\": {s1b}, \"n\": 3\}").unwrap()
print("{r.n} {r.b.b.up.unwrap().n} {r.b.b.x}")
let t: (Tree, Tree) = json.decode("[\{\"Node\": [\{\"Leaf\": [1]\}, \{\"Leaf\": [2]\}]\}, \{\"Leaf\": [3]\}]").unwrap()
print(t)
"#;
    assert_eq!(run_files(&[], src).unwrap(), "3 7 5\n(Node(Leaf(1), Leaf(2)), Leaf(3))\n");
}

#[cfg(feature = "os")]
#[test]
fn reflected_types_with_the_same_name() {
    // Two structs named `Config` in different modules are different types
    // (recursive references used to be resolved by name, decoding the inner
    // one as the outer one).
    let files = [("a.sand", "pub struct Config { p: int, q: int, r: int }\n")];
    let src = r#"import json
import "./a.sand"
struct Config { inner: a.Config? }
let c: Config = json.decode("\{\"inner\": \{\"p\": 1, \"q\": 2, \"r\": 3\}\}").unwrap()
print(c.inner.unwrap().r)
let bad: Result<Config> = json.decode("\{\"inner\": \{\"inner\": null\}\}")
print(bad.is_err())
"#;
    assert_eq!(run_files(&files, src).unwrap(), "3\ntrue\n");
}

#[test]
fn reflected_types_too_large_to_describe() {
    // Polymorphic recursion: `P<int>` contains `P<[int]>`, which contains
    // `P<[[int]]>`, ... (each new). Many call sites stay cheap: the work of
    // all descriptions of a compilation is bounded too.
    let mut src = String::from("import json\nstruct P<T> { v: T, next: P<[T]>? }\n");
    for i in 0..300 {
        src += &format!("struct Q{i} {{ x: int }}\nlet r{i}: Result<P<Q{i}>> = json.decode(\"1\")\n");
    }
    rejects(&src, "type too large to describe at runtime");
}

/// Compiles the sources `source(n)` and `source(4 * n)` and asserts that the larger
/// takes about four times as long.
fn compiles_in_linear_time(what: &str, n: usize, source: impl Fn(usize) -> String) {
    let (_, small) = check(Config::new(), &source(n));
    let (_, large) = check(Config::new(), &source(4 * n));
    assert!(large < small * 7 + Duration::from_millis(500), "{what}: {n}: {small:?}, {}: {large:?}", 4 * n);
}

#[test]
fn interpolations_parse_in_linear_time() {
    compiles_in_linear_time("interpolations", 25_000, |n| format!("let x = 1\nlet s = \"{}\"\nprint(s.len())\n", "{x}".repeat(n)));
}

#[test]
fn many_loop_constants_compile_in_linear_time() {
    compiles_in_linear_time("loop constants", 25_000, |n| {
        let body: String = (0..n).map(|i| format!("        x = x * {}\n", i + 100_000)).collect();
        format!("fn main() {{\n    var x = 0\n    while x < 1 {{\n{body}        x += 1\n    }}\n    print(x)\n}}\nmain()\n")
    });
}

#[test]
fn many_locals_resolve_in_linear_time() {
    compiles_in_linear_time("locals", 25_000, |n| {
        format!("fn main() {{\n    let x = 1\n{}    print(a)\n}}\nmain()\n", "    let a = x\n".repeat(n))
    });
}

#[test]
fn many_inlined_lambdas_compile_in_linear_time() {
    compiles_in_linear_time("inlined lambdas", 25_000, |n| {
        format!("fn main() {{\n    var xs = [1, 2, 3]\n{}    print(xs)\n}}\nmain()\n", "    xs = xs.map(x => x + 1)\n".repeat(n))
    });
}
