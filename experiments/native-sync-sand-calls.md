# Synchronous native functions that call Sand

A synchronous native Rust function can call a Sand function directly on the Rust stack, like Lua's `lua_call`. The callee runs to completion on the calling task's fiber without suspending. This design complements the async `NativeContext::call` API from #44.

## Terms

- **`CallContext` function**: a synchronous native function whose first parameter is `&mut CallContext`.
- **Callback**: a Sand function that a `CallContext` function runs through `CallContext::call`.
- **Suspension**: a fiber stop that yields control to the scheduler instead of continuing execution.
- **Run ending**: a terminal `RunEnding` value: either an exit code or a VM trap.

## Public API

`NativeModule::function` accepts a fourth kind of Rust function. A `&mut CallContext` first parameter selects this kind:

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

`NativeFn` changes from `Fn(&mut Vm, &[VmValue])` to `Fn(&mut Vm, &mut Fiber, usize)`. The `usize` argument is the exclusive end index of the call's arguments on `Fiber::stack`. Each generated wrapper slices `&fiber.stack[..args_end]` and reads its trailing script parameters. A `CallContext` wrapper reads its owned parameters first. It then passes `&mut Vm` and `&mut Fiber` to `CallContext::new`.

Adding a separate function kind preserves all 282 existing `&mut Vm` functions unchanged. `&mut Vm` functions restart from the beginning on limit checks, whereas `CallContext` functions run once per call.

### 2. Store roots on the calling fiber

Both `CallContext` and `NativeContext` return `RootedValue` handles. An async native call stores its roots in `NativeCallState` so its `'static` future can reach them during polls. A `CallContext` call pushes its roots onto `Fiber::native_roots`. The garbage collector traces `Fiber::native_roots`, and `Fiber::footprint` counts its capacity against `max_memory`. `CallContext::finish` truncates `Fiber::native_roots` on return. Idle fibers retain up to 1,024 root slots to reuse capacity across calls.

`CallContext::call` returns an unrooted `VmValue` so callback loops run in constant memory. Garbage collection runs only inside `call` and `step`. Each returned `VmValue` therefore stays valid until the next `call` or `step`. By contrast, `NativeContext::call` roots every result because garbage collection can run between polls.

### 3. Run each `CallContext` function once

The engine runs each `CallContext` function once per call to avoid repeating callback side effects:

- `CallContext::vm` returns shared `&Vm` access. Only `CallContext::step` provides `&mut Vm` for operations that check limits or permissions.
- On a memory or resource limit, `CallContext::step` collects garbage in place before running its closure again. `CallContext::step` and `NativeContext::step` share `attempt_step`.
- `CallContext::finish` converts any `Fail` into a terminal `RunEnding`. `call_native` therefore returns that `RunEnding` directly without retrying the call.

A permission prompt requires suspending the task. `CallContext::step` therefore ends the run with an `invalid` error on any prompt request.

### 4. Wrap callback run endings in `Fail`

`FailKind::Ended(RunEnding)` stores a callback's terminal `RunEnding` and stack trace inside `Fail`. Host code propagates that `Fail` with `?`. `Vm::run_ending_of` unwraps `FailKind::Ended` directly. `Fail::as_limit` returns `None` for `FailKind::Ended`. That `None` result prevents limit-retry handling in `call_native`.

### 5. Reuse the `CALLBACK_PROTO` trampoline

`Vm::run_callback` pushes a `CALLBACK_PROTO` frame with `begin_callback`. `Vm::run_callback_slices` then runs `run_slice` in a loop until `Op::ResumeNative` returns:

- `CallbackReturn(value)`: return `value` to the caller.
- `Preempt`: check time and interrupt limits with `VmMeter::check_limits`. Start the next fuel slice under the active deadlines and continue.
- `YieldNow`: continue running the callback.
- `Blocked`, `AsyncNative`, `Prompt`, `Cancelled`: end the run with an `invalid` error and the stack trace at the suspension site.
- `Done`, `Unwound`, `CollectAndRetry`: unreachable because `CALLBACK_PROTO` catches top-level returns and `run_slice` handles `CollectAndRetry`.

Before returning, `run_callback` truncates `fiber.frames` to its pre-call depth and restores `Vm::caller_site`. Under SPEC §7.1, terminal run endings skip `defer` blocks. Frame truncation therefore completes callee cleanup.

`CallContext` stores the first terminal run ending so host code cannot ignore it. Subsequent `call` and `step` calls return that stored ending. `CallContext::finish` returns the same ending after the function exits.

If a callback starts a new fuel slice, `finish` zeroes the remaining slice fuel. The scheduler then preempts the calling task after the native call returns.

Self-cancellation inside a callback also ends the run. An active Rust stack frame sits between the callee frames and the caller frames. The scheduler cannot unwind across that Rust frame. It therefore rejects `Cancelled` with an `invalid` error.

### 6. Bound Rust stack growth and call depth

Nested synchronous callbacks can consume at most 256 KiB of Rust stack, half of a 512 KiB thread stack. `run_callback` measures stack growth from the stack address recorded at the start of the fiber turn. A fixed callback count like Lua's `LUAI_MAXCCALLS` leaves Rust stack usage unbounded. Native frames between callbacks can allocate arbitrary stack space. `begin_callback` also enforces `max_call_depth` across both synchronous and async callbacks.

### 7. Pass `CallContext` to `Num` evidence methods

`Num::zero` and `Num::one` take `&mut CallContext` so they can pass the calling fiber to the prelude's `NativeFn` entries. `List.sum` and `List.product` therefore take `&mut CallContext` as well. `Num::call_method` invokes the prelude `NativeFn` directly rather than going through `CallContext::call`. Routing that call through `CallContext::call` made `xs.sum()` on a three-element list 1.9 times slower.

## Speed

Each number below is the best of four interleaved runs of 1,000,000 calls to `fn inc(x: int) -> int { x + 1 }` on an Apple M1:

| Call                               | ns per call |
| ---------------------------------- | ----------: |
| Sand call through a function value |        16.2 |
| Synchronous native call            |        29.0 |
| Async native call                  |        93.6 |

Five profile-guided changes reduced callback overhead:

- **Align `VmValue` payloads with `#[repr(u32)]`.** The default enum layout placed `Bool(bool)`'s payload at byte 1. `rustc` then copied bytes 1 through 7 of a `VmValue` with two overlapping stores. Reading the copied value stalled store-to-load forwarding and added 13 ns per callback. A 4-byte tag aligns every payload to byte 4 or byte 8.
- **Pass `&Arc<Program>` and `&Arc<Vec<NativeEntry>>` to `Vm::interp`.** The scheduler clones both handles once per batch of turns. `CallContext` clones them on its first callback. Passing `&Program` and `&[NativeEntry]` instead spilled an extra register in the dispatch loop and slowed `struct method dispatch 2M` by 22%.
- **Catch panics once per fiber turn.** Wrapping `run_fiber_turn` instead of `run_slice` removes `catch_unwind` from individual synchronous callbacks.
- **Cache the callback stack base on `CallContext`.** The caller's frame stays fixed during a synchronous native call. `CallContext` computes `ProgramCode` and the callback frame base on its first callback and reuses them.
- **Box `Capability` inside `FiberStop::Prompt`.** Boxing `Capability` shrinks `FiberStop` from 104 bytes to 40 bytes.

Running the callee in the interpreter accounts for about 16 ns, matching a direct Sand call. Callback setup and slice bookkeeping account for the remaining 13 ns, with every helper under 2 ns.

`begin_callback` keeps the `CALLBACK_PROTO` trampoline instead of pushing closure frames directly. Skipping the trampoline's `CallR` saved 3 ns per synchronous callback and 6 ns per async callback in prototype benchmarks. Direct frame setup duplicated the interpreter's call-entry logic, so we dropped it.

`native method calls 10M` in `benches/vm.rs` regressed by about 1.5%. Under the new `NativeFn` signature, each wrapper loads the stack pointer and length from `Fiber` instead of receiving `&[VmValue]` in registers. Avoiding those loads would require lending the fiber separately, splitting `HostFn` by call kind, or using `unsafe` code.
