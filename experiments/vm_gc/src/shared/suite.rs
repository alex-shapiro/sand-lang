//! The engine interface that every arm implements, and the B1 to B8 checks
//! that run against it.

use super::isa::Program;
use super::programs::{Variant, program};
use super::{Fail, Limits, Result};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

/// An argument that the harness passes to a bytecode function.
#[derive(Clone, Debug)]
pub enum Arg {
    Int(i64),
    Str(String),
}

/// A bytecode result converted for the harness.
#[derive(Clone, Debug, PartialEq)]
pub enum HostValue {
    Unit,
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Any other value, by kind name.
    Other(String),
}

/// Host state that the B8 host modules report to the harness.
#[derive(Clone, Default)]
pub struct HostConfig {
    /// Counts dropped B8 callback host objects.
    pub host_drops: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemStats {
    pub tracked_bytes: usize,
    pub live_objects: usize,
    pub collections: u64,
}

/// One engine of an arm.
pub trait ArmEngine: Sized + Send + 'static {
    const NAME: &'static str;

    fn new(program: Arc<Program>, limits: Limits, host: &HostConfig) -> Result<Self>;

    /// Calls the bytecode function `name` synchronously.
    fn call(&mut self, name: &str, args: &[Arg]) -> Result<HostValue>;

    /// Runs one full collection: mark-sweep in Arms 1 and 2, and dead `Weak`
    /// pruning plus cycle collection in Arm 3.
    fn collect(&mut self);

    fn stats(&self) -> MemStats;

    /// Pauses or resumes automatic collections.
    fn pause_gc(&mut self, paused: bool);

    /// Adds a fiber that runs the bytecode function `name`.
    fn spawn(&mut self, name: &str, args: &[Arg]) -> Result<()>;

    /// Runs every fiber to completion and returns their results in spawn order.
    fn run_fibers(&mut self) -> impl Future<Output = Vec<Result<HostValue>>> + Send + '_;
}

/// Runs `future` to completion on the current thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// Limits for steady-state runs: the GC floor sits above every working set.
pub fn roomy_limits() -> Limits {
    Limits { max_memory: 8 << 30, gc_min_threshold: 4 << 30, ..Limits::default() }
}

pub fn engine<E: ArmEngine>(variant: Variant, limits: Limits) -> E {
    E::new(program(variant), limits, &HostConfig::default()).unwrap_or_else(|e| panic!("{}: {e}", E::NAME))
}

pub fn int(i: i64) -> Arg {
    Arg::Int(i)
}

fn call_ok<E: ArmEngine>(e: &mut E, name: &str, args: &[Arg]) -> HostValue {
    e.call(name, args).unwrap_or_else(|err| panic!("{}: {name}: {err}", E::NAME))
}

fn call_str<E: ArmEngine>(e: &mut E, name: &str, args: &[Arg]) -> String {
    match call_ok(e, name, args) {
        HostValue::Str(s) => s,
        other => panic!("{}: {name}: expected a string, got {other:?}", E::NAME),
    }
}

/// Runs `f` on a thread with a 512 KB stack.
pub fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(512 << 10).spawn(f).expect("thread").join().expect("small-stack thread panicked")
}

// -------------------------------------------------------------------------
// B1
// -------------------------------------------------------------------------

/// The checksum that `build_walk(n)` returns.
pub fn build_walk_checksum(n: i64) -> i64 {
    (0..n)
        .map(|i| {
            let active = (i % 2 == 0) as i64;
            let high = (i as f64 * 0.5 > n as f64 * 0.25) as i64;
            active + 2 + i + "Springfield".len() as i64 + i % 3 + 2 * i + high
        })
        .sum()
}

/// The checksum that `micro(iters)` returns.
pub fn micro_checksum(iters: i64) -> i64 {
    iters + (0..iters).sum::<i64>() + 3 * iters + iters
}

pub fn b1_build_walk<E: ArmEngine>() {
    let mut e: E = engine(Variant::Structural, Limits::default());
    assert_eq!(call_ok(&mut e, "build_walk", &[int(300)]), HostValue::Int(build_walk_checksum(300)));
    assert_eq!(call_ok(&mut e, "micro", &[int(1000)]), HostValue::Int(micro_checksum(1000)));
}

pub fn b1_fuel<E: ArmEngine>() {
    let limits = Limits { fuel: 100_000, ..Limits::default() };
    let mut e: E = engine(Variant::Structural, limits);
    assert_eq!(e.call("spin", &[]), Err(Fail::OutOfFuel));
    assert_eq!(e.call("micro", &[int(1)]), Err(Fail::OutOfFuel), "fuel must not refill");

    let junk = format!("{{\"junk\": [{}0], \"a\": 7}}", "123456789,".repeat(200_000));
    let mut e: E = engine(Variant::Structural, Limits { fuel: 10_000, ..Limits::default() });
    assert_eq!(e.call("decode_small", &[Arg::Str(junk.clone())]), Err(Fail::OutOfFuel));
    let mut e: E = engine(Variant::Structural, Limits::default());
    assert_eq!(e.call("decode_small", &[Arg::Str(junk)]), Ok(HostValue::Int(7)));

    // Native work costs fuel per input byte, not per value: many short
    // unknown keys, long whitespace, and long strings all exhaust it.
    let keys: String = (0..100_000).map(|i| format!("\"k{i}\":0,")).collect();
    let spaces = format!("{}{{\"a\": 7}}", " ".repeat(2_000_000));
    for input in [format!("{{{keys}\"a\": 7}}"), spaces] {
        let mut e: E = engine(Variant::Structural, Limits { fuel: 10_000, ..Limits::default() });
        assert_eq!(e.call("decode_small", &[Arg::Str(input.clone())]), Err(Fail::OutOfFuel));
        let mut e: E = engine(Variant::Structural, Limits::default());
        assert_eq!(e.call("decode_small", &[Arg::Str(input)]), Ok(HostValue::Int(7)));
    }
    let mut e: E = engine(Variant::Structural, Limits { fuel: 10_000, ..Limits::default() });
    assert_eq!(e.call("huge_string", &[int(16 << 20)]), Err(Fail::OutOfFuel));
}

// -------------------------------------------------------------------------
// B2 and B3
// -------------------------------------------------------------------------

pub const ADDRESS_JSON: &str = r#"{"street":"1 Main St","city":"Springfield","zip":"01234"}"#;

pub const FIRST_USERS_JSON: &str = concat!(
    r#"{"users":[{"id":0,"name":"user 0","email":"u0@example.com","score":0.0,"active":true,"tags":["a","b0"],"#,
    r#""address":{"street":"0 Main St","city":"Springfield","zip":null},"status":"Active","logins":[0,1,2],"meta":{"k":0,"j":0}},"#,
    r#"{"id":1,"name":"user 1","email":"u1@example.com","score":0.5,"active":false,"tags":["a","b1"],"#,
    r#""address":{"street":"1 Main St","city":"Springfield","zip":"01"},"status":{"Suspended":{"reason":"r1"}},"logins":[1,2,3],"meta":{"k":1,"j":2}},"#,
    r#"{"id":2,"name":"user 2","email":"u2@example.com","score":1.0,"active":true,"tags":["a","b2"],"#,
    r#""address":{"street":"2 Main St","city":"Springfield","zip":"02"},"status":{"Banned":[2,"b"]},"logins":[2,3,4],"meta":{"k":2,"j":4}}]}"#
);

/// Checks encoding and decoding of the dataset with `variant`'s codec methods.
pub fn b2_b3_codec<E: ArmEngine>(variant: Variant) {
    let mut e: E = engine(variant, Limits::default());
    // Each custom `Address` and `Status` method call adds 1 to `callbacks`.
    let per_user = if variant == Variant::Custom { 2 } else { 0 };
    let per_address = if variant == Variant::Custom { 1 } else { 0 };
    call_ok(&mut e, "setup_doc", &[int(3)]);
    let before = callback_count(&mut e);
    assert_eq!(call_str(&mut e, "encode_doc", &[]), FIRST_USERS_JSON);
    assert_eq!(callback_count(&mut e), before + 3 * per_user, "{}: custom encode calls", E::NAME);
    call_ok(&mut e, "setup_address", &[]);
    assert_eq!(call_ok(&mut e, "bench_small_encode", &[int(10)]), HostValue::Int(10 * ADDRESS_JSON.len() as i64));
    let before = callback_count(&mut e);
    assert_eq!(call_ok(&mut e, "bench_small_decode", &[int(10)]), HostValue::Int(10 * "Springfield".len() as i64));
    assert_eq!(callback_count(&mut e), before + 10 * per_address, "{}: custom decode calls", E::NAME);
    if variant == Variant::Custom {
        assert_eq!(call_str(&mut e, "evil_encode", &[]), r#"[[1,"a long string that lives on the heap"]]"#);
    }

    call_ok(&mut e, "setup_text", &[int(500)]);
    call_ok(&mut e, "setup_doc", &[int(500)]);
    let text = call_str(&mut e, "encode_doc", &[]);
    assert_eq!(call_str(&mut e, "roundtrip_text", &[]), text);
    assert_eq!(call_ok(&mut e, "bench_decode", &[]), HostValue::Int(499));

    let doc_ty = program(variant).types.iter().position(|t| *t == super::isa::Ty::Struct(program(variant).struct_id("Doc").unwrap()));
    let doc_ty = doc_ty.expect("a Doc type operand") as i64;
    let reordered = FIRST_USERS_JSON.replace(
        r#""street":"1 Main St","city":"Springfield""#,
        r#""city":"Springfield", "extra": [1, {"x": null}], "street":"1 Main St""#,
    );
    assert_eq!(call_str(&mut e, "reencode", &[Arg::Str(reordered), int(doc_ty)]), FIRST_USERS_JSON);
    let escaped = FIRST_USERS_JSON.replace("user 1", r#"user \"1\""#);
    assert_eq!(call_str(&mut e, "reencode", &[Arg::Str(escaped), int(doc_ty)]), FIRST_USERS_JSON.replace("user 1", r#"user \"1\""#));
    let missing = FIRST_USERS_JSON.replace(r#""meta":{"k":0,"j":0}"#, r#""meta":{}"#);
    let without_zip = missing.replacen(r#","zip":null"#, "", 1);
    assert_eq!(call_str(&mut e, "reencode", &[Arg::Str(without_zip), int(doc_ty)]), missing);
    let broken = FIRST_USERS_JSON.replace(r#""Banned""#, r#""Bogus""#);
    assert!(e.call("reencode", &[Arg::Str(broken), int(doc_ty)]).is_err());
}

fn callback_count<E: ArmEngine>(e: &mut E) -> i64 {
    match call_ok(e, "callback_count", &[]) {
        HostValue::Int(n) => n,
        other => panic!("{}: callback_count returned {other:?}", E::NAME),
    }
}

/// Runs the codec, host, and async checks while every allocation collects
/// garbage, so a value that native code forgot to root fails the check.
pub fn gc_stress<E: ArmEngine>() {
    let stress = Limits { gc_stress: true, ..Limits::default() };
    for variant in [Variant::Structural, Variant::Custom] {
        let mut roomy: E = engine(variant, Limits::default());
        call_ok(&mut roomy, "setup_doc", &[int(20)]);
        let expected = call_str(&mut roomy, "encode_doc", &[]);
        let mut e: E = engine(variant, stress);
        call_ok(&mut e, "setup_doc", &[int(20)]);
        assert_eq!(call_str(&mut e, "encode_doc", &[]), expected);
        assert_eq!(call_str(&mut e, "roundtrip_text_of_doc", &[]), expected);
        assert!(matches!(call_ok(&mut e, "sexpr_roundtrip", &[]), HostValue::Int(n) if n > 1000));
        call_ok(&mut e, "setup_address", &[]);
        assert_eq!(call_ok(&mut e, "bench_small_decode", &[int(3)]), HostValue::Int(3 * "Springfield".len() as i64));
        assert_eq!(call_ok(&mut e, "map_mutating", &[]), HostValue::Int(12 * 100 + 3));
        if variant == Variant::Custom {
            assert_eq!(call_str(&mut e, "evil_encode", &[]), r#"[[1,"a long string that lives on the heap"]]"#);
        }
    }
    let host = HostConfig::default();
    let mut e = E::new(program(Variant::Structural), stress, &host).unwrap();
    call_ok(&mut e, "host_setup", &[]);
    assert_eq!(call_ok(&mut e, "host_fire_twice", &[]), HostValue::Int(13));
    let mut e: E = engine(Variant::Structural, stress);
    e.spawn("holder", &[int(7)]).unwrap();
    e.spawn("churn", &[int(20)]).unwrap();
    let results = block_on(e.run_fibers());
    assert_eq!(results[0], Ok(HostValue::Str(format!("user 7|u7@example.com|b2|{HELD_STRING}"))));
}

// -------------------------------------------------------------------------
// B4
// -------------------------------------------------------------------------

/// The hard cap for the B4 callback and oversized checks, and the user count of B4 documents.
pub const B4_MAX_MEMORY: usize = 48 << 20;
pub const B4_USERS: i64 = 5000;

/// The B2 and B3 work that B4 repeats under a cap.
fn b4_workload<E: ArmEngine>(e: &mut E) {
    call_ok(e, "setup_text", &[int(B4_USERS)]);
    assert_eq!(call_ok(e, "bench_decode", &[]), HostValue::Int(B4_USERS - 1));
    call_ok(e, "clear_out", &[]);
    call_ok(e, "setup_doc", &[int(B4_USERS)]);
    let encoded = call_ok(e, "bench_encode", &[]);
    assert!(matches!(encoded, HostValue::Int(n) if n > 1_000_000), "{encoded:?}");
}

/// Runs B2 and B3 under a cap after filling 70% of it with `generator`
/// garbage while automatic collections are paused. The cap is 1.5 times the
/// bytes that the workload tracks on an empty heap without collecting, so the
/// run must reclaim the garbage to finish.
pub fn b4_dead_garbage<E: ArmEngine>(generator: &str) -> Vec<B4Run> {
    let mut runs = Vec::new();
    for variant in [Variant::Structural, Variant::Custom] {
        let mut roomy: E = engine(variant, roomy_limits());
        b4_workload(&mut roomy);
        let cap = roomy.stats().tracked_bytes * 3 / 2;
        let limits = Limits { max_memory: cap, gc_min_threshold: cap, ..Limits::default() };
        let mut e: E = engine(variant, limits);
        e.pause_gc(true);
        while e.stats().tracked_bytes < cap * 7 / 10 {
            call_ok(&mut e, generator, &[int(1000)]);
        }
        e.pause_gc(false);
        let before = e.stats().collections;
        let start = std::time::Instant::now();
        b4_workload(&mut e);
        let elapsed = start.elapsed();
        let collections = e.stats().collections - before;
        assert!(collections > 0, "{}: the run must reclaim the garbage", E::NAME);
        assert!(e.stats().tracked_bytes <= cap);
        runs.push(B4Run { variant, cap, collections, elapsed });
    }
    runs
}

/// What one B4 run under a cap needed.
#[derive(Debug)]
pub struct B4Run {
    pub variant: Variant,
    pub cap: usize,
    pub collections: u64,
    pub elapsed: std::time::Duration,
}

/// Runs a decode whose `Address.decode` allocates garbage in a loop.
pub fn b4_garbage_callback<E: ArmEngine>() -> B4Run {
    let limits = Limits { max_memory: B4_MAX_MEMORY, gc_min_threshold: 4 << 20, ..Limits::default() };
    let mut e: E = engine(Variant::GarbageCallback, limits);
    call_ok(&mut e, "setup_text", &[int(B4_USERS)]);
    let before = e.stats().collections;
    let start = std::time::Instant::now();
    assert_eq!(call_ok(&mut e, "bench_decode", &[]), HostValue::Int(B4_USERS - 1));
    let run = B4Run {
        variant: Variant::GarbageCallback,
        cap: B4_MAX_MEMORY,
        collections: e.stats().collections - before,
        elapsed: start.elapsed(),
    };
    assert!(run.collections > 0, "{}: garbage must be collected inside the decode call", E::NAME);
    call_ok(&mut e, "setup_doc", &[int(B4_USERS)]);
    let text = call_str(&mut e, "encode_doc", &[]);
    assert_eq!(call_str(&mut e, "roundtrip_text", &[]), text);
    run
}

/// Rejects allocations larger than the cap before memory grows.
pub fn b4_oversized<E: ArmEngine>() {
    let limits = Limits { max_memory: B4_MAX_MEMORY, gc_min_threshold: 4 << 20, ..Limits::default() };
    let mut e: E = engine(Variant::Structural, limits);
    call_ok(&mut e, "huge_string", &[int(1000)]);
    call_ok(&mut e, "huge_list", &[int(10)]);
    let before = e.stats().tracked_bytes;
    assert_eq!(e.call("huge_string", &[int(B4_MAX_MEMORY as i64 + 1)]), Err(Fail::OutOfMemory));
    assert_eq!(e.call("huge_list", &[int((B4_MAX_MEMORY / 16) as i64 + 1)]), Err(Fail::OutOfMemory));
    assert_eq!(e.call("huge_list", &[int(i64::MAX / 2)]), Err(Fail::OutOfMemory));
    // The byte count of this string overflows `usize`.
    assert_eq!(e.call("huge_pair_string", &[int(i64::MAX)]), Err(Fail::OutOfMemory));
    assert!(e.stats().tracked_bytes <= before + 4096, "{} > {before}", e.stats().tracked_bytes);
    assert_eq!(call_ok(&mut e, "huge_string", &[int(1000)]), HostValue::Int(1000));
}

// -------------------------------------------------------------------------
// B5
// -------------------------------------------------------------------------

pub fn deep_node_json(depth: usize) -> String {
    let mut s = String::with_capacity(depth * 24);
    for i in (0..depth).rev() {
        s.push_str(&format!("{{\"value\":{i},\"next\":"));
    }
    s.push_str("null");
    s.push_str(&"}".repeat(depth));
    s
}

/// Encodes, decodes, and drops 500-level structural nodes on a 512 KB stack.
pub fn b5_structural<E: ArmEngine>() {
    on_small_stack(|| {
        let mut e: E = engine(Variant::Structural, Limits::default());
        assert_eq!(call_ok(&mut e, "node_roundtrip", &[int(500)]), HostValue::Int(500));
        assert_eq!(call_ok(&mut e, "node_decode", &[Arg::Str(deep_node_json(500))]), HostValue::Int(500));
        // Dropping a much deeper list or host-object chain must not recurse.
        assert_eq!(call_ok(&mut e, "node_depth_of_built", &[int(200_000)]), HostValue::Int(200_000));
        call_ok(&mut e, "host_chain", &[int(200_000)]);
        e.collect();
    });
}

/// Nested custom callbacks stop with a stack-overflow error on a 512 KB stack.
pub fn b5_callbacks<E: ArmEngine>() {
    on_small_stack(|| {
        let mut e: E = engine(Variant::NodeCustom, Limits::default());
        assert_eq!(call_ok(&mut e, "node_roundtrip", &[int(50)]), HostValue::Int(50));
        // The structural walker handles 500 levels, so these overflows come from the callbacks.
        assert_eq!(e.call("node_encode", &[int(400)]), Err(Fail::StackOverflow));
        assert_eq!(e.call("node_decode", &[Arg::Str(deep_node_json(400))]), Err(Fail::StackOverflow));
        assert_eq!(e.call("node_encode", &[int(100_000)]), Err(Fail::StackOverflow));
        assert_eq!(e.call("node_decode", &[Arg::Str(deep_node_json(100_000))]), Err(Fail::StackOverflow));
        assert_eq!(call_ok(&mut e, "node_roundtrip", &[int(50)]), HostValue::Int(50));
    });
}

// -------------------------------------------------------------------------
// B6
// -------------------------------------------------------------------------

pub const HELD_STRING: &str = "a long string held across await";

pub fn b6_async<E: ArmEngine>() {
    fn assert_send<T: Send>(_: &T) {}
    let mut e: E = engine(Variant::Structural, Limits { gc_min_threshold: 64 << 10, ..Limits::default() });
    e.spawn("holder", &[int(7)]).unwrap();
    e.spawn("churn", &[int(2000)]).unwrap();
    let before = e.stats().collections;
    let future = e.run_fibers();
    assert_send(&future);
    let results = block_on(future);
    assert_eq!(results[0], Ok(HostValue::Str(format!("user 7|u7@example.com|b2|{HELD_STRING}"))));
    assert_eq!(results[1], Ok(HostValue::Unit));
    assert!(e.stats().collections > before + 1, "fiber 2 must collect while fiber 1 waits");
    assert_send(&e);

    // Finished fibers return their stack bytes.
    e.collect();
    let settled = e.stats().tracked_bytes;
    for _ in 0..50 {
        e.spawn("holder", &[int(7)]).unwrap();
        e.spawn("churn", &[int(2)]).unwrap();
        block_on(e.run_fibers());
    }
    e.collect();
    assert!(e.stats().tracked_bytes <= settled + 4096, "{}: {} > {settled}", E::NAME, e.stats().tracked_bytes);
}

// -------------------------------------------------------------------------
// B8
// -------------------------------------------------------------------------

pub fn b8_list_map<E: ArmEngine>() {
    let mut e: E = engine(Variant::Structural, Limits { gc_min_threshold: 64 << 10, ..Limits::default() });
    // Mapped [2, 4, 6] sum to 12; the closure shrinks the input to 3 items.
    assert_eq!(call_ok(&mut e, "map_mutating", &[]), HostValue::Int(12 * 100 + 3));
}

pub fn b8_sexpr<E: ArmEngine>() {
    let mut e: E = engine(Variant::Structural, Limits::default());
    assert_eq!(call_str(&mut e, "sexpr_text", &[]), r#"(:"street" "1 Main St" :"city" "Springfield" :"zip" "01234")"#);
    call_ok(&mut e, "setup_doc", &[int(200)]);
    assert!(matches!(call_ok(&mut e, "sexpr_roundtrip", &[]), HostValue::Int(n) if n > 10_000));
    let mut e: E = engine(Variant::Custom, Limits::default());
    call_ok(&mut e, "setup_doc", &[int(200)]);
    assert!(matches!(call_ok(&mut e, "sexpr_roundtrip", &[]), HostValue::Int(n) if n > 10_000));
}

pub fn b8_host_cycle<E: ArmEngine>() {
    let host = HostConfig::default();
    let mut e = E::new(program(Variant::Structural), Limits::default(), &host).unwrap();
    call_ok(&mut e, "host_setup", &[]);
    // Only the host object keeps the closure alive across this collection.
    e.collect();
    // fire(5) twice: 5 + 1 call, then 5 + 2 calls.
    assert_eq!(call_ok(&mut e, "host_fire_twice", &[]), HostValue::Int(13));
    assert_eq!(host.host_drops.load(Ordering::Relaxed), 0);
    call_ok(&mut e, "host_release", &[]);
    e.collect();
    assert_eq!(host.host_drops.load(Ordering::Relaxed), 1, "{}: the host object and its callback form a garbage cycle", E::NAME);
}

pub fn b8_two_engines<E: ArmEngine>() {
    let mut a: E = engine(Variant::Structural, Limits { gc_min_threshold: 64 << 10, ..Limits::default() });
    let mut b: E = engine(Variant::Custom, Limits { gc_min_threshold: 64 << 10, ..Limits::default() });
    call_ok(&mut a, "setup_doc", &[int(300)]);
    call_ok(&mut b, "setup_doc", &[int(301)]);
    for _ in 0..3 {
        let ta = call_str(&mut a, "encode_doc", &[]);
        call_ok(&mut b, "build_walk", &[int(200)]);
        let tb = call_str(&mut b, "encode_doc", &[]);
        call_ok(&mut a, "build_walk", &[int(200)]);
        assert!(tb.len() > ta.len());
        assert_eq!(call_str(&mut a, "roundtrip_text_of_doc", &[]), ta);
        assert_eq!(call_str(&mut b, "roundtrip_text_of_doc", &[]), tb);
    }
    let threads =
        [std::thread::spawn(move || call_str(&mut a, "encode_doc", &[])), std::thread::spawn(move || call_str(&mut b, "encode_doc", &[]))];
    let [ta, tb] = threads.map(|t| t.join().unwrap());
    assert!(tb.len() > ta.len());
}

/// Generates one `#[test]` per check for an arm.
#[macro_export]
macro_rules! arm_tests {
    ($engine:ty) => {
        use $crate::shared::programs::Variant;
        use $crate::shared::suite;
        #[test]
        fn b1_build_walk() {
            suite::b1_build_walk::<$engine>();
        }
        #[test]
        fn b1_fuel() {
            suite::b1_fuel::<$engine>();
        }
        #[test]
        fn b2_structural_codec() {
            suite::b2_b3_codec::<$engine>(Variant::Structural);
        }
        #[test]
        fn b3_custom_codec() {
            suite::b2_b3_codec::<$engine>(Variant::Custom);
        }
        #[test]
        fn b4_dead_acyclic() {
            suite::b4_dead_garbage::<$engine>("make_garbage");
        }
        #[test]
        fn b4_dead_cycles() {
            suite::b4_dead_garbage::<$engine>("make_cycles");
        }
        #[test]
        fn b4_garbage_callback() {
            suite::b4_garbage_callback::<$engine>();
        }
        #[test]
        fn b4_oversized() {
            suite::b4_oversized::<$engine>();
        }
        #[test]
        fn b5_structural() {
            suite::b5_structural::<$engine>();
        }
        #[test]
        fn b5_callbacks() {
            suite::b5_callbacks::<$engine>();
        }
        #[test]
        fn b6_async() {
            suite::b6_async::<$engine>();
        }
        #[test]
        fn b8_list_map() {
            suite::b8_list_map::<$engine>();
        }
        #[test]
        fn b8_sexpr() {
            suite::b8_sexpr::<$engine>();
        }
        #[test]
        fn b8_host_cycle() {
            suite::b8_host_cycle::<$engine>();
        }
        #[test]
        fn gc_stress() {
            suite::gc_stress::<$engine>();
        }
        #[test]
        fn b8_two_engines() {
            suite::b8_two_engines::<$engine>();
        }
    };
}
