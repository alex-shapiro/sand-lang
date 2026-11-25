//! Benchmarks for native functions that call Sand functions.
//!
//! `cargo bench --bench callbacks` runs 1,000,000 calls to `inc` per path and
//! reports the fastest run, including compilation. Set `BENCH_RUNS` to change
//! the default five runs. `cargo test --benches` checks output once.

use sandlang::engine::{Config, Engine, HostModules, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::context::CallContext;
use sandlang::native::func::Later;
use sandlang::native::ty::{Param, VmVal};
use sandlang::vm::{Fail, WaitKind};
use std::time::{Duration, Instant};

const CALL_COUNT: u64 = 1_000_000;

struct Bench {
    name: &'static str,
    /// Sand expression that applies `inc` `CALL_COUNT` times to `0`.
    call_expr: &'static str,
}

const BENCHES: &[Bench] = &[
    Bench { name: "Sand call through a function value", call_expr: "apply(inc, 0, 1000000)" },
    Bench { name: "synchronous native call", call_expr: "calls.apply(inc, 0, 1000000)" },
    Bench { name: "async native call", call_expr: "calls.apply_async(inc, 0, 1000000)" },
];

const SHARED_SOURCE: &str = "import calls\n\
                             fn inc(x: int) -> int { x + 1 }\n\
                             fn apply<T>(f: fn(T) -> T, x: T, times: int) -> T {\n    var y = x\n    for _ in 0..times { y = f(y) }\n    y\n}\n";

/// Builds the `calls` host module with synchronous and async callback functions.
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

fn run_once(source: &str) -> (Duration, String) {
    let stdout = MemorySink::new();
    let config = Config::new().stdout(stdout.clone()).host_modules(HostModules::new([calls_module()]).unwrap());
    let mut engine = Engine::new(config);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let start = Instant::now();
    runtime.block_on(engine.run_source("bench.sand", source)).unwrap();
    (start.elapsed(), stdout.contents())
}

fn main() {
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let is_quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|arg| arg == "--bench");
    let run_count =
        if is_quick { 1 } else { std::env::var("BENCH_RUNS").ok().and_then(|env_value| env_value.parse().ok()).unwrap_or(5usize).max(1) };
    println!("{:<38} {:>10} {:>8}", "benchmark", "best", "ns/call");
    for bench in BENCHES.iter().filter(|bench| filter.as_ref().is_none_or(|pattern| bench.name.contains(pattern.as_str()))) {
        let source = format!("{SHARED_SOURCE}print({})", bench.call_expr);
        let min_elapsed = (0..run_count)
            .map(|_| {
                let (elapsed, stdout) = run_once(&source);
                assert_eq!(stdout, format!("{CALL_COUNT}\n"), "{}", bench.name);
                elapsed
            })
            .min()
            .unwrap();
        println!(
            "{:<38} {:>8.1}ms {:>8.1}",
            bench.name,
            min_elapsed.as_secs_f64() * 1e3,
            min_elapsed.as_secs_f64() * 1e9 / CALL_COUNT as f64
        );
    }
}
