//! Verification and execution of `sand` and `policy` code blocks in site Markdown.

use sandlang::policy::PolicyFile;
use sandlang::{Config, Engine, Error, MemFs, MemorySink};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Run,
    NoRun,
    CompileError,
    RuntimeError,
    Ignore,
}

impl Mode {
    pub fn parse(words: &[&str]) -> Result<Mode, String> {
        match words {
            [] => Ok(Mode::Run),
            ["no_run"] => Ok(Mode::NoRun),
            ["error"] => Ok(Mode::CompileError),
            ["fails"] => Ok(Mode::RuntimeError),
            ["ignore"] => Ok(Mode::Ignore),
            _ => Err(format!("unknown code block mode `{}` (use no_run, error, fails or ignore)", words.join(" "))),
        }
    }
}

#[derive(Debug)]
pub struct Example {
    /// Complete program source, including lines prefixed with `# `.
    pub program: String,
    /// Display source with `# ` prefix lines stripped.
    pub shown: String,
    pub result: Option<Shown>,
}

#[derive(Debug)]
pub enum Shown {
    Output(String),
    CompileError(String),
    RuntimeError { output: String, error: String },
}

/// Formats, type-checks, and optionally runs a `sand` code block.
pub fn check_sand(runtime: &tokio::runtime::Runtime, source: &str, mode: Mode, files: &[(String, String)]) -> Result<Example, String> {
    let program: String =
        source.lines().map(|l| l.strip_prefix("# ").unwrap_or(if l == "#" { "" } else { l })).map(|l| format!("{l}\n")).collect();
    let shown: String = source.lines().filter(|l| !l.starts_with("# ") && *l != "#").map(|l| format!("{l}\n")).collect();
    let example = |result| Ok(Example { program: program.clone(), shown: shown.clone(), result });
    if mode == Mode::Ignore {
        return example(None);
    }
    match sandlang::fmt::format_source(&program) {
        Ok(formatted) if formatted == program => {}
        Ok(formatted) => return Err(format!("not formatted as `sand fmt` formats it:\n{}", diff(&program, &formatted))),
        Err(e) if mode == Mode::CompileError => {
            let _ = e;
        }
        Err(e) => return Err(format!("does not parse: {e}")),
    }
    let (engine, sink) = engine(files);
    match mode {
        Mode::Ignore => unreachable!(),
        Mode::NoRun => engine.check_source("main.sand", &program).map_err(|e| e.render()).and_then(|()| example(None)),
        Mode::CompileError => match engine.check_source("main.sand", &program) {
            Err(e @ Error::Compile(_)) => example(Some(Shown::CompileError(e.render()))),
            Err(e) => Err(format!("expected a compile error, found: {}", e.render())),
            Ok(()) => Err("expected a compile error, but the program type checks".into()),
        },
        Mode::Run | Mode::RuntimeError => {
            let mut engine = engine;
            let result = runtime.block_on(engine.run_source("main.sand", &program));
            let output = sink.contents();
            match (mode, result) {
                (Mode::Run, Ok(o)) if o.exit_code == 0 => example((!output.is_empty()).then_some(Shown::Output(output))),
                (Mode::Run, Ok(o)) => Err(format!("exited with code {}:\n{output}", o.exit_code)),
                (Mode::Run, Err(e)) => Err(format!("{output}{}", e.render())),
                (_, Err(e @ Error::Runtime { .. })) => {
                    let error = e.render();
                    example(Some(Shown::RuntimeError { output, error }))
                }
                (_, Err(e)) => Err(format!("expected a runtime error, found: {}", e.render())),
                (_, Ok(_)) => Err(format!("expected a runtime error, but the program ran:\n{output}")),
            }
        }
    }
}

/// Default sandbox policy for documentation examples and playground snippets.
pub const POLICY: &str = "\
# Allow in-memory file access, standard streams, and module imports.
allow write /
allow stdout
allow stderr
allow import /
limit time 10s
";

pub fn check_policy(source: &str) -> Result<PolicyFile, String> {
    PolicyFile::parse(source, Path::new("/project"), Some(Path::new("/home/user")))
        .map_err(|errors| errors.iter().map(|e| format!("{e}\n")).collect())
}

fn engine(files: &[(String, String)]) -> (Engine, MemorySink) {
    let sink = MemorySink::new();
    let fs = MemFs::new();
    for (name, text) in files {
        fs.write(format!("/{name}"), text).expect("module fits in memory");
    }
    let policy = check_policy(POLICY).expect("default policy is valid");
    let config = Config::new().memory_fs(fs).policy(policy.rules).limits(policy.limits).seed(42).stdout(sink.clone()).stderr(sink.clone());
    (Engine::new(config).unwrap(), sink)
}

fn diff(a: &str, b: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let mut out = String::new();
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(""), b.get(i).copied().unwrap_or(""));
        if x != y {
            out.push_str(&format!("  line {}:\n    found:    {x}\n    expected: {y}\n", i + 1));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str, mode: Mode) -> Result<Example, String> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        check_sand(&runtime, source, mode, &[])
    }

    #[test]
    fn examples_run_and_show_their_output() {
        let e = run("# let x = 2\nprint(x * 21)\n", Mode::Run).unwrap();
        assert_eq!(e.shown, "print(x * 21)\n");
        assert!(matches!(e.result, Some(Shown::Output(o)) if o == "42\n"));
    }

    #[test]
    fn examples_may_write_files() {
        let e = run("import fs\n\nfs.write_text(\"a.txt\", \"hi\")?\nprint(fs.read_text(\"a.txt\")?)\n", Mode::Run).unwrap();
        assert!(matches!(e.result, Some(Shown::Output(o)) if o == "hi\n"));
    }

    #[test]
    fn modes_are_enforced() {
        assert!(run("let x: int = \"a\"\n", Mode::Run).is_err());
        assert!(matches!(run("let x: int = \"a\"\n", Mode::CompileError).unwrap().result, Some(Shown::CompileError(_))));
        assert!(run("print(1)\n", Mode::CompileError).is_err());
        assert!(matches!(
            run("let x: int? = None\nprint(x.unwrap())\n", Mode::RuntimeError).unwrap().result,
            Some(Shown::RuntimeError { .. })
        ));
        assert!(run("print( 1 )\n", Mode::Run).unwrap_err().contains("not formatted"));
    }

    #[test]
    fn examples_import_the_pages_modules() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let files = [("geo.sand".to_string(), "pub fn two() -> int {\n    2\n}\n".to_string())];
        let e = check_sand(&runtime, "import \"./geo\"\n\nprint(geo.two())\n", Mode::Run, &files).unwrap();
        assert!(matches!(e.result, Some(Shown::Output(o)) if o == "2\n"));
    }

    #[test]
    fn the_network_is_denied() {
        let e = run("import http\n\nprint(http.get(\"https://example.com\").is_err())\n", Mode::Run).unwrap();
        assert!(matches!(e.result, Some(Shown::Output(o)) if o == "true\n"));
    }
}
