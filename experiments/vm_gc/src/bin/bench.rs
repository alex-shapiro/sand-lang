//! Prints B1 to B3 timings in nanoseconds per item for every arm.
//!
//! `cargo run --release --bin bench [filter]`. The binary runs `BENCH_ROUNDS`
//! rounds (default 5) that alternate the arms. Each round runs every
//! operation in `BENCH_RUNS` fresh engines (default 5) for `BENCH_REPS`
//! repetitions (default 10). Each number is the fastest over all rounds.
//! `BENCH_ARMS=arm1,arm3` selects arms.

use std::collections::BTreeMap;
use std::time::Duration;
use vm_gc::shared::bench::{Row, run};
use vm_gc::shared::suite::ArmEngine;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default).max(1)
}

/// The fastest row of each arm and operation, in run order.
#[derive(Default)]
struct Best {
    order: Vec<(&'static str, &'static str)>,
    rows: BTreeMap<(&'static str, &'static str, &'static str), Row>,
}

impl Best {
    fn add(&mut self, arm: &'static str, rows: Vec<Row>) {
        for row in rows {
            let op = (row.group, row.name);
            if !self.order.contains(&op) {
                self.order.push(op);
            }
            let best = self.rows.entry((op.0, op.1, arm)).or_insert_with(|| row.clone());
            best.op = best.op.min(row.op);
            best.total = best.total.min(row.total);
            best.cleanup = best.cleanup.zip(row.cleanup).map(|(a, b)| a.min(b));
        }
    }
}

fn round<E: ArmEngine>(best: &mut Best, runs: usize, reps: usize, filter: &str) {
    let arms = std::env::var("BENCH_ARMS").unwrap_or_default();
    if arms.is_empty() || arms.split(',').any(|a| a == E::NAME) {
        best.add(E::NAME, run::<E>(runs, reps, filter));
    }
}

fn main() {
    let filter = std::env::args().nth(1).unwrap_or_default();
    let (rounds, runs, reps) = (env_usize("BENCH_ROUNDS", 5), env_usize("BENCH_RUNS", 5), env_usize("BENCH_REPS", 10));
    let mut best = Best::default();
    for _ in 0..rounds {
        round::<vm_gc::arm1::Engine>(&mut best, runs, reps, &filter);
        round::<vm_gc::arm2::Engine>(&mut best, runs, reps, &filter);
        round::<vm_gc::arm3::Engine>(&mut best, runs, reps, &filter);
    }
    println!("ns per item: fastest of {rounds} rounds x {runs} engines x {reps} repetitions");
    println!("{:<5} {:<3} {:<13} {:>9} {:>9} {:>9}", "arm", "", "operation", "op", "cleanup", "total");
    let ns = |row: &Row, d: Duration| format!("{:.1}", row.per_item(d));
    for (group, name) in &best.order {
        for arm in ["arm1", "arm2", "arm3"] {
            if let Some(r) = best.rows.get(&(*group, *name, arm)) {
                let cleanup = r.cleanup.map_or("-".to_string(), |c| ns(r, c));
                println!("{arm:<5} {group:<3} {name:<13} {:>9} {cleanup:>9} {:>9}", ns(r, r.op), ns(r, r.total));
            }
        }
    }
}
