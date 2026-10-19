//! Golden-file acceptance tests for the language specification.
//!
//! Each `tests/spec/*.test` file contains many cases:
//!
//! ```text
//! === case name
//! source code...
//! --- stdout
//! expected standard output
//! --- error
//! 3: substring of a compile error reported on line 3
//! ```
//!
//! Sections:
//! * `--- stdout` / `--- stderr`: exact expected output (a trailing newline is implied).
//! * `--- error`: compile errors; each line is `LINE: substring` or `substring`.
//! * `--- fail`: substring of the message of the error the run fails with.
//! * `--- exit N`: expected exit code.
//! * `--- stdin`: data supplied on standard input.
//! * `--- file PATH`: an extra file written next to `main.sand`.
//! * `--- lint`: expected lint warnings, one `LINE: code` per line (exact set).
//! * `--- fmt`: expected output of the formatter applied to the source.
//! * `--- test`: run with the test runner; expected report.
//! * `--- args`: whitespace-separated script arguments.
//! * `--- env`: `NAME=value` lines forming the script's environment.
//!
//! Every file also runs in two more modes (`Mode`), whose results must be
//! the same: with `Config::gc_stress`, which collects garbage at every safe
//! point (alternating minor and major collections), to check that the
//! collector never frees live values; and with `Config::optimize(false)`,
//! to check that optimized code behaves like the plain code.
//!
//! Every case runs with scripts seeing the host's file system (a temporary
//! directory; with the `os` feature) and with a memory file system
//! (`MemFs`), which must behave alike.

mod common;

use common::rt;
use sandlang::{Config, Engine, Error, MemFs, MemorySink, Policy};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a case's program is compiled and run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Plain,
    GcStress,
    Unoptimized,
}

impl Mode {
    fn note(self) -> &'static str {
        match self {
            Mode::Plain => "",
            Mode::GcStress => " (under GC stress)",
            Mode::Unoptimized => " (unoptimized)",
        }
    }
}

/// The file system a case runs against.
#[derive(Clone, Copy, Debug)]
enum Backend {
    /// A temporary directory of the host's file system.
    #[cfg(feature = "os")]
    Host,
    /// The directory `/case` of a memory file system.
    Memory,
}

/// The directory of a case's files.
enum CaseDir {
    #[cfg(feature = "os")]
    Host(common::TempDir),
    Memory(MemFs),
}

impl CaseDir {
    fn new(backend: Backend, name: &str) -> CaseDir {
        match backend {
            #[cfg(feature = "os")]
            Backend::Host => CaseDir::Host(common::TempDir::new(name)),
            Backend::Memory => {
                let _ = name;
                let fs = MemFs::new();
                fs.create_dir_all("/case").unwrap();
                CaseDir::Memory(fs)
            }
        }
    }

    fn path(&self) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            CaseDir::Host(d) => d.path().to_path_buf(),
            CaseDir::Memory(_) => PathBuf::from("/case"),
        }
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            CaseDir::Host(d) => d.write(rel, contents),
            CaseDir::Memory(fs) => {
                let p = Path::new("/case").join(rel);
                fs.write(&p, contents).unwrap();
                p
            }
        }
    }

    /// `config` with scripts seeing this directory's file system.
    fn configure(&self, config: Config) -> Config {
        let config = config.cwd(self.path());
        match self {
            #[cfg(feature = "os")]
            CaseDir::Host(_) => config,
            CaseDir::Memory(fs) => config.memory_fs(fs.clone()),
        }
    }
}

#[derive(Default, Debug)]
struct Case {
    name: String,
    line: usize,
    source: String,
    sections: Vec<(String, String)>,
}

fn parse_cases(text: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    let mut current: Option<Case> = None;
    let mut section: Option<(String, String)> = None;
    for (i, line) in text.lines().enumerate() {
        if let Some(name) = line.strip_prefix("=== ") {
            if let Some(mut c) = current.take() {
                if let Some(s) = section.take() {
                    c.sections.push(s);
                }
                cases.push(c);
            }
            current = Some(Case { name: name.trim().to_string(), line: i + 1, ..Default::default() });
            continue;
        }
        if let Some(header) = line.strip_prefix("--- ") {
            let c = current.as_mut().expect("section outside of case");
            if let Some(s) = section.take() {
                c.sections.push(s);
            }
            section = Some((header.trim().to_string(), String::new()));
            continue;
        }
        match (&mut section, &mut current) {
            (Some((_, body)), _) => {
                body.push_str(line);
                body.push('\n');
            }
            (None, Some(c)) => {
                c.source.push_str(line);
                c.source.push('\n');
            }
            (None, None) => {}
        }
    }
    if let Some(mut c) = current.take() {
        if let Some(s) = section.take() {
            c.sections.push(s);
        }
        cases.push(c);
    }
    for c in &mut cases {
        // Trailing blank lines in sections are not significant.
        for (_, body) in &mut c.sections {
            while body.ends_with("\n\n") {
                body.pop();
            }
        }
        while c.source.ends_with("\n\n") {
            c.source.pop();
        }
    }
    cases
}

fn run_case(case: &Case, mode: Mode, backend: Backend) -> Result<(), String> {
    let get = |name: &str| case.sections.iter().find(|(h, _)| h == name).map(|(_, b)| b.clone());
    let dir = CaseDir::new(backend, &case.name);
    for (h, body) in &case.sections {
        if let Some(path) = h.strip_prefix("file ") {
            dir.write(path.trim(), body);
        }
    }

    if let Some(expected) = get("fmt") {
        let got = sandlang::fmt::format_source(&case.source).map_err(|e| format!("fmt failed: {e}"))?;
        if got != expected {
            return Err(format!("fmt mismatch\n--- expected\n{expected}--- got\n{got}"));
        }
        let again = sandlang::fmt::format_source(&got).map_err(|e| format!("re-fmt failed: {e}"))?;
        if again != got {
            return Err(format!("fmt not idempotent\n--- first\n{got}--- second\n{again}"));
        }
        if get("stdout").is_none() && get("lint").is_none() {
            return Ok(());
        }
    }

    if let Some(expected) = get("lint") {
        dir.write("main.sand", &case.source);
        let engine = Engine::new(dir.configure(Config::new().policy(Policy::allow_all())));
        let warnings = engine.lint_source("main.sand", &case.source).map_err(|e| format!("lint failed: {e}"))?;
        let mut got: Vec<String> = warnings.iter().map(|w| format!("{}: {}", w.line, w.code)).collect();
        got.sort();
        let mut want: Vec<String> = expected.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()).collect();
        want.sort();
        if got != want {
            let details: Vec<String> = warnings.iter().map(|w| format!("  {}: {} {}", w.line, w.code, w.message)).collect();
            return Err(format!("lint mismatch\nexpected: {want:?}\ngot: {got:?}\n{}", details.join("\n")));
        }
        return Ok(());
    }

    let main = dir.write("main.sand", &case.source);
    let out = MemorySink::new();
    let err = MemorySink::new();
    let config = Config::new().policy(Policy::allow_all()).stdout(out.clone()).stderr(err.clone()).seed(42);
    let mut config = dir.configure(config.gc_stress(mode == Mode::GcStress).optimize(mode != Mode::Unoptimized));
    if let Some(stdin) = get("stdin") {
        config = config.stdin_bytes(stdin.into_bytes());
    }
    if let Some(args) = get("args") {
        config = config.args(args.split_whitespace().map(String::from).collect());
    }
    if let Some(env) = get("env") {
        let vars: BTreeMap<String, String> =
            env.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        config = config.env(vars.into_iter().collect());
    }
    let mut engine = Engine::new(config);
    let runtime = rt();

    if let Some(expected) = get("test") {
        let report = runtime.block_on(engine.run_tests(&main));
        let report = match report {
            Ok(r) => r,
            Err(e) => return Err(format!("test run failed: {e}")),
        };
        let got = report.render();
        if got.trim_end() != expected.trim_end() {
            return Err(format!("test report mismatch\n--- expected\n{expected}--- got\n{got}"));
        }
        return Ok(());
    }

    let result = runtime.block_on(engine.run_file(&main));
    let stdout = out.contents();
    let stderr = err.contents();

    if let Some(expected) = get("error") {
        let diags = match &result {
            Err(Error::Compile(d)) => d.clone(),
            Err(other) => return Err(format!("expected compile error, got: {other}")),
            Ok(_) => {
                return Err(format!("expected compile error, but program ran. stdout:\n{stdout}"));
            }
        };
        for want in expected.lines().filter(|l| !l.trim().is_empty()) {
            let (line, text) = match want.split_once(": ") {
                Some((l, t)) if l.trim().parse::<usize>().is_ok() => (Some(l.trim().parse::<usize>().unwrap()), t.trim()),
                _ => (None, want.trim()),
            };
            let found = diags.iter().any(|d| d.message.contains(text) && line.is_none_or(|l| d.line == l));
            if !found {
                let all: Vec<String> = diags.iter().map(|d| format!("  {}: {}", d.line, d.message)).collect();
                return Err(format!("missing compile error {want:?}; got:\n{}", all.join("\n")));
            }
        }
        return Ok(());
    }

    if let Some(expected) = get("fail") {
        match &result {
            Err(Error::Runtime { message, .. }) => {
                if !message.contains(expected.trim()) {
                    return Err(format!("error message {message:?} does not contain {:?}", expected.trim()));
                }
            }
            Err(other) => {
                return Err(format!("expected failure {:?}, got: {other}", expected.trim()));
            }
            Ok(_) => {
                return Err(format!("expected failure {:?}, but run succeeded. stdout:\n{stdout}", expected.trim()));
            }
        }
    } else if let Some(code) = case.sections.iter().find_map(|(h, _)| h.strip_prefix("exit ")) {
        let code: i32 = code.trim().parse().unwrap();
        match &result {
            Ok(o) if o.exit_code == code => {}
            Ok(o) => return Err(format!("expected exit {code}, got {}", o.exit_code)),
            Err(e) => {
                return Err(format!("expected exit {code}, got error: {e}\nstdout:\n{stdout}"));
            }
        }
    } else if let Err(e) = &result {
        return Err(format!("run failed: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}"));
    }

    if let Some(expected) = get("stdout")
        && stdout != expected
    {
        return Err(format!("stdout mismatch\n--- expected\n{expected}--- got\n{stdout}"));
    }
    if let Some(expected) = get("stderr")
        && stderr != expected
    {
        return Err(format!("stderr mismatch\n--- expected\n{expected}--- got\n{stderr}"));
    }
    Ok(())
}

fn run_file(name: &str, mode: Mode, backend: Backend) {
    let path = format!("{}/tests/spec/{name}.test", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let cases = parse_cases(&text);
    assert!(!cases.is_empty(), "{path} has no cases");
    let filter = std::env::var("SPEC_FILTER").ok();
    let mut failures = Vec::new();
    let mut ran = 0;
    for case in &cases {
        if let Some(f) = &filter
            && !case.name.contains(f.as_str())
        {
            continue;
        }
        ran += 1;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(case, mode, backend)));
        let note = format!("{} ({backend:?} file system)", mode.note());
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => failures.push(format!("[{name}.test:{}] {}{note}\n{msg}", case.line, case.name)),
            Err(_) => failures.push(format!("[{name}.test:{}] {}{note}\nHARNESS PANIC", case.line, case.name)),
        }
    }
    if !failures.is_empty() {
        panic!("{} of {ran} cases failed:\n\n{}", failures.len(), failures.join("\n\n"));
    }
}

macro_rules! spec_files {
    ($($name:ident),* $(,)?) => {
        #[cfg(feature = "os")]
        mod host {
            use super::{Backend, Mode, run_file};
            spec_files!(@modes Backend::Host; $($name),*);
        }
        mod memory {
            use super::{Backend, Mode, run_file};
            spec_files!(@modes Backend::Memory; $($name),*);
        }
    };
    (@modes $backend:expr; $($name:ident),*) => {
        $(
            #[test]
            fn $name() {
                run_file(stringify!($name), Mode::Plain, $backend);
            }
        )*
        mod gc_stress {
            use super::{Backend, Mode, run_file};
            $(
                #[test]
                fn $name() {
                    run_file(stringify!($name), Mode::GcStress, $backend);
                }
            )*
        }
        mod unoptimized {
            use super::{Backend, Mode, run_file};
            $(
                #[test]
                fn $name() {
                    run_file(stringify!($name), Mode::Unoptimized, $backend);
                }
            )*
        }
    };
}

spec_files!(
    lexical,
    arithmetic,
    strings,
    format,
    collections,
    control_flow,
    codegen,
    defer,
    use_bindings,
    functions,
    closures,
    structs,
    enums,
    patterns,
    option_result,
    generics,
    iterators,
    traits,
    types,
    modules,
    concurrency,
    runtime_errors,
    lint,
    fmt,
    test_runner,
    help,
    std_base64,
    std_crypto,
    std_fs,
    std_io_env,
    std_json,
    std_log,
    std_math,
    std_random,
    std_regex,
    std_statistics,
    std_time,
    std_uri,
    std_uuid,
);
