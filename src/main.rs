//! The `sand` command-line tool.

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
    sand test FILE...                      run test blocks
    sand repl [OPTIONS]                    interactive session
    sand --version

RUN OPTIONS (all access is denied unless allowed):
    --allow-read=PATHS       comma-separated directories readable by the script
    --allow-write=PATHS      directories writable (and readable) by the script
    --allow-net=HOSTS        hosts the script may connect to (host, host:port, *.domain)
    --allow-listen=ADDRS     addresses the script may listen on (host:port, port 0 = any)
    --allow-env=NAMES        environment variables the script may read
    --allow-stdin            allow reading standard input
    --allow-all              allow everything (trusted scripts only)
    --prompt                 ask on the terminal for anything not allowed
    --max-memory=SIZE        heap limit, e.g. 256M (default 512M)
    --max-time=DURATION      wall-clock limit, e.g. 10s, 500ms, 2m
    --max-cpu=DURATION       CPU time limit
    --max-instructions=N     instruction (fuel) limit
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
    let mut rules = RulePolicy::new();
    let mut allow_all = false;
    let mut prompt = false;
    let mut limits = Limits::default();
    let mut seed = None;
    let mut file = None;
    let mut script_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
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
                    rules = rules.allow_net(&h);
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
            "--allow-stdin" => rules = rules.allow_stdin(),
            "--allow-all" => allow_all = true,
            "--prompt" => prompt = true,
            "--max-memory" => limits.max_memory = Some(parse_size(value.as_deref().unwrap_or(""))?),
            "--max-time" => limits.max_wall_time = Some(parse_duration(value.as_deref().unwrap_or(""))?),
            "--max-cpu" => limits.max_cpu_time = Some(parse_duration(value.as_deref().unwrap_or(""))?),
            "--max-instructions" => {
                limits.max_instructions =
                    Some(value.as_deref().unwrap_or("").parse().map_err(|_| "invalid --max-instructions".to_string())?)
            }
            "--seed" => seed = Some(value.as_deref().unwrap_or("").parse::<u64>().map_err(|_| "invalid --seed".to_string())?),
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
    if prompt {
        rules = rules.fallback(Decision::Prompt);
    }
    let policy = if allow_all { Policy::allow_all() } else { Policy::rules(rules) };
    let mut config = Config::new().policy(policy).limits(limits).args(script_args.clone()).inherit_stdin();
    if prompt {
        config = config.prompter(terminal_prompt);
    }
    if let Some(s) = seed {
        config = config.seed(s);
    }
    Ok(RunOptions { config, file, script_args })
}

fn absolute(p: &str) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// Asks on the controlling terminal whether to allow a capability.
async fn terminal_prompt(cap: Capability) -> PromptAnswer {
    tokio::task::spawn_blocking(move || {
        let question = format!("sand: allow the script to {cap}? [y]es once, [a]lways, [n]o, [N]ever: ");
        let read = |input: &mut dyn BufRead| -> Option<String> {
            let mut line = String::new();
            input.read_line(&mut line).ok()?;
            Some(line.trim().to_string())
        };
        let answer = match std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(mut tty) => {
                let _ = tty.write_all(question.as_bytes());
                let _ = tty.flush();
                read(&mut std::io::BufReader::new(tty))
            }
            Err(_) => {
                eprint!("{question}");
                read(&mut std::io::stdin().lock())
            }
        };
        match answer.as_deref() {
            Some("y") | Some("Y") | Some("yes") => PromptAnswer::AllowOnce,
            Some("a") | Some("A") | Some("always") => PromptAnswer::AllowAlways,
            Some("N") | Some("never") => PromptAnswer::DenyAlways,
            _ => PromptAnswer::DenyOnce,
        }
    })
    .await
    .unwrap_or(PromptAnswer::DenyOnce)
}

fn report_error(e: &Error, main: Option<&Path>) -> u8 {
    match e {
        Error::Compile(diags) => {
            let root = main.and_then(|m| m.parent()).map(Path::to_path_buf).unwrap_or_default();
            for d in diags {
                let source = main
                    .filter(|m| m.file_name().map(|n| n.to_string_lossy() == d.file).unwrap_or(false))
                    .and_then(|m| std::fs::read_to_string(m).ok())
                    .or_else(|| std::fs::read_to_string(root.join(&d.file)).ok());
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
        Ok(o) => Ok(o.exit_code.clamp(0, 255) as u8),
        Err(e) => Ok(report_error(&e, Some(&file))),
    }
}

fn cmd_check(files: &[String]) -> Result<u8, String> {
    if files.is_empty() {
        return Err("missing FILE".into());
    }
    let mut code = 0;
    for f in files {
        let path = Path::new(f);
        let mut engine = Engine::new(Config::new().policy(Policy::allow_all()));
        match engine.check_file(path) {
            Ok(_) => println!("{f}: ok"),
            Err(e) => code = code.max(report_error(&e, Some(path))),
        }
    }
    Ok(code)
}

fn cmd_lint(args: &[String]) -> Result<u8, String> {
    let allow = args.iter().any(|a| a == "--allow-warnings");
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        return Err("missing FILE".into());
    }
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
    let check = args.iter().any(|a| a == "--check");
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        return Err("missing FILE".into());
    }
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

fn cmd_test(files: &[String]) -> Result<u8, String> {
    if files.is_empty() {
        return Err("missing FILE".into());
    }
    let rt = runtime()?;
    let mut failed = 0;
    for f in files {
        let mut engine = Engine::new(Config::new().policy(Policy::allow_all()));
        match rt.block_on(engine.run_tests(Path::new(f))) {
            Ok(report) => {
                print!("{}", report.render());
                failed += report.failed();
            }
            Err(e) => return Ok(report_error(&e, Some(Path::new(f)))),
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

/// A simple REPL: each input is checked together with the previous inputs and
/// the whole session is re-run with the output of earlier inputs hidden.
fn cmd_repl(args: &[String]) -> Result<u8, String> {
    let opts = run_options(args)?;
    let rt = runtime()?;
    let mut session: Vec<String> = Vec::new();
    let stdin = std::io::stdin();
    println!("sand {} — type :quit to exit, :reset to clear the session", env!("CARGO_PKG_VERSION"));
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let input = line.trim_end().to_string();
        match input.trim() {
            "" => continue,
            ":quit" | ":q" => break,
            ":reset" => {
                session.clear();
                continue;
            }
            _ => {}
        }
        // Expressions are printed automatically.
        let is_decl = ["let ", "var ", "fn ", "struct ", "enum ", "import ", "from ", "type ", "extend "]
            .iter()
            .any(|k| input.trim_start().starts_with(k));
        let snippet = if is_decl {
            input.clone()
        } else {
            format!("let __value = {{\n{input}\n}}\nif __value.debug() != \"()\" {{ print(__value.debug()) }}")
        };
        let marker = "__repl_marker__";
        let mut program = session.join("\n");
        program.push_str(&format!("\nprint(\"{marker}\")\n{snippet}\n"));
        let out = sandlang::MemorySink::new();
        let config = opts.config.clone().stdout(out.clone());
        let mut engine = Engine::new(config);
        let result = rt.block_on(engine.run_source("repl.sand", &program));
        let text = out.contents();
        let shown = text.split_once(&format!("{marker}\n")).map(|(_, after)| after.to_string()).unwrap_or_default();
        print!("{shown}");
        match result {
            Ok(_) => {
                if is_decl {
                    session.push(input);
                }
            }
            Err(Error::Compile(diags)) => {
                if is_decl {
                    // Retry as an expression statement.
                }
                for d in diags {
                    eprintln!("{}", d.message);
                }
            }
            Err(e) => eprintln!("{e}"),
        }
    }
    Ok(0)
}
