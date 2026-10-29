//! End-to-end tests of the `sand` command-line tool: subcommands, flag
//! parsing, exit codes and standard streams. `prompt_tty.rs` tests prompts.

mod common;

use common::TempDir;
use std::io::{Read, Write};
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
/// `run`, `test` and `repl` get `--no-prompt`:
/// the CLI asks on the controlling terminal, which a test run may share.
/// `cli/tests/prompt_tty.rs` tests prompts on a pseudo-terminal.
fn sand_in(dir: &Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sand"));
    let mut args = args.to_vec();
    if matches!(args.first(), Some(&("run" | "test" | "repl"))) && !args.contains(&"--no-prompt") {
        args.insert(1, "--no-prompt");
    }
    cmd.args(&args).current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
    assert!(o.stderr.contains("error: invalid: division by zero"), "{o:?}");
    assert!(o.stderr.contains("main.sand:1"), "{o:?}");

    // `?` on an error at the top level of the main module.
    let o = run("fn f() -> Result<int> { Err(Error(\"nope\", kind: \"invalid\")) }\nlet x = f()?\nprint(x)", &[]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(o.stderr.contains("error: invalid: nope\n    at <top level> (main.sand:2)"), "{o:?}");
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
        ("--allow", "--allow requires a rule, e.g. --allow='net api.example.com'"),
        ("--prompt", "--prompt requires a rule, e.g. --prompt='net api.example.com' (sand asks about what no rule decides by default)"),
        ("--allow-private-net", "unknown option `--allow-private-net` (no rule denies private addresses unless you add one"),
        ("--allow=raed ./x", "invalid rule `--allow=raed ./x`: unknown capability `raed`"),
        ("--deny=read data", "invalid rule `--deny=read data`: path `data` is not anchored"),
        ("--prompt=net", "invalid rule `--prompt=net`: `prompt net` needs at least one argument"),
        ("--no-prompt=yes", "--no-prompt does not take a value"),
        ("--allow-all=1", "--allow-all does not take a value"),
        ("--allow-read=data", "unknown option `--allow-read` (use --allow='read ./PATH')"),
        ("--allow-net=*", "unknown option `--allow-net` (use --allow='net HOST')"),
        ("--prompt-stdin", "unknown option `--prompt-stdin` (questions go to the terminal, and --no-prompt denies instead)"),
    ];
    // A relative path starts from the working directory, not from a policy file.
    let dir = script("anchor", "print(1)");
    let o = sand(dir.path(), &["run", "main.sand", "--allow=read data"]);
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    assert!(o.stderr.contains(&format!("write `./data` for `{}`", cwd.join("data").display())), "{o:?}");
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
    let o = sand(dir.path(), &["run", "main.sand", "--allow=read ./data", "--allow=write ./out"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Ok(\"input\")\nfalse\ntrue\nfalse\n"), "{o:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("out/result.txt")).unwrap(), "written");
    assert_eq!(std::fs::read_to_string(dir.path().join("data/in.txt")).unwrap(), "input");

    // Files are not allowed by default; --allow-all allows everything.
    let o = sand(dir.path(), &["run", "main.sand"]);
    assert!(o.stdout.starts_with("Err(Error(message: \"permission denied") && o.stdout.ends_with("false\nfalse\nfalse\n"), "{o:?}");
    let o = sand(dir.path(), &["run", "main.sand", "--allow-all"]);
    assert_eq!(o.stdout, "Ok(\"input\")\ntrue\ntrue\ntrue\n", "{o:?}");
}

#[test]
fn policy_files() {
    let dir = script(
        "policyfile",
        r#"import fs
import env
print(fs.read_text("data/in.txt"))
print(fs.read_text("data/secret.txt").is_ok())
print(fs.write_text("out/result.txt", "written").is_ok())
print(env.get("SAND_CLI_POLICY"))
var i = 0
while true { i += 1 }
"#,
    );
    dir.write("data/in.txt", "input");
    dir.write("data/secret.txt", "secret");
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    // Paths are relative to the policy file's directory, not the working directory.
    dir.write(
        "conf/app.policy",
        "# the app's policy\nallow read ../data\ndeny read ../data/secret.txt\nallow env SAND_CLI_POLICY\nlimit instructions 100000\n",
    );
    let vars = [("SAND_CLI_POLICY", "on")];
    let o = sand_in(dir.path(), &["run", "main.sand", "--policy=conf/app.policy"], "", &vars);
    assert_eq!((o.code, o.stdout.as_str()), (3, "Ok(\"input\")\nfalse\nfalse\nSome(\"on\")\n"), "{o:?}");
    assert!(o.stderr.contains("instruction limit exceeded"), "{o:?}");

    // Options add rules to the file's and override its limits.
    let o = sand_in(
        dir.path(),
        &["run", "main.sand", "--allow=write ./out", "--policy=conf/app.policy", "--max-instructions=200000"],
        "",
        &vars,
    );
    assert_eq!((o.code, o.stdout.as_str()), (3, "Ok(\"input\")\nfalse\ntrue\nSome(\"on\")\n"), "{o:?}");

    // Errors name the file and line; the script does not run.
    dir.write("bad.policy", "allow read .\nalow net *\nlimit memory lots\n");
    let o = sand(dir.path(), &["run", "main.sand", "--policy=bad.policy"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("bad.policy:2:1: unknown rule `alow`"), "{o:?}");
    assert!(o.stderr.contains("bad.policy:3:7: invalid size `lots`"), "{o:?}");
    assert_eq!(o.stdout, "");
    let o = sand(dir.path(), &["run", "main.sand", "--policy=missing.policy"]);
    assert!(o.code == 2 && o.stderr.contains("cannot read policy file missing.policy"), "{o:?}");
    let o = sand(dir.path(), &["run", "main.sand", "--policy=a", "--policy=b"]);
    assert!(o.code == 2 && o.stderr.contains("--policy given more than once"), "{o:?}");
}

#[test]
fn policy_files_are_checked_formatted_and_linted() {
    let dir = script("policytools", "");
    dir.write("app.policy", "allow  read ./data,./data/x   # data\nlet unused = a\n\n\ndeny ~/.ssh # keys\n");
    let o = sand(dir.path(), &["check", "app.policy"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "app.policy: ok\n"), "{o:?}");
    let o = sand(dir.path(), &["lint", "app.policy"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(
        o.stdout,
        "app.policy:1:20: warning[redundant-rule]: line 1 (`./data`) already allows this\n\
         app.policy:2:5: warning[unused-set]: no rule uses the set `unused`\n",
        "{o:?}"
    );
    let o = sand(dir.path(), &["fmt", "--check", "app.policy"]);
    assert_eq!((o.code, o.stdout.as_str()), (1, "app.policy: not formatted\n"), "{o:?}");
    let o = sand(dir.path(), &["fmt", "app.policy"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("app.policy")).unwrap(),
        "allow read ./data ./data/x  # data\nlet unused = a\n\ndeny ~/.ssh  # keys\n"
    );

    dir.write("bad.policy", "allow read data\nallow read \"open\n");
    let o = sand(dir.path(), &["check", "bad.policy"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("bad.policy:1:12: path `data` is not anchored"), "{o:?}");
    let o = sand(dir.path(), &["lint", "bad.policy"]);
    assert!(o.code == 2 && o.stderr.contains("bad.policy:2:12: unterminated quotes"), "{o:?}");
    let o = sand(dir.path(), &["fmt", "bad.policy"]);
    assert!(o.code == 2 && o.stderr == "bad.policy:2:12: unterminated quotes\n", "{o:?}");
}

#[test]
fn allow_env_names() {
    let dir = script("envflags", "import env\nprint(env.get(\"SAND_CLI_ALLOWED\"))\nprint(env.get(\"SAND_CLI_OTHER\"))");
    let vars = [("SAND_CLI_ALLOWED", "yes"), ("SAND_CLI_OTHER", "hidden")];
    let o = sand_in(dir.path(), &["run", "main.sand", "--allow=env SAND_CLI_ALLOWED"], "", &vars);
    assert_eq!((o.code, o.stdout.as_str()), (0, "Some(\"yes\")\nNone\n"), "{o:?}");
    let o = sand_in(dir.path(), &["run", "main.sand", "--allow=env SAND_CLI_ALLOWED,SAND_CLI_OTHER"], "", &vars);
    assert_eq!(o.stdout, "Some(\"yes\")\nSome(\"hidden\")\n", "{o:?}");
}

#[test]
fn standard_input() {
    let src = r#"import io
match io.stdin.read_line() {
    Ok(Some(line)) => print("line: \(line)")
    Ok(None) => print("eof")
    Err(e) => print("error: \(e.kind)")
}
print(io.stdin.read_text().map(s => s.len()))
"#;
    let dir = script("stdin", src);
    // Scripts may read standard input unless a rule denies it.
    let o = sand_in(dir.path(), &["run", "main.sand"], "first\r\nsecond\n", &[]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "line: first\nOk(7)\n"), "{o:?}");
    let o = sand_in(dir.path(), &["run", "main.sand"], "", &[]);
    assert_eq!(o.stdout, "eof\nOk(0)\n", "{o:?}");
    let o = sand_in(dir.path(), &["run", "main.sand", "--deny=stdin"], "first\nsecond\n", &[]);
    assert_eq!(
        o.stdout,
        "error: permission_denied\nErr(Error(message: \"permission denied: read standard input\", kind: \"permission_denied\"))\n",
        "{o:?}"
    );
    // So may they read the time zone.
    let zone = script("zone", "import time\nprint(time.local_zone().id())");
    let o = sand_in(zone.path(), &["run", "main.sand"], "", &[("TZ", "Asia/Tokyo")]);
    assert_eq!(o.stdout, "Asia/Tokyo\n", "{o:?}");
    let o = sand_in(zone.path(), &["run", "main.sand", "--deny=timezone"], "", &[("TZ", "Asia/Tokyo")]);
    assert_eq!(o.stdout, "UTC\n", "{o:?}");
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
    assert_eq!(o.code, 2, "{o:?}");
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
fn output_stops_quietly_if_stdout_is_closed() {
    let dir = TempDir::new("lint-pipe");
    let rules: String = (0..20_000).map(|i| format!("deny net h{i}.example.com\n")).collect();
    dir.write("big.policy", &format!("deny net *\n{rules}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sand"))
        .args(["lint", "big.policy"])
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 16];
    child.stdout.take().unwrap().read_exact(&mut first).unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!((out.status.code(), stderr.as_ref()), (Some(1), ""));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sand"))
        .args(["repl", "--no-prompt"])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let line = format!("\"{}\"\n", "x".repeat(200));
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(line.repeat(3000).as_bytes());
    });
    child.stdout.take().unwrap().read_exact(&mut first).unwrap();
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!((out.status.code(), stderr.as_ref()), (Some(0), ""));
}

// Rust ignores `EBADF` on standard output, and macOS does not provide `/dev/full`.
#[cfg(target_os = "linux")]
#[test]
fn output_errors_are_reported() {
    let full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sand")).arg("--version").stdout(full).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.starts_with("sand: cannot write to standard output: "), "{stderr}");
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

    // Standard input to standard output.
    let o = sand_in(dir.path(), &["fmt", "-"], ugly, &[]);
    assert_eq!((o.code, o.stdout.as_str()), (0, pretty), "{o:?}");
    let o = sand_in(dir.path(), &["fmt", "-", "--check"], ugly, &[]);
    assert_eq!((o.code, o.stdout.as_str()), (1, ""), "{o:?}");
    let o = sand_in(dir.path(), &["fmt", "-"], "fn (\n", &[]);
    assert_eq!(o.code, 2, "{o:?}");
    assert!(o.stderr.contains("<stdin>"), "{o:?}");
    let o = sand(dir.path(), &["fmt", "--help"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains("sand fmt -"), "{o:?}");
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
    assert_eq!((o.code, o.stdout.as_str()), (1, "ok doubles\nFAIL fails: assert: wrong answer\n1 passed, 1 failed\n"), "{o:?}");
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
    assert!(o.stdout.contains("FAIL reads: assert: denied") && o.stdout.contains("FAIL env: assert: no env"), "{o:?}");
    let allow = format!("--allow=read {}", dir.path().display());
    let o = sand(dir.path(), &["test", "t.sand", &allow, "--allow=env HOME"]);
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
    let input = "import fs\nfs.write_text(\"log.txt\", \"x\")?\nfs.write_text(\"log.txt\", fs.read_text(\"log.txt\")? + \"y\")?\nfs.read_text(\"log.txt\")?\n";
    let o = sand_in(dir.path(), &["repl", "--allow=write ./"], input, &[]);
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
    let input = "var x = 1\nx = \"s\"\nx\nfn boom() { fail(\"boom\") }\nx = 2\nboom()\nx\nlet y = [1][9]\n:quit\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["1", "2"], "{o:?}");
    assert!(o.stderr.contains("error: type mismatch: expected int, found str\n  --> <input 2>:1:5"), "{o:?}");
    assert!(o.stderr.contains("1 | x = \"s\""), "{o:?}");
    assert!(o.stderr.contains("error: boom\n    at boom (<input 4>:1)\n"), "{o:?}");
    assert!(o.stderr.contains("error: bounds: index out of bounds"), "{o:?}");
}

#[test]
fn repl_commands() {
    let dir = TempDir::new("repl-commands");
    let input = ":help\nlet xs = [1, 2]\n:type xs.map(x => x.to_float())\nfn id<T>(x: T) -> T { x }\n:type id\n:type undefined_name\n:type let z = 1\n\
                 test \"len\" { assert(xs.len() == 2) }\ntest \"bad\" { assert(false, \"nope\") }\n:test\n:reset\nxs\n:nope\n:q\n1\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains(":type EXPR"), "{o:?}");
    assert!(o.stdout.contains("> [float]\n"), "{o:?}");
    assert!(o.stdout.contains("> fn<T>(T) -> T\n"), "{o:?}");
    assert!(o.stdout.contains("ok len\nFAIL bad: assert: nope\n1 passed, 1 failed\n"), "{o:?}");
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
fn repl_passes_lines_to_scripts_that_wait_for_them() {
    let dir = TempDir::new("repl-stdin");
    let input = "import io\nio.stdin.read_line()?\nfor the script\nlet t = spawn io.stdin.read_line()\n40 + 2\nt.wait()?\nlate\nio.stdin.read_line()?\n";
    let o = sand_in(dir.path(), &["repl"], input, &[]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["Some(\"for the script\")", "42", "Some(\"late\")", "None"], "{o:?}");
}

/// Ctrl-C (SIGINT) interrupts the running input, or discards a partial one,
/// without ending the session.
#[cfg(unix)]
#[test]
fn repl_ctrl_c_interrupts_the_input() {
    let dir = TempDir::new("repl-sigint");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sand"))
        .args(["repl", "--no-prompt"])
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

/// A running `sand repl` whose standard output and error are collected as
/// they arrive.
struct LiveRepl {
    child: std::process::Child,
    input: Option<std::process::ChildStdin>,
    streams: [std::sync::Arc<std::sync::Mutex<String>>; 2],
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl LiveRepl {
    fn start(dir: &Path) -> LiveRepl {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sand"))
            .args(["repl", "--no-prompt"])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cannot start sand");
        let input = child.stdin.take();
        let streams: [std::sync::Arc<std::sync::Mutex<String>>; 2] = Default::default();
        let sources: [Box<dyn std::io::Read + Send>; 2] = [Box::new(child.stdout.take().unwrap()), Box::new(child.stderr.take().unwrap())];
        let readers = sources
            .into_iter()
            .zip(streams.clone())
            .map(|(mut source, collected)| {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n @ 1..) = source.read(&mut buf) {
                        collected.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                })
            })
            .collect();
        LiveRepl { child, input, streams, readers }
    }

    fn send(&mut self, text: &str) {
        self.input.as_mut().unwrap().write_all(text.as_bytes()).unwrap();
    }

    /// Waits until standard output (0) or error (1) contains `text`.
    fn wait_for(&self, stream: usize, text: &str) {
        let start = std::time::Instant::now();
        while !self.streams[stream].lock().unwrap().contains(text) {
            assert!(start.elapsed() < std::time::Duration::from_secs(20), "no {text:?} in {:?}", self.streams[stream].lock().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Waits for the REPL to end by itself (standard input stays open).
    fn wait_exit(&mut self) {
        let start = std::time::Instant::now();
        while self.child.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < std::time::Duration::from_secs(20), "sand did not exit");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Closes standard input and waits for the REPL to end.
    fn finish(mut self) -> Out {
        drop(self.input.take());
        let status = self.child.wait().unwrap();
        for r in self.readers {
            r.join().unwrap();
        }
        let [stdout, stderr] = self.streams.map(|s| s.lock().unwrap().clone());
        Out { code: status.code().expect("sand was killed by a signal"), stdout, stderr }
    }
}

/// Tasks keep running while the REPL waits for input; their output starts on
/// a line of its own and is followed by the prompt again.
#[test]
fn repl_runs_tasks_while_waiting_for_input() {
    let dir = TempDir::new("repl-background");
    let mut repl = LiveRepl::start(dir.path());
    repl.send("var n = 0\nlet t = spawn {\ntask.sleep(200ms)\nn = 1\nprint(\"tick\")\n}\n");
    repl.wait_for(0, "tick\n");
    repl.send("n\nspawn { task.sleep(100ms)\nfail(\"bad task\") };\n");
    repl.wait_for(1, "(in a background task; all tasks were cancelled)\n");
    repl.send("spawn { task.sleep(100ms)\nexit(3) };\n");
    repl.wait_exit();
    let o = repl.finish();
    assert_eq!(o.code, 3, "{o:?}");
    assert!(o.stdout.contains("> \ntick\n> "), "{o:?}");
    assert_eq!(repl_shown(&o.stdout), ["tick", "1"], "{o:?}");
    assert!(
        o.stderr.contains("error: bad task\n    at <spawn> (<input 4>:2)\n(in a background task; all tasks were cancelled)\n"),
        "{o:?}"
    );
}

/// Ctrl-C at an empty prompt cancels the tasks left running.
#[cfg(unix)]
#[test]
fn repl_ctrl_c_cancels_tasks() {
    let dir = TempDir::new("repl-sigint-tasks");
    let mut repl = LiveRepl::start(dir.path());
    repl.send("let t = spawn { while true { task.yield_now() } }\n1 + 1\n");
    repl.wait_for(0, "2\n> ");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let sent = Command::new("kill").args(["-INT", &repl.child.id().to_string()]).status().is_ok_and(|s| s.success());
    assert!(sent, "cannot send SIGINT");
    std::thread::sleep(std::time::Duration::from_millis(300));
    repl.send("t.wait()\n");
    let o = repl.finish();
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stderr.contains("cancelled 1 task\n"), "{o:?}");
    assert!(o.stderr.contains("task cancelled"), "{o:?}");
}

#[test]
fn many_syntax_errors_are_reported_quickly() {
    let dir = TempDir::new("cli-errors");
    dir.write("bad.sand", &")\n".repeat(40_000));
    let start = std::time::Instant::now();
    let o = sand(dir.path(), &["check", "bad.sand"]);
    let took = start.elapsed();
    assert_ne!(o.code, 0, "{o:?}");
    let errors = o.stderr.matches("error").count();
    assert!((1..=60).contains(&errors), "{errors} errors reported");
    assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
}

/// A local HTTP server (on a thread) answering every request with `hi`;
/// returns its port.
fn hi_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        }
    });
    port
}

// Relies on the resolver accepting `127.1` for 127.0.0.1 (glibc, macOS).
#[cfg(unix)]
#[test]
fn net_rules_take_transports_and_decide_resolved_addresses() {
    let port = hi_server();
    let dir = script("net", &format!("import http\nprint(http.get(\"http://127.1:{port}/\").map(r => r.text().unwrap()))"));
    let run = |flags: &[&str]| {
        let mut args = vec!["run", "main.sand"];
        args.extend_from_slice(flags);
        sand(dir.path(), &args)
    };
    // `*` matches TLS connections only: `http://` needs `plaintext`.
    assert!(run(&["--allow=net *"]).stdout.starts_with("Err(Error(message: \"permission denied: GET http://127.1"), "TLS only");
    assert_eq!(run(&["--allow=net plaintext *"]).stdout, "Ok(\"hi\")\n");
    let denied = run(&["--allow=net plaintext *", "--deny=net tls plaintext 127.0.0.0/8"]).stdout;
    assert!(denied.contains("address 127.0.0.1 is not allowed by the rule `deny net tls plaintext 127.0.0.0/8`"), "{denied}");
    let removed = run(&["--allow-private-net"]);
    assert_eq!(removed.code, 2, "{removed:?}");
    assert!(removed.stderr.contains("unknown option `--allow-private-net`"), "{removed:?}");
}

/// `localhost` and the names under it are the loopback host (RFC 6761):
/// Sand resolves them without the system resolver.
#[test]
fn localhost_names_reach_loopback() {
    let port = hi_server();
    let get = |host: &str| format!("print(http.get(\"http://{host}:{port}/\").map(r => r.text().unwrap()))\n");
    let dir = script("localhost", &format!("import http\n{}{}", get("localhost"), get("app.localhost")));
    let run = |flags: &[&str]| {
        let mut args = vec!["run", "main.sand"];
        args.extend_from_slice(flags);
        sand(dir.path(), &args)
    };
    let o = run(&[&format!("--allow=net plaintext localhost:{port} *.localhost")]);
    assert_eq!(o.stdout, "Ok(\"hi\")\nOk(\"hi\")\n", "{o:?}");
    let o = run(&[]);
    assert_eq!(o.stdout.matches("permission denied: GET http://").count(), 2, "{o:?}");
}

/// A policy file's rules add to the rules of the CLI.
#[test]
fn policy_files_add_to_the_cli_rules() {
    let port = hi_server();
    let dir = script("cli-rules", &format!("import http\nprint(http.get(\"http://localhost:{port}/\").is_ok())\nprint(\"out\")"));
    dir.write("app.policy", "allow net plaintext localhost\ndeny stdout\n");
    let o = sand(dir.path(), &["run", "main.sand", "--policy=app.policy"]);
    assert_ne!(o.code, 0, "{o:?}");
    assert!(o.stderr.contains("permission denied: stdout"), "{o:?}");
    dir.write("app.policy", "allow net plaintext localhost\n");
    let o = sand(dir.path(), &["run", "main.sand", "--policy=app.policy"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "true\nout\n"), "{o:?}");
    for old in ["allow private-net\n", "default prompt\n"] {
        dir.write("old.policy", old);
        let o = sand(dir.path(), &["check", "old.policy"]);
        assert_eq!(o.code, 2, "{o:?}");
        assert!(o.stderr.contains("old.policy:1:"), "{o:?}");
    }
}

/// The rules of the user (flags and a policy file) replace the CLI's
/// default rule for the same requests, whatever their decision.
#[test]
fn user_rules_replace_the_defaults_for_the_same_requests() {
    let dir = script("cli-defaults", "import io\nprint(io.stdin.read_line().map_err(e => e.kind))");
    dir.write("p.policy", "prompt stdin\n");
    let read = |flags: &[&str]| {
        let mut args = vec!["run", "main.sand"];
        args.extend_from_slice(flags);
        sand_in(dir.path(), &args, "line\n", &[]).stdout
    };
    assert_eq!(read(&[]), "Ok(Some(\"line\"))\n");
    // Without a prompter, a prompt denies.
    assert_eq!(read(&["--prompt=stdin"]), "Err(\"permission_denied\")\n");
    assert_eq!(read(&["--policy=p.policy"]), "Err(\"permission_denied\")\n");
    assert_eq!(read(&["--deny=stdin"]), "Err(\"permission_denied\")\n");
    assert_eq!(read(&["--allow-all", "--prompt=stdin"]), "Err(\"permission_denied\")\n");
    // A broader rule of the user still decides what it matches.
    dir.write("lib.sand", "pub let X = 1");
    dir.write("w.sand", "import fs\nimport \"./lib.sand\"\nprint(lib.X)\nprint(fs.write_text(\"out.txt\", \"x\").map_err(e => e.kind))");
    let o = sand(dir.path(), &["run", "w.sand", "--allow-all", "--prompt=write **"]);
    assert_eq!(o.stdout, "1\nErr(\"permission_denied\")\n", "{o:?}");
    let o = sand(dir.path(), &["run", "w.sand", "--prompt=read **"]);
    assert!(o.stderr.contains("cannot import `./lib.sand`: permission denied"), "{o:?}");
}

/// `--allow-all` allows what no other rule decides: `--deny` rules and the
/// rules of a policy file still hold.
#[test]
fn allow_all_keeps_the_other_rules() {
    let dir = script(
        "cli-allow-all",
        "import fs\nimport env\nprint(fs.read_text(\"secret.txt\").is_ok())\nprint(env.get(\"HOME\").is_some())\nprint(fs.read_text(\"main.sand\").is_ok())",
    );
    dir.write("secret.txt", "s3cret");
    dir.write("p.policy", "deny ./secret.txt\ndeny env HOME\n");
    let o = sand(dir.path(), &["run", "main.sand", "--allow-all", "--deny=./secret.txt", "--deny=env HOME"]);
    assert_eq!(o.stdout, "false\nfalse\ntrue\n", "{o:?}");
    let o = sand(dir.path(), &["run", "main.sand", "--allow-all", "--policy=p.policy"]);
    assert_eq!(o.stdout, "false\nfalse\ntrue\n", "{o:?}");
}

#[test]
fn invalid_network_patterns_are_explained() {
    let out = run("print(1)", &["--allow=net 10.0.0.1/8"]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("`10.0.0.1/8` has bits set after its prefix length: write `10.0.0.0/8`"), "{}", out.stderr);
    let out = run("print(1)", &["--deny=net a..b"]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("invalid host pattern `a..b`"), "{}", out.stderr);
}

/// If a script's standard output is a full pipe, then the script stops at its
/// time limit.
#[test]
fn a_blocked_stdout_does_not_outlast_the_time_limit() {
    let dir = script("blocked", "while true { print(\"x\".repeat(1000)) }");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sand"))
        .args(["run", "main.sand", "--no-prompt", "--max-time=300ms"])
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot start sand");
    let mut stderr = child.stderr.take().unwrap();
    let mut err = String::new();
    // Read the error message while the pipe for standard output is still full.
    let mut buf = [0u8; 256];
    while !err.contains("time limit exceeded") {
        let n = stderr.read(&mut buf).unwrap();
        assert!(n > 0, "stderr closed: {err}");
        err.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(3));
    assert!(out.len() > 1 << 20);
}

#[test]
fn a_denial_without_asking_names_the_rule_that_allows_it() {
    let dir = script("hint", "import fs\nfor i in 0..2 {\n    print(fs.read_text(\"a.txt\").is_ok())\n}\n");
    dir.write("a.txt", "A");
    let o = sand(dir.path(), &["run", "main.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "false\nfalse\n"), "{o:?}");
    let path = std::fs::canonicalize(dir.path()).unwrap().join("a.txt");
    let hint = format!("sand: denied: read {} (--allow='read {}' allows it)\n", path.display(), path.display());
    // Once per request: the answer is remembered.
    assert_eq!(o.stderr, hint, "{o:?}");
}

#[test]
fn rule_options_take_their_value_after_a_space() {
    let dir = script("space", "import fs\nprint(fs.read_text(\"a.txt\").unwrap())\n");
    dir.write("a.txt", "A");
    dir.write("read.policy", "allow read ./a.txt\n");
    let o = sand(dir.path(), &["run", "main.sand", "--allow", "read ./a.txt"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "A\n"), "{o:?}");
    let o = sand(dir.path(), &["run", "--policy", "read.policy", "main.sand"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "A\n"), "{o:?}");
    let o = sand(dir.path(), &["run", "main.sand", "--deny", "read ./a.txt", "--allow", "read ./"]);
    assert_eq!(o.code, 1, "{o:?}");
}
