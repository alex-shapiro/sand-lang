//! Integration tests for async native functions and Sand callbacks.

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
use sandlang::vm::value::VmValue;
use sandlang::vm::{Fail, Vm, WaitKind};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Wake, Waker};
use std::time::Duration;

type EventLog = Arc<Mutex<Vec<String>>>;

/// Appends `"native dropped"` to the log on drop.
struct LogOnDrop(EventLog);

impl Drop for LogOnDrop {
    fn drop(&mut self) {
        self.0.lock().push("native dropped".into());
    }
}

type Callback = VmVal<fn() -> ()>;

/// Wrapper future that polls `inner` again only after `inner` wakes its waker,
/// matching combinators such as `FuturesUnordered`.
struct WakeGatedFuture<F> {
    inner: Pin<Box<F>>,
    gate: Arc<WakeGate>,
}

/// Waker state for [`WakeGatedFuture`]. The scheduler polls the outer future
/// after each callback, so this waker records readiness locally without
/// forwarding wakes.
struct WakeGate {
    is_woken: AtomicBool,
}

impl Wake for WakeGate {
    fn wake(self: Arc<Self>) {
        self.is_woken.store(true, Ordering::SeqCst);
    }
}

impl<F> WakeGatedFuture<F> {
    fn new(inner: F) -> WakeGatedFuture<F> {
        WakeGatedFuture { inner: Box::pin(inner), gate: Arc::new(WakeGate { is_woken: AtomicBool::new(true) }) }
    }
}

impl<F: Future> Future for WakeGatedFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<F::Output> {
        if !self.gate.is_woken.swap(false, Ordering::SeqCst) {
            return Poll::Pending;
        }
        let waker = Waker::from(self.gate.clone());
        self.inner.as_mut().poll(&mut std::task::Context::from_waker(&waker))
    }
}

/// Builds the `calls` host module and records events in `log`.
fn calls_module(log: &EventLog) -> NativeModule {
    let mut module = NativeModule::new("calls");
    module.source("pub native fn apply<T>(f: fn(T) -> T, x: T, times: int) -> T");
    module.function(
        "apply",
        |sand_fn: VmVal<fn(Param<0>) -> Param<0>>, value: Param<0>, times: i64| -> Result<Later<Param<0>, Fail>, Fail> {
            Ok(Later::new(WaitKind::Io, move |mut context| {
                let (sand_fn, mut current) = (context.root(sand_fn.raw()), context.root(value.raw()));
                async move {
                    for _ in 0..times {
                        current = context.call(sand_fn, &[current]).await;
                    }
                    Ok(Param::from_raw(context.value(current)))
                }
            }))
        },
    );
    // Allocates `"kept"` before the callback so only the call's root table references it during GC.
    module.function("keep", |vm: &mut Vm, callback: Callback| -> Result<Later<String, Fail>, Fail> {
        let kept = "kept".into_vm_value(vm)?;
        Ok(Later::new(WaitKind::Io, |mut context| {
            let (kept, callback) = (context.root(kept), context.root(callback.raw()));
            async move {
                context.call(callback, &[]).await;
                Ok(context.with_vm(|vm| vm.str(context.value(kept)).to_string()))
            }
        }))
    });
    let log_for_fn = log.clone();
    module.function("log", move |entry: String| log_for_fn.lock().push(entry));
    let log_for_guard = log.clone();
    module.function("guard", move |callback: Callback| -> Result<Later<(), Fail>, Fail> {
        let guard = LogOnDrop(log_for_guard.clone());
        Ok(Later::new(WaitKind::Io, |mut context| {
            let callback = context.root(callback.raw());
            async move {
                let _guard = guard;
                context.call(callback, &[]).await;
                Ok(())
            }
        }))
    });
    // Races a callback request against an immediately ready branch, then sleeps.
    module.function("race_callback", |callback: Callback| -> Result<Later<String, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, |mut context| {
            let callback = context.root(callback.raw());
            async move {
                let winner = tokio::select! {
                    biased;
                    _ = context.call(callback, &[]) => "callback",
                    _ = std::future::ready(()) => "ready",
                };
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(winner.to_string())
            }
        }))
    });
    module.function("wait_ms", |ms: i64| async move { tokio::time::sleep(Duration::from_millis(ms as u64)).await });
    module.function("spend_budget", |count: i64| async move {
        for _ in 0..count {
            tokio::task::consume_budget().await;
        }
    });
    module.function("allocate", |byte_count: i64| -> Result<Later<i64, Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, move |mut context| async move {
            context
                .step(|vm| -> Result<i64, Fail> {
                    let text = "x".repeat(byte_count as usize).into_vm_value(vm)?;
                    Ok(vm.str(text).len() as i64)
                })
                .await
        }))
    });
    module.function("open_resource", || -> Result<Later<(), HostError>, HostError> {
        Ok(Later::new(WaitKind::Io, |mut context| async move {
            tokio::task::yield_now().await;
            context.step(|vm| vm.resource_slot().map(drop)).await
        }))
    });
    module.function("open_door", |name: String| -> Result<Later<String, HostError>, HostError> {
        Ok(Later::new(WaitKind::Io, |mut context| async move {
            let door = Capability::Custom { name: "door".into(), detail: name.clone() };
            // Check a resource slot before prompting so `retry_stress` triggers garbage collection first.
            context
                .step(|vm| {
                    vm.resource_slot()?;
                    vm.require(&door)
                })
                .await?;
            Ok(format!("opened {name}"))
        }))
    });
    // Stores the first call's `NativeContext` so a second call can test out-of-poll access.
    let saved_context: Arc<Mutex<Option<NativeContext>>> = Arc::default();
    let saved_for_store = saved_context.clone();
    module.function("save_context", move || -> Result<Later<(), Fail>, Fail> {
        let saved_for_store = saved_for_store.clone();
        Ok(Later::new(WaitKind::Io, move |context| {
            *saved_for_store.lock() = Some(context);
            async { Ok(()) }
        }))
    });
    module.function("use_saved_context", move || -> Result<Later<(bool, bool), Fail>, Fail> {
        let mut stale_context = saved_context.lock().take().expect("a saved context");
        Ok(Later::new(WaitKind::Io, |_| async move {
            let can_access_vm = catch_unwind(AssertUnwindSafe(|| stale_context.with_vm(|_| ()))).is_ok();
            let callee = stale_context.root(VmValue::Unit);
            let mut request_future = std::pin::pin!(stale_context.call(callee, &[]));
            let can_poll_request =
                std::future::poll_fn(|cx| Poll::Ready(catch_unwind(AssertUnwindSafe(|| request_future.as_mut().poll(cx))).is_ok())).await;
            Ok((can_access_vm, can_poll_request))
        }))
    });
    module.function("call_behind_own_waker", |callback: Callback| -> Result<Later<(), Fail>, Fail> {
        Ok(Later::new(WaitKind::Io, |mut context| {
            let callback = context.root(callback.raw());
            async move {
                WakeGatedFuture::new(context.call(callback, &[])).await;
                Ok(())
            }
        }))
    });
    module
}

fn run(config: Config, log: &EventLog, source: &str) -> (Result<i32, Error>, String) {
    let stdout = MemorySink::new();
    let config = config.host_modules(HostModules::new([calls_module(log)]).unwrap()).stdout(stdout.clone());
    let result = rt().block_on(Engine::new(config).run_source("main.sand", &format!("import calls\n{source}")));
    (result.map(|outcome| outcome.exit_code), stdout.contents())
}

fn run_ok(config: Config, source: &str) -> String {
    let (result, stdout) = run(config, &EventLog::default(), source);
    result.unwrap();
    stdout
}

fn memory_limit(bytes: usize) -> Config {
    Config::new().limits(Limits { max_memory: Some(bytes), ..Limits::default() })
}

#[test]
fn callbacks_return_their_results_to_the_native_call() {
    assert_eq!(run_ok(Config::new(), "print(calls.apply((s) => s + \"!\", \"hi\", 1000).len())"), "1002\n");
}

#[test]
fn callbacks_wait_on_io_and_channels_while_other_tasks_run() {
    // Wait for a channel message sent by another task during the callback.
    let source = "let ch: Channel<int> = Channel()\n\
                  let sender = spawn {\n\
                      calls.wait_ms(5)\n\
                      ch.send(7)\n\
                  }\n\
                  print(calls.apply((x) => {\n\
                      task.sleep(1ms)\n\
                      x + (ch.recv() ?? 0)\n\
                  }, 35, 1))";
    assert_eq!(run_ok(Config::new(), source), "42\n");
}

#[test]
fn collector_keeps_rooted_values_during_callbacks() {
    let source = "print(calls.keep(() => {\n\
                      var xs: [str] = []\n\
                      for i in 0..2000 { xs.push(\"garbage \\(i)\") }\n\
                  }))";
    assert_eq!(run_ok(Config::new().gc_stress(true), source), "kept\n");
}

#[test]
fn engine_enforces_call_depth_limit_across_nested_callbacks() {
    let source = "fn down(n: int) -> int { if n == 0 { 0 } else { calls.apply(down, n - 1, 1) + 1 } }\nprint(down(100))";
    assert_eq!(run_ok(Config::new().gc_stress(true), source), "100\n");

    let limits = Limits { max_call_depth: 50, ..Limits::default() };
    let (result, _) = run(Config::new().limits(limits), &EventLog::default(), source);
    assert!(matches!(&result, Err(Error::Runtime { message, .. }) if message.contains("stack overflow")), "{result:?}");
}

#[test]
fn exit_in_a_callback_ends_the_run() {
    let (result, stdout) = run(Config::new(), &EventLog::default(), "calls.apply((x) => { exit(3)\nx }, 1, 1)\nprint(\"unreachable\")");
    assert_eq!(result.unwrap(), 3);
    assert_eq!(stdout, "");
}

#[test]
fn callback_error_traces_include_outer_caller() {
    let source = "fn divide(x: int) -> int { x / 0 }\nfn outer() -> int { calls.apply(divide, 1, 1) }\nprint(outer())";
    let (result, _) = run(Config::new(), &EventLog::default(), source);
    let Err(Error::Runtime { trace, .. }) = result else { panic!("{result:?}") };
    let functions: Vec<&str> = trace.iter().map(|frame| frame.function.as_str()).collect();
    assert_eq!(functions, ["divide", "outer", "<top level>"]);
}

#[test]
fn cancellation_drops_native_futures_in_reverse_call_order() {
    let log = EventLog::default();
    let source = "let t = spawn {\n\
                      defer { calls.log(\"caller defer\") }\n\
                      calls.guard(() => {\n\
                          defer { calls.log(\"callee defer\") }\n\
                          calls.wait_ms(10000)\n\
                      })\n\
                  }\n\
                  task.sleep(5ms)\n\
                  t.cancel()";
    run(Config::new(), &log, source).0.unwrap();
    assert_eq!(*log.lock(), ["callee defer", "native dropped", "caller defer"]);
}

#[test]
fn cancelling_a_host_wait_lets_the_engine_report_a_deadlock() {
    // Dropping the cancelled host wait must decrement the wait count so `ch.recv()` reports a deadlock.
    let limits = Limits { max_wall_time: Some(Duration::from_secs(5)), ..Limits::default() };
    let source = "let t = spawn calls.wait_ms(10000)\n\
                  task.sleep(5ms)\n\
                  t.cancel()\n\
                  let ch: Channel<int> = Channel()\n\
                  ch.recv()";
    let (result, _) = run(Config::new().limits(limits), &EventLog::default(), source);
    assert!(matches!(&result, Err(Error::Runtime { message, .. }) if message.contains("deadlock")), "{result:?}");
}

#[test]
fn ending_the_run_drops_native_futures_waiting_on_callbacks() {
    let log = EventLog::default();
    run(Config::new(), &log, "spawn calls.guard(() => calls.wait_ms(10000))\ntask.sleep(5ms)").0.unwrap();
    assert_eq!(*log.lock(), ["native dropped"]);
}

#[test]
fn step_collects_garbage_in_place_on_allocation() {
    // Reassigning `strings` leaves 8 MB unreachable before a safe point, so `step` must collect garbage in place.
    let source = "var block = \"x\"\n\
                  for _ in 0..10 { block = block + block }\n\
                  var strings: [str] = []\n\
                  for i in 0..8000 { strings.push(\"\\(i)\" + block) }\n\
                  strings = []\n\
                  print(calls.allocate(10 << 20))\n\
                  calls.allocate(64 << 20)";
    let (result, stdout) = run(memory_limit(16 << 20), &EventLog::default(), source);
    assert!(matches!(result, Err(Error::Limit(LimitKind::Memory))), "{result:?}");
    assert_eq!(stdout, "10485760\n");
}

#[test]
fn engine_retries_step_resource_checks_after_collection() {
    // `retry_stress` forces the first check in the poll to collect garbage and retry.
    assert_eq!(run_ok(Config::new().retry_stress(true), "print(calls.open_resource())"), "Ok(())\n");
}

#[test]
fn engine_counts_callback_results_against_memory_limit() {
    let (result, _) = run(memory_limit(16 << 20), &EventLog::default(), "calls.apply((x) => x + 1, 0, 1200000)");
    assert!(matches!(result, Err(Error::Limit(LimitKind::Memory))), "{result:?}");
}

#[test]
fn step_prompts_for_permission_and_resumes_future() {
    for stress in [false, true] {
        let answers = Arc::new(Mutex::new(vec![PromptAnswer::AllowOnce, PromptAnswer::DenyOnce]));
        let config = Config::new()
            .retry_stress(stress)
            .custom_policy(|cap: &Capability| if matches!(cap, Capability::Custom { .. }) { Decision::Prompt } else { Decision::Allow })
            .prompter(move |_: Capability| {
                let answer = answers.lock().remove(0);
                async move { answer }
            });
        let stdout = run_ok(config, "print(calls.open_door(\"front\"))\nprint(calls.open_door(\"back\"))");
        let denied = "Err(Error(message: \"permission denied: door back\", kind: \"permission_denied\"))";
        assert_eq!(stdout, format!("Ok(\"opened front\")\n{denied}\n"), "retry stress: {stress}");
    }
}

#[test]
fn context_panics_outside_its_call_poll() {
    assert_eq!(run_ok(Config::new(), "calls.save_context()\nprint(calls.use_saved_context())"), "(false, false)\n");
}

#[test]
fn dropping_losing_select_branch_withdraws_callback_request() {
    let log = EventLog::default();
    let (result, stdout) = run(Config::new(), &log, "print(calls.race_callback(() => calls.log(\"callback ran\")))");
    result.unwrap();
    assert_eq!(stdout, "ready\n");
    assert!(log.lock().is_empty(), "{:?}", log.lock());
}

#[test]
fn request_wakes_combinator_with_custom_waker() {
    let limits = Limits { max_wall_time: Some(Duration::from_secs(5)), ..Limits::default() };
    let log = EventLog::default();
    run(Config::new().limits(limits), &log, "calls.call_behind_own_waker(() => calls.log(\"called\"))").0.unwrap();
    assert_eq!(*log.lock(), ["called"]);
}

#[test]
fn engine_yields_to_tokio_after_spending_coop_budget() {
    let clock = Arc::new(StoppedClock(sandlang::platform::Instant::now()));
    let source = "let t = spawn calls.spend_budget(1000)\ncalls.spend_budget(1000)\nt.wait()\nprint(\"done\")";
    assert_eq!(run_ok(Config::new().clock(clock), source), "done\n");
}

#[test]
fn sessions_run_callbacks_in_inputs_and_in_background_tasks() {
    let config = Config::new().host_modules(HostModules::new([calls_module(&EventLog::default())]).unwrap());
    let mut session = sandlang::session::Session::new(config);
    let runtime = rt();
    let doubled = runtime.block_on(session.eval("import calls\ncalls.apply((x) => x * 2, 21, 1)")).unwrap();
    assert_eq!(doubled.as_deref(), Some("42"));

    runtime.block_on(session.eval("let t = spawn calls.apply((x) => {\n    task.sleep(5ms)\n    x + 1\n}, 1, 1)")).unwrap();
    let wait = async { tokio::time::sleep(Duration::from_millis(50)).await };
    runtime.block_on(session.run_tasks_until(wait, |e| panic!("{e}")));
    assert_eq!(runtime.block_on(session.eval("t.wait()")).unwrap().as_deref(), Some("2"));
}
