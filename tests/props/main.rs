//! Property-based tests: the implementation is compared against simple Rust
//! reference models on randomly generated inputs.
//!
//! - `values`: operators, strings, collections and standard modules against
//!   Rust's.
//! - `observers`: every way of observing a range, list, string, map or set
//!   (methods, loops, iterators, comprehensions) agrees with one model.
//! - `programs`: random programs against a reference evaluator.
//!
//! Every program runs both optimized and with `Config::optimize(false)`,
//! which must behave alike (`check`). Integers come from `int()`, which
//! favors the values implementations tend to break at. `PROPTEST_CASES`
//! raises the number of cases of every property, for long runs.

mod observers;
mod programs;
mod values;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use sandlang::{Config, Engine, Error, MemorySink, Policy};
use std::sync::LazyLock;

/// `cases` cases per property, or `PROPTEST_CASES` if it is set.
fn config(cases: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(cases);
    ProptestConfig::with_cases(cases)
}

/// Integers at which implementations tend to break: 0 and ±1, the powers of
/// two and ten and their neighbours, and the extremes.
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

/// Small integers, boundary values (`BOUNDARY`) and arbitrary ones.
fn int() -> impl Strategy<Value = i64> + Clone {
    prop_oneof![4 => -20i64..20, 3 => prop::sample::select(BOUNDARY.as_slice()), 1 => any::<i64>()]
}

/// `v` as a Sand expression.
fn int_lit(v: i64) -> String {
    if v == i64::MIN {
        "(-9223372036854775807 - 1)".to_string()
    } else if v < 0 {
        format!("({v})")
    } else {
        v.to_string()
    }
}

/// `s` as a Sand string literal, which is also how Sand prints a string
/// inside a collection.
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

/// How Sand prints a list of integers.
fn list_str(xs: &[i64]) -> String {
    format!("[{}]", xs.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "))
}

/// How Sand prints an `int?`.
fn opt_str(x: Option<i64>) -> String {
    x.map_or("None".into(), |x| format!("Some({x})"))
}

/// What a program does: what it prints, then the message of the error it
/// fails with, if it fails.
#[derive(Debug, Clone, PartialEq, Default)]
struct Run {
    stdout: String,
    failure: Option<String>,
}

/// How a program is compiled and run.
#[derive(Debug, Clone, Copy)]
enum Mode {
    Optimized,
    Unoptimized,
    GcStress,
}

/// Runs `src` in `mode`; a compile error panics, as generated programs must
/// compile.
fn run_in(src: &str, mode: Mode) -> Run {
    let out = MemorySink::new();
    let config = Config::new()
        .policy(Policy::deny_all())
        .stdout(out.clone())
        .stderr(MemorySink::new())
        .optimize(!matches!(mode, Mode::Unoptimized))
        .gc_stress(matches!(mode, Mode::GcStress));
    let mut engine = Engine::new(config);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let failure = match rt.block_on(engine.run_source("main.sand", src)) {
        Ok(_) => None,
        Err(Error::Runtime { message, .. }) => Some(message),
        Err(e) => panic!("{e}\nprogram:\n{src}"),
    };
    Run { stdout: out.contents(), failure }
}

/// Runs `src` in every mode of `modes`, which must all behave alike, and
/// returns what it does.
fn run_modes(src: &str, modes: &[Mode]) -> Result<Run, TestCaseError> {
    let first = run_in(src, modes[0]);
    for &mode in &modes[1..] {
        let other = run_in(src, mode);
        prop_assert_eq!(&other, &first, "{:?} and {:?} differ; program:\n{}", modes[0], mode, src);
    }
    Ok(first)
}

/// Runs `src` optimized and unoptimized, which must behave alike, and
/// returns what it does.
fn run(src: &str) -> Result<Run, TestCaseError> {
    run_modes(src, &[Mode::Optimized, Mode::Unoptimized])
}

/// Checks that `src` prints `stdout` and then fails with a message
/// containing `failure`, if given, or succeeds.
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

/// What printing an expression must print, or the failure it must cause.
type Expect = Result<String, &'static str>;

/// Checks `setup` followed by `print(e)` for each `(e, expect)` of `prints`:
/// those expected to succeed in one program, each one expected to fail in a
/// program of its own.
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
