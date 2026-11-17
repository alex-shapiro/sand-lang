# Synchronous native functions that call Sand

A synchronous native Rust function can call a Sand function directly on the Rust stack, like Lua's `lua_call`. The engine runs the callee to completion on the calling task's fiber. The engine rejects any task suspension during the call. `CallContext::call` complements the async `NativeContext::call` API from #44.

## Terms

- **`CallContext` function**: a synchronous native function whose first parameter is `&mut CallContext`.
- **Callback**: a Sand function that a native function runs through `CallContext::call` or `NativeContext::call`.
- **Suspension**: a fiber stop that returns control to the scheduler.
- **Run ending**: an exit code or VM trap in `RunEnding`.

## Public API

`NativeModule::function` accepts a fourth kind of Rust function. The caller selects this kind with a `&mut CallContext` first parameter:

```rust
module.function("apply", |context: &mut CallContext, sand_fn: VmVal<fn(Param<0>) -> Param<0>>, input: Param<0>| -> Result<Param<0>, Fail> {
    Ok(Param::from_raw(context.call(sand_fn.raw(), &[input.raw()])?))
});
```

```rust
impl CallContext<'_> {
    pub fn vm(&self) -> &Vm;
    pub fn call(&mut self, callee: VmValue, args: &[VmValue]) -> Result<VmValue, Fail>;
    pub fn root(&mut self, value: VmValue) -> RootedValue;
    pub fn value(&self, rooted: RootedValue) -> VmValue;
    pub fn step<R, E: NativeError>(&mut self, f: impl FnMut(&mut Vm) -> Result<R, E>) -> Result<R, E::FutureError>;
}
```

Both `CallContext` functions and async native futures require a `NativeReturn<Fail>` output type.

## Decisions

### 1. Pass the calling fiber through `NativeFn`

`NativeFn` takes `(&mut Vm, &mut Fiber, usize)`. The `usize` argument is the exclusive end index of the call's arguments on `Fiber::stack`. Each generated wrapper slices `&fiber.stack[..args_end]`. It then reads its trailing script parameters. A `CallContext` wrapper reads its owned parameters first. It then passes `&mut Vm` and `&mut Fiber` to `CallContext::new`.

We keep all 282 existing `&mut Vm` functions unchanged with a fourth function kind. The VM reruns a `&mut Vm` function on a limit retry. It runs a `CallContext` function once per call.

### 2. Store roots on the calling fiber

Both `CallContext` and `NativeContext` return `RootedValue` handles. `NativeContext` stores its roots in `NativeCallState`. Its `'static` future reads that shared state during each poll. `CallContext::root` pushes values onto `Fiber::native_roots`. The garbage collector traces `Fiber::native_roots`. `Fiber::footprint` counts that vector's capacity against `max_memory`. `CallContext::finish` truncates `Fiber::native_roots` on return. Idle fibers retain up to 1,024 root slots across calls.

`CallContext::call` returns an unrooted `VmValue`. Callback loops therefore use constant memory. Only `call` and `step` can trigger garbage collection. The collector therefore preserves each returned `VmValue` until the next `call` or `step`. By contrast, `NativeContext::call` roots every callback result. The scheduler can collect garbage between async polls.

### 3. Run each `CallContext` function once

The engine runs each `CallContext` function once per call. Callbacks therefore execute their side effects once:

- `CallContext::vm` returns shared `&Vm` access. Only `CallContext::step` provides `&mut Vm` for limit and permission checks.
- On a memory or resource limit, `CallContext::step` collects garbage in place. It then runs its closure again. `CallContext::step` and `NativeContext::step` share `attempt_step`.
- `CallContext::finish` converts any `Fail` into a terminal `RunEnding`. `call_native` therefore returns that `RunEnding` directly.

The scheduler must suspend a task for a permission prompt. `CallContext::step` therefore ends the run with an `invalid` error on any prompt request.

### 4. Wrap callback run endings in `Fail`

`FailKind::Ended(RunEnding)` stores a terminal `RunEnding` inside `Fail`. Host code propagates that `Fail` with `?`. `Vm::run_ending_of` unwraps `FailKind::Ended` directly. `Fail::as_limit` returns `None` for `FailKind::Ended`. `call_native` therefore skips limit-retry handling.

### 5. Reuse the `CALLBACK_PROTO` trampoline

`Vm::run_callback` pushes a `CALLBACK_PROTO` frame with `begin_callback`. `Vm::run_callback_slices` then runs `run_slice` in a loop for each `FiberStop`:

- `CallbackReturn(value)`: return `value` to the caller.
- `Preempt`: check time and interrupt limits with `VmMeter::check_limits`. Start the next fuel slice under the active deadlines. Continue the loop.
- `YieldNow`: continue running the callback.
- `Blocked`, `AsyncNative`, `Prompt`, `Cancelled`: end the run with an `invalid` error and the stack trace at that stop site.
- `Done`, `Unwound`, `CollectAndRetry`: unreachable. `CALLBACK_PROTO` catches top-level returns. `run_slice` handles `CollectAndRetry`.

On exit, `run_callback` truncates `fiber.frames` to its pre-call depth. It also restores `Vm::caller_site`. Under SPEC §7.1, terminal run endings skip `defer` blocks. The VM needs only that frame truncation for callee cleanup.

`CallContext` stores the first terminal run ending. Later `call` and `step` calls return that stored ending. `CallContext::finish` returns the same ending even if host code ignores the error.

If a callback starts a new fuel slice, `finish` zeroes the remaining slice fuel. The scheduler then preempts the calling task after the native call returns.

The engine also ends the run if a callback cancels its own task. An active Rust stack frame sits between the callee frames and the caller frames. The scheduler cannot unwind across that Rust frame. It therefore rejects `Cancelled` with an `invalid` error.

### 6. Bound Rust stack growth and call depth

The engine caps synchronous callbacks at 256 KiB of Rust stack. 256 KiB is half of the minimum 512 KiB thread stack. `run_callback` measures stack growth from the stack address recorded at the start of the fiber turn. Lua's `LUAI_MAXCCALLS` bounds only the callback count. Native frames between callbacks can allocate arbitrary stack space. `begin_callback` also enforces `max_call_depth` across both synchronous and async callbacks.

### 7. Pass `CallContext` to `Num` evidence methods

`Num::zero` and `Num::one` invoke prelude `NativeFn` entries. Those entries require `&mut Fiber`. `Num::zero` and `Num::one` therefore take `&mut CallContext`. `List.sum` and `List.product` take `&mut CallContext` as well. `Num::call_method` invokes that prelude `NativeFn` directly. In prototype benchmarks, calling `Num` methods through `CallContext::call` slowed `xs.sum()` on a three-element list by 1.9x.

## Speed

Each number below is the best of four interleaved runs of 1,000,000 calls to `fn inc(x: int) -> int { x + 1 }` on an Apple M1:

| Call                               | ns per call |
| ---------------------------------- | ----------: |
| Sand call through a function value |        16.2 |
| Synchronous native call            |        29.0 |
| Async native call                  |        93.6 |

Five profile-guided changes reduced callback overhead:

- **Align `VmValue` payloads with `#[repr(u32)]`.** The default enum layout placed `Bool(bool)`'s payload at byte 1. `rustc` then copied bytes 1 through 7 of a `VmValue` with two overlapping stores. Each load from a copied value stalled store-to-load forwarding for 13 ns. A 4-byte tag aligns every payload to byte 4 or byte 8.
- **Pass `&Arc<Program>` and `&Arc<Vec<NativeEntry>>` to `Vm::interp`.** The scheduler clones both handles once per batch of turns. `CallContext` clones them on its first callback. In prototype benchmarks, `&Program` and `&[NativeEntry]` parameters spilled an extra register in the dispatch loop. That spill slowed `struct method dispatch 2M` by 22%.
- **Catch panics once per fiber turn.** The scheduler wraps `run_fiber_turn` in `unwind::catch` once per turn. Synchronous callbacks run inside that turn guard.
- **Cache the callback stack base on `CallContext`.** The caller's frame stays fixed during a synchronous native call. `CallContext` computes `ProgramCode` and the callback frame base on its first callback. Later callbacks in the same call reuse both values.
- **Box `Capability` inside `FiberStop::Prompt`.** A boxed `Capability` shrinks `FiberStop` from 104 bytes to 40 bytes.

The interpreter spends about 16 ns in the callee. A direct Sand call takes the same 16 ns. Callback setup and slice bookkeeping take the remaining 13 ns. Each helper takes less than 2 ns.

`begin_callback` enters every callee through the `CALLBACK_PROTO` trampoline. A prototype pushed closure frames directly in `begin_callback` to skip `CallR`. That prototype saved 3 ns per synchronous callback and 6 ns per async callback. It also duplicated the interpreter's call-entry logic. We therefore kept the trampoline.

`native method calls 10M` in `benches/vm.rs` runs about 1.5% slower on this branch. Under the new `NativeFn` signature, each wrapper loads the stack pointer and length from `Fiber`. Previously, the caller passed `&[VmValue]` in registers. Only a lent fiber, separate `HostFn` variants, or `unsafe` code can eliminate those loads.
