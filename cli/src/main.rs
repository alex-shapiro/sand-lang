//! The `sand` command-line tool.
#![forbid(unsafe_code)]

macro_rules! out {
    ($($arg:tt)*) => { crate::write_out(format_args!($($arg)*)) };
}

macro_rules! outln {
    () => { crate::write_out(format_args!("\n")) };
    ($($arg:tt)*) => { crate::write_out(format_args!("{}\n", format_args!($($arg)*))) };
}

mod repl;

use sandlang::policy::{PolicyFile, parse_duration, parse_size};
use sandlang::time::TimeZone;
use sandlang::{Capability, Config, Engine, Error, PromptAnswer, RulePolicy};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU8, Ordering};

static STDOUT: AtomicU8 = AtomicU8::new(OPEN);
const OPEN: u8 = 0;
const CLOSED: u8 = 1;
const FAILED: u8 = 2;

/// Writes to standard output, ignoring broken pipes and recording other I/O errors.
fn write_out(args: std::fmt::Arguments) {
    write_to(&mut std::io::stdout(), &STDOUT, args);
}

fn write_to(out: &mut impl Write, state: &AtomicU8, args: std::fmt::Arguments) {
    if state.load(Ordering::Relaxed) != OPEN {
        return;
    }
    if let Err(e) = out.write_fmt(args) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            state.store(CLOSED, Ordering::Relaxed);
        } else {
            eprintln!("sand: cannot write to standard output: {e}");
            state.store(FAILED, Ordering::Relaxed);
        }
    }
}

const USAGE: &str = "\
sand — the Sand language

USAGE:
    sand run FILE [OPTIONS] [-- ARGS...]   run a script
    sand check FILE...                     type check scripts or validate .policy files
    sand lint FILE... [--allow-warnings]   report lint warnings in scripts or .policy files
    sand fmt FILE... [--check]             format scripts or .policy files in place, or check formatting
    sand fmt - [--check]                   format standard input to standard output
    sand test FILE... [OPTIONS]            run test blocks with run options
    sand repl [OPTIONS]                    interactive session
    sand --version
    sand COMMAND --help                    show this help

RUN, TEST AND REPL OPTIONS:
    Scripts may print, read standard input and the time zone, and import modules.
    For other capabilities, sand prompts on the terminal unless matched by a rule.
    Without a terminal, sand denies unmatched requests and prints the required flag.

    --allow=RULE             allow capabilities matching RULE, e.g.
                             --allow='read ./data' --allow='net api.example.com'
                             --allow='net plaintext localhost:8080' --allow='env HOME'
    --deny=RULE              deny capabilities matching RULE, e.g. --deny='net 10.0.0.0/8'
    --prompt=RULE            prompt for capabilities matching RULE, e.g. --prompt='write ./out'
    --policy=FILE            load rules and limits from FILE; CLI flags add rules
                             and override limits
    --allow-all              allow all unmatched capabilities for trusted scripts
    --no-prompt              deny unmatched requests instead of prompting
    --max-memory=SIZE        heap limit, e.g. 256M; defaults to 512M
    --max-time=DURATION      wall-clock limit, e.g. 10s, 500ms, 2m
    --max-cpu=DURATION       CPU time limit
    --max-instructions=N     instruction fuel limit
    --max-compile-fuel=N     type-checking work limit; defaults to 10000000
    --seed=N                 seed the random number generator
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    // Options end at `--` (what follows goes to the script).
    let options = rest.iter().position(|a| a == "--").map_or(rest, |end| &rest[..end]);
    if options.iter().any(|a| a == "--help" || a == "-h") {
        out!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let result = match cmd.as_str() {
        "run" => cmd_run(rest),
        "check" => cmd_check(rest),
        "lint" => cmd_lint(rest),
        "fmt" => cmd_fmt(rest),
        "test" => cmd_test(rest),
        "repl" => repl::cmd_repl(rest),
        "--version" | "-V" | "version" => {
            outln!("sand {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            out!("{USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    // A run that hit a limit may have left unflushed output. Write it before exiting.
    sandlang::flush_stdio();
    match result {
        Ok(_) if STDOUT.load(Ordering::Relaxed) == FAILED => ExitCode::from(2),
        Ok(code) => ExitCode::from(code.clamp(0, 255)),
        Err(msg) => {
            eprintln!("sand: {msg}");
            ExitCode::from(2)
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())
}

struct RunOptions {
    config: Config,
    file: Option<PathBuf>,
    script_args: Vec<String>,
}

/// Parses run options into an engine configuration.
fn run_options(args: &[String]) -> Result<RunOptions, String> {
    parse_run_options(args, false)
}

/// Like [`run_options`]; with `files_only` every positional argument is a
/// file (`file` is the first, `script_args` the others) and `--` is denied.
fn parse_run_options(args: &[String], files_only: bool) -> Result<RunOptions, String> {
    // The policy file first, wherever it is given: the options add to it.
    let options = args.iter().position(|a| a == "--").map_or(args, |end| &args[..end]);
    let mut policy_files = options.iter().enumerate().filter_map(|(i, a)| match a.strip_prefix("--policy=") {
        Some(path) => Some(path),
        None => (a == "--policy").then(|| options.get(i + 1).filter(|v| !v.starts_with("--")).map_or("", String::as_str)),
    });
    let policy_file = match (policy_files.next(), policy_files.next()) {
        (Some(_), Some(_)) => return Err("--policy given more than once".into()),
        (Some(""), None) => return Err("--policy requires a value, e.g. --policy=FILE".into()),
        (Some(path), None) => Some(read_policy(Path::new(path))?),
        (None, _) => None,
    };
    let home = std::env::home_dir();
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?;
    let (mut rules, mut limits) = match policy_file {
        Some(f) => (f.rules, f.limits),
        None => Default::default(),
    };
    let mut allow_all = false;
    let mut prompt = true;
    let mut seed = None;
    let mut file = None;
    let mut script_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" && files_only {
            return Err("unexpected `--`".into());
        }
        if a == "--" {
            script_args.extend(args[i..].iter().cloned());
            break;
        }
        let (flag, mut value) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        if value.is_none() && TAKES_VALUE.contains(&flag) && i < options.len() && !args[i].starts_with("--") {
            value = Some(args[i].clone());
            i += 1;
        }
        let required =
            |v: &Option<String>| -> Result<String, String> { v.clone().ok_or_else(|| format!("{flag} requires a value, e.g. {flag}=...")) };
        match flag {
            "--allow" | "--deny" | "--prompt" => {
                let text = value.as_deref().ok_or_else(|| format!("{flag} requires a rule, e.g. {flag}='net api.example.com'"))?;
                rules = rules.extend(parse_cli_rule(&flag[2..], text, &cwd, home.as_deref())?);
            }
            "--allow-all" | "--no-prompt" if value.is_some() => return Err(format!("{flag} does not take a value")),
            "--allow-all" => allow_all = true,
            "--no-prompt" => prompt = false,
            "--policy" => {}
            "--max-memory" => limits.max_memory = Some(parse_size(&required(&value)?)?),
            "--max-time" => limits.max_wall_time = Some(parse_duration(&required(&value)?)?),
            "--max-cpu" => limits.max_cpu_time = Some(parse_duration(&required(&value)?)?),
            "--max-instructions" => {
                let v = required(&value)?;
                limits.max_instructions = Some(v.trim().parse().map_err(|_| format!("invalid --max-instructions value `{v}`"))?)
            }
            "--max-compile-fuel" => {
                let v = required(&value)?;
                limits.max_compile_fuel = Some(v.trim().parse().map_err(|_| format!("invalid --max-compile-fuel value `{v}`"))?)
            }
            "--seed" => {
                let v = required(&value)?;
                seed = Some(v.trim().parse::<u64>().map_err(|_| format!("invalid --seed value `{v}`"))?)
            }
            f if f.starts_with("--") => return Err(unknown_option(f)),
            _ => {
                if file.is_none() {
                    file = Some(PathBuf::from(a));
                } else {
                    // Arguments after the file go to the script.
                    script_args.push(a.clone());
                }
            }
        }
    }
    let policy = rules.with_defaults(default_rules(allow_all));
    let mut config = Config::new()
        .policy(policy.clone())
        .limits(limits)
        .inherit_stdin()
        .gc_stress(std::env::var_os("SAND_GC_STRESS").is_some())
        .dump_bytecode(std::env::var_os("SAND_DUMP_BYTECODE").is_some());
    if !files_only {
        config = config.args(script_args.clone());
    }
    config = config.prompter(prompter(prompt, policy));
    if let Some(s) = seed {
        config = config.seed(s);
    }
    if let Some(zone) = TimeZone::system() {
        config = config.time_zone(zone);
    }
    Ok(RunOptions { config, file, script_args })
}

const TAKES_VALUE: &[&str] = &[
    "--allow",
    "--deny",
    "--prompt",
    "--policy",
    "--max-memory",
    "--max-time",
    "--max-cpu",
    "--max-instructions",
    "--max-compile-fuel",
    "--seed",
];

fn read_policy(path: &Path) -> Result<PolicyFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read policy file {}: {e}", path.display()))?;
    PolicyFile::parse(&text, &policy_base(path), std::env::home_dir().as_deref()).map_err(|errors| {
        let lines: Vec<String> = errors.iter().map(|e| format!("{}:{e}", path.display())).collect();
        format!("invalid policy file\n{}", lines.join("\n"))
    })
}

fn policy_base(path: &Path) -> PathBuf {
    absolute(&path.to_string_lossy()).parent().map(Path::to_path_buf).unwrap_or_default()
}

fn is_policy(file: &str) -> bool {
    Path::new(file).extension().is_some_and(|e| e == "policy")
}

fn report_policy_errors(file: &str, errors: &[sandlang::policy::PolicyError]) -> u8 {
    for e in errors {
        eprintln!("{file}:{e}");
    }
    2
}

fn parse_cli_rule(verb: &str, text: &str, cwd: &Path, home: Option<&Path>) -> Result<RulePolicy, String> {
    if verb == "allow" && text.trim() == "all" {
        return Err(format!("`--allow={text}` is not a rule: use --allow-all to allow all capabilities by default"));
    }
    let file = PolicyFile::parse(&format!("{verb} {text}"), cwd, home).map_err(|errors| {
        let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        format!("invalid rule `--{verb}={text}`: {}", messages.join("; "))
    })?;
    Ok(file.rules)
}

/// Default fallback rules for `sand run`, `sand test`, and `sand repl`.
fn default_rules(all: bool) -> RulePolicy {
    if all { RulePolicy::allow_all() } else { RulePolicy::standard().allow_stdin().allow_timezone().extend(allow_all_imports_policy()) }
}

fn allow_all_imports_policy() -> RulePolicy {
    RulePolicy::new().extend(PolicyFile::parse("allow import **", Path::new("/"), None).expect("a valid rule").rules)
}

fn unknown_option(flag: &str) -> String {
    let hint = match flag {
        "--allow-read" => Some("use --allow='read ./PATH'"),
        "--allow-write" => Some("use --allow='write ./PATH'"),
        "--allow-net" => Some("use --allow='net HOST' for TLS, or --allow='net tls plaintext HOST'"),
        "--deny-net" => Some("use --deny='net HOST'"),
        "--allow-listen" => Some("use --allow='listen ADDR'"),
        "--allow-env" => Some("use --allow='env NAME'"),
        "--allow-private-net" => Some("specify internal hosts or IP ranges in `net` rules"),
        "--allow-stdin" | "--allow-timezone" => Some("scripts may read standard input and the time zone by default"),
        "--prompt-stdin" => Some("prompts use the controlling terminal, and --no-prompt denies unprompted requests"),
        _ => None,
    };
    match hint {
        Some(h) => format!("unknown option `{flag}`: {h}"),
        None => format!("unknown option `{flag}`"),
    }
}

fn absolute(p: &str) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p))
}

const PROMPT_ATTEMPTS: usize = 3;

/// Writes a prompt for `cap` to `output` and reads the user's answer from `input`.
fn prompt_stream(cap: &Capability, input: &mut impl BufRead, output: &mut impl Write) -> PromptAnswer {
    let question = format!("sand: allow the script to {}? [y]es once, [a]lways, [n]o, [N]ever: ", sanitize(&cap.to_string()));
    for _ in 0..PROMPT_ATTEMPTS {
        let _ = output.write_all(question.as_bytes());
        let _ = output.flush();
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = writeln!(output);
                return PromptAnswer::DenyOnce;
            }
            Ok(_) => {}
        }
        match line.trim() {
            "y" | "Y" | "yes" => return PromptAnswer::AllowOnce,
            "a" | "A" | "always" => return PromptAnswer::AllowAlways,
            "" | "n" | "no" => return PromptAnswer::DenyOnce,
            "N" | "never" => return PromptAnswer::DenyAlways,
            _ => {
                let _ = writeln!(output, "sand: please answer y, a, n or N");
            }
        }
    }
    PromptAnswer::DenyOnce
}

/// Escapes control, formatting, and non-ASCII whitespace characters so that
/// script-controlled text cannot spoof terminal output.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || (c.escape_debug().len() > 1 && !matches!(c, '\\' | '\'' | '"')) {
                c.escape_debug().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Soft terminal reset (DECSTR `ESC [ ! p`) and attribute reset (`ESC [ 0 m`)
/// written before each terminal prompt so that script output cannot hide or
/// alter the prompt text.
const TERMINAL_RESET: &str = if cfg!(windows) { "\r\n" } else { "\x1b[!p\x1b[0m\r\n" };
const PROMPT_MARKER: &str = "---- sand: permission request (the script is paused) ----\r\n";

fn prompt_terminal(cap: &Capability, input: &mut impl BufRead, output: &mut impl Write) -> PromptAnswer {
    let _ = output.write_all(TERMINAL_RESET.as_bytes());
    let _ = output.write_all(PROMPT_MARKER.as_bytes());
    prompt_stream(cap, input, output)
}

#[cfg(unix)]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    Ok((tty.try_clone()?, tty))
}

#[cfg(windows)]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let input = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$")?;
    let output = std::fs::OpenOptions::new().write(true).open("CONOUT$")?;
    Ok((input, output))
}

#[cfg(not(any(unix, windows)))]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "controlling terminal is unavailable"))
}

/// Prompts on the controlling terminal (`/dev/tty` or `CONIN$`/`CONOUT$`), or
/// denies if `terminal` is false or unavailable.
fn ask(cap: Capability, terminal: bool, policy: &RulePolicy) -> PromptAnswer {
    match open_terminal().ok().filter(|_| terminal) {
        Some((input, mut output)) => prompt_terminal(&cap, &mut std::io::BufReader::new(input), &mut output),
        None => deny_without_prompt(&cap, policy),
    }
}

/// Denies `cap` for the rest of the run and prints the `--allow` flag that grants it.
fn deny_without_prompt(cap: &Capability, policy: &RulePolicy) -> PromptAnswer {
    let hint = sandlang::policy::rule_for(cap, true)
        .and_then(|rule| Some((PolicyFile::parse(&rule, Path::new("/"), None).ok()?.rules, rule)))
        .filter(|(rule, text)| sanitize(text) == *text && rule.clone().extend(policy.clone()).check(cap) == sandlang::Decision::Allow)
        .map(|(_, rule)| format!("; pass --allow='{}' to allow it", rule.trim_start_matches("allow ").replace('\'', r"'\''")));
    eprintln!("sand: denied: {}", sanitize(&format!("{cap}{}", hint.unwrap_or_default())));
    PromptAnswer::DenyAlways
}

fn prompter(terminal: bool, policy: RulePolicy) -> impl Fn(Capability) -> sandlang::policy::PromptFuture + Send + Sync + 'static {
    let policy = std::sync::Arc::new(policy);
    move |cap: Capability| -> sandlang::policy::PromptFuture {
        let policy = policy.clone();
        Box::pin(async move { tokio::task::spawn_blocking(move || ask(cap, terminal, &policy)).await.unwrap_or(PromptAnswer::DenyOnce) })
    }
}

/// Reports an error on standard error; returns the exit status for it.
fn report_error(e: &Error) -> u8 {
    match e {
        Error::Compile(_) | Error::Runtime { .. } | Error::Internal { .. } => eprint!("{}", e.render()),
        e => eprintln!("sand: {e}"),
    }
    exit_status(e.exit_code())
}

fn cmd_run(args: &[String]) -> Result<u8, String> {
    let opts = run_options(args)?;
    let file = opts.file.ok_or("missing FILE to run")?;
    let mut engine = Engine::new(opts.config.exit_after_run(true)).map_err(|e| e.render())?;
    let rt = runtime()?;
    let r = rt.block_on(engine.run_file(&file));
    if std::env::var_os("SAND_GC_STATS").is_some() {
        eprintln!("{:#?}", engine.gc_stats());
    }
    match r {
        Ok(o) => Ok(exit_status(o.exit_code)),
        Err(e) => Ok(report_error(&e)),
    }
}

/// The process exit status for a script's `exit(code)`: codes outside 0..=255
/// become 255, so that a failure code never turns into success.
fn exit_status(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(255)
}

/// Splits arguments into files and the given boolean `flags` (which are
/// returned in order of `flags`); any other option is an error.
fn files_and_flags<'a>(args: &'a [String], flags: &[&str]) -> Result<(Vec<&'a String>, Vec<bool>), String> {
    let mut set = vec![false; flags.len()];
    let mut files = Vec::new();
    for a in args {
        if let Some(i) = flags.iter().position(|f| f == a) {
            set[i] = true;
        } else if a.starts_with('-') && a != "-" {
            return Err(format!("unknown option `{a}`"));
        } else {
            files.push(a);
        }
    }
    if files.is_empty() {
        return Err("missing FILE".into());
    }
    Ok((files, set))
}

fn cmd_check(args: &[String]) -> Result<u8, String> {
    let (files, _) = files_and_flags(args, &[])?;
    let mut code = 0;
    for f in files {
        let path = Path::new(f);
        if is_policy(f) {
            match read_policy(path) {
                Ok(_) => outln!("{f}: ok"),
                Err(e) => {
                    eprintln!("sand: {e}");
                    code = code.max(2);
                }
            }
            continue;
        }
        let engine = Engine::new(Config::new().policy(allow_all_imports_policy())).map_err(|e| e.render())?;
        match engine.check_file(path) {
            Ok(()) => outln!("{f}: ok"),
            Err(e) => code = code.max(report_error(&e)),
        }
    }
    Ok(code)
}

fn cmd_lint(args: &[String]) -> Result<u8, String> {
    let (files, flags) = files_and_flags(args, &["--allow-warnings"])?;
    let allow = flags[0];
    let mut count = 0;
    for f in files {
        let warnings = if is_policy(f) {
            let text = std::fs::read_to_string(f).map_err(|e| format!("cannot read {f}: {e}"))?;
            match sandlang::policy::lint_policy(&text, &policy_base(Path::new(f)), std::env::home_dir().as_deref()) {
                Ok(ws) => Ok(ws),
                Err(errors) => return Ok(report_policy_errors(f, &errors)),
            }
        } else {
            Engine::new(Config::new().policy(allow_all_imports_policy())).and_then(|e| e.lint_file(f)).map_err(|e| e.to_string())
        };
        match warnings {
            Ok(ws) => {
                for w in ws {
                    outln!("{f}:{w}");
                    count += 1;
                }
            }
            Err(e) => {
                eprintln!("{f}: {e}");
                return Ok(2);
            }
        }
    }
    Ok(if count > 0 && !allow { 1 } else { 0 })
}

fn cmd_fmt(args: &[String]) -> Result<u8, String> {
    let (files, flags) = files_and_flags(args, &["--check"])?;
    let check = flags[0];
    if files.iter().any(|f| *f == "-") {
        if files.len() > 1 {
            return Err("`sand fmt -` formats standard input only".into());
        }
        return fmt_stdin(check);
    }
    let mut code = 0;
    for f in files {
        let src = std::fs::read_to_string(f).map_err(|e| format!("cannot read {f}: {e}"))?;
        let formatted = if is_policy(f) {
            match sandlang::policy::format_policy(&src) {
                Ok(out) => Ok(out),
                Err(errors) => {
                    code = report_policy_errors(f, &errors);
                    continue;
                }
            }
        } else {
            sandlang::fmt::format_source(&src)
        };
        match formatted {
            Ok(out) => {
                if out != src {
                    if check {
                        outln!("{f}: not formatted");
                        code = 1;
                    } else {
                        std::fs::write(f, out).map_err(|e| format!("cannot write {f}: {e}"))?;
                        outln!("{f}: formatted");
                    }
                }
            }
            Err(e) => {
                eprintln!("{f}: {e}");
                code = 2;
            }
        }
    }
    Ok(code)
}

/// `sand fmt -`: formats standard input to standard output (with
/// `--check`, prints nothing and exits with 1 if it is not formatted).
fn fmt_stdin(check: bool) -> Result<u8, String> {
    let mut src = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut src).map_err(|e| format!("cannot read standard input: {e}"))?;
    match sandlang::fmt::format_source(&src) {
        Ok(out) if check => Ok(if out == src { 0 } else { 1 }),
        Ok(out) => {
            out!("{out}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("<stdin>: {e}");
            Ok(2)
        }
    }
}

/// Runs test blocks under the same policy and limits as `sand run` (tests
/// are code like any other: everything is denied unless allowed).
fn cmd_test(args: &[String]) -> Result<u8, String> {
    let opts = parse_run_options(args, true)?;
    let files: Vec<PathBuf> = opts.file.into_iter().chain(opts.script_args.iter().map(PathBuf::from)).collect();
    if files.is_empty() {
        return Err("missing FILE".into());
    }
    let rt = runtime()?;
    let mut failed = 0;
    for f in &files {
        let mut engine = Engine::new(opts.config.clone()).map_err(|e| e.render())?;
        match rt.block_on(engine.run_tests(f)) {
            Ok(report) => {
                out!("{}", report.render());
                failed += report.failed();
            }
            Err(e) => return Ok(report_error(&e)),
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run_prompt(cap: &Capability, input: &str) -> (PromptAnswer, String) {
        let mut out = Vec::new();
        let answer = prompt_stream(cap, &mut input.as_bytes(), &mut out);
        (answer, String::from_utf8(out).unwrap())
    }

    struct FailingWriter(std::io::ErrorKind, usize);

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.1 += 1;
            Err(self.0.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_stops_after_an_error() {
        for (kind, after) in [(std::io::ErrorKind::BrokenPipe, CLOSED), (std::io::ErrorKind::StorageFull, FAILED)] {
            let (mut out, state) = (FailingWriter(kind, 0), AtomicU8::new(OPEN));
            write_to(&mut out, &state, format_args!("a"));
            write_to(&mut out, &state, format_args!("b"));
            assert_eq!((out.1, state.load(Ordering::Relaxed)), (1, after), "{kind:?}");
        }
    }

    fn read_cap() -> Capability {
        Capability::FsRead(PathBuf::from("data.txt"))
    }

    #[test]
    fn prompt_answers() {
        let cases = [
            ("y\n", PromptAnswer::AllowOnce),
            ("Y\n", PromptAnswer::AllowOnce),
            ("yes\n", PromptAnswer::AllowOnce),
            ("  y \r\n", PromptAnswer::AllowOnce),
            ("a\n", PromptAnswer::AllowAlways),
            ("A\n", PromptAnswer::AllowAlways),
            ("always\n", PromptAnswer::AllowAlways),
            ("n\n", PromptAnswer::DenyOnce),
            ("no\n", PromptAnswer::DenyOnce),
            ("\n", PromptAnswer::DenyOnce),
            ("N\n", PromptAnswer::DenyAlways),
            ("never\n", PromptAnswer::DenyAlways),
            // The last line need not be terminated.
            ("a", PromptAnswer::AllowAlways),
        ];
        for (input, want) in cases {
            let (got, out) = run_prompt(&read_cap(), input);
            assert_eq!(got, want, "input {input:?}");
            assert_eq!(out, "sand: allow the script to read data.txt? [y]es once, [a]lways, [n]o, [N]ever: ", "input {input:?}");
        }
    }

    #[test]
    fn prompt_asks_again_after_invalid_input() {
        let (got, out) = run_prompt(&read_cap(), "maybe\nyess\na\ny\n");
        assert_eq!(got, PromptAnswer::AllowAlways);
        assert_eq!(out.matches("allow the script to read data.txt?").count(), 3);
        assert_eq!(out.matches("please answer y, a, n or N").count(), 2);
    }

    #[test]
    fn prompt_denies_after_repeated_invalid_input() {
        let (got, out) = run_prompt(&read_cap(), "1\n2\n3\ny\n");
        assert_eq!(got, PromptAnswer::DenyOnce);
        assert_eq!(out.matches("allow the script").count(), PROMPT_ATTEMPTS);
    }

    #[test]
    fn prompt_denies_at_end_of_input_or_on_error() {
        assert_eq!(run_prompt(&read_cap(), "").0, PromptAnswer::DenyOnce);
        assert_eq!(run_prompt(&read_cap(), "what\n").0, PromptAnswer::DenyOnce);

        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("terminal went away"))
            }
        }
        let mut out = Vec::new();
        assert_eq!(prompt_stream(&read_cap(), &mut std::io::BufReader::new(Broken), &mut out), PromptAnswer::DenyOnce);
        assert_eq!(prompt_stream(&read_cap(), &mut &b"\xff\xfe\n"[..], &mut out), PromptAnswer::DenyOnce);
    }

    #[test]
    fn prompt_escapes_control_characters_from_the_script() {
        let cap = Capability::FsRead(PathBuf::from("x\nsand: allow the script to read safe.txt? \u{1b}[2K"));
        let (_, out) = run_prompt(&cap, "n\n");
        assert_eq!(out.lines().count(), 1, "{out:?}");
        assert!(out.contains("x\\nsand: allow") && out.contains("\\u{1b}[2K"), "{out:?}");
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn prompt_escapes_invisible_and_reordering_characters() {
        let tricky = [
            '\u{202e}',
            '\u{2066}',
            '\u{200b}',
            '\u{200f}',
            '\u{feff}',
            '\u{2028}',
            '\u{2029}',
            '\u{a0}',
            '\u{3000}',
            '\u{e0041}',
            '\u{85}',
        ];
        for c in tricky {
            let cap = Capability::FsRead(PathBuf::from(format!("/tmp/a{c}b")));
            let (_, out) = run_prompt(&cap, "n\n");
            assert!(!out.contains(c), "{c:?} not escaped: {out:?}");
            assert!(out.contains(&c.escape_unicode().to_string()), "{out:?}");
        }
        let cap = Capability::FsRead(PathBuf::from("/tmp/données/日本語 file's \"x\"\\y"));
        let (_, out) = run_prompt(&cap, "n\n");
        assert!(out.contains("/tmp/données/日本語 file's \"x\"\\y?"), "{out:?}");
    }

    #[test]
    fn terminal_prompt_resets_the_terminal_first() {
        let mut out = Vec::new();
        let answer = prompt_terminal(&read_cap(), &mut "n\n".as_bytes(), &mut out);
        assert_eq!(answer, PromptAnswer::DenyOnce);
        let out = String::from_utf8(out).unwrap();
        let marker = out.find(PROMPT_MARKER).unwrap();
        let question = out.find("sand: allow the script to read data.txt?").unwrap();
        assert!(out.starts_with(TERMINAL_RESET) && marker < question, "{out:?}");
        if cfg!(not(windows)) {
            assert!(TERMINAL_RESET.contains("\x1b[0m") && TERMINAL_RESET.contains("\x1b[!p"));
        }
    }

    #[test]
    fn exit_status_never_turns_failure_into_success() {
        assert_eq!(exit_status(0), 0);
        assert_eq!(exit_status(7), 7);
        assert_eq!(exit_status(255), 255);
        assert_eq!(exit_status(256), 255);
        assert_eq!(exit_status(-1), 255);
        assert_eq!(exit_status(i32::MIN), 255);
    }

    fn opts(args: &[&str]) -> Result<RunOptions, String> {
        run_options(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn run_option_parsing() {
        let o = opts(&["main.sand", "a", "--seed=3", "--", "--b", "c"]).unwrap();
        assert_eq!(o.file, Some(PathBuf::from("main.sand")));
        assert_eq!(o.script_args, ["a", "--b", "c"]);
        for bad in [
            &["f.sand", "--max-memory"][..],
            &["f.sand", "--max-memory=12Q"],
            &["f.sand", "--max-time=soon"],
            &["f.sand", "--max-cpu=-1s"],
            &["f.sand", "--max-instructions=many"],
            &["f.sand", "--seed=-1"],
            &["f.sand", "--seed"],
            &["f.sand", "--allow-read"],
            &["f.sand", "--allow-stdin=no"],
            &["f.sand", "--allow-all=yes"],
            &["f.sand", "--prompt=always"],
            &["f.sand", "--allow-everything"],
        ] {
            assert!(opts(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(parse_size("1K"), Ok(1024));
        assert_eq!(parse_size("2m"), Ok(2 << 20));
        assert_eq!(parse_size("1G"), Ok(1 << 30));
        assert_eq!(parse_duration("1.5s"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
    }

    #[test]
    fn subcommand_option_parsing() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let a = args(&["x.sand", "--check", "y.sand"]);
        let (files, flags) = files_and_flags(&a, &["--check"]).unwrap();
        assert_eq!(files, ["x.sand", "y.sand"]);
        assert_eq!(flags, [true]);
        assert!(files_and_flags(&args(&["x.sand", "--chek"]), &["--check"]).is_err());
        assert!(files_and_flags(&args(&["--check"]), &["--check"]).is_err());
        assert!(files_and_flags(&args(&["x.sand", "-v"]), &[]).is_err());
    }
}
