# Synchronous native functions that call Sand

A synchronous native Rust function can call a Sand function on the Rust stack, as Lua's `lua_call` does. The callee runs on the fiber of the calling task until it returns. The callee cannot suspend. This design is the synchronous counterpart of the async `NativeContext::call` from #44.

## Terms

- **Calling native**: a native function whose first parameter is `&mut CallContext`.
- **Callback**: a Sand function that a calling native runs through `CallContext::call`.
- **Suspension**: a stop that returns control to the scheduler: a task wait, an async native call, a permission prompt, or a self-cancellation.
- **Run ending**: a `RunEnding` value, which is an exit code or a trap with its stack trace.

## Public API

`NativeModule::function` accepts a fourth kind of function. The first parameter selects the kind:

```rust
module.function("apply", |cx: &mut CallContext, f: VmVal<fn(Param<0>) -> Param<0>>, x: Param<0>| -> Result<Param<0>, Fail> {
    Ok(Param::from_raw(cx.call(f.raw(), &[x.raw()])?))
});
```

```rust
impl CallContext<'_> {
    pub fn vm(&mut self) -> &mut Vm;
    pub fn call(&mut self, callee: VmValue, args: &[VmValue]) -> Result<VmValue, Fail>;
    pub fn root(&mut self, value: VmValue) -> RootedValue;
    pub fn value(&self, rooted: RootedValue) -> VmValue;
    pub fn step<R, E: NativeError>(&mut self, f: impl FnMut(&mut Vm) -> Result<R, E>) -> Result<R, E::FutureError>;
}
```

The return type of a calling native implements `NativeReturn<Fail>`, as the output of an async native future does.

## Decisions

### 1. A native reaches the caller's fiber through `NativeFn`

`NativeFn` changes from `Fn(&mut Vm, &[VmValue])` to `Fn(&mut Vm, &mut Fiber, usize)`. The `usize` is the end of the argument slots on the fiber stack. Each wrapper reads the last `n` slots, where `n` is the constant number of script parameters. A calling native reads its owned parameters first. It then lends `&mut Vm` and `&mut Fiber` to `CallContext`.

The new kind keeps the 282 `&mut Vm` closures unchanged. Those natives follow a restart contract that a calling native cannot follow (decision 3), so the two contracts need two kinds.

### 2. Roots live on the fiber

`RootedValue` is the handle for both kinds of call. An async native keeps its roots in its `NativeCallState`, because its `'static` future cannot reach the fiber during a poll. A calling native pushes its roots onto `Fiber::native_roots`. The collector traces that stack, `Fiber::footprint` counts it, and the wrapper truncates it when the native returns. The stack is reused, so a call does not allocate.

### 3. A calling native runs once

The engine never runs a calling native a second time, so the VM does not need to know whether the native called Sand:

- The return type cannot express a permission request or a collection request. `Vm::require` and `Vm::resource_slot` return `Ctl`, so `?` on them does not compile in a calling native. The native calls them through `CallContext::step`.
- `CallContext::step` collects garbage in place and runs its closure again. `NativeContext::step` shares the same attempt function.
- The wrapper converts any `Fail` from the closure into a run ending, as `poll_native_call` does for a future's output. `call_native` therefore never restarts the native.

A calling native cannot prompt for a permission. A prompt is a suspension.

### 4. A run ending travels inside `Fail`

`FailKind::Ended(RunEnding)` holds the run ending of a callback with its trace. The native propagates it with `?`. `Vm::run_ending_of` returns it unchanged. `Fail::as_limit` returns `None` for it, so `call_native` does not restart the native.

### 5. A callback reuses the async trampoline

`Vm::run_callback` pushes a `CALLBACK_PROTO` frame with `begin_callback`, and runs `run_slice` until `Op::ResumeNative` stops it:

| `FiberStop`                                     | Handling                                                                                       |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `CallbackReturn`                                | Return the value.                                                                              |
| `Preempt`                                       | Check limits with `VmMeter::check_limits`, start the next slice under the same deadlines, and continue. |
| `YieldNow`                                      | Continue.                                                                                      |
| `Blocked`, `AsyncNative`, `Prompt`, `Cancelled` | End the run with an `invalid` error and the trace at the suspension.                           |
| `Done`, `Unwound`, `CollectAndRetry`            | Cannot occur.                                                                                  |

On every outcome, `run_callback` restores the frames of the fiber. A run ending does not run `defer` blocks (SPEC §7.1), so the callee needs no other cleanup. `run_callback` also saves and restores `Vm::collected` and `Vm::caller_site`, because the callee makes native calls of its own.

A task can cancel itself inside a callback. The Rust frame of the native sits between the callee frames and the caller frames, and the scheduler cannot unwind it. Self-cancellation therefore ends the run, as other suspensions do.

### 6. Depth limit

`Vm::nested_callbacks` counts the callbacks on the Rust stack. Lua counts nested C calls the same way (`nCcalls`, `LUAI_MAXCCALLS`). The limit lets nested callbacks use at most half of a 512 KiB stack. Each level uses 2,368 bytes in a release build and 2,320 bytes in the test profile, so the limit is 110. `begin_callback` also checks `max_call_depth`, so callback frames count toward it for both kinds of call.

### 7. `Num` evidence uses the context

`Num::zero` and `Num::one` call a native from inside a native. That call now needs the fiber, so they take `&mut CallContext`, and `List.sum` and `List.product` are calling natives. The evidence call stays a direct native call: a callback through `CallContext::call` made `xs.sum()` on a 3-element list 1.9 times slower.

## Speed

Each number is the best of 7 runs of 1M calls to `fn inc(x: int) -> int { x + 1 }`, on an Apple M1:

| Call                               | ns per call |
| ---------------------------------- | ----------: |
| Sand call through a function value |        15.8 |
| Synchronous native call            |        26.2 |
| Async native call                  |        88.3 |

The CPU Profiler attributed the cost of a callback before the changes below. These changes made the callback faster:

- **`#[repr(u32)]` on `VmValue`.** The default layout put the `bool` of `Bool(bool)` at byte 1, so `rustc` copied bytes 1–7 of a `VmValue` with two overlapping stores. The next load needed both stores and stalled store-to-load forwarding. The stall cost 13 ns of each callback in host code. With a 4-byte tag, every payload starts at byte 4 or byte 8.
- **The interpreter borrows the program and native table.** `Vm::interp` takes `&Arc<Program>` and `&Arc<Vec<NativeEntry>>`. The scheduler clones them once for each batch of turns, and a `CallContext` clones them on its first callback. Passing `&Program` and `&[NativeEntry]` instead added a register to the dispatch loop and slowed `struct method dispatch 2M` by 22%.
- **Panics are caught once per turn,** not once per slice, so a callback skips `catch_unwind`.
- **`CallContext` caches the callback frame base,** because the caller's frame stays the same during the call.
- **`begin_callback` pushes a closure callee's frame itself,** which skips the dispatch of the trampoline's `CallR`. Native and iterator callees still go through `CallR`.
- **`begin_callback` copies arguments in a loop,** because `copy_from_slice` called `memmove` for a single argument.
- **`FiberStop::Prompt` boxes its `Capability`,** which shrinks `FiberStop` from 104 bytes to 40.

The interpreter run of the callee costs about the same as one Sand call. The rest of a callback is spread over `run_callback`, `finish_callback`, `run_slice`, and `begin_callback`, and no part of it is above 2 ns.

`native method calls 10M` in `benches/vm.rs` is about 1.5% slower, because each native wrapper loads the stack pointer and length from the fiber. The old signature received them in registers. Only a lent fiber, a `HostFn` variant per call kind, or `unsafe` code removes those loads.
