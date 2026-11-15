//! Tests for async native functions that call Sand functions.

mod common;

use common::{StoppedClock, rt};
use parking_lot::Mutex;
use sandlang::engine::{Config, Engine, Error, HostModules, LimitKind, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::context::NativeContext;
use sandlang::native::convert::{HostError, IntoVmValue};
use sandlang::native::func::Later;
use sandlang::native::ty::{Param, VmVal};
use sandlang::policy::{Capability, Decision, PromptAnswer};
use sandlang::vm::{Fail, Vm, WaitKind};
use std::sync::Arc;
use std::time::Duration;

type Log = Arc<Mutex<Vec<String>>>;

/// Appends "native dropped" to the log when dropped.
struct LogOnDrop(Log);

impl Drop for LogOnDrop {
    fn drop(&mut self) {
        self.0.lock().push("native dropped".into());
    }
}

type Callback = VmVal<fn() -> ()>;

/// The `calls` module, whose functions record events in `log`.
fn calls_module(log: &Log) -> NativeModule {
    let mut m = NativeModule::new("calls");
    m.source("pub native fn apply<T>(f: fn(T) -> T, x: T, times: int) -> T");
    m.function("apply", |f: VmVal<fn(Param<0>) -> Param<0>>, x: Param<0>, times: i64| -> Result<Later<Param<0>, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, move |cx| {
            let (f, mut x) = (cx.root(f.raw()), cx.root(x.raw()));
            async move {
                for _ in 0..times {
                    x = cx.call(f, &[x]).await;
                }
                Ok(Param::from_raw(cx.value(x)))
            }
        }))
    });
    // Returns a string that only the call's roots reference after `f` runs.
    m.function("keep", |vm: &mut Vm, f: Callback| -> Result<Later<String, Fail>, Fail> {
        let kept = "kept".into_vm_value(vm)?;
        Ok(Later::new(WaitKind::Io, |cx| {
            let (kept, f) = (cx.root(kept), cx.root(f.raw()));
            async move {
                cx.call(f, &[]).await;
                Ok(cx.with_vm(|vm| vm.str(cx.value(kept)).to_string()))
            }
        }))
    });
    let l = log.clone();
    m.function("log", move |entry: String| l.lock().push(entry));
    let l = log.clone();
    m.function("guard", move |f: Callback| -> Result<Later<(), Fail>, Fail> {
        let guard = LogOnDrop(l.clone());
        Ok(Later::new(WaitKind::Io, |cx| {
            let f = cx.root(f.raw());
            async move {
                let _guard = guard;
                cx.call(f, &[]).await;
                Ok(())
            }
        }))
    });
    m.function("wait_ms", |ms: i64| async move { tokio::time::sleep(Duration::from_millis(ms as u64)).await });
    m.function("spend_budget", |n: i64| async move {
        for _ in 0..n {
            tokio::task::consume_budget().await;
        }
    });
    m.function("check_memory", |bytes: i64| -> Result<Later<(), Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, move |cx| async move { cx.check_memory(bytes as usize).await }))
    });
    m.function("open_door", |name: String| -> Result<Later<String, HostError>, HostError> {
        Ok(Later::new(WaitKind::Io, |cx| async move {
            cx.require(&Capability::Custom { name: "door".into(), detail: name.clone() }).await?;
            Ok(format!("opened {name}"))
        }))
    });
    // Saves the context of one call, and uses it during the poll of another.
    let saved: Arc<Mutex<Option<NativeContext>>> = Arc::default();
    let save = saved.clone();
    m.function("save_context", move || -> Result<Later<(), Fail>, Fail> {
        let save = save.clone();
        Ok(Later::new(WaitKind::Io, move |cx| {
            *save.lock() = Some(cx);
            async { Ok(()) }
        }))
    });
    m.function("use_saved_context", move || -> Result<Later<bool, Fail>, Fail> {
        let saved = saved.lock().take().expect("a saved context");
        Ok(Later::new(WaitKind::Io, |_| async move {
            Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| saved.with_vm(|_| ()))).is_ok())
        }))
    });
    m.function("two_calls", |f: Callback| -> Result<Later<(), Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, |cx| {
            let f = cx.root(f.raw());
            async move {
                let (mut first, mut second) = (std::pin::pin!(cx.call(f, &[])), std::pin::pin!(cx.call(f, &[])));
                std::future::poll_fn(|w| {
                    let _ = (first.as_mut().poll(w), second.as_mut().poll(w));
                    std::task::Poll::<()>::Pending
                })
                .await;
                Ok(())
            }
        }))
    });
    m
}

fn run(config: Config, log: &Log, src: &str) -> (Result<i32, Error>, String) {
    let out = MemorySink::new();
    let config = config.host_modules(HostModules::new([calls_module(log)]).unwrap()).stdout(out.clone());
    let r = rt().block_on(Engine::new(config).run_source("main.sand", &format!("import calls\n{src}")));
    (r.map(|outcome| outcome.exit_code), out.contents())
}

fn run_ok(config: Config, src: &str) -> String {
    let (r, out) = run(config, &Log::default(), src);
    r.unwrap();
    out
}

fn memory_limit(bytes: usize) -> Config {
    Config::new().limits(Limits { max_memory: Some(bytes), ..Limits::default() })
}

#[test]
fn callbacks_run_on_the_calling_task_and_suspend_on_io_and_channels() {
    // The callback waits for a message that only another task sends.
    let src = "print(calls.apply((s) => s + \"!\", \"hi\", 1000).len())\n\
               let ch: Channel<int> = Channel()\n\
               let sender = spawn {\n\
                   calls.wait_ms(5)\n\
                   ch.send(7)\n\
               }\n\
               print(calls.apply((x) => {\n\
                   task.sleep(1ms)\n\
                   x + (ch.recv() ?? 0)\n\
               }, 35, 1))";
    assert_eq!(run_ok(Config::new(), src), "1002\n42\n");
}

#[test]
fn rooted_values_survive_collections_during_callbacks() {
    let src = "print(calls.keep(() => {\n    var xs: [str] = []\n    for i in 0..2000 { xs.push(\"garbage \\(i)\") }\n}))";
    assert_eq!(run_ok(Config::new().gc_stress(true), src), "kept\n");
}

#[test]
fn callbacks_nest_up_to_the_call_depth_limit() {
    let src = "fn down(n: int) -> int { if n == 0 { 0 } else { calls.apply(down, n - 1, 1) + 1 } }\nprint(down(100))";
    assert_eq!(run_ok(Config::new().gc_stress(true), src), "100\n");
    let (r, _) = run(Config::new().limits(Limits { max_call_depth: 50, ..Limits::default() }), &Log::default(), src);
    assert!(matches!(&r, Err(Error::Runtime { message, .. }) if message.contains("stack overflow")), "{r:?}");
}

#[test]
fn callbacks_end_runs_with_exits_and_traced_errors() {
    let (r, out) = run(Config::new(), &Log::default(), "calls.apply((x) => { exit(3)\nx }, 1, 1)\nprint(\"unreachable\")");
    assert_eq!((r.unwrap(), out.as_str()), (3, ""));
    let src = "fn divide(x: int) -> int { x / 0 }\nfn outer() -> int { calls.apply(divide, 1, 1) }\nprint(outer())";
    let (r, _) = run(Config::new(), &Log::default(), src);
    let Err(Error::Runtime { trace, .. }) = r else { panic!("{r:?}") };
    let functions: Vec<&str> = trace.iter().map(|frame| frame.function.as_str()).collect();
    assert_eq!(functions, ["divide", "outer", "<top level>"]);
}

#[test]
fn cancellation_and_run_ends_drop_native_futures_in_reverse_call_order() {
    let log = Log::default();
    let src = "let t = spawn {\n\
                   defer { calls.log(\"caller defer\") }\n\
                   calls.guard(() => {\n\
                       defer { calls.log(\"callee defer\") }\n\
                       calls.wait_ms(10000)\n\
                   })\n\
               }\n\
               task.sleep(5ms)\n\
               t.cancel()\n\
               spawn calls.guard(() => calls.wait_ms(10000))\n\
               task.sleep(5ms)";
    run(Config::new(), &log, src).0.unwrap();
    assert_eq!(*log.lock(), ["callee defer", "native dropped", "caller defer", "native dropped"]);
}

#[test]
fn native_calls_enforce_the_memory_limit_in_place() {
    // No safe point runs between dropping `junk` and the native call, so
    // only the call's own collection frees the 8 MB of strings.
    let src = "var block = \"x\"\n\
               for _ in 0..10 { block = block + block }\n\
               var junk: [str] = []\n\
               for i in 0..8000 { junk.push(\"\\(i)\" + block) }\n\
               junk = []\n\
               calls.check_memory(10 << 20)\n\
               print(\"fits\")\n\
               calls.check_memory(64 << 20)";
    let (r, out) = run(memory_limit(16 << 20), &Log::default(), src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))) && out == "fits\n", "{r:?} {out:?}");
    // Each callback result stays rooted until the call returns.
    let (r, _) = run(memory_limit(16 << 20), &Log::default(), "calls.apply((x) => x + 1, 0, 1200000)");
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
}

#[test]
fn require_prompts_and_resumes_the_future() {
    let answers = Arc::new(Mutex::new(vec![PromptAnswer::AllowOnce, PromptAnswer::DenyOnce]));
    let config = Config::new()
        .custom_policy(|cap: &Capability| if matches!(cap, Capability::Custom { .. }) { Decision::Prompt } else { Decision::Allow })
        .prompter(move |_: Capability| {
            let answer = answers.lock().remove(0);
            async move { answer }
        });
    let out = run_ok(config, "print(calls.open_door(\"front\"))\nprint(calls.open_door(\"back\"))");
    assert_eq!(out, "Ok(\"opened front\")\nErr(Error(message: \"permission denied: door back\", kind: \"permission_denied\"))\n");
}

#[test]
fn misused_contexts_fail() {
    assert_eq!(run_ok(Config::new(), "calls.save_context()\nprint(calls.use_saved_context())"), "false\n");
    let (r, _) = run(Config::new(), &Log::default(), "calls.two_calls(() => ())");
    assert!(matches!(&r, Err(Error::Internal { message, .. }) if message.contains("two VM requests")), "{r:?}");
}

#[test]
fn a_spent_tokio_budget_does_not_hang_a_clock_that_yields_at_once() {
    let clock = Arc::new(StoppedClock(sandlang::platform::Instant::now()));
    let src = "let t = spawn calls.spend_budget(1000)\ncalls.spend_budget(1000)\nt.wait()\nprint(\"done\")";
    assert_eq!(run_ok(Config::new().clock(clock), src), "done\n");
}

#[test]
fn engines_and_sessions_run_native_calls() {
    fn assert_send<T: Send>(_: &T) {}
    let modules = || HostModules::new([calls_module(&Log::default())]).unwrap();
    let mut engine = Engine::new(Config::new().host_modules(modules()));
    assert_send(&engine);
    assert_send(&engine.run_source("main.sand", "import calls"));

    let mut session = sandlang::session::Session::new(Config::new().host_modules(modules()));
    let rt = rt();
    assert_eq!(rt.block_on(session.eval("import calls\ncalls.apply((x) => x * 2, 21, 1)")).unwrap().as_deref(), Some("42"));
    rt.block_on(session.eval("let t = spawn calls.apply((x) => {\n    task.sleep(5ms)\n    x + 1\n}, 1, 1)")).unwrap();
    rt.block_on(session.run_tasks_until(async { tokio::time::sleep(Duration::from_millis(50)).await }, |e| panic!("{e}")));
    assert_eq!(rt.block_on(session.eval("t.wait()")).unwrap().as_deref(), Some("2"));
}
