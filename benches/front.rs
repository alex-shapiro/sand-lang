//! Front-end benchmarks: lexing, parsing, type checking, formatting and
//! linting the example programs. Run with `cargo bench --bench front`;
//! `cargo test --benches` runs each stage once.
//!
//! Each stage runs over every example, repeatedly; the harness reports the
//! best time of several runs and the throughput in source bytes per second.

use sandlang::syntax::{lexer, parse_module};
use sandlang::{Config, Engine, Policy};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Passes over the examples per run.
const PASSES: usize = 20;

/// A stage of the front end, run on one source.
type Stage<'a> = Box<dyn Fn(&str) + 'a>;

fn main() {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    // `cargo bench` passes `--bench`; without it (`cargo test --benches`),
    // one run checks that every stage still works.
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let runs = if quick { 1 } else { std::env::var("BENCH_RUNS").ok().and_then(|n| n.parse().ok()).unwrap_or(5usize).max(1) };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut sources: Vec<(PathBuf, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sand"))
        .map(|p| (p.clone(), std::fs::read_to_string(&p).unwrap()))
        .collect();
    sources.sort();
    let bytes: usize = sources.iter().map(|(_, s)| s.len()).sum::<usize>() * PASSES;

    // Checking and linting see the examples' directory (for their imports).
    let engine = || Engine::new(Config::new().policy(Policy::deny_all()).cwd(&dir));
    let stages: [(&str, Stage); 5] = [
        ("lex", Box::new(|src| assert!(lexer::lex(src, 0).errors.is_empty()))),
        ("parse", Box::new(|src| assert!(parse_module(src, 0).errors.is_empty()))),
        ("check", Box::new(move |src| engine().check_source("main.sand", src).map(|_| ()).unwrap())),
        ("format", Box::new(|src| assert_eq!(sandlang::fmt::format_source(src).unwrap(), src))),
        ("lint", Box::new(move |src| drop(engine().lint_source("main.sand", src).unwrap()))),
    ];

    println!("{:<10} {:>10} {:>10} {:>10}", "stage", "best", "median", "MB/s");
    for (name, stage) in &stages {
        if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        let mut times: Vec<Duration> = (0..runs)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..PASSES {
                    for (_, src) in &sources {
                        stage(src);
                    }
                }
                start.elapsed()
            })
            .collect();
        times.sort();
        let mbps = bytes as f64 / times[0].as_secs_f64() / 1e6;
        println!("{name:<10} {:>8.1}ms {:>8.1}ms {mbps:>10.1}", ms(times[0]), ms(times[times.len() / 2]));
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
