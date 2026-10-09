//! Tests of REPL sessions (`Engine::session`): incremental checking and a
//! persistent VM.

mod common;

use common::{TempDir, rt};
use sandlang::{Capability, Config, Engine, Error, LimitKind, Limits, MemorySink, Policy, PromptAnswer, RulePolicy, Session};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    r.ok("var n = 0\nn += 1\nprint(\"n = {n}\")");
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
    r.ok("impl Describe for Cat {\n    fn describe(self) -> str { \"cat {self.name}\" }\n}");
    assert_eq!(r.value("Cat { name: \"tom\" }.shout()"), "\"CAT TOM\"");
    r.ok("fn all<T: Describe>(xs: [T]) -> [str] { xs.map(x => x.describe()) }");
    r.ok("impl Describe for int {\n    fn describe(self) -> str { \"int {self}\" }\n}");
    assert_eq!(r.value("all([1, 2])"), "[\"int 1\", \"int 2\"]");
    // An impl can be replaced by a later input; earlier code keeps the old one.
    r.ok("fn describe_one() -> str { 7.describe() }");
    r.ok("impl Describe for int {\n    fn describe(self) -> str { \"number {self}\" }\n}");
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

#[test]
fn imports_mid_session() {
    let dir = TempDir::new("session-import");
    dir.write("geo.sand", "pub fn area(w: float, h: float) -> float { w * h }\nprint(\"geo loaded\")\n");
    let mut r = Repl::with(Config::new().policy(Policy::allow_all()).module_root(dir.path()).cwd(dir.path()));
    r.ok("import json");
    assert_eq!(r.value("json.stringify(json.parse(\"[1, 2]\")?)"), "\"[1,2]\"");
    r.ok("from math import sqrt");
    assert_eq!(r.value("sqrt(16.0)"), "4.0");
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
fn panics_keep_earlier_effects_and_the_session_usable() {
    let mut r = Repl::new();
    r.ok("var a = 1");
    let msg = r.runtime_error("a = 2\nlet b = 10\nlet c = [1][5]\nlet d = 4");
    assert!(msg.contains("index out of bounds"), "{msg}");
    // Assignments before the panic happened, and its declarations exist.
    assert_eq!(r.value("a"), "2");
    assert_eq!(r.value("b"), "10");
    let msg = r.runtime_error("d");
    assert!(msg.contains("global used before initialization: `d`"), "{msg}");
    assert_eq!(r.value("a + b"), "12");
    // A failed top-level `?` aborts the input like a panic.
    r.ok("import json");
    let msg = r.runtime_error("let n = json.parse(\"x\")?\nprint(\"not reached\")");
    assert!(msg.contains("parse"), "{msg}");
    assert_eq!(r.value("[a].len()"), "1");
    assert_eq!(r.take_out(), "");
    // Panics inside functions have a trace.
    r.ok("fn boom() { panic(\"boom\") }");
    match r.eval("boom()") {
        Err(Error::Runtime { message, trace }) => {
            assert_eq!(message, "boom");
            assert_eq!(trace[0].function, "boom");
            assert!(trace[0].file.starts_with("<input "), "{trace:?}");
        }
        other => panic!("{other:?}"),
    }
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
    let _ = r.runtime_error("panic(\"stop\")");
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
    assert_eq!(report.render(), "ok doubles\nFAIL fails: wrong\n1 passed, 1 failed\n");
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
    for complete in ["1 + 2", "let x = [1,\n2]", "fn f() {\n}", "\"a\"", "x?", "s = \"{a}\"", "// c", "", "r\"{\"", "/// Doc.\nfn f() {}"] {
        assert!(Session::is_complete(complete), "{complete:?}");
    }
    for incomplete in [
        "fn f() {",
        "[1,",
        "foo(",
        "\"abc",
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
