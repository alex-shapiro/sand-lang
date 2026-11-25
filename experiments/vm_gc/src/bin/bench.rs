//! Prints B1 to B3 timings in nanoseconds per item for every arm.
//!
//! `cargo run --release --bin bench [filter]`. `BENCH_RUNS` (default 3) sets
//! the fresh engines per operation, and `BENCH_REPS` (default 5) sets the
//! repetitions per engine. `BENCH_ARMS=arm1,arm3` selects arms.

use vm_gc::shared::bench::{print_rows, run};
use vm_gc::shared::suite::ArmEngine;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default).max(1)
}

fn arm<E: ArmEngine>(runs: usize, reps: usize, filter: &str) {
    let arms = std::env::var("BENCH_ARMS").unwrap_or_default();
    if arms.is_empty() || arms.split(',').any(|a| a == E::NAME) {
        print_rows(E::NAME, &run::<E>(runs, reps, filter));
    }
}

fn main() {
    let filter = std::env::args().nth(1).unwrap_or_default();
    let (runs, reps) = (env_usize("BENCH_RUNS", 3), env_usize("BENCH_REPS", 5));
    println!("ns per item: fastest of {runs} engines x {reps} repetitions");
    println!("{:<5} {:<3} {:<13} {:>9} {:>9} {:>9}", "arm", "", "operation", "op", "cleanup", "total");
    arm::<vm_gc::arm1::Engine>(runs, reps, &filter);
    arm::<vm_gc::arm2::Engine>(runs, reps, &filter);
    arm::<vm_gc::arm3::Engine>(runs, reps, &filter);
}
