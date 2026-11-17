//! Benchmarks for native functions that call Sand functions. Run with
//! `cargo bench --bench callbacks`. `cargo test --benches` runs each program
//! once and checks its output.
//!
//! Each benchmark calls `fn inc(x: int) -> int { x + 1 }` one million times
//! through a different caller. The harness reports the best wall time over
//! `BENCH_RUNS` runs (default 5), including compilation, and the time per call.

use sandlang::engine::{Config, Engine, HostModules, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::context::CallContext;
use sandlang::native::func::Later;
use sandlang::native::ty::{Param, VmVal};
use sandlang::vm::{Fail, WaitKind};
use std::time::{Duration, Instant};

const CALLS: u64 = 1_000_000;

struct Bench {
    name: &'static str,
    /// Sand expression that applies `inc` to 0 `CALLS` times.
    apply: &'static str,
}

const BENCHES: &[Bench] = &[
    Bench { name: "Sand call through a function value", apply: "apply(inc, 0, 1000000)" },
    Bench { name: "synchronous native call", apply: "calls.apply(inc, 0, 1000000)" },
    Bench { name: "async native call", apply: "calls.apply_async(inc, 0, 1000000)" },
];

const PRELUDE: &str = "import calls\n\
                       fn inc(x: int) -> int { x + 1 }\n\
                       fn apply<T>(f: fn(T) -> T, x: T, times: int) -> T {\n    var y = x\n    for _ in 0..times { y = f(y) }\n    y\n}\n";

/// Builds the `calls` module: `apply` calls Sand synchronously, and
/// `apply_async` calls Sand from an async native future.
fn calls_module() -> NativeModule {
    let mut module = NativeModule::new("calls");
    module.source(
        "pub native fn apply<T>(f: fn(T) -> T, x: T, times: int) -> T\n\
         pub native fn apply_async<T>(f: fn(T) -> T, x: T, times: int) -> T",
    );
    module.function(
        "apply",
        |context: &mut CallContext, sand_fn: VmVal<fn(Param<0>) -> Param<0>>, value: Param<0>, times: i64| -> Result<Param<0>, Fail> {
            let mut current = value.raw();
            for _ in 0..times {
                current = context.call(sand_fn.raw(), &[current])?;
            }
            Ok(Param::from_raw(current))
        },
    );
    module.function(
        "apply_async",
        |sand_fn: VmVal<fn(Param<0>) -> Param<0>>, value: Param<0>, times: i64| -> Result<Later<Param<0>, Fail>, Fail> {
            Ok(Later::new(WaitKind::Io, move |mut context| {
                let (sand_fn, mut current) = (context.root(sand_fn.raw()), context.root(value.raw()));
                async move {
                    for _ in 0..times {
                        current = context.call(sand_fn, &[current]).await;
                    }
                    Ok(Param::from_raw(context.value(current)))
                }
            }))
        },
    );
    module
}

fn run_once(src: &str) -> (Duration, String) {
    let out = MemorySink::new();
    let config = Config::new().stdout(out.clone()).host_modules(HostModules::new([calls_module()]).unwrap());
    let mut engine = Engine::new(config);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let start = Instant::now();
    rt.block_on(engine.run_source("bench.sand", src)).unwrap();
    (start.elapsed(), out.contents())
}

fn main() {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let runs = if quick { 1 } else { std::env::var("BENCH_RUNS").ok().and_then(|n| n.parse().ok()).unwrap_or(5usize).max(1) };
    println!("{:<38} {:>10} {:>8}", "benchmark", "best", "ns/call");
    for b in BENCHES.iter().filter(|b| filter.as_ref().is_none_or(|f| b.name.contains(f.as_str()))) {
        let src = format!("{PRELUDE}print({})", b.apply);
        let best = (0..runs)
            .map(|_| {
                let (time, out) = run_once(&src);
                assert_eq!(out, format!("{CALLS}\n"), "{}", b.name);
                time
            })
            .min()
            .unwrap();
        println!("{:<38} {:>8.1}ms {:>8.1}", b.name, best.as_secs_f64() * 1e3, best.as_secs_f64() * 1e9 / CALLS as f64);
    }
}
