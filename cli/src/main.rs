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
    sand check FILE...                     type check (FILE.policy: validate a policy)
    sand lint FILE... [--allow-warnings]   report lint warnings (also of FILE.policy)
    sand fmt FILE... [--check]             format files in place (or check; also FILE.policy)
    sand fmt - [--check]                   format standard input to standard output
    sand test FILE... [OPTIONS]            run test blocks (options as for run)
    sand repl [OPTIONS]                    interactive session
    sand --version
    sand COMMAND --help                    show this help

RUN, TEST AND REPL OPTIONS:
    Scripts may print, read standard input and the time zone, and import modules.
    For anything else, sand asks on the terminal unless a rule decides.

    --allow=RULE             allow what a policy file rule allows, e.g.
                             --allow='read ./data' --allow='net api.example.com'
                             --allow='net plaintext localhost:8080' --allow='env HOME'
    --deny=RULE              deny what a rule matches, e.g. --deny='net 10.0.0.0/8'
    --prompt=RULE            ask about what a rule matches, e.g. --prompt='write ./out'
    --policy=FILE            rules and limits from a policy file (the options add
                             to its rules and override its limits)
    --allow-all              allow everything (trusted scripts only)
    --no-prompt              deny instead of asking
    --max-memory=SIZE        heap limit, e.g. 256M (default 512M)
    --max-time=DURATION      wall-clock limit, e.g. 10s, 500ms, 2m
    --max-cpu=DURATION       CPU time limit
    --max-instructions=N     instruction (fuel) limit
    --max-compile-fuel=N     type checking work limit (default 10000000)
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
    let mut policy_files = options.iter().filter_map(|a| a.strip_prefix("--policy=").or((a == "--policy").then_some("")));
    let policy_file = match (policy_files.next(), policy_files.next()) {
        (Some(_), Some(_)) => return Err("--policy given more than once".into()),
        (Some(""), None) => return Err("--policy requires a value (--policy=FILE)".into()),
        (Some(path), None) => Some(read_policy(Path::new(path))?),
        (None, _) => None,
    };
    let home = std::env::home_dir();
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?;
    // The user at the terminal is the host: scripts may read standard
    // input and the time zone and import any module unless a rule says otherwise.
    let base = RulePolicy::standard().allow_stdin().allow_timezone().extend(any_import());
    let (mut rules, mut limits) = match policy_file {
        Some(f) => (base.extend(f.rules), f.limits),
        None => (base, Default::default()),
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
        let (flag, value) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let required =
            |v: &Option<String>| -> Result<String, String> { v.clone().ok_or_else(|| format!("{flag} requires a value ({flag}=...)")) };
        match flag {
            "--allow" | "--deny" | "--prompt" => {
                let text = value.as_deref().ok_or_else(|| format!("{flag} requires a rule, e.g. {flag}='net api.example.com'"))?;
                rules = rules.extend(rule(&flag[2..], text, &cwd, home.as_deref())?);
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
    let mut config = Config::new()
        .policy(if allow_all { RulePolicy::allow_all() } else { rules })
        .limits(limits)
        .inherit_stdin()
        .gc_stress(std::env::var_os("SAND_GC_STRESS").is_some())
        .dump_bytecode(std::env::var_os("SAND_DUMP_BYTECODE").is_some());
    if !files_only {
        config = config.args(script_args.clone());
    }
    if prompt {
        config = config.prompter(prompter());
    }
    if let Some(s) = seed {
        config = config.seed(s);
    }
    if let Some(zone) = TimeZone::system() {
        config = config.time_zone(zone);
    }
    Ok(RunOptions { config, file, script_args })
}

/// Reads and parses a policy file (relative paths in it are relative to
/// its directory).
fn read_policy(path: &Path) -> Result<PolicyFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read policy file {}: {e}", path.display()))?;
    PolicyFile::parse(&text, &policy_base(path), std::env::home_dir().as_deref()).map_err(|errors| {
        let lines: Vec<String> = errors.iter().map(|e| format!("{}:{e}", path.display())).collect();
        format!("invalid policy file\n{}", lines.join("\n"))
    })
}

/// The directory relative paths in the policy file at `path` start from.
fn policy_base(path: &Path) -> PathBuf {
    absolute(&path.to_string_lossy()).parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Check whether `file` is a policy file.
fn is_policy(file: &str) -> bool {
    Path::new(file).extension().is_some_and(|e| e == "policy")
}

/// Prints a policy file's errors; returns the exit status for them.
fn report_policy_errors(file: &str, errors: &[sandlang::policy::PolicyError]) -> u8 {
    for e in errors {
        eprintln!("{file}:{e}");
    }
    2
}

/// The rules of the policy file line `{verb} {text}`, with relative paths
/// starting from `cwd`.
fn rule(verb: &str, text: &str, cwd: &Path, home: Option<&Path>) -> Result<RulePolicy, String> {
    let file = PolicyFile::parse(&format!("{verb} {text}"), cwd, home).map_err(|errors| {
        let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        format!("invalid rule `--{verb}={text}`: {}", messages.join("; "))
    })?;
    Ok(file.rules)
}

/// The policy of `sand check` and `sand lint`, which compile every module
/// a script imports and run no code.
fn any_import() -> RulePolicy {
    RulePolicy::new().extend(PolicyFile::parse("allow import **", Path::new("/"), None).expect("a valid rule").rules)
}

/// The error for an unknown option, with the replacement of a removed one.
fn unknown_option(flag: &str) -> String {
    let rule = match flag {
        "--allow-read" => Some("--allow='read ./PATH'"),
        "--allow-write" => Some("--allow='write ./PATH'"),
        "--allow-net" => Some("--allow='net HOST'"),
        "--deny-net" => Some("--deny='net HOST'"),
        "--allow-listen" => Some("--allow='listen ADDR'"),
        "--allow-env" => Some("--allow='env NAME'"),
        "--allow-private-net" => Some("--allow='net 10.0.0.0/8' (or the hosts the script needs)"),
        "--allow-stdin" | "--allow-timezone" => Some("nothing: scripts may read standard input and the time zone by default"),
        "--prompt-stdin" => Some("nothing: questions go to the terminal, and --no-prompt denies instead"),
        _ => None,
    };
    match rule {
        Some(r) => format!("unknown option `{flag}` (use {r})"),
        None => format!("unknown option `{flag}`"),
    }
}

fn absolute(p: &str) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// How many unrecognised answers are tolerated before denying.
const PROMPT_ATTEMPTS: usize = 3;

/// The prompt dialogue: asks about `cap` on `output` and reads the answer from
/// `input`. An empty answer, end of input or an I/O error denies (once).
fn prompt_dialogue(cap: &Capability, input: &mut impl BufRead, output: &mut impl Write) -> PromptAnswer {
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

/// Escapes control characters (newlines, terminal escape sequences) and
/// other invisible or layout-changing characters (Unicode format characters
/// such as bidirectional overrides, line and paragraph separators, unusual
/// spaces, combining marks) so that script-chosen text such as a file name
/// cannot spoof the question.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            // `escape_debug` escapes exactly the characters Rust does not
            // consider printable (categories Cc, Cf, Zl, Zp, Zs except the
            // ASCII space, unassigned, ...) and grapheme extenders, besides
            // quotes and backslashes, which are kept.
            if c.is_control() || (c.escape_debug().len() > 1 && !matches!(c, '\\' | '\'' | '"')) {
                c.escape_debug().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Written to the terminal before each prompt, so that whatever state the
/// script's output left the terminal in (concealed or invisible colors,
/// alternate character sets, a half-written line), the question starts on a
/// fresh line in the default rendition, after a marker line. `ESC [ ! p` is
/// a soft terminal reset (DECSTR), `ESC [ 0 m` resets character attributes.
const TERMINAL_RESET: &str = if cfg!(windows) { "\r\n" } else { "\x1b[!p\x1b[0m\r\n" };
const PROMPT_MARKER: &str = "---- sand: permission request (the script is paused) ----\r\n";

/// The prompt dialogue on a terminal: resets the terminal's rendition first.
fn terminal_dialogue(cap: &Capability, input: &mut impl BufRead, output: &mut impl Write) -> PromptAnswer {
    let _ = output.write_all(TERMINAL_RESET.as_bytes());
    let _ = output.write_all(PROMPT_MARKER.as_bytes());
    prompt_dialogue(cap, input, output)
}

/// Opens the controlling terminal for reading and writing.
#[cfg(unix)]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    Ok((tty.try_clone()?, tty))
}

/// Opens the console for reading and writing.
#[cfg(windows)]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let input = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$")?;
    let output = std::fs::OpenOptions::new().write(true).open("CONOUT$")?;
    Ok((input, output))
}

#[cfg(not(any(unix, windows)))]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no terminal"))
}

/// Asks the user whether to allow a capability (blocking) on the controlling
/// terminal (`/dev/tty`, or `CONIN$`/`CONOUT$` on Windows), never on the
/// script's standard streams: piped data must not answer prompts.
fn ask(cap: Capability) -> PromptAnswer {
    match open_terminal() {
        Ok((input, mut output)) => terminal_dialogue(&cap, &mut std::io::BufReader::new(input), &mut output),
        Err(_) => {
            eprintln!("sand: no terminal to ask for permission; denied: {}", sanitize(&cap.to_string()));
            PromptAnswer::DenyOnce
        }
    }
}

/// The prompter of `sand run`, `sand test` and `sand repl` (unless `--no-prompt`).
fn prompter() -> impl Fn(Capability) -> sandlang::policy::PromptFuture + Send + Sync + 'static {
    move |cap: Capability| -> sandlang::policy::PromptFuture {
        Box::pin(async move { tokio::task::spawn_blocking(move || ask(cap)).await.unwrap_or(PromptAnswer::DenyOnce) })
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
        // Checking runs no script code; imports are confined to the module root.
        let engine = Engine::new(Config::new().policy(any_import())).map_err(|e| e.render())?;
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
            Engine::new(Config::new().policy(any_import())).and_then(|e| e.lint_file(f)).map_err(|e| e.to_string())
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

    fn dialogue(cap: &Capability, input: &str) -> (PromptAnswer, String) {
        let mut out = Vec::new();
        let answer = prompt_dialogue(cap, &mut input.as_bytes(), &mut out);
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
            let (got, out) = dialogue(&read_cap(), input);
            assert_eq!(got, want, "input {input:?}");
            assert_eq!(out, "sand: allow the script to read data.txt? [y]es once, [a]lways, [n]o, [N]ever: ", "input {input:?}");
        }
    }

    #[test]
    fn prompt_asks_again_after_invalid_input() {
        let (got, out) = dialogue(&read_cap(), "maybe\nyess\na\ny\n");
        assert_eq!(got, PromptAnswer::AllowAlways);
        assert_eq!(out.matches("allow the script to read data.txt?").count(), 3);
        assert_eq!(out.matches("please answer y, a, n or N").count(), 2);
    }

    #[test]
    fn prompt_denies_after_repeated_invalid_input() {
        let (got, out) = dialogue(&read_cap(), "1\n2\n3\ny\n");
        assert_eq!(got, PromptAnswer::DenyOnce);
        assert_eq!(out.matches("allow the script").count(), PROMPT_ATTEMPTS);
    }

    #[test]
    fn prompt_denies_at_end_of_input_or_on_error() {
        assert_eq!(dialogue(&read_cap(), "").0, PromptAnswer::DenyOnce);
        assert_eq!(dialogue(&read_cap(), "what\n").0, PromptAnswer::DenyOnce);

        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("terminal went away"))
            }
        }
        let mut out = Vec::new();
        assert_eq!(prompt_dialogue(&read_cap(), &mut std::io::BufReader::new(Broken), &mut out), PromptAnswer::DenyOnce);
        // Invalid UTF-8 is an error too.
        assert_eq!(prompt_dialogue(&read_cap(), &mut &b"\xff\xfe\n"[..], &mut out), PromptAnswer::DenyOnce);
    }

    #[test]
    fn prompt_escapes_control_characters_from_the_script() {
        let cap = Capability::FsRead(PathBuf::from("x\nsand: allow the script to read safe.txt? \u{1b}[2K"));
        let (_, out) = dialogue(&cap, "n\n");
        assert_eq!(out.lines().count(), 1, "{out:?}");
        assert!(out.contains("x\\nsand: allow") && out.contains("\\u{1b}[2K"), "{out:?}");
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn prompt_escapes_invisible_and_reordering_characters() {
        // Bidirectional overrides and isolates, zero-width and other format
        // characters, line/paragraph separators, unusual spaces.
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
            let (_, out) = dialogue(&cap, "n\n");
            assert!(!out.contains(c), "{c:?} not escaped: {out:?}");
            assert!(out.contains(&c.escape_unicode().to_string()), "{out:?}");
        }
        // Ordinary text is left alone.
        let cap = Capability::FsRead(PathBuf::from("/tmp/données/日本語 file's \"x\"\\y"));
        let (_, out) = dialogue(&cap, "n\n");
        assert!(out.contains("/tmp/données/日本語 file's \"x\"\\y?"), "{out:?}");
    }

    #[test]
    fn terminal_prompt_resets_the_terminal_first() {
        let mut out = Vec::new();
        let answer = terminal_dialogue(&read_cap(), &mut "n\n".as_bytes(), &mut out);
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
