//! Times the ports of four `bench/programs` workloads on every arm, with
//! the default GC settings, and the original programs on the real `sand`.
//!
//! `cargo run --release --bin realistic`. `RUNS` (default 5) sets the fresh
//! engines per workload; each time is the fastest run. `SAND` sets the
//! `sand` binary (default `../../target/release/sand`).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use vm_gc::shared::realistic::run_port;
use vm_gc::shared::suite::ArmEngine;

const WORKLOADS: &[(&str, i64)] = &[("binary_trees", 15), ("fannkuch", 9), ("nbody", 500_000), ("richards", 40)];

/// Returns the fastest time, the output, the collection count, and the
/// instructions run (fuel used) of `runs` runs.
fn time_arm<E: ArmEngine>(name: &str, size: i64, runs: usize) -> (Duration, String, u64, u64) {
    let mut best = (Duration::MAX, String::new(), 0, 0);
    for _ in 0..runs {
        let start = Instant::now();
        let (out, e) = run_port::<E>(name, size).unwrap_or_else(|err| panic!("{}: {name}: {err}", E::NAME));
        let elapsed = start.elapsed();
        if elapsed < best.0 {
            best = (elapsed, out, e.stats().collections, u64::MAX - e.stats().fuel_left);
        }
    }
    best
}

/// Returns the fastest whole-process time and the output of the original program.
fn time_sand(sand: &Path, name: &str, runs: usize) -> Option<(Duration, String)> {
    let program = Path::new("../../bench/programs").join(format!("{name}.sand"));
    let mut best: Option<(Duration, String)> = None;
    for _ in 0..runs {
        let start = Instant::now();
        let out = Command::new(sand).arg("run").arg(&program).arg("--max-memory=8G").output().ok()?;
        let elapsed = start.elapsed();
        if best.as_ref().is_none_or(|(t, _)| elapsed < *t) {
            best = Some((elapsed, String::from_utf8_lossy(&out.stdout).into_owned()));
        }
    }
    best
}

fn main() {
    let runs = std::env::var("RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(5usize).max(1);
    let sand = std::env::var("SAND").unwrap_or_else(|_| "../../target/release/sand".into());
    println!("ms, fastest of {runs} runs; (n) is the collection count");
    println!("{:<13} {:>14} {:>14} {:>14} {:>10}", "program", "arm1", "arm2", "arm3", "sand");
    for (name, size) in WORKLOADS {
        let reference = time_sand(Path::new(&sand), name, runs);
        let arms = [
            time_arm::<vm_gc::arm1::Engine>(name, *size, runs),
            time_arm::<vm_gc::arm2::Engine>(name, *size, runs),
            time_arm::<vm_gc::arm3::Engine>(name, *size, runs),
        ];
        let mut row = format!("{name:<13}");
        for (time, out, collections, _) in &arms {
            if let Some((_, expected)) = &reference {
                assert_eq!(out, expected, "{name}: the port's output differs from `sand`");
            }
            row += &format!(" {:>8.1} ({collections:>3})", time.as_secs_f64() * 1e3);
        }
        row += &reference.map_or(" -".into(), |(t, _)| format!(" {:>10.1}", t.as_secs_f64() * 1e3));
        println!("{row}");
        let (time, _, _, fuel) = &arms[0];
        println!("{:<13} arm1 ran {:.1} M instructions, {:.1} ns each", "", *fuel as f64 / 1e6, time.as_nanos() as f64 / *fuel as f64);
    }
}
