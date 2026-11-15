//! Tests for async native functions that call Sand functions.

mod common;

use common::{StoppedClock, rt};
use parking_lot::Mutex;
use sandlang::engine::{Config, Engine, Error, HostModules, LimitKind, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::convert::{HostError, IntoVmValue};
use sandlang::native::func::Later;
use sandlang::native::ty::{Param, VmVal};
use sandlang::policy::{Capability, Decision, PromptAnswer};
use sandlang::vm::{Fail, Vm, WaitKind};
use std::sync::Arc;
use std::time::Duration;

type Log = Arc<Mutex<Vec<String>>>;

/// Appends `entry` to `log` when dropped.
struct LogOnDrop(Log, &'static str);

impl Drop for LogOnDrop {
    fn drop(&mut self) {
        self.0.lock().push(self.1.into());
    }
}

/// The `calls` module, whose functions record events in `log`.
fn calls_module(log: &Log) -> NativeModule {
    let mut m = NativeModule::new("calls");
    m.source(
        "pub native fn apply<T>(f: fn(T) -> T, x: T) -> T\n\
         pub native fn apply_n<T>(f: fn(T) -> T, x: T, n: int) -> T\n\
         pub native fn combine<T>(f: fn(T, T) -> T, a: T, b: T) -> T",
    );
    m.function("apply", |f: VmVal<fn(Param<0>) -> Param<0>>, x: Param<0>| -> Result<Later<Param<0>, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, |cx| {
            let (f, x) = (cx.root(f.raw()), cx.root(x.raw()));
            async move {
                let y = cx.call(f, &[x]).await;
                Ok(Param::from_raw(cx.value(y)))
            }
        }))
    });
    m.function("apply_n", |f: VmVal<fn(Param<0>) -> Param<0>>, x: Param<0>, n: i64| -> Result<Later<Param<0>, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, move |cx| {
            let (f, mut x) = (cx.root(f.raw()), cx.root(x.raw()));
            async move {
                for _ in 0..n {
                    x = cx.call(f, &[x]).await;
                }
                Ok(Param::from_raw(cx.value(x)))
            }
        }))
    });
    m.function(
        "combine",
        |f: VmVal<fn(Param<0>, Param<0>) -> Param<0>>, a: Param<0>, b: Param<0>| -> Result<Later<Param<0>, Fail>, Fail> {
            Ok(Later::new(WaitKind::Io, |cx| {
                let (f, a, b) = (cx.root(f.raw()), cx.root(a.raw()), cx.root(b.raw()));
                async move { Ok(Param::from_raw(cx.value(cx.call(f, &[a, b]).await))) }
            }))
        },
    );
    // Allocates a string that only the call's roots reference, runs `f`
    // three times, and returns the string.
    m.function("keep", |vm: &mut Vm, n: i64, f: VmVal<fn() -> ()>| -> Result<Later<String, Fail>, Fail> {
        let kept = format!("kept {n}").into_vm_value(vm)?;
        Ok(Later::new(WaitKind::Io, |cx| {
            let (kept, f) = (cx.root(kept), cx.root(f.raw()));
            async move {
                for _ in 0..3 {
                    cx.call(f, &[]).await;
                }
                Ok(cx.with_vm(|vm| vm.str(cx.value(kept)).to_string()))
            }
        }))
    });
    let l = log.clone();
    m.function("log", move |entry: String| l.lock().push(entry));
    let l = log.clone();
    m.function("guard", move |f: VmVal<fn() -> ()>| -> Result<Later<(), Fail>, Fail> {
        let l = l.clone();
        Ok(Later::new(WaitKind::Io, |cx| {
            let f = cx.root(f.raw());
            async move {
                let _guard = LogOnDrop(l, "native dropped");
                cx.call(f, &[]).await;
                Ok(())
            }
        }))
    });
    m.function("spend_budget", |n: i64| async move {
        for _ in 0..n {
            tokio::task::consume_budget().await;
        }
    });
    // Saves the context of one call, and uses it during the poll of another.
    let saved: Arc<Mutex<Option<sandlang::native::context::NativeContext>>> = Arc::default();
    let save = saved.clone();
    m.function("save_context", move || -> Result<Later<(), Fail>, Fail> {
        let save = save.clone();
        Ok(Later::new(WaitKind::Io, move |cx| {
            *save.lock() = Some(cx);
            async { Ok(()) }
        }))
    });
    m.function("use_saved_context", move || -> Result<Later<bool, Fail>, Fail> {
        let saved = saved.lock().take();
        Ok(Later::new(WaitKind::Io, |_| async move {
            let saved = saved.expect("a saved context");
            Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| saved.with_vm(|_| ()))).is_ok())
        }))
    });
    m.function("wait_ms", |ms: i64| async move { tokio::time::sleep(Duration::from_millis(ms as u64)).await });
    m.function("check_memory", |bytes: i64| -> Result<Later<(), Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, move |cx| async move { cx.check_memory(bytes as usize).await }))
    });
    m.function("open_door", |name: String| -> Result<Later<String, HostError>, HostError> {
        Ok(Later::new(WaitKind::Io, |cx| async move {
            cx.require(&Capability::Custom { name: "door".into(), detail: name.clone() }).await?;
            Ok(format!("opened {name}"))
        }))
    });
    // Makes two VM requests in one poll.
    m.function("two_calls", |f: VmVal<fn() -> ()>| -> Result<Later<(), Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, |cx| {
            let f = cx.root(f.raw());
            async move {
                let mut first = std::pin::pin!(cx.call(f, &[]));
                let mut second = std::pin::pin!(cx.call(f, &[]));
                std::future::poll_fn(|w| {
                    let _ = first.as_mut().poll(w);
                    let _ = second.as_mut().poll(w);
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
    let mut engine = Engine::new(config);
    let r = rt().block_on(engine.run_source("main.sand", &format!("import calls\n{src}")));
    (r.map(|outcome| outcome.exit_code), out.contents())
}

fn run_ok(config: Config, src: &str) -> String {
    let (r, out) = run(config, &Log::default(), src);
    r.unwrap();
    out
}

#[test]
fn async_natives_call_sand_functions() {
    let src = "print(calls.apply((x) => x * 2, 21))\n\
               print(calls.apply((s) => s + \"!\", \"hi\"))\n\
               print(calls.apply_n((x) => x + 1, 0, 1000))\n\
               print(calls.combine((a, b) => a * 10 + b, 4, 2))";
    assert_eq!(run_ok(Config::new(), src), "42\nhi!\n1000\n42\n");
}

#[test]
fn callbacks_suspend_on_io_and_channels_while_other_tasks_run() {
    // The callback waits for a message that only another task sends.
    let src = "let ch: Channel<int> = Channel()\n\
               let sender = spawn {\n\
                   calls.wait_ms(5)\n\
                   task.sleep(5ms)\n\
                   ch.send(7)\n\
               }\n\
               print(calls.apply((x) => {\n\
                   task.sleep(1ms)\n\
                   calls.wait_ms(1)\n\
                   x + (ch.recv() ?? 0)\n\
               }, 35))\n\
               sender.wait()";
    assert_eq!(run_ok(Config::new(), src), "42\n");
}

#[test]
fn rooted_values_survive_collections_during_callbacks() {
    let src = "fn churn() {\n\
                   var xs: [str] = []\n\
                   for i in 0..2000 { xs.push(\"garbage \\(i)\") }\n\
               }\n\
               print(calls.keep(7, churn))\n\
               print(calls.apply_n((s) => \"<\" + s + \">\", \"x\", 200).len())";
    assert_eq!(run_ok(Config::new().gc_stress(true), src), "kept 7\n401\n");
}

#[test]
fn callbacks_nest_up_to_the_call_depth_limit() {
    let src = "fn down(n: int) -> int { if n == 0 { 0 } else { calls.apply(down, n - 1) + 1 } }\nprint(down(100))";
    assert_eq!(run_ok(Config::new().gc_stress(true), src), "100\n");

    let limits = Limits { max_call_depth: 50, ..Limits::default() };
    let (r, _) = run(Config::new().limits(limits), &Log::default(), src);
    assert!(matches!(&r, Err(Error::Runtime { message, .. }) if message.contains("stack overflow")), "{r:?}");
}

#[test]
fn cancellation_unwinds_callbacks_and_drops_native_futures_in_reverse_call_order() {
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
               task.sleep(5ms)\n\
               print(t.is_done())";
    let (r, out) = run(Config::new(), &log, src);
    r.unwrap();
    assert_eq!(out, "true\n");
    assert_eq!(*log.lock(), ["callee defer", "native dropped", "caller defer"]);
}

#[test]
fn ending_the_run_drops_native_futures_waiting_on_callbacks() {
    let log = Log::default();
    let src = "spawn calls.guard(() => calls.wait_ms(10000))\ntask.sleep(5ms)";
    let (r, _) = run(Config::new(), &log, src);
    r.unwrap();
    assert_eq!(*log.lock(), ["native dropped"]);
}

#[test]
fn check_memory_collects_garbage_in_place() {
    let limits = Limits { max_memory: Some(16 << 20), ..Limits::default() };
    // No safe point runs between dropping `junk` and the native call, so
    // only the call's own collection frees the 8 MB of strings.
    let src = "var block = \"x\"\n\
               for _ in 0..10 { block = block + block }\n\
               var junk: [str] = []\n\
               for i in 0..8000 { junk.push(\"\\(i)\" + block) }\n\
               junk = []\n\
               calls.check_memory(10 << 20)\n\
               print(\"fits\")\n\
               calls.check_memory(64 << 20)\n\
               print(\"unreachable\")";
    let (r, out) = run(Config::new().limits(limits), &Log::default(), src);
    assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
    assert_eq!(out, "fits\n");
}

#[test]
fn require_prompts_and_resumes_the_future() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let answers = Arc::new(Mutex::new(vec![PromptAnswer::AllowOnce, PromptAnswer::DenyOnce]));
    let (a, n) = (asked.clone(), answers.clone());
    let config = Config::new()
        .custom_policy(|cap: &Capability| if matches!(cap, Capability::Custom { .. }) { Decision::Prompt } else { Decision::Allow })
        .prompter(move |cap: Capability| {
            a.lock().push(cap);
            let answer = n.lock().remove(0);
            async move { answer }
        });
    let src = "print(calls.open_door(\"front\"))\nprint(calls.open_door(\"back\"))";
    assert_eq!(
        run_ok(config, src),
        "Ok(\"opened front\")\nErr(Error(message: \"permission denied: door back\", kind: \"permission_denied\"))\n"
    );
    assert_eq!(asked.lock().len(), 2);
}

#[test]
fn two_requests_in_one_poll_are_an_internal_error() {
    let (r, _) = run(Config::new(), &Log::default(), "calls.two_calls(() => ())");
    assert!(matches!(&r, Err(Error::Internal { message, .. }) if message.contains("two VM requests")), "{r:?}");
}

#[test]
fn engines_and_runs_with_native_calls_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let mut engine = Engine::new(Config::new().host_modules(HostModules::new([calls_module(&Log::default())]).unwrap()));
    assert_send(&engine);
    assert_send(&engine.run_source("main.sand", "import calls\nprint(calls.apply((x) => x, 1))"));
}

#[test]
fn a_spent_tokio_budget_does_not_hang_a_clock_that_yields_at_once() {
    let clock = Arc::new(StoppedClock(sandlang::platform::Instant::now()));
    let src = "let t = spawn calls.spend_budget(1000)\ncalls.spend_budget(1000)\nt.wait()\nprint(\"done\")";
    assert_eq!(run_ok(Config::new().clock(clock), src), "done\n");
}

#[test]
fn a_context_works_only_while_the_engine_polls_its_call() {
    assert_eq!(run_ok(Config::new(), "calls.save_context()\nprint(calls.use_saved_context())"), "false\n");
}
