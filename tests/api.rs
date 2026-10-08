//! Acceptance tests for the embedding API, sandbox policy, and resource limits.

mod common;

use common::{TempDir, rt, run_with};
use sandlang::{
    Capability, Config, Decision, Engine, Error, LimitKind, Limits, MemorySink, NativeModule, Policy, PromptAnswer, RulePolicy,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn engine_with(config: Config) -> (Engine, MemorySink, MemorySink) {
    let out = MemorySink::new();
    let err = MemorySink::new();
    let engine = Engine::new(config.stdout(out.clone()).stderr(err.clone()));
    (engine, out, err)
}

fn run_src(config: Config, src: &str) -> (Result<sandlang::Outcome, Error>, String, String) {
    let (mut engine, out, err) = engine_with(config);
    let r = rt().block_on(engine.run_source("main.sand", src));
    (r, out.contents(), err.contents())
}

// ---------------------------------------------------------------------------
// Basic embedding
// ---------------------------------------------------------------------------

#[test]
fn engine_and_run_future_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let mut engine = Engine::new(Config::new());
    assert_send(&engine);
    let fut = engine.run_source("main.sand", "print(1)");
    assert_send(&fut);
}

#[test]
fn run_source_and_exit_code() {
    let (r, out, _) = run_src(Config::new(), "print(\"hi\")\nexit(4)");
    assert_eq!(r.unwrap().exit_code, 4);
    assert_eq!(out, "hi\n");
    let (r, _, _) = run_src(Config::new(), "let x = 1");
    assert_eq!(r.unwrap().exit_code, 0);
}

#[test]
fn compile_errors_are_reported_as_diagnostics() {
    let (r, _, _) = run_src(Config::new(), "let x: int = \"s\"\nlet y = nope");
    match r {
        Err(Error::Compile(diags)) => {
            assert_eq!(diags.len(), 2);
            assert_eq!(diags[0].line, 1);
            assert_eq!(diags[0].file, "main.sand");
            assert!(diags[0].message.contains("type mismatch"));
            let rendered = diags[0].render(Some("let x: int = \"s\"\nlet y = nope"));
            assert!(rendered.contains("--> main.sand:1:14"), "{rendered}");
            assert!(rendered.contains("^^^"), "{rendered}");
        }
        other => panic!("expected compile error, got {other:?}"),
    }
}

#[test]
fn check_source_does_not_run() {
    let mut engine = Engine::new(Config::new());
    let diags = engine.check_source("main.sand", "print(1)\nlet x: int = 2").unwrap();
    assert!(diags.is_empty());
    let err = engine.check_source("main.sand", "let x: int = true").unwrap_err();
    assert!(matches!(err, Error::Compile(_)));
}

#[test]
fn runtime_errors_have_stack_traces() {
    // Non-tail calls, so every frame appears in the trace.
    let src = "fn a() -> int { panic(\"boom\") }\nfn b() -> int { a() + 1 }\nprint(b())\n";
    let (r, _, _) = run_src(Config::new(), src);
    match r {
        Err(Error::Runtime { message, trace }) => {
            assert!(message.contains("boom"));
            let lines: Vec<usize> = trace.iter().map(|f| f.line).collect();
            assert_eq!(lines, vec![1, 2, 3], "{trace:?}");
            assert_eq!(trace[0].function, "a");
            assert_eq!(trace[0].file, "main.sand");
        }
        other => panic!("expected runtime error, got {other:?}"),
    }
}

#[test]
fn engine_can_run_multiple_sources_independently() {
    let (mut engine, out, _) = engine_with(Config::new());
    let rt = rt();
    rt.block_on(engine.run_source("a.sand", "var x = 1\nprint(x)")).unwrap();
    rt.block_on(engine.run_source("b.sand", "print(2)")).unwrap();
    assert_eq!(out.contents(), "1\n2\n");
}

#[test]
fn script_args_and_env_are_virtual() {
    let config = Config::new().policy(Policy::allow_all()).args(vec!["a".into(), "b".into()]).env(vec![("ONLY".into(), "this".into())]);
    let (r, out, _) = run_src(config, "import env\nprint(env.args())\nprint(env.get(\"ONLY\"))\nprint(env.get(\"PATH\"))");
    r.unwrap();
    assert_eq!(out, "[\"a\", \"b\"]\nSome(\"this\")\nNone\n");
}

// ---------------------------------------------------------------------------
// Native modules
// ---------------------------------------------------------------------------

fn geo_module() -> NativeModule {
    let mut m = NativeModule::new("geo");
    m.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });
    m.function("count", |s: String| -> i64 { s.chars().count() as i64 });
    m.function("split", |s: String| -> Vec<String> { s.split(',').map(String::from).collect() });
    m.function("find", |xs: Vec<i64>, t: i64| -> Option<i64> { xs.iter().position(|&x| x == t).map(|i| i as i64) });
    m.function("checked", |a: i64| -> Result<i64, String> {
        if a >= 0 { Ok(a) } else { Err(format!("negative: {a}")) }
    });
    m.function("tally", |words: Vec<String>| -> HashMap<String, i64> {
        let mut m = HashMap::new();
        for w in words {
            *m.entry(w).or_insert(0) += 1;
        }
        m
    });
    m.function("pair", |a: i64, s: String| -> (i64, String) { (a * 2, s.to_uppercase()) });
    m.function("raw", |b: Vec<u8>| -> Vec<u8> { b.into_iter().rev().collect() });
    m.function("flag", |b: bool| -> bool { !b });
    m.doc("dist", "Euclidean length of (a, b).");
    m
}

#[test]
fn native_module_typed_functions() {
    let config = Config::new().module(geo_module());
    let src = r#"
import geo
import help
print(geo.dist(3.0, 4.0))
print(geo.count("héllo"))
print(geo.split("a,b"))
print(geo.find([5, 6, 7], 7))
print(geo.find([5], 1))
print(geo.checked(3))
print(geo.checked(-1).unwrap_err().message)
print(geo.tally(["a", "b", "a"])["a"])
print(geo.pair(21, "x"))
print(geo.raw(b"abc"))
print(geo.flag(false))
print(help.signature(geo.dist))
print(help.doc(geo.dist))
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(
        out,
        "5.0\n5\n[\"a\", \"b\"]\nSome(2)\nNone\nOk(3)\nnegative: -1\n2\n(42, \"X\")\nb\"cba\"\ntrue\nfn dist(a: float, b: float) -> float\nEuclidean length of (a, b).\n"
    );
}

#[test]
fn native_module_signatures_are_type_checked() {
    let config = Config::new().module(geo_module());
    let (r, _, _) = run_src(config, "import geo\nprint(geo.dist(1, 2))");
    match r {
        Err(Error::Compile(d)) => {
            assert!(d[0].message.contains("expected float, found int"), "{d:?}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn native_async_functions_suspend_only_their_task() {
    let mut m = NativeModule::new("slow");
    m.async_function("echo", |s: String, ms: i64| async move {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        s
    });
    let src = r#"
import slow
let a = spawn slow.echo("a", 40)
let b = spawn slow.echo("b", 5)
print(task.race([a, b]))
print(a.wait())
"#;
    let (r, out, _) = run_src(Config::new().module(m), src);
    match r {
        // `a` was cancelled by race, so waiting on it panics.
        Err(Error::Runtime { message, .. }) => assert!(message.contains("task cancelled")),
        other => panic!("{other:?}"),
    }
    assert_eq!(out, "b\n");
}

#[test]
fn native_module_with_sand_source() {
    let mut m = NativeModule::new("shapes");
    m.source(
        r#"
/// A square.
pub struct Square { side: float }
pub fn area(s: Square) -> float { s.side * s.side }
pub native fn scale(x: float, k: float) -> float
"#,
    );
    m.function("scale", |x: f64, k: f64| -> f64 { x * k });
    let src = "import shapes\nlet s = shapes.Square { side: shapes.scale(2.0, 1.5) }\nprint(shapes.area(s))";
    let (r, out, _) = run_src(Config::new().module(m), src);
    r.unwrap();
    assert_eq!(out, "9.0\n");
}

#[test]
fn native_module_custom_capability() {
    let mut m = NativeModule::new("door");
    m.guarded_function(
        "open",
        |name: &String| Capability::Custom { name: "door".into(), detail: name.clone() },
        |name: String| -> String { format!("opened {name}") },
    );
    let src = "import door\nprint(door.open(\"front\"))\nprint(door.open(\"back\"))";
    let policy = Policy::custom(|cap: &Capability| match cap {
        Capability::Custom { detail, .. } if detail == "front" => Decision::Allow,
        Capability::Custom { .. } => Decision::Deny,
        _ => Decision::Allow,
    });
    let (r, out, _) = run_src(Config::new().policy(policy).module(m), src);
    r.unwrap();
    assert_eq!(out, "Ok(\"opened front\")\nErr(Error { kind: \"permission_denied\", message: \"permission denied: door back\" })\n");
}

// ---------------------------------------------------------------------------
// Sandbox policy
// ---------------------------------------------------------------------------

#[test]
fn deny_all_blocks_io_but_allows_output() {
    let dir = TempDir::new("deny");
    dir.write("secret.txt", "s3cret");
    let config = Config::new().policy(Policy::deny_all()).cwd(dir.path());
    let src = r#"
import fs
import env
import net
let r = fs.read_text("secret.txt")
print(r.unwrap_err().kind)
print(fs.write_text("out.txt", "x").unwrap_err().kind)
print(env.get("HOME"))
print(net.connect("127.0.0.1:1").unwrap_err().kind)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "permission_denied\npermission_denied\nNone\npermission_denied\n");
    assert!(!dir.path().join("out.txt").exists());
}

#[test]
fn stdout_can_be_denied() {
    let policy = Policy::custom(|cap: &Capability| if matches!(cap, Capability::Stdout) { Decision::Deny } else { Decision::Allow });
    let (r, out, _) = run_src(Config::new().policy(policy), "print(\"x\")");
    match r {
        Err(Error::Runtime { message, .. }) => {
            assert!(message.contains("permission denied: stdout"))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out, "");
}

#[test]
fn stdin_requires_permission() {
    let config = Config::new().policy(Policy::deny_all()).stdin_bytes(b"line\n".to_vec());
    let (r, out, _) = run_src(config, "import io\nprint(io.read_line().unwrap_err().kind)");
    r.unwrap();
    assert_eq!(out, "permission_denied\n");
}

#[test]
fn rule_policy_confines_paths() {
    let dir = TempDir::new("rules");
    dir.write("public/a.txt", "A");
    dir.write("private/b.txt", "B");
    std::fs::create_dir_all(dir.path().join("public/sub")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("private/b.txt"), dir.path().join("public/link.txt")).unwrap();

    let policy = RulePolicy::new().allow_read(dir.path().join("public")).allow_write(dir.path().join("public/sub"));
    let config = Config::new().policy(Policy::rules(policy)).cwd(dir.path());
    let src = r#"
import fs
print(fs.read_text("public/a.txt"))
print(fs.read_text("private/b.txt").unwrap_err().kind)
print(fs.read_text("public/../private/b.txt").unwrap_err().kind)
print(fs.read_text("public/sub/../../private/b.txt").unwrap_err().kind)
print(fs.read_text("public/link.txt").unwrap_err().kind)
print(fs.write_text("public/sub/new.txt", "n").is_ok())
print(fs.write_text("public/new.txt", "n").unwrap_err().kind)
print(fs.list_dir("public").is_ok())
print(fs.list_dir(".").unwrap_err().kind)
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    let expected_link = if cfg!(unix) { "permission_denied" } else { "not_found" };
    assert_eq!(
        out,
        format!(
            "Ok(\"A\")\npermission_denied\npermission_denied\npermission_denied\n{expected_link}\ntrue\npermission_denied\ntrue\npermission_denied\n"
        )
    );
}

#[test]
fn rule_policy_deny_overrides_allow() {
    let dir = TempDir::new("denyover");
    dir.write("data/ok.txt", "ok");
    dir.write("data/secret/key.txt", "key");
    let policy = RulePolicy::new().allow_read(dir.path().join("data")).deny_read(dir.path().join("data/secret"));
    let config = Config::new().policy(Policy::rules(policy)).cwd(dir.path());
    let (r, out, _) =
        run_src(config, "import fs\nprint(fs.read_text(\"data/ok.txt\").is_ok())\nprint(fs.read_text(\"data/secret/key.txt\").is_ok())");
    r.unwrap();
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn rule_policy_network_hosts() {
    let p = RulePolicy::new().allow_net("example.com").allow_net("*.example.org").allow_net("localhost:8080");
    let check = |host: &str, port: u16| p.check(&Capability::Net { host: host.into(), port });
    assert_eq!(check("example.com", 443), Decision::Allow);
    assert_eq!(check("example.com", 80), Decision::Allow);
    assert_eq!(check("evil.com", 80), Decision::Deny);
    assert_eq!(check("api.example.org", 443), Decision::Allow);
    assert_eq!(check("example.org", 443), Decision::Deny);
    assert_eq!(check("localhost", 8080), Decision::Allow);
    assert_eq!(check("localhost", 8081), Decision::Deny);
    let prompting = RulePolicy::new().fallback(Decision::Prompt);
    assert_eq!(prompting.check(&Capability::Env("X".into())), Decision::Prompt);
    assert_eq!(prompting.check(&Capability::Stdout), Decision::Allow);
}

#[test]
fn rule_policy_env_and_listen() {
    let p = RulePolicy::new().allow_env("HOME").allow_listen("127.0.0.1:0").allow_stdin();
    assert_eq!(p.check(&Capability::Env("HOME".into())), Decision::Allow);
    assert_eq!(p.check(&Capability::Env("SECRET".into())), Decision::Deny);
    assert_eq!(p.check(&Capability::NetListen("127.0.0.1:9000".into())), Decision::Allow);
    assert_eq!(p.check(&Capability::NetListen("0.0.0.0:9000".into())), Decision::Deny);
    assert_eq!(p.check(&Capability::Stdin), Decision::Allow);
}

#[test]
fn denied_import_is_a_compile_error() {
    let dir = TempDir::new("imp");
    dir.write("lib.sand", "pub let X = 1");
    let main = dir.write("main.sand", "import \"./lib.sand\"\nprint(lib.X)");
    let policy = Policy::custom(|cap: &Capability| if matches!(cap, Capability::Import(_)) { Decision::Deny } else { Decision::Allow });
    let mut engine = Engine::new(Config::new().policy(policy));
    match rt().block_on(engine.run_file(&main)) {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("permission denied"), "{d:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn prompt_answers() {
    let dir = TempDir::new("prompt");
    dir.write("a.txt", "A");
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let answers = Arc::new(Mutex::new(vec![PromptAnswer::AllowOnce, PromptAnswer::DenyOnce, PromptAnswer::AllowAlways]));
    let asked2 = asked.clone();
    let answers2 = answers.clone();
    let config = Config::new().policy(Policy::rules(RulePolicy::new().fallback(Decision::Prompt))).cwd(dir.path()).prompter(
        move |cap: Capability| {
            asked2.lock().unwrap().push(cap.to_string());
            let answer = answers2.lock().unwrap().remove(0);
            async move { answer }
        },
    );
    let src = r#"
import fs
print(fs.read_text("a.txt").is_ok())
print(fs.read_text("a.txt").is_ok())
print(fs.read_text("a.txt").is_ok())
print(fs.read_text("a.txt").is_ok())
"#;
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "true\nfalse\ntrue\ntrue\n");
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert!(asked[0].contains("read") && asked[0].contains("a.txt"), "{asked:?}");
}

#[test]
fn prompt_without_prompter_denies() {
    let config = Config::new().policy(Policy::rules(RulePolicy::new().fallback(Decision::Prompt)));
    let (r, out, _) = run_src(config, "import env\nprint(env.get(\"X\"))");
    r.unwrap();
    assert_eq!(out, "None\n");
}

#[test]
fn prompts_suspend_all_execution() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let stable = Arc::new(AtomicBool::new(false));
    let mut m = NativeModule::new("probe");
    let t = ticks.clone();
    m.function("tick", move || {
        t.fetch_add(1, Ordering::SeqCst);
    });
    let (t2, s2) = (ticks.clone(), stable.clone());
    let config =
        Config::new().module(m).policy(Policy::rules(RulePolicy::new().fallback(Decision::Prompt))).prompter(move |_cap: Capability| {
            let (t, s) = (t2.clone(), s2.clone());
            async move {
                let before = t.load(Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                let after = t.load(Ordering::SeqCst);
                s.store(before > 0 && before == after, Ordering::SeqCst);
                PromptAnswer::AllowOnce
            }
        });
    let src = r#"
import probe
import env
let ticker = spawn {
    loop {
        probe.tick()
        task.sleep(1ms)
    }
}
task.sleep(20ms)
print(env.get("ANY").is_some() or true)
"#;
    let (r, _, _) = run_src(config, src);
    r.unwrap();
    assert!(stable.load(Ordering::SeqCst), "ticker ran while the prompt was pending");
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

#[test]
fn instruction_limit() {
    let config = Config::new().limits(Limits { max_instructions: Some(100_000), ..Limits::default() });
    let (r, _, _) = run_src(config, "var i = 0\nwhile true { i += 1 }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
}

#[test]
fn memory_limit() {
    let config = Config::new().limits(Limits { max_memory: Some(8 << 20), ..Limits::default() });
    let (r, _, _) = run_src(config, "let xs = []\nwhile true { xs.push(\"some string data\".repeat(10)) }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

#[test]
fn memory_limit_counts_big_buffers() {
    let config = Config::new().limits(Limits { max_memory: Some(4 << 20), ..Limits::default() });
    let (r, _, _) = run_src(config, "let s = \"x\".repeat(100000000)\nprint(s.len())");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

#[test]
fn garbage_is_collected_under_memory_limit() {
    let config = Config::new().limits(Limits { max_memory: Some(16 << 20), ..Limits::default() });
    let src = "var total = 0\nfor i in 0..200000 {\n    let tmp = [i, i + 1, i + 2].map(x => x.to_str())\n    total += tmp.len()\n}\nprint(total)";
    let (r, out, _) = run_src(config, src);
    r.unwrap();
    assert_eq!(out, "600000\n");
}

#[test]
fn wall_time_limit() {
    let config = Config::new().limits(Limits { max_wall_time: Some(Duration::from_millis(100)), ..Limits::default() });
    let start = Instant::now();
    let (r, _, _) = run_src(config, "task.sleep(10s)");
    assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn cpu_time_limit_excludes_waiting() {
    let config = Config::new().limits(Limits { max_cpu_time: Some(Duration::from_millis(200)), ..Limits::default() });
    let (r, out, _) = run_src(config.clone(), "task.sleep(300ms)\nprint(\"slept\")");
    r.unwrap();
    assert_eq!(out, "slept\n");
    let (r, _, _) = run_src(config, "var i = 0\nwhile true { i += 1 }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::CpuTime))), "{r:?}");
}

#[test]
fn call_depth_limit() {
    let config = Config::new().limits(Limits { max_call_depth: 100, ..Limits::default() });
    let (r, _, _) = run_src(config, "fn f(n: int) -> int { if n == 0 { 0 } else { 1 + f(n - 1) } }\nprint(f(50))\nprint(f(200))");
    match r {
        Err(Error::Runtime { message, .. }) => assert!(message.contains("stack overflow")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn task_limit() {
    let config = Config::new().limits(Limits { max_tasks: 10, ..Limits::default() });
    let (r, _, _) = run_src(config, "let ts = (0..100).map(i => spawn { task.sleep(10ms) })");
    match r {
        Err(Error::Runtime { message, .. }) => assert!(message.contains("task limit exceeded")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn output_limit() {
    let config = Config::new().limits(Limits { max_output_bytes: Some(1000), ..Limits::default() });
    let (r, out, _) = run_src(config, "for i in 0..1000 { print(\"line {i}\") }");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Output))), "{r:?}");
    assert!(out.len() <= 1000);
}

#[test]
fn open_resource_limit() {
    let config = Config::new().limits(Limits { max_open_resources: 2, ..Limits::default() });
    let src = r#"
import net
let a = net.listen("127.0.0.1:0")?
let b = net.listen("127.0.0.1:0")?
let c = net.listen("127.0.0.1:0")
print(c.unwrap_err().kind)
a.close()
print(net.listen("127.0.0.1:0").is_ok())
"#;
    let (r, out, _) = run_src(config.policy(Policy::allow_all()), src);
    r.unwrap();
    assert_eq!(out, "limit\ntrue\n");
}

#[test]
fn source_size_limit() {
    let config = Config::new().limits(Limits { max_source_bytes: 100, ..Limits::default() });
    let (r, _, _) = run_src(config, &"print(1)\n".repeat(50));
    match r {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("source too large")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn interrupt_from_another_thread() {
    let mut engine = Engine::new(Config::new());
    let handle = engine.interrupt_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        handle.interrupt();
    });
    let r = rt().block_on(engine.run_source("main.sand", "while true {}"));
    t.join().unwrap();
    assert!(matches!(r, Err(Error::Interrupted)), "{r:?}");
}

#[test]
fn limits_are_per_engine() {
    // One engine hitting its limit must not affect another.
    let limited = Config::new().limits(Limits { max_instructions: Some(10_000), ..Limits::default() });
    let (r1, _, _) = run_src(limited, "var i = 0\nwhile true { i += 1 }");
    assert!(matches!(r1, Err(Error::Limit(LimitKind::Instructions))));
    let (r2, out, _) = run_src(Config::new(), "var s = 0\nfor i in 0..100000 { s += i }\nprint(s)");
    r2.unwrap();
    assert_eq!(out, "4999950000\n");
}

// ---------------------------------------------------------------------------
// Concurrency of engines
// ---------------------------------------------------------------------------

const FIB: &str = "fn fib(n: int) -> int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\nprint(fib(22))";

#[test]
fn engines_run_concurrently_on_threads() {
    let handles: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let out = MemorySink::new();
                let mut engine = Engine::new(Config::new().stdout(out.clone()).seed(i));
                rt().block_on(engine.run_source("main.sand", FIB)).unwrap();
                out.contents()
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), "17711\n");
    }
}

#[test]
fn engines_run_concurrently_on_a_multithreaded_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    runtime.block_on(async {
        let mut tasks = Vec::new();
        for i in 0..16 {
            tasks.push(tokio::spawn(async move {
                let out = MemorySink::new();
                let mut engine = Engine::new(Config::new().stdout(out.clone()));
                let src = format!("task.sleep(10ms)\nprint({i} * 2)");
                engine.run_source("main.sand", &src).await.unwrap();
                out.contents()
            }));
        }
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), format!("{}\n", i * 2));
        }
    });
}

#[test]
fn engines_on_one_thread_interleave() {
    // A busy engine must yield to the host runtime so others make progress.
    let runtime = rt();
    runtime.block_on(async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let busy = tokio::task::spawn_local(async {
                    let mut e =
                        Engine::new(Config::new().limits(Limits { max_wall_time: Some(Duration::from_millis(500)), ..Limits::default() }));
                    e.run_source("busy.sand", "while true {}").await
                });
                let quick = tokio::task::spawn_local(async {
                    let out = MemorySink::new();
                    let mut e = Engine::new(Config::new().stdout(out.clone()));
                    let start = Instant::now();
                    e.run_source("quick.sand", "task.sleep(10ms)\nprint(\"done\")").await.unwrap();
                    (out.contents(), start.elapsed())
                });
                let (out, elapsed) = quick.await.unwrap();
                assert_eq!(out, "done\n");
                assert!(elapsed < Duration::from_millis(400), "quick engine starved: {elapsed:?}");
                assert!(matches!(busy.await.unwrap(), Err(Error::Limit(LimitKind::WallTime))));
            })
            .await;
    });
}

#[test]
fn seeded_engines_are_deterministic() {
    let src = "import random\nprint([random.int(0, 1000000) for _ in 0..5])";
    let a = run_src(Config::new().seed(5), src).1;
    let b = run_src(Config::new().seed(5), src).1;
    let c = run_src(Config::new().seed(6), src).1;
    assert_eq!(a, b);
    assert_ne!(a, c);
}

// ---------------------------------------------------------------------------
// Test runner API and file loading
// ---------------------------------------------------------------------------

#[test]
fn run_tests_api() {
    let dir = TempDir::new("tests");
    let main = dir.write("t.sand", "test \"a\" { assert(true) }\ntest \"b\" { assert(false, \"nope\") }");
    let mut engine = Engine::new(Config::new());
    let report = rt().block_on(engine.run_tests(&main)).unwrap();
    assert_eq!(report.passed(), 1);
    assert_eq!(report.failed(), 1);
    assert_eq!(report.results[1].name, "b");
    assert_eq!(report.results[1].message.as_deref(), Some("nope"));
}

#[test]
fn run_file_with_imports_and_cwd() {
    let r = run_with("import \"./util.sand\"\nprint(util.X)", |c| c);
    // util.sand does not exist in the run() temp dir.
    assert!(matches!(r.result, Err(Error::Compile(_))));
}

// ---------------------------------------------------------------------------
// Source audit: the implementation must stay inside the sandbox rules.
// ---------------------------------------------------------------------------

#[test]
fn implementation_has_no_unsafe_or_process_access() {
    fn visit(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                visit(&p, files);
            } else if p.extension().is_some_and(|e| e == "rs") {
                files.push(p);
            }
        }
    }
    let mut files = Vec::new();
    visit(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(!files.is_empty());
    let forbidden =
        ["unsafe ", "unsafe{", "std::process::Command", "process::Command", "libc::", "memmap", "mmap(", "dlopen", "extern \"C\""];
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for bad in forbidden {
                // The CLI binary may exit the process; nothing else may touch processes.
                assert!(!code.contains(bad), "{}:{}: forbidden `{bad}`", f.display(), i + 1);
            }
        }
    }
    let lib = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(lib.contains("#![forbid(unsafe_code)]"));
}
