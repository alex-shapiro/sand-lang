//! Metering of native work and heavy operations: natives charge fuel, so
//! `max_instructions`, `max_cpu_time`, `max_wall_time` and the interrupt
//! handle apply while they run, and Rust-side scratch space counts against
//! `max_memory`.

mod common;

use common::{rt, run_with};
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
fn long_straight_line_code_costs_fuel() {
    // A loop iteration or a call used to cost one unit however much
    // straight-line code it ran, so a loop body or function of a few hundred
    // thousand statements ran ~10^5 times longer per unit than the cost
    // model says (and the scheduler checked the time limits and the
    // interrupt flag only every few seconds). Each case prints its
    // iteration count; 10,000 statements must cost far more than one unit.
    let stmts = "    x += 1\n".repeat(10_000);
    let lets: String = (0..10_000).map(|i| format!("    let a{i} = {i}\n")).collect();
    let caps: Vec<String> = (0..10_000).map(|i| format!("a{i}")).collect();
    let cases = [
        // A long loop body.
        format!("var n = 0\nwhile true {{\n    var x = n\n{stmts}    n += 1\n    print(n)\n}}"),
        // ... that ends with `continue`.
        format!("var n = 0\nwhile true {{\n    var x = n\n{stmts}    n += 1\n    print(n)\n    continue\n}}"),
        // A long function called in a short loop.
        format!(
            "fn f(y: int) -> int {{\n    var x = y\n{stmts}    x\n}}\nvar n = 0\nwhile true {{\n    n += 1\n    let _ = f(n)\n    print(n)\n}}"
        ),
        // A long closure.
        format!(
            "let f = (y: int) => {{\n    var x = y\n{stmts}    x\n}}\nvar n = 0\nwhile true {{\n    n += 1\n    let _ = f(n)\n    print(n)\n}}"
        ),
        // A long task body.
        format!(
            "var n = 0\nwhile true {{\n    n += 1\n    let t = spawn {{\n        var x = 0\n{stmts}        x\n    }}\n    let _ = t.wait()\n    print(n)\n}}"
        ),
        // A closure capturing many locals (one instruction copies them all).
        format!(
            "fn main() {{\n{lets}    var n = 0\n    while true {{\n        let f = () => [{}]\n        n += 1\n        print(n)\n    }}\n}}\nmain()",
            caps.join(", ")
        ),
    ];
    for src in &cases {
        let start = Instant::now();
        let (r, out) = run(limits(|l| l.max_instructions = Some(20_000)), src);
        assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
        let n: u64 = out.lines().last().map_or(0, |l| l.parse().unwrap());
        // About 10,000 / 64 units per iteration: some 130 iterations (it
        // used to be 20,000).
        assert!(n < 500, "{n} iterations of {} for 20,000 units", &src[..60]);
        assert!(start.elapsed() < Duration::from_secs(30), "took {:?}", start.elapsed());
    }
}

#[test]
fn appending_to_a_string_is_charged_for_its_copies() {
    // `s += x` appends in place to a private buffer, but after `s` has been
    // read (frozen) the next append copies all of `s`: that copy must be
    // charged, not just the appended text (it was: 16 GB copied for 2,000
    // units).
    let config = || limits(|l| l.max_instructions = Some(1_000_000));
    for append in ["s += \"y\"", "s += s[0..1]"] {
        let src = format!("var s = \"x\".repeat(8000000)\nvar n = 0\nfor i in 0..2000 {{\n    {append}\n    n += s.len()\n}}\nprint(n)");
        expect_stop(config(), &src, is_limit(LimitKind::Instructions), Duration::from_secs(30));
    }
    // Appending to a string that is not read meanwhile stays cheap.
    let src = "var s = \"x\".repeat(8000000)\nfor i in 0..100000 { s += \"y\" }\nprint(s.len())";
    let (r, out) = run(config(), src);
    r.unwrap();
    assert_eq!(out, "8100000\n");
}

#[test]
fn sending_to_a_channel_with_many_receivers_is_cheap() {
    // A send used to copy the channel's whole list of waiting receivers, so
    // with thousands of them each send did thousands of steps for a unit.
    let time = |receivers: u32| {
        let src = format!(
            "let ch: Channel<int> = task.channel()\nvar ts = []\nfor i in 0..{receivers} {{ ts.push(spawn {{ while true {{ let _ = ch.recv() }} }}) }}\n\
             task.yield_now()\nfor i in 0..300000 {{\n    ch.send(i)\n    task.yield_now()\n}}\nprint(\"done\")"
        );
        let start = Instant::now();
        let (r, out) = run(Config::new(), &src);
        r.unwrap();
        assert_eq!(out, "done\n");
        start.elapsed()
    };
    let (few, many) = (time(10), time(9000));
    assert!(many < few * 3 + Duration::from_millis(300), "10 receivers: {few:?}, 9,000: {many:?}");
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

#[test]
fn keys_chosen_to_collide_do_not_slow_down_maps() {
    // Map and set operations cost a unit or so each, which assumes that
    // their keys spread over the hash table. With a fixed multiplicative
    // hash, keys that differ only in their high bits (here multiples of
    // 2^32), or any keys computed offline to collide, all landed in one
    // probe sequence, so each operation scanned all the others.
    let time = |key: &str, ty: &str| {
        let (add, find) = (key.replace('X', "j"), key.replace('X', "(i + 100000)"));
        let src = format!(
            "var m: [{ty}: int] = [:]\nvar s: Set<{ty}> = set([])\nfor j in 0..100000 {{\n    m[{add}] = j\n    s.add({add})\n}}\n\
             var n = 0\nfor i in 0..200000 {{ if m.contains({find}) or s.contains({find}) {{ n += 1 }} }}\nprint(m.len() + n)"
        );
        let start = Instant::now();
        let (r, out) = run(Config::new(), &src);
        r.unwrap();
        assert_eq!(out, "100000\n");
        start.elapsed()
    };
    let plain = time("X * 7", "int");
    for (key, ty) in [("X * 4294967296", "int"), ("(X * 4294967296, 1)", "(int, int)")] {
        let t = time(key, ty);
        assert!(t < plain * 4 + Duration::from_millis(300), "{key}: {t:?}, X * 7: {plain:?}");
    }
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
fn comparing_shared_structures_counts_the_pairs_it_remembers() {
    // Comparison remembers each pair of mutable containers it has compared
    // (so that shared and cyclic structures end). Two DAGs of 2,000 small
    // structs per level, each pointing at two random ones of the next level,
    // have up to 4,000,000 such pairs per level for a heap of a few MB: the
    // set of pairs grew by gigabytes, far beyond the memory limit, for as
    // long as the time limits allowed.
    let src = "import random\nstruct N { kids: [N] }\nfn build(levels: int, k: int) -> N {\n    var next: [N] = (0..k).map(_ => N { kids: [] })\n    \
               for d in 0..levels {\n        var cur: [N] = []\n        for i in 0..k { cur.push(N { kids: [next[random.int(0, k - 1)], next[random.int(0, k - 1)]] }) }\n        \
               next = cur\n    }\n    N { kids: next }\n}\nlet a = build(60, 2000)\nlet b = build(60, 2000)\n";
    for cmp in ["a == b", "a < b"] {
        let src = format!("{src}print({cmp})");
        expect_stop(mem_limit(64), &src, is_limit(LimitKind::Memory), Duration::from_secs(60));
    }
}

#[test]
fn completed_io_is_charged() {
    // The values of completed I/O are built between slices (by finishers),
    // which charge the meter for it: that fuel used to be lost when the
    // next slice started, so reading a big file cost a few units.
    let src = "import fs\nfs.write_text(\"big.txt\", \"x\".repeat(4194304))?\nvar n = 0\nfor i in 0..300 { n += fs.read_text(\"big.txt\")?.len() }\nprint(n)";
    let r = run_with(src, |c| c.limits(Limits { max_instructions: Some(1_000_000), ..Limits::default() }));
    assert!(matches!(r.result, Err(Error::Limit(LimitKind::Instructions))), "{:?}", r.result);
}

#[test]
fn slices_are_checked_before_they_are_copied() {
    // A slice copies up to the whole list or string: like `+`, it must
    // check the memory first (it used to allocate the copy, doubling the
    // host's memory beyond the limit, and only the next collection noticed).
    for src in ["let xs = (0..2500000).to_list()\nprint(xs[1..].len())", "let s = \"x\".repeat(40000000)\nprint(s[1..].len())"] {
        expect_stop(mem_limit(64), src, is_limit(LimitKind::Memory), Duration::from_secs(20));
    }
    let (r, out) = run(mem_limit(64), "let xs = (0..1000000).to_list()\nprint(xs[1..].len())");
    r.unwrap();
    assert_eq!(out, "999999\n");
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

// ---------------------------------------------------------------------------
// Audit: string natives and host conversions

#[test]
fn string_natives_are_charged_for_every_argument_they_scan() {
    // These natives charged for scanning `self` only, but searching for a
    // needle (or comparing a prefix, or copying a separator or fill) costs
    // time in the size of the other argument too: a call on a one-character
    // string with a 10 MB argument did milliseconds of work for no fuel.
    let pre = "let big = \"x\".repeat(10000000)\nlet big_y = big + \"y\"\nlet y_big = \"y\" + big\nvar n = 0\n";
    for call in [
        "\"a\".find(big).is_some()",
        "\"a\".rfind(big).is_some()",
        "\"a\".contains(big)",
        "\"a\".replace(big, \"b\").len() > 5",
        "\"a\".split(big).len() > 5",
        "\"a\".split_once(big).is_some()",
        "big_y.strip_prefix(big).is_none()",
        "y_big.strip_suffix(big).is_none()",
    ] {
        // 20 calls: quick even when uncharged, but far over the budget.
        let src = format!("{pre}for i in 0..20 {{\n    if {call} {{ n += 1 }}\n}}\nprint(n)");
        expect_stop(limits(|l| l.max_instructions = Some(1_000_000)), &src, is_limit(LimitKind::Instructions), Duration::from_secs(30));
    }
    // Padding reads only as much of the fill as it needs (it used to copy
    // all of it, uncharged).
    let src = format!(
        "{pre}for i in 0..1000 {{\n    if \"a\".pad_start(3, big) == \"xxa\" and \"a\".pad_end(0, big) == \"a\" {{ n += 1 }}\n}}\nprint(n)"
    );
    let (r, out) = run(limits(|l| l.max_instructions = Some(1_000_000)), &src);
    r.unwrap();
    assert_eq!(out, "1000\n");
    // Ordinary uses keep their results.
    let src = "print((\"abcabc\".find(\"ca\"), \"abcabc\".rfind(\"bc\"), \"a-b-c\".split(\"-\"), \"key=v\".split_once(\"=\"), \
               \"prefix.x\".strip_prefix(\"prefix.\"), \"x.suffix\".strip_suffix(\".suffix\"), \"7\".pad_start(3, \"0\"), \"ab\".replace(\"b\", \"c\")))";
    let (r, out) = run(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "(Some(2), Some(4), [\"a\", \"b\", \"c\"], Some((\"key\", \"v\")), Some(\"x\"), Some(\"x\"), \"007\", \"ac\")\n");
}

#[test]
fn messages_quote_only_part_of_a_huge_argument() {
    // Panics and errors that quoted a whole script string (debug-escaped,
    // up to 6 bytes per byte) built messages far larger than the string,
    // outside the memory limit.
    let big = "let s = \"\\u{1}\".repeat(20000000)\n";
    let config = || limits(|l| l.max_memory = Some(64 << 20));
    for src in ["print(s.ord())", "import log\nlog.set_level(s)"] {
        let (r, _) = run(config(), &format!("{big}{src}"));
        match r {
            Err(Error::Runtime { message, .. }) => assert!(message.len() < 1000, "{} bytes", message.len()),
            other => panic!("{src}: {other:?}"),
        }
    }
    for call in ["time.parse(s)", "http.get(s)", "net.connect(s)"] {
        let module = call.split('.').next().unwrap();
        let src = format!("import {module}\n{big}match {call} {{\n    Ok(_) => print(-1),\n    Err(e) => print(e.message.len()),\n}}");
        let (r, out) = run(config(), &src);
        r.unwrap();
        let n: usize = out.trim().parse().unwrap();
        assert!(n < 2000, "{call}: {n} bytes");
    }
}

#[test]
fn host_function_arguments_count_against_the_memory_limit() {
    // A typed host function receives Rust copies of its arguments: a list
    // of 100 references to one 10 MB string is 1 GB of `String`s, which
    // must not fit under a 64 MB limit.
    let mut m = NativeModule::new("host");
    m.function("total", |xs: Vec<String>| -> i64 { xs.iter().map(|x| x.len() as i64).sum() });
    m.function("total_bytes", |xs: Vec<Vec<u8>>| -> i64 { xs.iter().map(|x| x.len() as i64).sum() });
    let config = || limits(|l| l.max_memory = Some(64 << 20)).module(m.clone());
    for src in [
        "import host\nlet s = \"x\".repeat(10000000)\nprint(host.total((0..100).map(_ => s)))",
        "import host\nlet b = \"x\".repeat(10000000).bytes()\nprint(host.total_bytes((0..100).map(_ => b)))",
    ] {
        expect_stop(config(), src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    }
    // Within the limit it works.
    let (r, out) = run(config(), "import host\nlet s = \"x\".repeat(1000000)\nprint(host.total((0..10).map(_ => s)))");
    r.unwrap();
    assert_eq!(out, "10000000\n");
}

// ---------------------------------------------------------------------------
// Audit: JSON and regex natives

#[test]
fn json_encode_charges_for_nested_options() {
    // `Some(Some(...(x)))` encodes as `x`: unwrapping a chain of 2,000 levels
    // for each of 100,000 elements used to cost one unit per element.
    let src = "import json\nfn deep<T>(x: T, n: int) -> int {\n    if n == 0 {\n        let xs = (0..100000).map(_ => x)\n        return json.encode(xs).len()\n    }\n    deep(Some(x), n - 1)\n}\nprint(deep(1, 2000))";
    expect_stop(limits(|l| l.max_instructions = Some(2_000_000)), src, is_limit(LimitKind::Instructions), Duration::from_secs(30));
}

#[test]
fn json_encode_reserves_escaped_strings() {
    // Control characters are escaped as `\u00XX`, six bytes each: 15 MB of
    // them make 90 MB of JSON, which must not be built under a 64 MB limit
    // (only twice the string's length used to be checked).
    let src = "import json\nlet s = \"\\u{1}\".repeat(15000000)\nprint(json.encode(s).len())";
    expect_stop(limits(|l| l.max_memory = Some(64 << 20)), src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    // The same for map keys.
    let src = "import json\nlet m = [\"\\u{1}\".repeat(15000000): 1]\nprint(json.encode(m).len())";
    expect_stop(limits(|l| l.max_memory = Some(64 << 20)), src, is_limit(LimitKind::Memory), Duration::from_secs(30));
}

#[test]
fn json_decode_charges_for_field_lookups() {
    // Each key of an object is looked up among the struct's fields: with
    // 1,000 fields and 200,000 unknown keys that is 200 million comparisons,
    // which used to cost about a unit per key.
    let fields: String = (0..1000).map(|i| format!("    f{i}: int?\n")).collect();
    let src = format!(
        "import json\nstruct Big {{\n{fields}}}\nlet t = \"\\{{\" + \"\\\"f999x\\\":0,\".repeat(200000) + \"\\\"f0\\\":1\\}}\"\nlet r: Result<Big> = json.decode(t)\nprint(r.is_ok())"
    );
    expect_stop(limits(|l| l.max_instructions = Some(3_000_000)), &src, is_limit(LimitKind::Instructions), Duration::from_secs(30));
}

#[test]
fn json_parse_counts_the_lists_it_builds() {
    // 9 million numbers are inline values (no heap objects), so only the
    // list being built grows: 144 MB or more of Rust-side buffer for 18 MB
    // of text. It is counted as it grows, so the parse stops at the limit
    // instead of after the host has allocated several times the limit.
    for src in [
        "import json\nlet t = \"[\" + \"0,\".repeat(9000000) + \"0]\"\nprint(json.parse(t).is_ok())",
        "import json\nlet t = \"[\" + \"0,\".repeat(9000000) + \"0]\"\nlet r: Result<[int]> = json.decode(t)\nprint(r.is_ok())",
    ] {
        expect_stop(limits(|l| l.max_memory = Some(64 << 20)), src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    }
}

#[test]
fn regex_captures_reserve_their_groups() {
    // 100 nested groups each matching the whole 2 MB text: 200 MB of group
    // texts, copied without any check under a 32 MB limit.
    let src = "import regex\nlet r = regex.new(\"(\".repeat(100) + \"a*\" + \")\".repeat(100))?\nlet s = \"a\".repeat(2000000)\nprint(r.captures(s).is_some())";
    expect_stop(limits(|l| l.max_memory = Some(32 << 20)), src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    let src = src.replace("r.captures(s).is_some()", "r.captures_all(s).len()");
    expect_stop(limits(|l| l.max_memory = Some(32 << 20)), &src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    // Ordinary captures still work.
    let src = "import regex\nlet r = regex.new(r\"(\\w+)@(\\w+)\")?\nlet c = r.captures(\"mail: joe@example now\").unwrap()\nprint((c.text(1), c.text(2), r.captures_all(\"a@b c@d\").len()))";
    let (r, out) = run(limits(|l| l.max_memory = Some(32 << 20)), src);
    r.unwrap();
    assert_eq!(out, "(Some(\"joe\"), Some(\"example\"), 2)\n");
}

#[test]
fn regex_replacements_are_reserved_before_they_are_expanded() {
    // Each `$0` repeats the whole match: one match of 200,000 bytes and a
    // replacement of 1,000 `$0`s expand to 200 MB, which used to be built
    // before the memory limit was checked.
    let src = "import regex\nlet r = regex.new(\"a*\")?\nlet s = \"a\".repeat(200000)\nprint(r.replace(s, \"$0\".repeat(1000)).len())";
    expect_stop(limits(|l| l.max_memory = Some(32 << 20)), src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    let src = src.replace("r.replace(", "r.replace_all(");
    expect_stop(limits(|l| l.max_memory = Some(32 << 20)), &src, is_limit(LimitKind::Memory), Duration::from_secs(30));
    // Ordinary replacements are unchanged.
    let src = "import regex\nlet r = regex.new(r\"(?<w>\\w+)\")?\nprint((r.replace_all(\"ab cd\", \"<$w$1>\"), r.replace(\"ab cd\", \"[$0]\"), r.replace_all(\"ab\", \"x\")))";
    let (r, out) = run(Config::new(), src);
    r.unwrap();
    assert_eq!(out, "(\"<abab> <cdcd>\", \"[ab] cd\", \"x\")\n");
}
