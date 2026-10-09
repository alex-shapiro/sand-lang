//! End-to-end tests of the `sand` command-line tool: subcommands, flag
//! parsing, exit codes, standard streams and the `--prompt-stdin` prompter.
//!
//! `--prompt` itself asks on the controlling terminal, which a test cannot
//! provide portably (and must not block on when the tests are run from one),
//! so the prompt dialogue is unit-tested in `src/main.rs` and exercised end to
//! end here through the hidden `--prompt-stdin` option, which differs only in
//! where it reads and writes.

mod common;

use common::TempDir;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl std::fmt::Debug for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}\n--- stdout\n{}--- stderr\n{}", self.code, self.stdout, self.stderr)
    }
}

/// Runs `sand ARGS` in `dir`, feeding `stdin` (closed after writing).
fn sand_in(dir: &Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sand"));
    cmd.args(args).current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("cannot start sand");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.as_bytes().to_vec();
    // Written from a thread so a child that does not read stdin cannot deadlock us.
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().expect("sand did not finish");
    writer.join().unwrap();
    Out {
        code: out.status.code().expect("sand was killed by a signal"),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn sand(dir: &Path, args: &[&str]) -> Out {
    sand_in(dir, args, "", &[])
}

/// A temporary directory holding `main.sand` with `src`.
fn script(tag: &str, src: &str) -> TempDir {
    let dir = TempDir::new(tag);
    dir.write("main.sand", src);
    dir
}

fn run(src: &str, flags: &[&str]) -> Out {
    let dir = script("run", src);
    let mut args = vec!["run", "main.sand"];
    args.extend_from_slice(flags);
    sand(dir.path(), &args)
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

#[test]
fn usage_version_and_unknown_commands() {
    let dir = TempDir::new("usage");
    let o = sand(dir.path(), &[]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("USAGE:"), "{o:?}");

    for help in ["--help", "-h", "help"] {
        let o = sand(dir.path(), &[help]);
        assert_eq!(o.code, 0, "{o:?}");
        assert!(o.stdout.contains("sand run FILE"), "{o:?}");
    }

    let o = sand(dir.path(), &["--version"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, format!("sand {}\n", env!("CARGO_PKG_VERSION")).as_str()), "{o:?}");

    let o = sand(dir.path(), &["frobnicate"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("sand: unknown command `frobnicate`"), "{o:?}");
}

// ---------------------------------------------------------------------------
// run: exit codes
// ---------------------------------------------------------------------------

#[test]
fn run_success_prints_output() {
    let o = run("print(\"hello\")\neprint(\"to stderr\")", &[]);
    assert_eq!((o.code, o.stdout.as_str(), o.stderr.as_str()), (0, "hello\n", "to stderr\n"), "{o:?}");
}

#[test]
fn run_exit_codes_from_the_script() {
    for (code, want) in [(0, 0), (3, 3), (42, 42), (255, 255), (256, 255), (-1, 255)] {
        let o = run(&format!("print(\"before\")\nexit({code})\nprint(\"after\")"), &[]);
        assert_eq!((o.code, o.stdout.as_str()), (want, "before\n"), "exit({code}): {o:?}");
    }
}

#[test]
fn run_runtime_errors_exit_1() {
    let o = run("fn f(n: int) -> int { 10 / n }\nprint(f(0))", &[]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(o.stderr.contains("panic: division by zero"), "{o:?}");
    assert!(o.stderr.contains("main.sand:1"), "{o:?}");

    // `?` on an error at the top level of the main module.
    let o = run("fn f() -> Result<int> { Err(error(\"nope\", kind: \"invalid\")) }\nlet x = f()?\nprint(x)", &[]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(o.stderr.contains("nope"), "{o:?}");
    assert_eq!(o.stdout, "");
}

#[test]
fn run_compile_errors_exit_2() {
    let o = run("let x: int = \"text\"\nprint(x)", &[]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("error") && o.stderr.contains("main.sand:1"), "{o:?}");
    assert!(o.stderr.contains("let x: int = \"text\""), "source excerpt expected: {o:?}");
    assert_eq!(o.stdout, "");
}

#[test]
fn compile_fuel_limit_is_a_compile_error() {
    let o = run("let x = [1, 2, 3].map(x => x * 2)\nprint(x)", &["--max-compile-fuel=10"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("program too complex to compile"), "{o:?}");
    let o = run("print(1)", &["--max-compile-fuel=10000000"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "1\n"), "{o:?}");
}

#[test]
fn run_limit_violations_exit_3() {
    let cases = [
        ("var i = 0\nwhile true { i += 1 }", "--max-instructions=10000", "instruction limit exceeded"),
        ("let xs = []\nwhile true { xs.push(\"some text\".repeat(100)) }", "--max-memory=4M", "memory limit exceeded"),
        ("task.sleep(30s)", "--max-time=200ms", "time limit exceeded"),
        ("var i = 0\nwhile true { i += 1 }", "--max-cpu=100ms", "cpu time limit exceeded"),
    ];
    for (src, flag, msg) in cases {
        let start = std::time::Instant::now();
        let o = run(src, &[flag]);
        assert_eq!(o.code, 3, "{flag}: {o:?}");
        assert!(o.stderr.contains(msg), "{flag}: {o:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "{flag} took too long");
    }
}

#[test]
fn run_missing_or_unreadable_file_exits_2() {
    let dir = TempDir::new("missing");
    let o = sand(dir.path(), &["run"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("missing FILE"), "{o:?}");
    let o = sand(dir.path(), &["run", "nope.sand"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("cannot read") && o.stderr.contains("nope.sand"), "{o:?}");
}

// ---------------------------------------------------------------------------
// run: flags
// ---------------------------------------------------------------------------

#[test]
fn bad_flags_and_values_exit_2_with_a_message() {
    let cases = [
        ("--bogus", "unknown option `--bogus`"),
        ("--max-memory=lots", "invalid size `lots`"),
        ("--max-memory", "--max-memory requires a value"),
        ("--max-time=soon", "invalid duration `soon`"),
        ("--max-cpu=-5s", "invalid duration `-5s`"),
        ("--max-instructions=many", "invalid --max-instructions value `many`"),
        ("--max-compile-fuel=lots", "invalid --max-compile-fuel value `lots`"),
        ("--seed=abc", "invalid --seed value `abc`"),
        ("--seed", "--seed requires a value"),
        ("--allow-read", "--allow-read requires a value"),
        ("--allow-stdin=yes", "--allow-stdin does not take a value"),
        ("--allow-all=1", "--allow-all does not take a value"),
        ("--prompt=y", "--prompt does not take a value"),
    ];
    for (flag, msg) in cases {
        let o = run("print(\"should not run\")", &[flag]);
        assert_eq!(o.code, 2, "{flag}: {o:?}");
        assert!(o.stderr.contains(msg), "{flag}: expected {msg:?} in {o:?}");
        assert_eq!(o.stdout, "", "{flag}");
    }
}

#[test]
fn script_arguments() {
    let dir = script("args", "import env\nprint(env.args())");
    let o = sand(dir.path(), &["run", "main.sand", "--", "a", "--not-a-flag", "-x", "--"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "[\"a\", \"--not-a-flag\", \"-x\", \"--\"]\n"), "{o:?}");
    // Positional arguments after the file also go to the script; options still apply.
    let o = sand(dir.path(), &["run", "main.sand", "one", "--seed=1", "two"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "[\"one\", \"two\"]\n"), "{o:?}");
    let o = sand(dir.path(), &["run", "main.sand"]);
    assert_eq!(o.stdout, "[]\n", "{o:?}");
}

#[test]
fn seed_makes_runs_reproducible() {
    let src = "import random\nprint([random.int(0, 1000000) for _ in 0..5])";
    let a = run(src, &["--seed=7"]);
    let b = run(src, &["--seed=7"]);
    let c = run(src, &["--seed=8"]);
    assert_eq!(a.code, 0, "{a:?}");
    assert_eq!(a.stdout, b.stdout);
    assert_ne!(a.stdout, c.stdout);
}

#[test]
fn allow_read_and_write_paths() {
    let dir = script(
        "fsflags",
        r#"import fs
print(fs.read_text("data/in.txt"))
print(fs.read_text("secret.txt").is_ok())
print(fs.write_text("out/result.txt", "written").is_ok())
print(fs.write_text("data/in.txt", "clobbered").is_ok())
"#,
    );
    dir.write("data/in.txt", "input");
    dir.write("secret.txt", "secret");
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    let o = sand(dir.path(), &["run", "main.sand", "--allow-read=data", "--allow-write=out"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Ok(\"input\")\nfalse\ntrue\nfalse\n"), "{o:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("out/result.txt")).unwrap(), "written");
    assert_eq!(std::fs::read_to_string(dir.path().join("data/in.txt")).unwrap(), "input");

    // Nothing is allowed by default; --allow-all allows everything.
    let o = sand(dir.path(), &["run", "main.sand"]);
    assert!(o.stdout.starts_with("Err(Error { kind: \"permission_denied\"") && o.stdout.ends_with("false\nfalse\nfalse\n"), "{o:?}");
    let o = sand(dir.path(), &["run", "main.sand", "--allow-all"]);
    assert_eq!(o.stdout, "Ok(\"input\")\ntrue\ntrue\ntrue\n", "{o:?}");
}

#[test]
fn allow_env_names() {
    let dir = script("envflags", "import env\nprint(env.get(\"SAND_CLI_ALLOWED\"))\nprint(env.get(\"SAND_CLI_OTHER\"))");
    let vars = [("SAND_CLI_ALLOWED", "yes"), ("SAND_CLI_OTHER", "hidden")];
    let o = sand_in(dir.path(), &["run", "main.sand", "--allow-env=SAND_CLI_ALLOWED"], "", &vars);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Some(\"yes\")\nNone\n"), "{o:?}");
    let o = sand_in(dir.path(), &["run", "main.sand", "--allow-env=SAND_CLI_ALLOWED,SAND_CLI_OTHER"], "", &vars);
    assert_eq!(o.stdout, "Some(\"yes\")\nSome(\"hidden\")\n", "{o:?}");
}

#[test]
fn standard_input() {
    let src = r#"import io
match io.read_line() {
    Ok(Some(line)) => print("line: {line}")
    Ok(None) => print("eof")
    Err(e) => print("error: {e.kind}")
}
print(io.read_all().map(s => s.len()))
"#;
    let dir = script("stdin", src);
    let o = sand_in(dir.path(), &["run", "main.sand"], "first\nsecond\n", &[]);
    assert_eq!(
        o.stdout,
        "error: permission_denied\nErr(Error { kind: \"permission_denied\", message: \"permission denied: read standard input\" })\n",
        "{o:?}"
    );
    let o = sand_in(dir.path(), &["run", "main.sand", "--allow-stdin"], "first\r\nsecond\n", &[]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "line: first\nOk(7)\n"), "{o:?}");
    let o = sand_in(dir.path(), &["run", "main.sand", "--allow-stdin"], "", &[]);
    assert_eq!(o.stdout, "eof\nOk(0)\n", "{o:?}");
}

#[test]
fn prompt_answers_from_stdin() {
    let src =
        "import fs\nprint(fs.read_text(\"a.txt\").is_ok())\nprint(fs.read_text(\"a.txt\").is_ok())\nprint(fs.read_text(\"a.txt\").is_ok())";
    let dir = script("prompt", src);
    dir.write("a.txt", "A");
    let cases = [
        ("y\ny\nn\n", "true\ntrue\nfalse\n", 3),
        ("a\n", "true\ntrue\ntrue\n", 1),
        ("N\n", "false\nfalse\nfalse\n", 1),
        ("n\nmaybe\ny\n\n", "false\ntrue\nfalse\n", 4),
        // End of input denies.
        ("", "false\nfalse\nfalse\n", 3),
    ];
    for (answers, want, questions) in cases {
        let o = sand_in(dir.path(), &["run", "main.sand", "--prompt-stdin"], answers, &[]);
        assert_eq!((o.code, o.stdout.as_str()), (0, want), "answers {answers:?}: {o:?}");
        assert_eq!(o.stderr.matches("sand: allow the script to read ").count(), questions, "answers {answers:?}: {o:?}");
        assert!(o.stderr.contains("a.txt?"), "{o:?}");
    }
    // Rules still apply first: allowed reads are not prompted for.
    let o = sand_in(dir.path(), &["run", "main.sand", "--prompt-stdin", "--allow-read=."], "n\n", &[]);
    assert_eq!((o.stdout.as_str(), o.stderr.as_str()), ("true\ntrue\ntrue\n", ""), "{o:?}");
}

// ---------------------------------------------------------------------------
// check, lint, fmt, test
// ---------------------------------------------------------------------------

#[test]
fn check_command() {
    let dir = TempDir::new("check");
    dir.write("good.sand", "fn f() -> int { 1 }\nprint(f())");
    dir.write("bad.sand", "print(1 + \"one\")");
    let o = sand(dir.path(), &["check", "good.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "good.sand: ok\n"), "{o:?}");
    let o = sand(dir.path(), &["check", "good.sand", "bad.sand"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(o.stdout, "good.sand: ok\n");
    assert!(o.stderr.contains("bad.sand:1"), "{o:?}");
    let o = sand(dir.path(), &["check", "missing.sand"]);
    assert_eq!(o.code, 2, "{o:?}");
    let o = sand(dir.path(), &["check"]);
    assert_eq!(o.code, 2, "{o:?}");
    let o = sand(dir.path(), &["check", "good.sand", "--strict"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("unknown option `--strict`"), "{o:?}");
}

#[test]
fn lint_command() {
    let dir = TempDir::new("lint");
    dir.write("warn.sand", "fn f() -> int {\n    let unused = 1\n    2\n}\nprint(f())\n");
    dir.write("clean.sand", "print(1)\n");
    dir.write("broken.sand", "print(\n");
    let o = sand(dir.path(), &["lint", "warn.sand"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(o.stdout, "warn.sand:2:9: warning[unused-variable]: unused variable `unused`\n");
    let o = sand(dir.path(), &["lint", "--allow-warnings", "warn.sand"]);
    assert_eq!((o.code, o.stdout.lines().count()), (0, 1), "{o:?}");
    let o = sand(dir.path(), &["lint", "clean.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{o:?}");
    let o = sand(dir.path(), &["lint", "broken.sand"]);
    assert_eq!(o.code, 2, "{o:?}");
    let o = sand(dir.path(), &["lint", "warn.sand", "--allow-warning"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("unknown option `--allow-warning`"), "{o:?}");
}

#[test]
fn fmt_command() {
    let dir = TempDir::new("fmt");
    let ugly = "fn f(a:int)->int{a*2}\nprint(f(1))\n";
    let pretty = "fn f(a: int) -> int { a * 2 }\n\nprint(f(1))\n";
    dir.write("ugly.sand", ugly);
    dir.write("pretty.sand", pretty);
    dir.write("broken.sand", "fn (\n");

    let o = sand(dir.path(), &["fmt", "--check", "ugly.sand", "pretty.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (1, "ugly.sand: not formatted\n"), "{o:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("ugly.sand")).unwrap(), ugly, "--check must not write");
    let o = sand(dir.path(), &["fmt", "pretty.sand", "--check"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{o:?}");

    // A misspelt flag is an error, not a request to format in place.
    let o = sand(dir.path(), &["fmt", "--chek", "ugly.sand"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("ugly.sand")).unwrap(), ugly);

    let o = sand(dir.path(), &["fmt", "ugly.sand", "pretty.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "ugly.sand: formatted\n"), "{o:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("ugly.sand")).unwrap(), pretty);
    let o = sand(dir.path(), &["fmt", "--check", "ugly.sand"]);
    assert_eq!(o.code, 0, "{o:?}");

    let o = sand(dir.path(), &["fmt", "--check", "broken.sand"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("broken.sand"), "{o:?}");
}

#[test]
fn test_command() {
    let dir = TempDir::new("testcmd");
    dir.write(
        "mixed.sand",
        "fn double(x: int) -> int { x * 2 }\nprint(\"top level output is captured\")\ntest \"doubles\" {\n    assert(double(2) == 4)\n}\ntest \"fails\" {\n    assert(double(2) == 5, \"wrong answer\")\n}\n",
    );
    dir.write("passing.sand", "test \"one\" {\n    assert(true)\n}\ntest \"two\" {\n    assert(1 < 2)\n}\n");
    dir.write("broken.sand", "test \"x\" { assert(1) }\n");
    let o = sand(dir.path(), &["test", "mixed.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (1, "ok doubles\nFAIL fails: wrong answer\n1 passed, 1 failed\n"), "{o:?}");
    let o = sand(dir.path(), &["test", "passing.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "ok one\nok two\n2 passed, 0 failed\n"), "{o:?}");
    let o = sand(dir.path(), &["test", "passing.sand", "mixed.sand"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(o.stdout.matches(" passed, ").count(), 2, "{o:?}");
    let o = sand(dir.path(), &["test", "broken.sand"]);
    assert_eq!(o.code, 2, "{o:?}");
}

#[test]
fn test_command_denies_by_default_and_takes_run_options() {
    let dir = TempDir::new("testpolicy");
    dir.write("secret.txt", "s3cret");
    dir.write(
        "t.sand",
        "import fs\nimport env\ntest \"reads\" {\n    assert(fs.read_text(\"secret.txt\").is_ok(), \"denied\")\n}\ntest \"env\" {\n    assert(env.get(\"HOME\") != None, \"no env\")\n}\n",
    );
    let o = sand(dir.path(), &["test", "t.sand"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(o.stdout.contains("FAIL reads: denied") && o.stdout.contains("FAIL env: no env"), "{o:?}");
    let allow = format!("--allow-read={}", dir.path().display());
    let o = sand(dir.path(), &["test", "t.sand", &allow, "--allow-env=HOME"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "ok reads\nok env\n2 passed, 0 failed\n"), "{o:?}");
    let o = sand(dir.path(), &["test", "--allow-all", "t.sand"]);
    assert_eq!(o.code, 0, "{o:?}");
    // Limits apply too.
    dir.write("spin.sand", "test \"spin\" {\n    var i = 0\n    while true { i += 1 }\n}\n");
    let o = sand(dir.path(), &["test", "spin.sand", "--max-instructions=100000"]);
    assert_ne!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains("instruction limit") || o.stderr.contains("instruction limit"), "{o:?}");
    for bad in [&["test", "t.sand", "--allow-everything"][..], &["test", "t.sand", "--", "x"], &["test", "--allow-all"]] {
        assert_eq!(sand(dir.path(), bad).code, 2, "{bad:?}");
    }
}

#[test]
fn repl_reads_from_stdin() {
    let dir = TempDir::new("repl");
    let o = sand_in(dir.path(), &["repl"], "1 + 2\nlet x = 5\nx * 2\nprint(\"hi\")\n:quit\nprint(\"not reached\")\n", &[]);
    assert_eq!(o.code, 0, "{o:?}");
    let shown: Vec<&str> = o.stdout.split("> ").map(str::trim).filter(|s| !s.is_empty()).collect();
    assert_eq!(shown[1..], ["3", "10", "hi"], "{o:?}");
    // End of input ends the session too.
    let o = sand_in(dir.path(), &["repl"], "40 + 2\n", &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains("42"), "{o:?}");
}

/// The values and messages a REPL session printed: its standard output split
/// at the prompts (`> ` and `... `), without the banner and empty pieces.
fn repl_shown(stdout: &str) -> Vec<String> {
    let (_, rest) = stdout.split_once('\n').unwrap_or_default();
    rest.replace("... ", "").split("> ").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

#[test]
fn repl_runs_each_input_once() {
    let dir = TempDir::new("repl-once");
    let input =
        "print(\"start\")\nvar n = 1\nfn add(x: int) -> int { x + n }\nadd(1)\nn = 10\nadd(1)\nfn add(x: int) -> int { x * n }\nadd(2)\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["start", "2", "11", "20"], "{o:?}");
    assert_eq!(o.stderr, "", "{o:?}");
    // Effects such as writing files happen once too.
    let input = "import fs\nfs.append_text(\"log.txt\", \"x\")?\nfs.append_text(\"log.txt\", \"y\")?\nfs.read_text(\"log.txt\")?\n";
    let o = sand_in(dir.path(), &["repl", "--allow-write=."], input, &[]);
    assert_eq!(repl_shown(&o.stdout), ["\"xy\""], "{o:?}");
}

#[test]
fn repl_multi_line_input() {
    let dir = TempDir::new("repl-lines");
    let input = "fn fact(n: int) -> int {\n    if n <= 1 {\n        1\n    } else {\n        n * fact(n - 1)\n    }\n}\nfact(5)\n\
                 let s = \"\"\"\n    a\n    b\n    \"\"\"\ns\n[1,\n 2] +\n [3]\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!((o.code, o.stderr.as_str()), (0, ""), "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["120", "\"a\\nb\\n\"", "[1, 2, 3]"], "{o:?}");
    assert_eq!(o.stdout.matches("... ").count(), 6 + 3 + 2, "{o:?}");
    // Input left open at the end is still run (and reported).
    let o = sand_in(dir.path(), &["repl"], "fn f() {\n", &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stderr.contains("error"), "{o:?}");
}

#[test]
fn repl_reports_errors_and_continues() {
    let dir = TempDir::new("repl-errors");
    let input = "var x = 1\nx = \"s\"\nx\nfn boom() { panic(\"boom\") }\nx = 2\nboom()\nx\nlet y = [1][9]\n:quit\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["1", "2"], "{o:?}");
    assert!(o.stderr.contains("error: type mismatch: expected int, found str\n  --> <input 2>:1:5"), "{o:?}");
    assert!(o.stderr.contains("1 | x = \"s\""), "{o:?}");
    assert!(o.stderr.contains("panic: boom\n    at boom (<input 4>:1)\n"), "{o:?}");
    assert!(o.stderr.contains("panic: index out of bounds"), "{o:?}");
}

#[test]
fn repl_commands() {
    let dir = TempDir::new("repl-commands");
    let input = ":help\nlet xs = [1, 2]\n:type xs.map(x => x.to_float())\n:type undefined_name\n:type let z = 1\n\
                 test \"len\" { assert(xs.len() == 2) }\ntest \"bad\" { assert(false, \"nope\") }\n:test\n:reset\nxs\n:nope\n:q\n1\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains(":type EXPR"), "{o:?}");
    assert!(o.stdout.contains("> [float]\n"), "{o:?}");
    assert!(o.stdout.contains("ok len\nFAIL bad: nope\n1 passed, 1 failed\n"), "{o:?}");
    assert!(o.stderr.contains("undefined name `undefined_name`"), "{o:?}");
    assert!(o.stderr.contains("expected an expression"), "{o:?}");
    assert!(o.stderr.contains("undefined name `xs`"), "{o:?}");
    assert!(o.stderr.contains("unknown command `:nope`"), "{o:?}");
    // `:q` ended the session before `1`.
    assert!(!o.stdout.contains("> 1\n"), "{o:?}");
    let o = sand_in(dir.path(), &["repl", "main.sand"], "", &[]);
    assert_eq!(o.code, 2, "{o:?}");
}

#[test]
fn repl_exit_ends_the_session() {
    let dir = TempDir::new("repl-exit");
    let o = sand_in(dir.path(), &["repl"], "print(\"a\")\nexit(4)\nprint(\"b\")\n", &[]);
    assert_eq!(o.code, 4, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["a"], "{o:?}");
}

#[test]
fn repl_prompts_with_the_prompter() {
    let dir = TempDir::new("repl-prompt");
    dir.write("a.txt", "A");
    // The answer is read from standard input after the line that asks.
    let input = "import fs\nfs.read_text(\"a.txt\")?\na\nfs.read_text(\"a.txt\")?\nfs.read_text(\"a.txt\").is_ok()\n";
    let o = sand_in(dir.path(), &["repl", "--prompt-stdin"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["\"A\"", "\"A\"", "true"], "{o:?}");
    assert_eq!(o.stderr.matches("sand: allow the script to read ").count(), 1, "{o:?}");
}

/// Ctrl-C (SIGINT) interrupts the running input, or discards a partial one,
/// without ending the session.
#[cfg(unix)]
#[test]
fn repl_ctrl_c_interrupts_the_input() {
    let dir = TempDir::new("repl-sigint");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sand"))
        .arg("repl")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot start sand");
    let mut input = child.stdin.take().unwrap();
    let interrupt = |child: &std::process::Child| {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let sent = Command::new("kill").args(["-INT", &child.id().to_string()]).status().is_ok_and(|s| s.success());
        assert!(sent, "cannot send SIGINT");
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    input.write_all(b"var i = 0\nwhile true { i += 1 }\n").unwrap();
    interrupt(&child);
    input.write_all(b"i > 0\nfn half_done() {\n").unwrap();
    interrupt(&child);
    input.write_all(b"half_done\n40 + 2\n").unwrap();
    drop(input);
    let out = child.wait_with_output().unwrap();
    let o = Out {
        code: out.status.code().expect("sand was killed by a signal"),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["true", "42"], "{o:?}");
    assert!(o.stderr.contains("interrupted"), "{o:?}");
    assert!(o.stderr.contains("undefined name `half_done`"), "{o:?}");
}
