//! The B1 to B3 timing runs and the B7 footprint measurement.
//!
//! Each operation runs in `runs` fresh engines for `reps` repetitions. A
//! row reports the fastest operation time, the fastest cleanup time, and the
//! fastest combined time of one repetition. Cleanup clears the kept output
//! and runs one full collection.

use super::programs::Variant;
use super::suite::{Arg, ArmEngine, HostValue, engine, int, roomy_limits};
use std::time::{Duration, Instant};

pub const USERS: i64 = 5000;
pub const SMALL_ITERS: i64 = 20_000;
pub const MICRO_ITERS: i64 = 1_000_000;

#[derive(Clone, Debug)]
pub struct Row {
    pub group: &'static str,
    pub name: &'static str,
    /// Items per operation, for per-item times.
    pub items: i64,
    pub op: Duration,
    pub cleanup: Option<Duration>,
    pub total: Duration,
}

impl Row {
    pub fn per_item(&self, d: Duration) -> f64 {
        d.as_nanos() as f64 / self.items as f64
    }
}

struct Operation {
    group: &'static str,
    name: &'static str,
    variant: Variant,
    setup: &'static [(&'static str, Option<i64>)],
    function: &'static str,
    arg: Option<i64>,
    items: i64,
    cleanup: bool,
}

const OPERATIONS: &[Operation] = &[
    Operation {
        group: "B1",
        name: "build_walk",
        variant: Variant::Structural,
        setup: &[],
        function: "build_walk",
        arg: Some(USERS),
        items: USERS,
        cleanup: true,
    },
    Operation {
        group: "B1",
        name: "micro",
        variant: Variant::Structural,
        setup: &[],
        function: "micro",
        arg: Some(MICRO_ITERS),
        items: MICRO_ITERS,
        cleanup: true,
    },
    Operation {
        group: "B2",
        name: "encode",
        variant: Variant::Structural,
        setup: &[("setup_doc", Some(USERS))],
        function: "bench_encode",
        arg: None,
        items: USERS,
        cleanup: false,
    },
    Operation {
        group: "B2",
        name: "decode",
        variant: Variant::Structural,
        setup: &[("setup_text", Some(USERS))],
        function: "bench_decode",
        arg: None,
        items: USERS,
        cleanup: true,
    },
    Operation {
        group: "B2",
        name: "small_encode",
        variant: Variant::Structural,
        setup: &[("setup_address", None)],
        function: "bench_small_encode",
        arg: Some(SMALL_ITERS),
        items: SMALL_ITERS,
        cleanup: true,
    },
    Operation {
        group: "B2",
        name: "small_decode",
        variant: Variant::Structural,
        setup: &[("setup_address", None)],
        function: "bench_small_decode",
        arg: Some(SMALL_ITERS),
        items: SMALL_ITERS,
        cleanup: true,
    },
    Operation {
        group: "B3",
        name: "encode",
        variant: Variant::Custom,
        setup: &[("setup_doc", Some(USERS))],
        function: "bench_encode",
        arg: None,
        items: USERS,
        cleanup: false,
    },
    Operation {
        group: "B3",
        name: "decode",
        variant: Variant::Custom,
        setup: &[("setup_text", Some(USERS))],
        function: "bench_decode",
        arg: None,
        items: USERS,
        cleanup: true,
    },
    Operation {
        group: "B3",
        name: "small_encode",
        variant: Variant::Custom,
        setup: &[("setup_address", None)],
        function: "bench_small_encode",
        arg: Some(SMALL_ITERS),
        items: SMALL_ITERS,
        cleanup: true,
    },
    Operation {
        group: "B3",
        name: "small_decode",
        variant: Variant::Custom,
        setup: &[("setup_address", None)],
        function: "bench_small_decode",
        arg: Some(SMALL_ITERS),
        items: SMALL_ITERS,
        cleanup: true,
    },
];

fn call<E: ArmEngine>(e: &mut E, name: &str, arg: Option<i64>) -> HostValue {
    let args: Vec<Arg> = arg.into_iter().map(int).collect();
    e.call(name, &args).unwrap_or_else(|err| panic!("{}: {name}: {err}", E::NAME))
}

/// Runs every operation whose `group/name` contains `filter`.
pub fn run<E: ArmEngine>(runs: usize, reps: usize, filter: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for op in OPERATIONS.iter().filter(|op| format!("{}/{}", op.group, op.name).contains(filter)) {
        let mut best = (Duration::MAX, Duration::MAX, Duration::MAX);
        for _ in 0..runs {
            let mut e: E = engine(op.variant, roomy_limits());
            for (setup, arg) in op.setup {
                call(&mut e, setup, *arg);
            }
            e.collect();
            for _ in 0..reps {
                let start = Instant::now();
                std::hint::black_box(call(&mut e, op.function, op.arg));
                let op_time = start.elapsed();
                let cleanup_start = Instant::now();
                if op.cleanup {
                    call(&mut e, "clear_out", None);
                    e.collect();
                }
                let cleanup = cleanup_start.elapsed();
                best.0 = best.0.min(op_time);
                best.1 = best.1.min(cleanup);
                best.2 = best.2.min(op_time + cleanup);
                if !op.cleanup {
                    // Keep the heap at the same size for the next repetition.
                    e.collect();
                }
            }
        }
        rows.push(Row {
            group: op.group,
            name: op.name,
            items: op.items,
            op: best.0,
            cleanup: op.cleanup.then_some(best.1),
            total: best.2,
        });
    }
    rows
}

/// B7 measurements for one arm.
#[derive(Clone, Debug)]
pub struct Footprint {
    pub tracked_bytes: usize,
    pub live_objects: usize,
    pub pause: Duration,
}

/// Builds `users` users, keeps them live, and times one full collection.
pub fn footprint<E: ArmEngine>(e: &mut E, users: i64) -> Footprint {
    let base = e.stats().tracked_bytes;
    let base_objects = e.stats().live_objects;
    call(e, "setup_doc", Some(users));
    e.collect();
    let mut pause = Duration::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        e.collect();
        pause = pause.min(start.elapsed());
    }
    Footprint { tracked_bytes: e.stats().tracked_bytes - base, live_objects: e.stats().live_objects - base_objects, pause }
}
