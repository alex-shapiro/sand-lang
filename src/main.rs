//! The `sand` command-line tool.
#![forbid(unsafe_code)]

use sandlang::{Capability, Config, Decision, Engine, Error, Limits, Policy, PromptAnswer, RulePolicy};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
sand — the Sand language

USAGE:
    sand run FILE [OPTIONS] [-- ARGS...]   run a script
    sand check FILE...                     type check
    sand lint FILE... [--allow-warnings]   report lint warnings
    sand fmt FILE... [--check]             format files in place (or check)
    sand test FILE... [OPTIONS]            run test blocks (options as for run)
    sand repl [OPTIONS]                    interactive session
    sand --version

RUN, TEST AND REPL OPTIONS (all access is denied unless allowed):
    --allow-read=PATHS       comma-separated directories readable by the script
    --allow-write=PATHS      directories writable (and readable) by the script
    --allow-net=HOSTS        hosts the script may connect to (host, host:port, *.domain,
                             IP address or CIDR range such as 10.0.0.0/8)
    --deny-net=HOSTS         hosts, addresses or ranges the script may not connect to
    --allow-private-net      allow connections to private and internal addresses
                             (loopback, RFC 1918, link-local, ...); by default only
                             IP literals, ranges and localhost allowed explicitly
    --allow-listen=ADDRS     addresses the script may listen on (host:port, port 0 = any)
    --allow-env=NAMES        environment variables the script may read
    --allow-stdin            allow reading standard input
    --allow-all              allow everything (trusted scripts only)
    --prompt                 ask on the terminal for anything not allowed
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
    let result = match cmd.as_str() {
        "run" => cmd_run(rest),
        "check" => cmd_check(rest),
        "lint" => cmd_lint(rest),
        "fmt" => cmd_fmt(rest),
        "test" => cmd_test(rest),
        "repl" => cmd_repl(rest),
        "--version" | "-V" | "version" => {
            println!("sand {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
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

fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1usize << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<usize>().map(|n| n.saturating_mul(mult)).map_err(|_| format!("invalid size `{s}`"))
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let units: [(&str, f64); 6] = [("ms", 1e-3), ("us", 1e-6), ("ns", 1e-9), ("s", 1.0), ("m", 60.0), ("h", 3600.0)];
    for (u, mult) in units {
        if let Some(n) = s.strip_suffix(u)
            && let Ok(v) = n.parse::<f64>()
            && v >= 0.0
            && v.is_finite()
        {
            return Ok(Duration::from_secs_f64(v * mult));
        }
    }
    Err(format!("invalid duration `{s}` (use e.g. 500ms, 10s, 2m)"))
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
/// file (`file` is the first, `script_args` the others) and `--` is refused.
fn parse_run_options(args: &[String], files_only: bool) -> Result<RunOptions, String> {
    let mut rules = RulePolicy::new();
    let mut allow_all = false;
    let mut allow_private_net = false;
    let mut prompt = None;
    let mut limits = Limits::default();
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
        let list = |v: &Option<String>| -> Result<Vec<String>, String> {
            match v {
                Some(v) => Ok(v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()),
                None => Err(format!("{flag} requires a value ({flag}=...)")),
            }
        };
        let required =
            |v: &Option<String>| -> Result<String, String> { v.clone().ok_or_else(|| format!("{flag} requires a value ({flag}=...)")) };
        match flag {
            "--allow-read" => {
                for p in list(&value)? {
                    rules = rules.allow_read(absolute(&p));
                }
            }
            "--allow-write" => {
                for p in list(&value)? {
                    rules = rules.allow_write(absolute(&p));
                }
            }
            "--allow-net" => {
                for h in list(&value)? {
                    rules = rules.allow_net(&net_pattern(&h)?);
                }
            }
            "--deny-net" => {
                for h in list(&value)? {
                    rules = rules.deny_net(&net_pattern(&h)?);
                }
            }
            "--allow-listen" => {
                for h in list(&value)? {
                    rules = rules.allow_listen(&h);
                }
            }
            "--allow-env" => {
                for n in list(&value)? {
                    rules = rules.allow_env(&n);
                }
            }
            "--allow-stdin" | "--allow-all" | "--allow-private-net" | "--prompt" | "--prompt-stdin" if value.is_some() => {
                return Err(format!("{flag} does not take a value"));
            }
            "--allow-stdin" => rules = rules.allow_stdin(),
            "--allow-private-net" => allow_private_net = true,
            "--allow-all" => allow_all = true,
            "--prompt" => prompt = prompt.or(Some(PromptVia::Terminal)),
            "--prompt-stdin" => prompt = Some(PromptVia::Stdin),
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
            f if f.starts_with("--") => return Err(format!("unknown option `{f}`")),
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
    if prompt.is_some() {
        rules = rules.fallback(Decision::Prompt);
    }
    if !allow_private_net {
        rules = rules.deny_private_networks();
    }
    let policy = if allow_all { Policy::allow_all() } else { Policy::rules(rules) };
    let mut config = Config::new().policy(policy).limits(limits);
    if !files_only {
        config = config.args(script_args.clone());
    }
    match prompt {
        // Prompt answers come from standard input, so the script gets none.
        Some(PromptVia::Stdin) => config = config.prompter(prompter(PromptVia::Stdin)),
        Some(via) => config = config.inherit_stdin().prompter(prompter(via)),
        None => config = config.inherit_stdin(),
    }
    if let Some(s) = seed {
        config = config.seed(s);
    }
    Ok(RunOptions { config, file, script_args })
}

/// Rejects malformed network patterns (which would silently match nothing).
fn net_pattern(p: &str) -> Result<String, String> {
    if sandlang::policy::is_valid_net_pattern(p) { Ok(p.to_string()) } else { Err(format!("invalid network pattern `{p}`")) }
}

fn absolute(p: &str) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// Where `--prompt` asks its questions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptVia {
    /// The controlling terminal (`/dev/tty`, or `CONIN$`/`CONOUT$` on Windows),
    /// never the script's standard streams: piped data must not answer prompts.
    Terminal,
    /// Standard input and error (hidden `--prompt-stdin` option, for tests and
    /// automation; whoever writes the command line could pass `--allow-all` anyway).
    Stdin,
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
/// script's output left the terminal in (concealed or invisible colours,
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

/// Asks the user whether to allow a capability (blocking).
fn ask(cap: Capability, via: PromptVia) -> PromptAnswer {
    match via {
        PromptVia::Terminal => match open_terminal() {
            Ok((input, mut output)) => terminal_dialogue(&cap, &mut std::io::BufReader::new(input), &mut output),
            Err(_) => {
                eprintln!("sand: no terminal to ask for permission; denied: {}", sanitize(&cap.to_string()));
                PromptAnswer::DenyOnce
            }
        },
        PromptVia::Stdin => prompt_dialogue(&cap, &mut std::io::stdin().lock(), &mut std::io::stderr()),
    }
}

/// The prompter installed by `--prompt` (and `--prompt-stdin`).
fn prompter(via: PromptVia) -> impl Fn(Capability) -> sandlang::policy::PromptFuture + Send + Sync + 'static {
    move |cap: Capability| -> sandlang::policy::PromptFuture {
        Box::pin(async move { tokio::task::spawn_blocking(move || ask(cap, via)).await.unwrap_or(PromptAnswer::DenyOnce) })
    }
}

fn report_error(e: &Error, main: Option<&Path>) -> u8 {
    match e {
        Error::Compile(diags) => {
            let root = main.and_then(|m| m.parent()).map(Path::to_path_buf).unwrap_or_default();
            // Each file is read once, however many diagnostics it has.
            let mut sources: std::collections::HashMap<&str, Option<String>> = std::collections::HashMap::new();
            for d in diags {
                let source = sources.entry(d.file.as_str()).or_insert_with(|| {
                    main.filter(|m| m.file_name().map(|n| n.to_string_lossy() == d.file).unwrap_or(false))
                        .and_then(|m| std::fs::read_to_string(m).ok())
                        .or_else(|| std::fs::read_to_string(root.join(&d.file)).ok())
                });
                eprint!("{}", d.render(source.as_deref()));
            }
            2
        }
        Error::Runtime { .. } => {
            eprintln!("{e}");
            1
        }
        Error::Limit(_) => {
            eprintln!("sand: {e}");
            3
        }
        Error::Interrupted => {
            eprintln!("sand: interrupted");
            130
        }
        Error::Io(m) => {
            eprintln!("sand: {m}");
            2
        }
    }
}

fn cmd_run(args: &[String]) -> Result<u8, String> {
    let opts = run_options(args)?;
    let file = opts.file.ok_or("missing FILE to run")?;
    let _ = opts.script_args;
    let mut engine = Engine::new(opts.config);
    let rt = runtime()?;
    match rt.block_on(engine.run_file(&file)) {
        Ok(o) => Ok(exit_status(o.exit_code)),
        Err(e) => Ok(report_error(&e, Some(&file))),
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
        } else if a.starts_with('-') && a.len() > 1 {
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
        // Checking runs no script code; imports are confined to the module root.
        let mut engine = Engine::new(Config::new().policy(Policy::deny_all()));
        match engine.check_file(path) {
            Ok(_) => println!("{f}: ok"),
            Err(e) => {
                report_error(&e, Some(path));
                // 1 for errors in the program (SPEC 17.1), 2 if it cannot be read.
                code = code.max(if matches!(e, Error::Compile(_)) { 1 } else { 2 });
            }
        }
    }
    Ok(code)
}

fn cmd_lint(args: &[String]) -> Result<u8, String> {
    let (files, flags) = files_and_flags(args, &["--allow-warnings"])?;
    let allow = flags[0];
    let mut count = 0;
    for f in files {
        match sandlang::lint::lint_file(Path::new(f)) {
            Ok(ws) => {
                for w in ws {
                    println!("{f}:{w}");
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
    let mut code = 0;
    for f in files {
        let src = std::fs::read_to_string(f).map_err(|e| format!("cannot read {f}: {e}"))?;
        match sandlang::fmt::format_source(&src) {
            Ok(out) => {
                if out != src {
                    if check {
                        println!("{f}: not formatted");
                        code = 1;
                    } else {
                        std::fs::write(f, out).map_err(|e| format!("cannot write {f}: {e}"))?;
                        println!("{f}: formatted");
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
        let mut engine = Engine::new(opts.config.clone());
        match rt.block_on(engine.run_tests(f)) {
            Ok(report) => {
                print!("{}", report.render());
                failed += report.failed();
            }
            Err(e) => return Ok(report_error(&e, Some(f))),
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

const REPL_HELP: &str = "\
Enter statements, expressions and declarations; each input runs once, in the
context of the previous ones. The value of a final expression is shown (end
the input with `;` to hide it). Input continues on `...` lines while brackets,
strings or comments are open. Tasks spawned by an input keep running, also
while the REPL waits for input. Ctrl-C interrupts a running input (and cancels
all tasks), discards a partial input, or at an empty prompt cancels the tasks
left running; Ctrl-D or :quit exits.

COMMANDS:
    :help          show this help
    :type EXPR     show the type of EXPR without running it
    :test          run the `test` blocks entered so far
    :reset         forget all inputs and start over
    :quit, :q      exit
";

/// The interactive session: reads inputs (possibly spanning several lines)
/// and runs each once in a persistent session.
fn cmd_repl(args: &[String]) -> Result<u8, String> {
    let opts = run_options(args)?;
    if let Some(f) = opts.file {
        return Err(format!("unexpected argument `{}` (`sand repl` takes options only)", f.display()));
    }
    let rt = runtime()?;
    let prompt = PromptLine::default();
    let config =
        opts.config.stdout(ReplSink { prompt: prompt.clone(), stderr: false }).stderr(ReplSink { prompt: prompt.clone(), stderr: true });
    let mut session = Engine::new(config).session();
    println!("sand {} — :help for help, :quit to exit", env!("CARGO_PKG_VERSION"));
    Ok(rt.block_on(repl(&mut session, &prompt)))
}

async fn repl(session: &mut sandlang::Session, prompt: &PromptLine) -> u8 {
    let mut lines = LineReader::new();
    let mut input = String::new();
    loop {
        prompt.show(if input.is_empty() { "> " } else { "... " });
        // Background tasks run while the REPL waits for a line (or Ctrl-C).
        let next = async {
            tokio::select! {
                line = lines.next() => Some(line),
                Ok(()) = tokio::signal::ctrl_c() => None,
            }
        };
        let on_error = |e: Error| {
            prompt.output(|| {
                report_repl_error(&e, "");
                eprintln!("(in a background task; all tasks were cancelled)");
            })
        };
        let got = session.run_tasks_until(next, on_error).await;
        prompt.hide();
        let Some(got) = got else {
            // A background task called `exit`.
            return exit_status(session.exit_code().unwrap_or(0));
        };
        let Some(line) = got else {
            // Ctrl-C: discard the partial input, or cancel the tasks left running.
            println!();
            if input.is_empty() {
                match session.cancel_tasks() {
                    0 => {}
                    1 => eprintln!("cancelled 1 task"),
                    n => eprintln!("cancelled {n} tasks"),
                }
            }
            input.clear();
            continue;
        };
        let Some(line) = line else {
            // End of input: run what was entered.
            if !input.trim().is_empty() {
                println!();
                eval(session, &input).await;
            }
            break;
        };
        if input.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(command) = trimmed.strip_prefix(':') {
                let (name, arg) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
                match name {
                    "quit" | "q" => break,
                    "help" | "h" => print!("{REPL_HELP}"),
                    "reset" => session.reset(),
                    "type" | "t" => match session.type_of(arg.trim()) {
                        Ok(t) => println!("{t}"),
                        Err(e) => report_repl_error(&e, arg),
                    },
                    "test" => match interruptible(session.interrupt_handle(), session.run_tests()).await {
                        Ok(report) => print!("{}", report.render()),
                        Err(e) => report_repl_error(&e, ""),
                    },
                    _ => eprintln!("unknown command `:{name}` (:help lists the commands)"),
                }
                continue;
            }
        }
        input.push_str(&line);
        if !sandlang::Session::is_complete(&input) {
            continue;
        }
        eval(session, &std::mem::take(&mut input)).await;
        if let Some(code) = session.exit_code() {
            return exit_status(code);
        }
    }
    0
}

/// Runs one input, printing its value or error.
async fn eval(session: &mut sandlang::Session, input: &str) {
    let handle = session.interrupt_handle();
    match interruptible(handle, session.eval(input)).await {
        Ok(Some(value)) => println!("{value}"),
        Ok(None) => {}
        Err(e) => report_repl_error(&e, input),
    }
}

/// Awaits `fut`, interrupting the session it runs on when Ctrl-C is pressed.
async fn interruptible<T>(handle: sandlang::InterruptHandle, fut: impl std::future::Future<Output = T>) -> T {
    let mut fut = std::pin::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return r,
            Ok(()) = tokio::signal::ctrl_c() => handle.interrupt(),
        }
    }
}

fn report_repl_error(e: &Error, input: &str) {
    match e {
        Error::Compile(diags) => {
            for d in diags {
                // Diagnostics are in the input, or in a module it imports.
                let source = if d.file.starts_with("<input ") || d.file == "<type>" {
                    Some(input.to_string())
                } else {
                    std::fs::read_to_string(&d.file).ok()
                };
                eprint!("{}", d.render(source.as_deref()));
            }
        }
        _ => eprintln!("{e}"),
    }
}

/// The REPL's prompt, while it waits for input: output written meanwhile (by
/// background tasks) starts on a new line, and the prompt is shown again
/// after it. (Text typed after the prompt stays in the terminal's line buffer,
/// but is not shown again.)
#[derive(Clone, Default)]
struct PromptLine(std::sync::Arc<std::sync::Mutex<PromptState>>);

#[derive(Default)]
struct PromptState {
    /// The prompt, while waiting for input.
    prompt: Option<&'static str>,
    /// Whether the prompt ends the output so far.
    visible: bool,
}

impl PromptLine {
    fn state(&self) -> std::sync::MutexGuard<'_, PromptState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Prints `prompt` and waits for input.
    fn show(&self, prompt: &'static str) {
        let mut st = self.state();
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(prompt.as_bytes());
        let _ = out.flush();
        *st = PromptState { prompt: Some(prompt), visible: true };
    }

    /// Stops waiting for input.
    fn hide(&self) {
        *self.state() = PromptState::default();
    }

    /// Writes output with `write`, which ends a line if `ends_line`.
    fn write<R>(&self, ends_line: bool, write: impl FnOnce() -> R) -> R {
        let mut st = self.state();
        if st.visible {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            st.visible = false;
        }
        let r = write();
        if let Some(prompt) = st.prompt
            && ends_line
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(prompt.as_bytes());
            let _ = out.flush();
            st.visible = true;
        }
        r
    }

    /// Writes whole lines with `write`.
    fn output(&self, write: impl FnOnce()) {
        self.write(true, write)
    }
}

/// Standard output or error of the REPL's scripts (see [`PromptLine`]).
struct ReplSink {
    prompt: PromptLine,
    stderr: bool,
}

impl sandlang::OutputSink for ReplSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.prompt.write(data.ends_with(b"\n"), || {
            if self.stderr {
                std::io::stderr().lock().write_all(data)
            } else {
                let mut out = std::io::stdout().lock();
                out.write_all(data)?;
                out.flush()
            }
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.stderr { std::io::stderr().flush() } else { std::io::stdout().flush() }
    }
}

/// Reads lines of standard input on a thread, one per request, so that the
/// REPL can wait for a line and for Ctrl-C at once. Nothing is read between
/// requests, so a prompter can read answers from standard input meanwhile.
struct LineReader {
    requests: std::sync::mpsc::Sender<()>,
    lines: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    pending: bool,
}

impl LineReader {
    fn new() -> LineReader {
        let (requests, wanted) = std::sync::mpsc::channel::<()>();
        let (send, lines) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for () in wanted {
                let mut line = String::new();
                let got = match std::io::stdin().lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line),
                };
                let end = got.is_none();
                if send.send(got).is_err() || end {
                    break;
                }
            }
        });
        LineReader { requests, lines, pending: false }
    }

    /// The next line, with its terminator; `None` at the end of input.
    /// Cancel-safe: if the future is dropped, the next call gets the line.
    async fn next(&mut self) -> Option<String> {
        if !self.pending {
            if self.requests.send(()).is_err() {
                return None;
            }
            self.pending = true;
        }
        let line = self.lines.recv().await.flatten();
        self.pending = false;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialogue(cap: &Capability, input: &str) -> (PromptAnswer, String) {
        let mut out = Vec::new();
        let answer = prompt_dialogue(cap, &mut input.as_bytes(), &mut out);
        (answer, String::from_utf8(out).unwrap())
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
