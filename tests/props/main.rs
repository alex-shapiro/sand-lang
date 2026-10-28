//! Property-based tests that compare Sand execution against Rust reference
//! models on generated inputs.

mod bindings;
mod construct;
mod generics;
mod natives;
mod observers;
mod programs;
mod reflect;
mod traits;
mod values;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use sandlang::{Config, Engine, Error, MemorySink, Policy};
use std::sync::LazyLock;

static BOUNDARY: LazyLock<Vec<i64>> = LazyLock::new(|| {
    let mut v = vec![0, 1, -1, i64::MIN, i64::MIN + 1, i64::MAX, i64::MAX - 1];
    let powers = (1..63).map(|n| 1i64 << n).chain((1..19).map(|n| 10i64.pow(n)));
    for p in powers {
        v.extend([p - 1, p, p + 1, -p - 1, -p, -p + 1]);
    }
    v.sort();
    v.dedup();
    v
});

fn int() -> impl Strategy<Value = i64> + Clone {
    prop_oneof![4 => -20i64..20, 3 => prop::sample::select(BOUNDARY.as_slice()), 1 => any::<i64>()]
}

fn int_lit(v: i64) -> String {
    if v == i64::MIN {
        "(-9223372036854775807 - 1)".to_string()
    } else if v < 0 {
        format!("({v})")
    } else {
        v.to_string()
    }
}

fn str_lit(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn list_str(xs: &[i64]) -> String {
    format!("[{}]", xs.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "))
}

fn opt_str(x: Option<i64>) -> String {
    x.map_or("None".into(), |x| format!("Some({x})"))
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Run {
    stdout: String,
    failure: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Optimized,
    Unoptimized,
    GcStress,
}

fn run_in(src: &str, mode: Mode) -> Run {
    let out = MemorySink::new();
    let config = Config::new()
        .policy(Policy::deny_all())
        .stdout(out.clone())
        .stderr(MemorySink::new())
        .optimize(!matches!(mode, Mode::Unoptimized))
        .gc_stress(matches!(mode, Mode::GcStress));
    let mut engine = Engine::new(config).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let failure = match rt.block_on(engine.run_source("main.sand", src)) {
        Ok(_) => None,
        Err(Error::Runtime { message, .. }) => Some(message),
        Err(e) => panic!("{e}\nprogram:\n{src}"),
    };
    Run { stdout: out.contents(), failure }
}

/// Runs `src` in each mode in `modes` and asserts that all modes produce the
/// same result.
fn run_modes(src: &str, modes: &[Mode]) -> Result<Run, TestCaseError> {
    let first = run_in(src, modes[0]);
    for &mode in &modes[1..] {
        let other = run_in(src, mode);
        prop_assert_eq!(&other, &first, "{:?} and {:?} differ; program:\n{}", modes[0], mode, src);
    }
    Ok(first)
}

fn run(src: &str) -> Result<Run, TestCaseError> {
    run_modes(src, &[Mode::Optimized, Mode::Unoptimized])
}

fn check(src: &str, stdout: &str, failure: Option<&str>) -> Result<(), TestCaseError> {
    let r = run(src)?;
    prop_assert_eq!(&r.stdout, stdout, "program:\n{}", src);
    match (&r.failure, failure) {
        (None, None) => {}
        (Some(got), Some(want)) => prop_assert!(got.contains(want), "failed with {got:?}, expected {want:?}; program:\n{src}"),
        (got, want) => prop_assert!(false, "failure {got:?}, expected {want:?}; program:\n{src}"),
    }
    Ok(())
}

type Expect = Result<String, &'static str>;

/// Runs all `Ok` cases from `prints` in a single program after `setup`, then
/// runs each `Err` case in a separate program.
fn check_prints(setup: &str, prints: &[(String, Expect)]) -> Result<(), TestCaseError> {
    let mut src = format!("{setup}\n");
    let mut stdout = String::new();
    for (e, expect) in prints {
        if let Ok(out) = expect {
            src.push_str(&format!("print({e})\n"));
            stdout.push_str(&format!("{out}\n"));
        }
    }
    check(&src, &stdout, None)?;
    for (e, expect) in prints {
        if let Err(failure) = expect {
            check(&format!("{setup}\nprint({e})\n"), "", Some(failure))?;
        }
    }
    Ok(())
}
