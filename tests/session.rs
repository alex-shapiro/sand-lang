//! Tests of REPL sessions (`Engine::session`): incremental checking and a
//! persistent VM.
// Helpers of the tests that need the host (feature `os`) are unused without it.
#![cfg_attr(not(feature = "os"), allow(unused_imports, dead_code))]

mod common;

use common::{TempDir, rt};
use sandlang::session::RESET_NOTE;
use sandlang::{
    Capability, Config, Decision, Engine, Error, LimitKind, Limits, MemorySink, NativeModule, Policy, PromptAnswer, RulePolicy, Session,
    StdinFeed,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

struct Repl {
    session: Session,
    out: MemorySink,
    err: MemorySink,
    rt: tokio::runtime::Runtime,
}

impl Repl {
    fn new() -> Repl {
        Repl::with(Config::new())
    }

    fn with(config: Config) -> Repl {
        let out = MemorySink::new();
        let err = MemorySink::new();
        let engine = Engine::new(config.stdout(out.clone()).stderr(err.clone()));
        Repl { session: engine.session(), out, err, rt: rt() }
    }

    fn eval(&mut self, src: &str) -> Result<Option<String>, Error> {
        self.rt.block_on(self.session.eval(src))
    }

    /// Evaluates `src`, which must succeed; returns its value.
    fn value(&mut self, src: &str) -> String {
        match self.eval(src) {
            Ok(Some(v)) => v,
            Ok(None) => panic!("{src:?} has no value"),
            Err(e) => panic!("{src:?} failed: {e}"),
        }
    }

    /// Evaluates `src`, which must succeed without a value.
    fn ok(&mut self, src: &str) {
        match self.eval(src) {
            Ok(None) => {}
            Ok(Some(v)) => panic!("{src:?} has a value: {v}"),
            Err(e) => panic!("{src:?} failed: {e}"),
        }
    }

    fn compile_error(&mut self, src: &str) -> String {
        match self.eval(src) {
            Err(Error::Compile(ds)) => ds.iter().map(|d| d.message.clone()).collect::<Vec<_>>().join("\n"),
            other => panic!("{src:?}: expected a compile error, got {other:?}"),
        }
    }

    fn runtime_error(&mut self, src: &str) -> String {
        match self.eval(src) {
            Err(Error::Runtime { message, .. }) => message,
            other => panic!("{src:?}: expected a runtime error, got {other:?}"),
        }
    }

    fn internal_error(&mut self, src: &str) -> String {
        match self.eval(src) {
            Err(Error::Internal { message, .. }) => message,
            other => panic!("{src:?}: expected an internal error, got {other:?}"),
        }
    }

    /// Output printed since the last call.
    fn take_out(&self) -> String {
        let s = self.out.contents();
        self.out.clear();
        s
    }
}

#[test]
fn expressions_show_their_debug_form() {
    let mut r = Repl::new();
    assert_eq!(r.value("1 + 2"), "3");
    assert_eq!(r.value("\"hi\""), "\"hi\"");
    assert_eq!(r.value("[1, 2].map(x => x * 2)"), "[2, 4]");
    assert_eq!(r.value("Some((1, \"a\"))"), "Some((1, \"a\"))");
    assert_eq!(r.value("if 1 < 2 { \"yes\" } else { \"no\" }"), "\"yes\"");
    // Unit values, statements and declarations have no value.
    r.ok("()");
    r.ok("print(\"x\")");
    r.ok("let x = 1");
    r.ok("fn f() -> int { 1 }");
    r.ok("");
    r.ok("// just a comment");
    // A final `;` discards the value.
    r.ok("1 + 2;");
    assert_eq!(r.take_out(), "x\n");
}

#[test]
fn side_effects_happen_once() {
    let mut r = Repl::new();
    r.ok("print(\"one\")");
    r.ok("var n = 0\nn += 1\nprint(\"n = \\(n)\")");
    assert_eq!(r.value("n"), "1");
    assert_eq!(r.value("n"), "1");
    r.ok("print(\"two\")");
    assert_eq!(r.take_out(), "one\nn = 1\ntwo\n");
}

#[test]
fn globals_persist_and_vars_can_be_assigned() {
    let mut r = Repl::new();
    r.ok("var count = 0");
    r.ok("fn bump() -> int {\n    count += 1\n    count\n}");
    assert_eq!(r.value("bump()"), "1");
    assert_eq!(r.value("bump()"), "2");
    r.ok("count = 10");
    assert_eq!(r.value("bump()"), "11");
    assert_eq!(r.value("count"), "11");
    r.ok("let (a, b) = (1, \"two\")");
    assert_eq!(r.value("(b, a)"), "(\"two\", 1)");
    let msg = r.compile_error("a = 3");
    assert!(msg.contains("cannot assign to immutable binding `a`"), "{msg}");
    // Mutable data in an immutable binding.
    r.ok("let xs = [1]");
    r.ok("xs.push(2)");
    assert_eq!(r.value("xs"), "[1, 2]");
}

#[test]
fn later_definitions_shadow_earlier_ones() {
    let mut r = Repl::new();
    r.ok("fn greet() -> str { \"hello\" }");
    r.ok("fn twice() -> str { greet() + greet() }");
    r.ok("let saved = greet");
    r.ok("fn greet() -> str { \"bye\" }");
    assert_eq!(r.value("greet()"), "\"bye\"");
    // Code checked earlier keeps the definition it saw.
    assert_eq!(r.value("twice()"), "\"hellohello\"");
    assert_eq!(r.value("saved()"), "\"hello\"");
    // Any kind of definition may replace any other.
    r.ok("let greet = 42");
    assert_eq!(r.value("greet + 1"), "43");
    r.ok("struct Point { x: int, y: int }");
    r.ok("let p = Point { x: 1, y: 2 }");
    r.ok("struct Point { x: float }");
    assert_eq!(r.value("Point { x: 1.5 }"), "Point { x: 1.5 }");
    assert_eq!(r.value("p.y"), "2");
    r.ok("enum Point { A, B }");
    assert_eq!(r.value("[Point.A, Point.B]"), "[A, B]");
    // Within one input, names must still be unique.
    let msg = r.compile_error("fn h() {}\nfn h() {}");
    assert!(msg.contains("`h` is already defined"), "{msg}");
}

#[test]
fn types_methods_and_extend_across_inputs() {
    let mut r = Repl::new();
    r.ok("struct Counter {\n    var n: int = 0\n    fn inc(self) { self.n += 1 }\n}");
    r.ok("let c = Counter {}");
    r.ok("c.inc()\nc.inc()");
    assert_eq!(r.value("c.n"), "2");
    r.ok("extend Counter {\n    fn get(self) -> int { self.n }\n}");
    assert_eq!(r.value("c.get()"), "2");
    // A method can be redefined by a later input.
    r.ok("extend Counter {\n    fn get(self) -> int { self.n * 100 }\n}");
    assert_eq!(r.value("c.get()"), "200");
    r.ok("type Pair = (int, int)");
    r.ok("fn swap(p: Pair) -> Pair { (p.1, p.0) }");
    assert_eq!(r.value("swap((1, 2))"), "(2, 1)");
}

#[test]
fn traits_and_impls_across_inputs() {
    let mut r = Repl::new();
    r.ok("trait Describe {\n    fn describe(self) -> str\n    fn shout(self) -> str { self.describe().to_upper() }\n}");
    r.ok("struct Cat { name: str }");
    r.ok("impl Describe for Cat {\n    fn describe(self) -> str { \"cat \\(self.name)\" }\n}");
    assert_eq!(r.value("Cat { name: \"tom\" }.shout()"), "\"CAT TOM\"");
    r.ok("fn all<T: Describe>(xs: [T]) -> [str] { xs.map(x => x.describe()) }");
    r.ok("impl Describe for int {\n    fn describe(self) -> str { \"int \\(self)\" }\n}");
    assert_eq!(r.value("all([1, 2])"), "[\"int 1\", \"int 2\"]");
    // An impl can be replaced by a later input; earlier code keeps the old one.
    r.ok("fn describe_one() -> str { 7.describe() }");
    r.ok("impl Describe for int {\n    fn describe(self) -> str { \"number \\(self)\" }\n}");
    assert_eq!(r.value("7.describe()"), "\"number 7\"");
    assert_eq!(r.value("all([3])"), "[\"number 3\"]");
    assert_eq!(r.value("describe_one()"), "\"int 7\"");
    let msg = r.compile_error("impl Describe for str {\n    fn describe(self) -> str { self }\n}\nimpl Describe for str {\n    fn describe(self) -> str { self }\n}");
    assert!(msg.contains("conflicting impls"), "{msg}");
}

#[test]
fn iterators_labels_and_defer_across_inputs() {
    let mut r = Repl::new();
    r.ok("struct Countdown {\n    var n: int\n}");
    r.ok("impl Iterator<int> for Countdown {\n    fn next(self) -> int? {\n        if self.n <= 0 { return None }\n        self.n -= 1\n        Some(self.n + 1)\n    }\n}");
    assert_eq!(r.value("Countdown { n: 3 }.iter().map(x => x * 10).to_list()"), "[30, 20, 10]");
    r.ok("let evens = Iter.count_from(0).filter(x => x % 2 == 0)");
    assert_eq!(r.value("evens.take(3).to_list()"), "[0, 2, 4]");
    // The iterator persists between inputs (and is single-pass).
    assert_eq!(r.value("evens.first()"), "Some(6)");
    r.ok("var found = 0
@outer for x in (Countdown { n: 5 }) {
    for y in 0..x {
        if x * y == 6 {
            found = x
            break @outer
        }
    }
}");
    assert_eq!(r.value("found"), "3");
    // A top-level defer runs when its input's code ends, after its value is computed.
    assert_eq!(r.value("defer print(\"cleanup\")\nfound + 1"), "4");
    assert_eq!(r.take_out(), "cleanup\n");
    assert_eq!(r.value("let none: [float] = []\nnone.sum()"), "0.0");
}

#[cfg(feature = "os")]
#[test]
fn imports_mid_session() {
    let dir = TempDir::new("session-import");
    dir.write("geo.sand", "pub fn area(w: float, h: float) -> float { w * h }\nprint(\"geo loaded\")\n");
    let mut r = Repl::with(Config::new().policy(Policy::allow_all()).module_root(dir.path()).cwd(dir.path()));
    r.ok("import json");
    assert_eq!(r.value("json.encode(json.parse(\"[1, 2]\")?)"), "\"[1,2]\"");
    r.ok("from math import cbrt");
    assert_eq!(r.value("cbrt(64.0)"), "4.0");
    r.ok("import \"geo.sand\"");
    assert_eq!(r.take_out(), "geo loaded\n");
    assert_eq!(r.value("geo.area(2.0, 3.0)"), "6.0");
    // Importing again binds the name without loading the module again.
    r.ok("import \"geo.sand\" as g");
    assert_eq!(r.value("g.area(1.0, 1.0)"), "1.0");
    assert_eq!(r.take_out(), "");
    // A module whose import failed to compile can be imported later.
    let msg = r.compile_error("import regex\nlet bad: int = \"x\"");
    assert!(msg.contains("type mismatch"), "{msg}");
    r.ok("import regex");
    assert_eq!(r.value("regex.new(\"a+\")?.is_match(\"caat\")"), "true");
    let msg = r.compile_error("import nope");
    assert!(msg.contains("unknown module `nope`"), "{msg}");
}

#[test]
fn import_library_module_by_name() {
    let dir = TempDir::new("session-library");
    dir.write("tools/geo.sand", "import units\npub fn area(w: float, h: float) -> float { units.scale(w * h) }\n");
    dir.write("tools/units.sand", "pub fn scale(x: float) -> float { x * 2.0 }\n");
    let library = dir.path().join("tools");
    let mut r = Repl::with(Config::new().policy(Policy::allow_all()).module_root(dir.path()).cwd(dir.path()).library(&library));
    r.ok("import geo");
    assert_eq!(r.value("geo.area(2.0, 3.0)"), "12.0");
    r.ok("from units import scale");
    assert_eq!(r.value("scale(1.5)"), "3.0");
    let msg = r.compile_error("import nope");
    assert!(msg.contains("unknown module `nope`"), "{msg}");
    let msg = r.compile_error("import json\nlet x: int = json");
    assert!(!msg.contains("unknown module"), "{msg}");
}

#[test]
fn check_library_import_policy() {
    let dir = TempDir::new("session-library-policy");
    dir.write("tools/geo.sand", "pub fn one() -> int { 1 }\n");
    let library = dir.path().join("tools");
    let policy = Policy::custom(|cap: &Capability| if matches!(cap, Capability::Import(_)) { Decision::Deny } else { Decision::Allow });
    let mut r = Repl::with(Config::new().policy(policy).module_root(dir.path()).library(&library));
    let msg = r.compile_error("import geo");
    assert!(msg.contains("cannot import `geo`: permission denied"), "{msg}");
}

#[test]
fn reload_modified_module_on_import() {
    let dir = TempDir::new("session-reload");
    dir.write("tools/geo.sand", "import units\npub fn area() -> float { units.scale(1.0) }\nprint(\"geo v1\")\n");
    dir.write("tools/units.sand", "pub fn scale(x: float) -> float { x * 2.0 }\n");
    let library = dir.path().join("tools");
    let mut r = Repl::with(Config::new().policy(Policy::allow_all()).module_root(dir.path()).library(&library));
    r.ok("import geo");
    assert_eq!(r.take_out(), "geo v1\n");
    r.ok("let before = geo.area");
    r.ok("import geo");
    assert_eq!(r.take_out(), "");
    dir.write("tools/units.sand", "pub fn scale(x: float) -> float { x * 3.0 }\n");
    r.ok("import geo");
    assert_eq!(r.take_out(), "geo v1\n");
    assert_eq!(r.value("geo.area()"), "3.0");
    assert_eq!(r.value("before()"), "2.0");
    dir.write("tools/geo.sand", "pub fn area() -> float { 5.0 }\n");
    let msg = r.compile_error("import geo\nlet bad: int = \"x\"");
    assert!(msg.contains("type mismatch"), "{msg}");
    r.ok("import geo");
    assert_eq!(r.value("geo.area()"), "5.0");
}

#[test]
fn compile_errors_leave_the_session_unchanged() {
    let mut r = Repl::new();
    r.ok("var x = 1");
    let msg = r.compile_error("x = 5\nprint(\"never\")\nlet y: int = \"s\"");
    assert!(msg.contains("type mismatch"), "{msg}");
    assert_eq!(r.value("x"), "1");
    let msg = r.compile_error("y");
    assert!(msg.contains("undefined name `y`"), "{msg}");
    let msg = r.compile_error("fn broken( {");
    assert!(!msg.is_empty());
    // The names of a rejected input can be defined again.
    let _ = r.compile_error("fn f() -> int { \"no\" }");
    r.ok("fn f() -> int { 3 }");
    assert_eq!(r.value("f()"), "3");
    assert_eq!(r.take_out(), "");
    // Diagnostics name the input.
    match r.eval("1 +") {
        Err(Error::Compile(ds)) => assert!(ds[0].file.starts_with("<input "), "{:?}", ds[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn errors_keep_earlier_effects_and_the_session_usable() {
    let mut r = Repl::new();
    r.ok("var a = 1");
    let msg = r.runtime_error("a = 2\nlet b = 10\nlet c = [1][5]\nlet d = 4");
    assert!(msg.contains("index out of bounds"), "{msg}");
    // Assignments before the error happened, and its declarations exist.
    assert_eq!(r.value("a"), "2");
    assert_eq!(r.value("b"), "10");
    let msg = r.runtime_error("d");
    assert!(msg.contains("global used before initialization: `d`"), "{msg}");
    assert_eq!(r.value("a + b"), "12");
    // A failed top-level `?` fails the input with its error.
    r.ok("import json");
    let msg = r.runtime_error("let n = json.parse(\"x\")?\nprint(\"not reached\")");
    assert!(msg.contains("parse"), "{msg}");
    assert_eq!(r.value("[a].len()"), "1");
    assert_eq!(r.take_out(), "");
    // Errors inside functions have a trace.
    r.ok("fn boom() { fail(\"boom\") }");
    match r.eval("boom()") {
        Err(Error::Runtime { message, trace }) => {
            assert_eq!(message, "error: boom");
            assert_eq!(trace[0].function, "boom");
            assert!(trace[0].file.starts_with("<input "), "{trace:?}");
        }
        other => panic!("{other:?}"),
    }
}

/// The functions of an input that fails outlive the globals it did not
/// initialize: their loops read those globals only where they do.
#[test]
fn loops_of_a_failed_input_read_its_uninitialized_globals_where_they_do() {
    let mut r = Repl::new();
    let late = "fn late(n: int) -> int {\n    var t = 0\n    for i in 0..n {\n        if i == 2 {\n            t += LATE\n        }\n        t += 1\n    }\n    t\n}\n";
    let msg = r.runtime_error(&format!("{late}fail(\"boom\")\nlet LATE = 10"));
    assert_eq!(msg, "error: boom");
    assert_eq!(r.value("late(2)"), "2");
    let msg = r.runtime_error("late(3)");
    assert!(msg.contains("global used before initialization: `LATE`"), "{msg}");
    // Later inputs read the globals of earlier ones that are initialized.
    r.ok("let K = 4");
    assert_eq!(r.value("fn sum(n: int) -> int {\n    var t = 0\n    for i in 0..n { t += K }\n    t\n}\nsum(3)"), "12");
}

#[test]
fn exit_ends_the_input() {
    let mut r = Repl::new();
    r.ok("print(\"a\")\nexit(3)\nprint(\"b\")");
    assert_eq!(r.session.exit_code(), Some(3));
    assert_eq!(r.take_out(), "a\n");
    assert_eq!(r.value("1"), "1");
    assert_eq!(r.session.exit_code(), None);
}

#[test]
fn tasks_persist_across_inputs() {
    let mut r = Repl::new();
    r.ok("let ch: Channel<int> = task.channel(10)");
    r.ok("let t = spawn {\n    var total = 0\n    for _ in 0..3 { total += ch.recv() ?? 0 }\n    total\n}");
    r.ok("ch.send(1);");
    r.ok("ch.send(2)\nch.send(3);");
    assert_eq!(r.value("t.wait()"), "6");
    // A task spawned by one input runs while later inputs run.
    r.ok("var log: [str] = []");
    r.ok("let bg = spawn {\n    log.push(\"started\")\n    task.sleep(20ms)\n    log.push(\"done\")\n}");
    r.ok("task.sleep(50ms)");
    assert_eq!(r.value("log"), "[\"started\", \"done\"]");
    // A failed input cancels all tasks.
    r.ok("let slow = spawn { task.sleep(10s)\n1 }");
    let _ = r.runtime_error("fail(\"stop\")");
    let msg = r.runtime_error("slow.wait()");
    assert!(msg.contains("task cancelled"), "{msg}");
}

#[test]
fn limits_apply_per_input_and_memory_to_the_heap() {
    let limits = Limits { max_instructions: Some(200_000), max_memory: Some(8 << 20), ..Limits::default() };
    let mut r = Repl::with(Config::new().limits(limits));
    // Each input gets the full instruction budget.
    for _ in 0..5 {
        r.ok("var i = 0\nwhile i < 50_000 { i += 1 }");
    }
    match r.eval("while true {}") {
        Err(Error::Limit(LimitKind::Instructions)) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(r.value("1 + 1"), "2");
    // The heap persists: data kept in globals counts towards the memory limit.
    r.ok("var keep: [str] = []");
    let mut hit = false;
    for _ in 0..40 {
        match r.eval("keep.push(\"x\".repeat(1 << 20))") {
            Ok(_) => {}
            Err(Error::Limit(LimitKind::Memory)) => {
                hit = true;
                break;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(hit, "the memory limit was never reached");
    r.ok("keep = []");
    r.ok("let small = \"y\".repeat(1000)");
    assert_eq!(r.value("small.len()"), "1000");
}

#[test]
fn interrupting_an_input() {
    let mut r = Repl::new();
    r.ok("var steps = 0");
    let handle = r.session.interrupt_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        handle.interrupt();
    });
    match r.eval("while true { steps += 1 }") {
        Err(Error::Interrupted) => {}
        other => panic!("{other:?}"),
    }
    t.join().unwrap();
    // The session continues, with the state reached.
    assert_eq!(r.value("steps > 0"), "true");
}

#[cfg(feature = "os")]
#[test]
fn dropping_an_eval_future_leaves_a_usable_session() {
    let dir = TempDir::new("session-drop");
    dir.write("m.sand", "pub fn f() -> int { 7 }\n");
    dir.write("elsewhere/a.sand", "pub fn g() -> int { 5 }\n");
    // The first prompt never gets an answer; later ones allow.
    let asked = Arc::new(AtomicUsize::new(0));
    let n = asked.clone();
    let prompter = move |_: Capability| -> sandlang::policy::PromptFuture {
        if n.fetch_add(1, Ordering::SeqCst) == 0 { Box::pin(std::future::pending()) } else { Box::pin(async { PromptAnswer::AllowOnce }) }
    };
    // Imports are prompted for (only those under `elsewhere` would be allowed).
    let rules = RulePolicy::new().allow_import(dir.path().join("elsewhere")).fallback(sandlang::Decision::Prompt);
    let policy = Policy::rules(rules);
    let mut r = Repl::with(Config::new().policy(policy).prompter(prompter).module_root(dir.path()).cwd(dir.path()));
    let timeout = |r: &mut Repl, src: &str| {
        let Repl { rt, session, .. } = r;
        let timed_out = rt.block_on(async { tokio::time::timeout(Duration::from_millis(100), session.eval(src)).await.is_err() });
        assert!(timed_out, "{src:?} finished");
    };
    // Dropped while running: effects so far persist, tasks are cancelled.
    r.ok("var x = 1\nlet t = spawn { task.sleep(10s) }");
    timeout(&mut r, "x = 2\nwhile true { task.yield_now() }");
    assert_eq!(r.value("x"), "2");
    let msg = r.runtime_error("t.wait()");
    assert!(msg.contains("task cancelled"), "{msg}");
    // Dropped while loading an import (waiting for a prompt): nothing is kept.
    timeout(&mut r, "import \"elsewhere/a.sand\"\nimport \"m.sand\"");
    r.ok("import \"elsewhere/a.sand\"\nimport \"m.sand\"");
    assert_eq!(r.value("a.g() + m.f()"), "12");
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

#[test]
fn types_of_expressions() {
    let mut r = Repl::new();
    r.ok("let xs = [1, 2]\nfn f(x: int, y: str = \"a\") -> str? { None }");
    let mut ty = |s: &str| r.session.type_of(s);
    assert_eq!(ty("xs").unwrap(), "[int]");
    assert_eq!(ty("xs.map(x => x.to_float())").unwrap(), "[float]");
    assert_eq!(ty("f(1)").unwrap(), "str?");
    assert_eq!(ty("print(\"not run\")").unwrap(), "()");
    assert!(matches!(ty("let y = 1"), Err(Error::Compile(_))));
    assert!(matches!(ty("nope"), Err(Error::Compile(_))));
    assert!(matches!(ty("1 +"), Err(Error::Compile(_))));
    assert_eq!(r.take_out(), "");
    // Nothing is defined by `type_of`.
    r.compile_error("y");
}

#[test]
fn test_blocks_run_on_request() {
    let mut r = Repl::new();
    r.ok("fn double(x: int) -> int { x * 2 }");
    r.ok("test \"doubles\" { assert(double(2) == 4) }");
    r.ok("test \"fails\" { assert(double(2) == 5, \"wrong\") }");
    let report = r.rt.block_on(r.session.run_tests()).unwrap();
    assert_eq!(report.render(), "ok doubles\nFAIL fails: assert: wrong\n1 passed, 1 failed\n");
    // A test defined again replaces the earlier one with that name.
    r.ok("test \"fails\" { assert(double(3) == 6) }");
    let report = r.rt.block_on(r.session.run_tests()).unwrap();
    assert_eq!(report.render(), "ok doubles\nok fails\n2 passed, 0 failed\n");
}

#[test]
fn reset_starts_over() {
    let mut r = Repl::new();
    r.ok("let x = 1");
    r.session.reset();
    let msg = r.compile_error("x");
    assert!(msg.contains("undefined name `x`"), "{msg}");
    r.ok("let x = \"fresh\"");
    assert_eq!(r.value("x"), "\"fresh\"");
}

#[test]
fn top_level_use_bindings_persist_across_inputs() {
    let mut r = Repl::new();
    r.ok("struct Res {\n    name: str\n    var closed: bool = false\n}");
    r.ok("impl Close for Res {\n    fn close(self) {\n        self.closed = true\n        print(\"closed \\(self.name)\")\n    }\n}");
    r.ok("let res = [1, 2]");
    r.ok("use res = Res { name: \"a\" }");
    assert_eq!(r.value("res.closed"), "false");
    assert_eq!(r.value("res.name"), "\"a\"");
    r.ok("use _ = Res { name: \"b\" }");
    assert_eq!(r.take_out(), "");
    r.ok("{\n    use inner = Res { name: \"c\" }\n}");
    assert_eq!(r.take_out(), "closed c\n");
    let msg = r.compile_error("use n = 1");
    assert!(msg.contains("`use` needs a value whose type implements `Close`"), "{msg}");
}

#[cfg(feature = "os")]
#[test]
fn a_file_bound_with_top_level_use_stays_open_for_later_inputs() {
    let dir = TempDir::new("session-use");
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let policy = Policy::rules(RulePolicy::new().fallback(sandlang::Decision::Allow));
    let mut r = Repl::with(Config::new().policy(policy).cwd(dir.path()).module_root(dir.path()));
    r.ok("import fs");
    r.ok("use f = fs.open(\"a.txt\")?");
    assert_eq!(r.value("f.read_text()?"), "\"hello\"");
}

#[cfg(feature = "os")]
#[test]
fn always_answers_persist_and_prompts_happen_once() {
    let dir = TempDir::new("session-prompt");
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let n = asked.clone();
    let prompter = move |cap: Capability| -> sandlang::policy::PromptFuture {
        n.fetch_add(1, Ordering::SeqCst);
        let _ = cap;
        Box::pin(async { PromptAnswer::AllowAlways })
    };
    let policy = Policy::rules(RulePolicy::new().fallback(sandlang::Decision::Prompt));
    let mut r = Repl::with(Config::new().policy(policy).prompter(prompter).cwd(dir.path()).module_root(dir.path()));
    r.ok("import fs");
    assert_eq!(r.value("fs.read_text(\"a.txt\")?"), "\"hello\"");
    assert_eq!(r.value("fs.read_text(\"a.txt\")?.len()"), "5");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // Stays remembered after a reset (it belongs to the engine).
    r.session.reset();
    r.ok("import fs");
    assert_eq!(r.value("fs.read_text(\"a.txt\")?.len()"), "5");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn output_goes_to_the_configured_sinks() {
    let mut r = Repl::new();
    r.ok("eprint(\"to stderr\")");
    assert_eq!(r.err.contents(), "to stderr\n");
}

#[test]
fn session_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let mut s = Engine::new(Config::new()).session();
    assert_send(&s);
    let fut = s.eval("1");
    assert_send(&fut);
}

#[test]
fn input_completeness() {
    for complete in
        ["1 + 2", "let x = [1,\n2]", "fn f() {\n}", "\"a\"", "x?", "s = \"\\(a)\"", "\"{\"", "// c", "", "r\"{\"", "/// Doc.\nfn f() {}"]
    {
        assert!(Session::is_complete(complete), "{complete:?}");
    }
    for incomplete in [
        "fn f() {",
        "[1,",
        "foo(",
        "\"abc",
        // An open interpolation keeps the input open.
        "print(\"a \\(x",
        "print(\"a \\(f(x,",
        "\"\"\"\nabc",
        "/* c",
        "1 +",
        "xs\n    .map(f).",
        "let x =",
        "if a { 1 } else",
        "/// Doc.",
        "/// Doc.\n/// More.\n",
    ] {
        assert!(!Session::is_complete(incomplete), "{incomplete:?}");
    }
}

// ---------------------------------------------------------------------------
// Internal errors
// ---------------------------------------------------------------------------

/// A host module whose `crash(n)` panics (in Rust) when `n > 0`: host natives
/// can have bugs.
fn buggy_module() -> NativeModule {
    let mut m = NativeModule::new("bug");
    m.function("crash", |n: i64| -> i64 {
        if n > 0 {
            panic!("host bug {n}");
        }
        n
    });
    m
}

fn assert_reset(r: &Repl, message: &str, panic: &str) {
    assert!(message.starts_with(panic), "{message}");
    assert!(message.ends_with(RESET_NOTE), "{message}");
    assert!(r.session.was_reset());
}

#[test]
fn internal_panics_reset_the_session() {
    let mut r = Repl::with(Config::new().module(buggy_module()));
    r.ok("import bug\nlet x = 1");
    assert_eq!(r.value("bug.crash(0) + x"), "1");
    assert!(!r.session.was_reset());
    let msg = r.internal_error("let y = x + 1\nbug.crash(1)");
    assert_reset(&r, &msg, "host bug 1");
    let msg = r.compile_error("x");
    assert!(msg.contains("undefined name `x`"), "{msg}");
    assert!(!r.session.was_reset());
    r.ok("import bug\nlet x = 2");
    assert_eq!(r.value("bug.crash(0) + x"), "2");
    let msg = r.internal_error("let t = spawn bug.crash(2)\ntask.sleep(50ms)");
    assert_reset(&r, &msg, "host bug 2");
    r.compile_error("t");
    r.ok("import bug");
    r.ok("test \"a\" { assert(bug.crash(0) == 0) }\ntest \"b\" { bug.crash(3) }\ntest \"c\" { assert(true) }");
    let report = r.rt.block_on(r.session.run_tests()).unwrap();
    assert!(r.session.was_reset());
    assert_eq!(report.results.len(), 2, "{}", report.render());
    assert!(report.results[0].passed);
    let msg = report.results[1].message.clone().unwrap();
    assert!(msg.starts_with("internal error: host bug 3") && msg.ends_with(RESET_NOTE), "{msg}");
    r.compile_error("bug.crash(0)");
}

#[test]
fn internal_panics_in_background_tasks_reset_the_session() {
    let mut r = Repl::with(Config::new().module(buggy_module()));
    r.ok("import bug\nlet t = spawn { task.sleep(20ms)\nbug.crash(1) }");
    let mut errors = Vec::new();
    let got = r.rt.block_on(r.session.run_tasks_until(async { tokio::time::sleep(Duration::from_millis(300)).await }, |e| errors.push(e)));
    assert_eq!(got, Some(()));
    match &errors[..] {
        [Error::Internal { message, .. }] => assert_reset(&r, message, "host bug 1"),
        other => panic!("{other:?}"),
    }
    r.compile_error("t");
}

#[cfg(feature = "os")]
#[test]
fn always_answers_survive_a_reset() {
    let dir = TempDir::new("session-reset-prompt");
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    std::fs::write(dir.path().join("b.txt"), "bye").unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let n = asked.clone();
    let prompter = move |cap: Capability| -> sandlang::policy::PromptFuture {
        if matches!(&cap, Capability::FsRead(p) if p.ends_with("b.txt")) {
            panic!("prompter bug");
        }
        n.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { PromptAnswer::AllowAlways })
    };
    let policy = Policy::rules(RulePolicy::new().fallback(sandlang::Decision::Prompt));
    let mut r = Repl::with(Config::new().policy(policy).prompter(prompter).cwd(dir.path()).module_root(dir.path()));
    let msg = r.internal_error("import fs\nlet a = fs.read_text(\"a.txt\")?\nlet b = fs.read_text(\"b.txt\")?");
    assert_reset(&r, &msg, "prompter bug");
    let asked_before = asked.load(Ordering::SeqCst);
    r.ok("import fs");
    assert_eq!(r.value("fs.read_text(\"a.txt\")?"), "\"hello\"");
    assert_eq!(asked.load(Ordering::SeqCst), asked_before);
}

// ---------------------------------------------------------------------------
// Tasks between inputs
// ---------------------------------------------------------------------------

impl Repl {
    /// Runs the session's tasks for `ms` milliseconds; returns the errors.
    fn idle(&mut self, ms: u64) -> Vec<Error> {
        let mut errors = Vec::new();
        let wait = async move { tokio::time::sleep(Duration::from_millis(ms)).await };
        let got = self.rt.block_on(self.session.run_tasks_until(wait, |e| errors.push(e)));
        assert_eq!(got, Some(()), "a task called exit");
        errors
    }
}

#[test]
fn tasks_run_between_inputs() {
    let mut r = Repl::new();
    let got = r.rt.block_on(r.session.run_tasks_until(async { 5 }, |e| panic!("{e}")));
    assert_eq!(got, Some(5));
    r.ok("var log: [str] = []");
    r.ok("let t = spawn {\n    task.sleep(20ms)\n    log.push(\"tick\")\n    print(\"tick\")\n    task.sleep(20ms)\n    log.push(\"tock\")\n}");
    assert_eq!(r.value("log"), "[]");
    assert!(r.idle(300).is_empty());
    assert_eq!(r.value("log"), "[\"tick\", \"tock\"]");
    assert_eq!(r.take_out(), "tick\n");
    r.ok("let ch: Channel<int> = task.channel(1)\nlet reader = spawn ch.recv()");
    assert!(r.idle(50).is_empty());
    r.ok("ch.send(4);");
    assert_eq!(r.value("reader.wait()"), "Some(4)");
    r.ok("let slow = spawn { task.sleep(10s) }\nlet bad = spawn { task.sleep(10ms)\nfail(\"bad task\") }");
    match &r.idle(300)[..] {
        [Error::Runtime { message, trace }] => {
            assert_eq!(message, "error: bad task");
            assert!(!trace.is_empty());
        }
        other => panic!("{other:?}"),
    }
    let msg = r.runtime_error("slow.wait()");
    assert!(msg.contains("task cancelled"), "{msg}");
    r.ok("let slow = spawn { task.sleep(10s) }");
    assert_eq!(r.session.cancel_tasks(), 1);
    assert_eq!(r.session.cancel_tasks(), 0);
    let msg = r.runtime_error("slow.wait()");
    assert!(msg.contains("task cancelled"), "{msg}");
}

#[test]
fn limits_of_tasks_between_inputs() {
    let limits = Limits { max_instructions: Some(200_000), max_wall_time: Some(Duration::from_millis(100)), ..Limits::default() };
    let mut r = Repl::with(Config::new().limits(limits));
    r.ok("var done = false\nlet t = spawn { task.sleep(150ms)\ndone = true }");
    assert!(r.idle(400).is_empty());
    assert_eq!(r.value("done"), "true");
    r.ok("var n = 0\nlet busy = spawn { while true { n += 1 } }");
    match &r.idle(500)[..] {
        [Error::Limit(LimitKind::Instructions)] => {}
        other => panic!("{other:?}"),
    }
    let msg = r.runtime_error("busy.wait()");
    assert!(msg.contains("task cancelled"), "{msg}");
    assert_eq!(r.value("n > 0"), "true");
    match r.eval("task.sleep(1s)") {
        Err(Error::Limit(LimitKind::WallTime)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn exit_in_a_task_between_inputs() {
    let mut r = Repl::new();
    r.ok("spawn { task.sleep(10ms)\nexit(7) };");
    let start = std::time::Instant::now();
    let got = r.rt.block_on(r.session.run_tasks_until(async { tokio::time::sleep(Duration::from_secs(20)).await }, |e| panic!("{e}")));
    assert_eq!(got, None);
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(r.session.exit_code(), Some(7));
}

#[cfg(feature = "os")]
#[test]
fn prompts_between_inputs_wait_for_the_input() {
    let dir = TempDir::new("session-idle-prompt");
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let have_input = Arc::new(AtomicBool::new(false));
    let asked_early = Arc::new(AtomicUsize::new(0));
    let asked = Arc::new(AtomicUsize::new(0));
    let (h, e, n) = (have_input.clone(), asked_early.clone(), asked.clone());
    let prompter = move |_: Capability| -> sandlang::policy::PromptFuture {
        n.fetch_add(1, Ordering::SeqCst);
        if !h.load(Ordering::SeqCst) {
            e.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { PromptAnswer::AllowOnce })
    };
    let policy = Policy::rules(RulePolicy::new().fallback(sandlang::Decision::Prompt));
    let mut r = Repl::with(Config::new().policy(policy).prompter(prompter).cwd(dir.path()).module_root(dir.path()));
    r.ok("import fs");
    let before = asked.load(Ordering::SeqCst);
    r.ok("let t = spawn { task.sleep(10ms)\nfs.read_text(\"a.txt\") }");
    let input = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        have_input.store(true, Ordering::SeqCst);
        "the input"
    };
    let got = r.rt.block_on(r.session.run_tasks_until(input, |e| panic!("{e}")));
    assert_eq!(got, Some("the input"));
    assert_eq!(asked.load(Ordering::SeqCst), before + 1);
    assert_eq!(asked_early.load(Ordering::SeqCst), 0);
    assert_eq!(r.value("t.wait()"), "Ok(\"hello\")");
}

#[test]
fn dropping_a_run_tasks_until_future_keeps_the_tasks() {
    let mut r = Repl::new();
    r.ok("var k = 0\nlet t = spawn { task.sleep(150ms)\nk = 1 }");
    let Repl { rt, session, .. } = &mut r;
    let wait = session.run_tasks_until(std::future::pending::<()>(), |e| panic!("{e}"));
    assert!(rt.block_on(async { tokio::time::timeout(Duration::from_millis(50), wait).await }).is_err());
    assert_eq!(r.value("t.wait()\nk"), "1");
    assert!(r.idle(10).is_empty());
}

// ---------------------------------------------------------------------------
// Types in messages
// ---------------------------------------------------------------------------

#[test]
fn types_of_generic_functions() {
    let mut r = Repl::new();
    r.ok("fn id<T>(x: T) -> T { x }\nfn pair<A, B>(a: A, b: B) -> (A, B) { (a, b) }\nfn smallest<T: Ord>(xs: [T]) -> T? { None }");
    r.ok("trait Shape { fn area(self) -> float }\nfn total<S: Shape>(xs: [S]) -> float { 0.0 }");
    let mut ty = |s: &str| r.session.type_of(s).unwrap();
    assert_eq!(ty("id"), "fn<T>(T) -> T");
    assert_eq!(ty("pair"), "fn<A, B>(A, B) -> (A, B)");
    assert_eq!(ty("smallest"), "fn<T: Ord>([T]) -> T?");
    assert_eq!(ty("total"), "fn<S: Shape>([S]) -> float");
    assert_eq!(ty("x => pair(x, 1)"), "fn<A>(A) -> (A, int)");
    assert_eq!(ty("x => x"), "fn<T>(T) -> T");
    assert_eq!(ty("(a, b) => a < b"), "fn<T: Ord>(T, T) -> bool");
    assert_eq!(ty("Some"), "fn<T>(T) -> T?");
    assert_eq!(ty("(x, y) => pair(id(x), y)"), "fn<A, B>(A, B) -> (A, B)");
    assert_eq!(ty("id(3)"), "int");
    assert_eq!(ty("[]"), "[_]");
}

#[test]
fn scripts_read_the_input_the_host_feeds() {
    let feed = StdinFeed::new();
    let mut r = Repl::with(Config::new().policy(Policy::allow_all()).stdin_feed(&feed));
    r.ok("import io");
    // Data sent ahead is kept until read.
    feed.send("early\npart");
    assert_eq!(r.value("io.stdin.read_line()?"), "Some(\"early\")");
    assert!(!feed.is_wanted());
    // A read that waits, with nothing else to run, asks for input.
    let Repl { session, rt, .. } = &mut r;
    let (value, ()) = rt.block_on(async {
        let answer = async {
            feed.wanted().await;
            feed.send("ial\n");
        };
        tokio::join!(session.eval("io.stdin.read_line()?"), answer)
    });
    assert_eq!(value.unwrap().as_deref(), Some("Some(\"partial\")"));
    // An end of input ends one read.
    feed.end();
    assert_eq!(r.value("io.stdin.read_line()?"), "None");
    feed.send("again\n");
    assert_eq!(r.value("io.stdin.read_line()?"), "Some(\"again\")");
    // A task's read waits between inputs, until the host sends a line.
    r.ok("let t = spawn io.stdin.read_line()");
    let got = r.rt.block_on(r.session.run_tasks_until(feed.wanted(), |e| panic!("{e}")));
    assert_eq!(got, Some(()));
    assert!(r.idle(50).is_empty());
    assert_eq!(r.value("t.is_done()"), "false");
    feed.send("late\n");
    assert_eq!(r.value("t.wait()?"), "Some(\"late\")");
    assert!(!feed.is_wanted());
}

#[test]
fn redefined_types_are_told_apart_in_messages() {
    let mut r = Repl::new();
    r.ok("struct Point { x: int }");
    r.ok("let p = Point { x: 1 }");
    r.ok("struct Point { x: int }");
    r.ok("fn f(q: Point) -> int { q.x }");
    let msg = r.compile_error("f(p)");
    assert_eq!(msg, "type mismatch: expected Point (defined in input 3), found Point (defined in input 1)");
    let msg = r.compile_error("let o: Point? = Some(p)");
    assert_eq!(msg, "type mismatch: expected Option<Point (defined in input 3)>, found Option<Point (defined in input 1)>");
    let msg = r.compile_error("let n: int = p");
    assert_eq!(msg, "type mismatch: expected int, found Point");
}

#[test]
fn inputs_can_be_named() {
    let mut session = Engine::new(Config::new()).session();
    match rt().block_on(session.eval_as("main.sand", "let a = 1\nlet b: int = \"no\"")) {
        Err(Error::Compile(d)) => assert_eq!((d[0].file.as_str(), d[0].line), ("main.sand", 2)),
        other => panic!("{other:?}"),
    }
    rt().block_on(session.eval_as("main.sand", "fn f() -> int { 7 }")).unwrap();
    assert_eq!(rt().block_on(session.eval("f() * 6")).unwrap().as_deref(), Some("42"));
}

#[test]
fn wall_time_set_per_session() {
    let limits = Limits { max_wall_time: Some(Duration::from_millis(50)), ..Limits::default() };
    let mut r = Repl::with(Config::new().limits(limits));
    assert!(matches!(r.eval("task.sleep(200ms)"), Err(Error::Limit(LimitKind::WallTime))));
    r.session.set_wall_time(Some(Duration::from_secs(5)));
    r.ok("task.sleep(200ms)");
    r.session.set_wall_time(Some(Duration::from_millis(20)));
    assert!(matches!(r.eval("task.sleep(200ms)"), Err(Error::Limit(LimitKind::WallTime))));
    r.session.set_wall_time(None);
    r.ok("task.sleep(100ms)");
}
