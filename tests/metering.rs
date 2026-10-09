//! Metering of native work and heavy operations: natives charge fuel, so
//! `max_instructions`, `max_cpu_time`, `max_wall_time` and the interrupt
//! handle apply while they run, and Rust-side scratch space counts against
//! `max_memory`.

mod common;

use common::rt;
use sandlang::vm::Vm;
use sandlang::vm::value::Value;
use sandlang::{Config, Engine, Error, LimitKind, Limits, MemorySink, NativeModule, Outcome};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn run(config: Config, src: &str) -> (Result<Outcome, Error>, String) {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone()));
    let r = rt().block_on(engine.run_source("main.sand", src));
    (r, out.contents())
}

fn limits(f: impl FnOnce(&mut Limits)) -> Config {
    let mut l = Limits::default();
    f(&mut l);
    Config::new().limits(l)
}

/// Two structurally equal values whose trees have 2^60 nodes (each level
/// shares its child), so comparing, displaying or hashing them never ends
/// unless the work is metered.
const DAG: &str = r#"
enum T { Leaf(int), Node(T, T) }
fn dag(n: int) -> T {
    var t = T.Leaf(1)
    for i in 0..n { t = T.Node(t, t) }
    t
}
let a = dag(60)
let b = dag(60)
"#;

/// Runs `src` and checks that it stops with `want` within `within`.
fn expect_stop(config: Config, src: &str, want: impl Fn(&Error) -> bool, within: Duration) {
    let start = Instant::now();
    let (r, _) = run(config, src);
    let took = start.elapsed();
    match r {
        Err(e) if want(&e) => {}
        other => panic!("unexpected result {other:?}"),
    }
    assert!(took < within, "took {took:?}");
}

fn is_limit(k: LimitKind) -> impl Fn(&Error) -> bool {
    move |e| matches!(e, Error::Limit(x) if *x == k)
}

// ---------------------------------------------------------------------------
// Instruction limit

#[test]
fn native_work_counts_against_the_instruction_limit() {
    let config = || limits(|l| l.max_instructions = Some(1_000_000));
    // Cheap natives stay cheap.
    let (r, out) = run(config(), "let xs = (0..10000).to_list()\nprint(xs.sorted().len())");
    r.unwrap();
    assert_eq!(out, "10000\n");
    // A few calls doing a lot of work exceed the budget.
    let (r, out) = run(config(), "let xs = (0..2000000).to_list()\nprint(xs.len())");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
    assert_eq!(out, "");
    let src = "import random\nlet xs = (0..200000).to_list()\nrandom.shuffle(xs)\nlet s = xs.sorted()\nprint(\"sorted\")";
    let (r, out) = run(config(), src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
    assert_eq!(out, "");
}

#[test]
fn string_and_json_natives_are_metered() {
    let config = || limits(|l| l.max_instructions = Some(500_000));
    for src in [
        // 64 MB of copying: one unit per 64 bytes.
        "let s = \"x\".repeat(64000000)\nprint(s.len())",
        // Scanning a big string many times.
        "let s = \"x\".repeat(1000000)\nvar n = 0\nfor i in 0..2000 { if s.contains(\"y\") { n += 1 } }\nprint(n)",
        // JSON: one unit per value.
        "import json\nlet t = \"[\" + \"1,\".repeat(600000) + \"1]\"\nprint(json.parse(t).is_ok())",
        // Hashing.
        "import crypto\nlet b = \"x\".repeat(8000000).bytes()\nprint(crypto.sha256(b).len())",
    ] {
        let (r, _) = run(config(), src);
        assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{src}: {r:?}");
    }
}

#[test]
fn loops_that_only_continue_are_metered() {
    // `continue` jumps back to the loop's start without reaching its back
    // edge: it must consume fuel too, or such a loop escapes every limit.
    let cases = [
        "while true { continue }",
        "var i = 0\nwhile true {\n    i += 1\n    continue\n}",
        "loop { continue }",
        "@outer loop {\n    for x in 0..10 { continue @outer }\n}",
        "for x in 0..9223372036854775807 { if x >= 0 { continue } }",
        // The filter of a fused pipeline skips elements with `continue`.
        "for x in Iter.repeat(1).filter(x => x > 1) { print(x) }",
        "print(Iter.count_from(0).filter(x => x < 0).count())",
    ];
    for src in cases {
        expect_stop(limits(|l| l.max_instructions = Some(1_000_000)), src, is_limit(LimitKind::Instructions), Duration::from_secs(10));
        let wall = limits(|l| l.max_wall_time = Some(Duration::from_millis(200)));
        expect_stop(wall, src, is_limit(LimitKind::WallTime), Duration::from_secs(10));
    }
}

#[test]
fn deep_comparison_is_metered() {
    // `==` on composite values is a single instruction doing unbounded work.
    let src = format!("{DAG}print(a == b)");
    expect_stop(limits(|l| l.max_instructions = Some(1_000_000)), &src, is_limit(LimitKind::Instructions), Duration::from_secs(10));
    let src = format!("{DAG}print(a < b)");
    expect_stop(limits(|l| l.max_instructions = Some(1_000_000)), &src, is_limit(LimitKind::Instructions), Duration::from_secs(10));
    // Inline variants (`Some(x)` without a heap object) wrap heap values too.
    for cmp in ["Some(a) == Some(b)", "Ok(a) < Ok(b)", "[Some(a)] == [Some(b)]"] {
        let src = format!("{DAG}print({cmp})");
        expect_stop(limits(|l| l.max_instructions = Some(1_000_000)), &src, is_limit(LimitKind::Instructions), Duration::from_secs(10));
    }
}

// ---------------------------------------------------------------------------
// Time limits are checked while a native runs

#[test]
fn cpu_and_wall_time_limits_stop_a_long_operation() {
    let src = format!("{DAG}print(a == b)");
    let cpu = limits(|l| l.max_cpu_time = Some(Duration::from_millis(100)));
    expect_stop(cpu, &src, is_limit(LimitKind::CpuTime), Duration::from_secs(10));
    let wall = limits(|l| l.max_wall_time = Some(Duration::from_millis(100)));
    expect_stop(wall, &src, is_limit(LimitKind::WallTime), Duration::from_secs(10));
    // A list whose elements compare expensively: the sort stops midway.
    let src = format!("{DAG}let xs = [a, b, a, b, a]\nprint(xs.sorted().len())");
    let cpu = limits(|l| l.max_cpu_time = Some(Duration::from_millis(100)));
    expect_stop(cpu, &src, is_limit(LimitKind::CpuTime), Duration::from_secs(10));
}

#[test]
fn big_keys_cost_time() {
    // Each lookup hashes 10 MB: the loop is short but the work is not.
    let src = "let s = \"x\".repeat(10000000)\nlet m = [s: 1]\nvar n = 0\nfor i in 0..1000000 { if m.contains(s) { n += 1 } }\nprint(n)";
    let cpu = limits(|l| l.max_cpu_time = Some(Duration::from_millis(200)));
    expect_stop(cpu, src, is_limit(LimitKind::CpuTime), Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// Interrupts reach long natives

/// A host module whose `ready()` tells the test that the script reached the
/// interesting part.
fn signal() -> (NativeModule, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let mut m = NativeModule::new("signal");
    m.function("ready", move || {
        let _ = tx.lock().unwrap().send(());
    });
    (m, rx)
}

/// Runs `src` and interrupts it `delay` after it calls `signal.ready()`.
fn interrupt_after_ready(src: &str, delay: Duration) -> (Result<Outcome, Error>, String, Duration) {
    let (m, rx) = signal();
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().module(m).stdout(out.clone()));
    let handle = engine.interrupt_handle();
    let t = std::thread::spawn(move || {
        rx.recv().unwrap();
        std::thread::sleep(delay);
        let at = Instant::now();
        handle.interrupt();
        at
    });
    let r = rt().block_on(engine.run_source("main.sand", src));
    let done = Instant::now();
    let at = t.join().unwrap();
    (r, out.contents(), done.saturating_duration_since(at))
}

#[test]
fn interrupt_stops_a_giant_sort() {
    // Comparing two elements never finishes: only a mid-sort check can stop it.
    let src = format!("import signal\n{DAG}let xs = [a, b, a, b]\nsignal.ready()\nlet s = xs.sorted()\nprint(\"sorted\")");
    let (r, out, latency) = interrupt_after_ready(&src, Duration::from_millis(50));
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
    assert_eq!(out, "");
    assert!(latency < Duration::from_secs(2), "stopped {latency:?} after the interrupt");

    // A plain big sort of numbers.
    let src = "import signal\nimport random\nlet xs = (0..2000000).to_list()\nrandom.shuffle(xs)\nsignal.ready()\nlet s = xs.sorted()\nprint(\"sorted\")";
    let (r, out, latency) = interrupt_after_ready(src, Duration::from_millis(5));
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
    assert_eq!(out, "", "the sort finished before the interrupt");
    assert!(latency < Duration::from_secs(2), "stopped {latency:?} after the interrupt");
}

#[test]
fn interrupt_stops_a_big_json_parse() {
    let src = "import signal\nimport json\nlet t = \"[\" + \"[1, 2.5, \\\"abc\\\", true, null],\".repeat(400000) + \"1]\"\nsignal.ready()\nlet j = json.parse(t)\nprint(\"parsed\")";
    let (r, out, latency) = interrupt_after_ready(src, Duration::from_millis(5));
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
    assert_eq!(out, "", "the parse finished before the interrupt");
    assert!(latency < Duration::from_secs(2), "stopped {latency:?} after the interrupt");
}

// ---------------------------------------------------------------------------
// Host natives

#[test]
fn host_natives_can_charge_and_be_stopped() {
    // A raw native that works forever, charging the meter as it goes.
    let mut m = NativeModule::new("busy");
    m.source("pub native fn spin() -> int");
    m.raw_function("spin", |vm: &mut Vm, _: &[Value]| {
        let mut n: i64 = 0;
        loop {
            vm.charge(1)?;
            n = n.wrapping_add(1);
            std::hint::black_box(n);
        }
    });
    let config = limits(|l| l.max_instructions = Some(5_000_000)).module(m.clone());
    expect_stop(config, "import busy\nprint(busy.spin())", is_limit(LimitKind::Instructions), Duration::from_secs(10));
    let config = limits(|l| l.max_cpu_time = Some(Duration::from_millis(50))).module(m);
    expect_stop(config, "import busy\nprint(busy.spin())", is_limit(LimitKind::CpuTime), Duration::from_secs(10));

    // Typed host functions are charged for converting their arguments.
    let mut m = NativeModule::new("host");
    m.function("total", |xs: Vec<i64>| -> i64 { xs.iter().sum() });
    let config = limits(|l| l.max_instructions = Some(1_000_000)).module(m);
    let src = "import host\nlet xs = [1, 2, 3]\nvar t = 0\nfor i in 0..1000000 { t += host.total(xs) }\nprint(t)";
    let (r, _) = run(config, src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
}

#[test]
fn natives_stop_exactly_at_the_instruction_limit() {
    // A native that charges one unit at a time and counts the units it was
    // granted (its last charge fails).
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let granted = Arc::new(AtomicU64::new(0));
    let mut m = NativeModule::new("busy");
    m.source("pub native fn spin() -> int");
    let g = granted.clone();
    m.raw_function("spin", move |vm: &mut Vm, _: &[Value]| {
        loop {
            vm.charge(1)?;
            g.fetch_add(1, Ordering::Relaxed);
        }
    });
    let max = 100_000;
    let spin_after = |iterations: u64| {
        granted.store(0, Ordering::Relaxed);
        let config = limits(|l| l.max_instructions = Some(max)).module(m.clone());
        let src = format!("import busy\nvar t = 0\nfor i in 0..{iterations} {{ t += i }}\nprint(busy.spin() + t)");
        let (r, _) = run(config, &src);
        assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
        granted.load(Ordering::Relaxed)
    };
    // The native gets what is left of the budget: no more (the overrun used
    // to be up to 20,000 units), and no less than that minus the few units
    // the program spends besides its loop.
    for iterations in [0, 1_000, 15_000, 19_990, 30_000, 99_000] {
        let n = spin_after(iterations);
        assert!(n + iterations <= max, "{iterations} loop iterations, then {n} units in the native");
        assert!(n + iterations >= max - 100, "{iterations} loop iterations, then only {n} units in the native");
    }
    // A program whose bytecode alone exceeds the budget stops within a unit
    // of it (before the native is called).
    assert_eq!(spin_after(max), 0);
}

// ---------------------------------------------------------------------------
// Scratch space counts against the memory limit

#[test]
fn scratch_buffers_count_against_the_memory_limit() {
    let config = || limits(|l| l.max_memory = Some(64 << 20));
    for src in [
        // Display of a value with shared structure: 2^60 leaves.
        &format!("{DAG}print(a.to_str().len())") as &str,
        &format!("{DAG}print(\"{{a:?}}\".len())"),
        // JSON encoding of the same.
        &format!("import json\n{DAG}print(json.encode(a).len())"),
        // Hashing it as a set element or map key.
        &format!("{DAG}print(set([a]).len())"),
        // Many small pieces from a modest string.
        "let s = \",\".repeat(20000000)\nprint(s.split(\",\").len())",
        // Output much larger than the input.
        "let s = \"a\".repeat(1000000)\nprint(s.replace(\"a\", \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\").len())",
        "let xs = (0..100000).to_list().map(i => \"x\".repeat(1000))\nprint(\"\".join(xs).len())",
        "import base64\nlet b = \"x\".repeat(40000000).bytes()\nprint(base64.encode(b).len())",
    ] {
        let start = Instant::now();
        let (r, _) = run(config(), src);
        assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{src}: {r:?}");
        assert!(start.elapsed() < Duration::from_secs(20), "{src}: took {:?}", start.elapsed());
    }
}

#[test]
fn memory_checks_do_not_count_collectable_garbage() {
    // Each round leaves ~23 MB of garbage; the live data stays under 60 MB.
    // Reservations count garbage not yet collected, so the engine collects
    // and retries before reporting the limit.
    let src = "let xs = (0..1000000).to_list()\nvar n = 0\nfor round in 0..6 {\n    let ys = xs.sorted()\n    let s = ys.to_str()\n    n += s.len()\n}\nprint(n)";
    let (r, out) = run(limits(|l| l.max_memory = Some(80 << 20)), src);
    r.unwrap();
    assert_eq!(out, "47333340\n");
    // Below the live size it still fails.
    let (r, _) = run(limits(|l| l.max_memory = Some(30 << 20)), src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

// ---------------------------------------------------------------------------
// Memory the VM uses besides heap objects

fn mem_limit(mib: usize) -> Config {
    limits(|l| {
        l.max_memory = Some(mib << 20);
        l.max_wall_time = Some(Duration::from_secs(60));
    })
}

#[test]
fn fiber_stacks_count_against_the_memory_limit() {
    // 20 tasks blocked 9,000 frames deep in a function with 400 locals
    // would hold over 1 GB of stack.
    let lets: String = (0..400).map(|i| format!("    let a{i} = n + {i}\n")).collect();
    let sum: Vec<String> = (0..400).step_by(40).map(|i| format!("a{i}")).collect();
    let src = format!(
        "fn deep(n: int) -> int {{\n{lets}    if n == 0 {{\n        task.sleep(5s)\n        return 0\n    }}\n    deep(n - 1) + {}\n}}\n\
         var ts = []\nfor i in 0..20 {{ ts.push(spawn {{ deep(9000) }}) }}\nfor t in ts {{ print(t.wait()) }}\n",
        sum.join(" + ")
    );
    expect_stop(mem_limit(64), &src, is_limit(LimitKind::Memory), Duration::from_secs(20));
    // Deep recursion within the limit still works (and its stack is freed).
    let src = "fn f(n: int) -> int { if n == 0 { 0 } else { 1 + f(n - 1) } }\nfor i in 0..5 { print(f(9000)) }";
    let (r, out) = run(mem_limit(16), src);
    r.unwrap();
    assert_eq!(out, "9000\n".repeat(5));
}

#[test]
fn map_and_set_keys_count_their_copies() {
    // Each key copies the shared 1 MiB string four times into the index.
    let src = "let s = \"x\".repeat(1 << 20)\nvar m: [(int, str, str, str, str): int] = [:]\nfor i in 0..2000 { m[(i, s, s, s, s)] = i }\nprint(m.len())";
    expect_stop(mem_limit(64), src, is_limit(LimitKind::Memory), Duration::from_secs(20));
    let src = "let s = \"x\".repeat(1 << 20)\nvar m: Set<(int, str, str)> = set([])\nfor i in 0..2000 { m.add((i, s, s)) }\nprint(m.len())";
    expect_stop(mem_limit(64), src, is_limit(LimitKind::Memory), Duration::from_secs(20));
    // Removing keys gives their memory back.
    let src = "let s = \"x\".repeat(1 << 20)\nvar m: [(int, str): int] = [:]\nfor i in 0..200 {\n    m[(i, s)] = i\n    m.remove((i, s))\n}\nprint(m.len())";
    let (r, out) = run(mem_limit(32), src);
    r.unwrap();
    assert_eq!(out, "0\n");
}

#[test]
fn race_lists_are_charged_and_counted() {
    // The same task many times: one waiter per element.
    let src = "let t = spawn { task.sleep(5s) }\nlet ts = (0..3000000).map(_ => t)\nprint(task.race(ts))";
    expect_stop(mem_limit(64), src, is_limit(LimitKind::Memory), Duration::from_secs(20));
    let src = "let t = spawn { task.sleep(5s) }\nlet ts = (0..100000).map(_ => t)\nfor i in 0..100 { print(task.race(ts)) }";
    let config = limits(|l| l.max_instructions = Some(2_000_000));
    expect_stop(config, src, is_limit(LimitKind::Instructions), Duration::from_secs(20));
    // Racing a task against itself (its waiter list is pruned meanwhile).
    let src = "let t = spawn {\n    task.sleep(10ms)\n    5\n}\nlet u = spawn { t.wait() }\nprint(task.race((0..100).map(_ => t)))\nprint(u.wait())";
    let (r, out) = run(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "5\n5\n");
}

/// `probe.receivers(ch)` and `probe.waiters(task)`: the lengths of the wait
/// lists of a channel and of a running task.
fn probe() -> NativeModule {
    use sandlang::vm::value::Obj;
    let mut m = NativeModule::new("probe");
    m.source("pub native fn receivers(ch: Channel<int>) -> int\npub native fn waiters(t: Task<()>) -> int");
    m.raw_function("receivers", |vm: &mut Vm, a: &[Value]| {
        Ok(Value::Int(match vm.heap.get(a[0]) {
            Obj::Channel(c) => c.receivers.len() as i64,
            _ => -1,
        }))
    });
    m.raw_function("waiters", |vm: &mut Vm, a: &[Value]| {
        let fid = match vm.heap.get(a[0]) {
            Obj::Task(t) => t.fiber as usize,
            _ => return Ok(Value::Int(-1)),
        };
        Ok(Value::Int(vm.fibers.get(fid).and_then(|f| f.as_ref()).map_or(-1, |f| f.waiters.len() as i64)))
    });
    m
}

#[test]
fn cancelled_waits_do_not_accumulate() {
    // Each cancelled wait leaves a stale entry behind until it is pruned.
    let src = "import probe\nlet ch: Channel<int> = task.channel()\nlet long = spawn { task.sleep(60s) }\n\
               for i in 0..100000 {\n    let a = spawn { ch.recv() }\n    let b = spawn { long.wait() }\n    task.yield_now()\n    a.cancel()\n    b.cancel()\n}\n\
               print((probe.receivers(ch) < 100, probe.waiters(long) < 100))\nlong.cancel()";
    let (r, out) = run(mem_limit(16).module(probe()), src);
    r.unwrap();
    assert_eq!(out, "(true, true)\n");
    // Live waits are kept.
    let src = "let ch: Channel<int> = task.channel()\nvar ts = []\nfor i in 0..100 {\n    let c = spawn { ch.recv() }\n    task.yield_now()\n    c.cancel()\n    ts.push(spawn { ch.recv() })\n}\nfor i in 0..100 { ch.send(i) }\nvar n = 0\nfor t in ts { n += t.wait().unwrap() }\nprint(n)";
    let (r, out) = run(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "4950\n");
}

#[test]
fn channel_buffers_are_counted_before_they_grow() {
    let src = "let ch: Channel<int> = task.channel()\nvar i = 0\nwhile true {\n    ch.send(i)\n    i += 1\n}";
    expect_stop(mem_limit(32), src, is_limit(LimitKind::Memory), Duration::from_secs(20));
}

#[test]
fn metered_operations_keep_their_results() {
    // Metering must not change behaviour of ordinary programs.
    let src = r##"
import json
import regex
import crypto
let xs = [5, 3, 9, 1, 7, 3]
print(xs.sorted())
print([(2, "b"), (1, "z"), (2, "a")].sorted())
print(["pear", "apple", "fig"].sorted())
print((0..40).to_list().reversed().sorted() == (0..40).to_list())
print(([[1, 2], [1]] == [[1, 2], [1]], [[1], [2]] != [[1], [3]]))
print(("a,b,,c".split(","), "x y  z".split_whitespace(), "héllo".chars()))
print(("aaa".replace("a", "bc"), "abcabc".replace("bc", "")))
print((b"hello world".find(b"o w"), b"aaab".contains(b"aab"), b"abc".find(b"abd")))
print(json.stringify(json.parse(r#"{"k": [1, 2, {"x": null}]}"#)?))
print(regex.new(r"\d+")?.find_all("a1 b22 c333").map(m => m.text))
print(regex.new(r"\s*,\s*")?.split("a , b,c"))
print(crypto.sha256("abc".bytes()).to_hex())
let m = [(1, "a"): 1, (2, "b"): 2]
print((m[(2, "b")], set([(1, 2), (1, 2), (2, 3)]).len()))
print("{3.14159:.2} {42:>5} {"ab":*^6}")
"##;
    let (r, out) = run(Config::new(), src);
    r.unwrap();
    assert_eq!(
        out,
        "[1, 3, 3, 5, 7, 9]\n[(1, \"z\"), (2, \"a\"), (2, \"b\")]\n[\"apple\", \"fig\", \"pear\"]\ntrue\n(true, true)\n\
         ([\"a\", \"b\", \"\", \"c\"], [\"x\", \"y\", \"z\"], [\"h\", \"é\", \"l\", \"l\", \"o\"])\n(\"bcbcbc\", \"aa\")\n\
         (Some(4), true, None)\n{\"k\":[1,2,{\"x\":null}]}\n[\"1\", \"22\", \"333\"]\n[\"a\", \"b\", \"c\"]\n\
         ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n(2, 2)\n3.14    42 **ab**\n"
    );
}

// ---------------------------------------------------------------------------
// Regular expressions

#[test]
fn quadratic_regex_iteration_is_charged_for_its_work() {
    // Every one-letter match makes the search scan to the end of the text
    // (the first alternative might still match there), so iterating does
    // quadratic work; it used to cost seconds of CPU for a linear charge.
    let src = "import regex\nlet s = \"a\".repeat(200000)\nlet r = regex.new(r\"[a-z]*X|[a-z]\")?\nprint(r.find_all(s).len())";
    for call in [".find_all(s).len()", ".split(s).len()", ".replace_all(s, \"b\").len()", ".captures_all(s).len()"] {
        let src = src.replace(".find_all(s).len()", call);
        expect_stop(limits(|l| l.max_instructions = Some(200_000)), &src, is_limit(LimitKind::Instructions), Duration::from_secs(2));
    }
    // The same calls on ordinary patterns are charged linearly.
    let src = "import regex\nlet s = \"ab cd \".repeat(100000)\nlet r = regex.new(r\"[a-z]+\")?\n\
               print((r.find_all(s).len(), r.split(s).len(), r.replace_all(s, \"$0!\").len(), r.captures_all(s).len()))";
    let (r, out) = run(limits(|l| l.max_instructions = Some(5_000_000)), src);
    r.unwrap();
    assert_eq!(out, "(200000, 200001, 800000, 200000)\n");
}

#[test]
fn compiled_regexes_count_against_the_memory_limit() {
    // Each of these compiles to a few MB of automata, and searching non-ASCII
    // text grows its lazy DFA cache: they must not let a script make the
    // host allocate gigabytes under a 32 MB limit.
    let src = r#"
import regex
var rs = []
for i in 0..300 {
    let r = regex.new(r"\w{60}" + "{i}")?
    rs.push(r)
    let _ = r.find_all("é".repeat(100000))
}
print(rs.len())
"#;
    expect_stop(limits(|l| l.max_memory = Some(32 << 20)), src, is_limit(LimitKind::Memory), Duration::from_secs(10));
    // Programs over the size limit are refused.
    let (r, out) = run(Config::new(), "import regex\nprint(regex.new(r\"\\w{150}\").unwrap_err().kind)");
    r.unwrap();
    assert_eq!(out, "limit\n");
}

#[test]
fn compiling_a_regex_is_charged_for_the_program_it_builds() {
    // About 3 MB of automata: far more work than the 7-byte pattern suggests.
    let src = "import regex\nfor i in 0..1000 { let _ = regex.new(r\"\\w{60}\" + \"{i}\") }";
    expect_stop(limits(|l| l.max_instructions = Some(2_000_000)), src, is_limit(LimitKind::Instructions), Duration::from_secs(5));
}
