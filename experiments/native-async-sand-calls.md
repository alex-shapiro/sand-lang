# Async native functions that call Sand

We propose letting async native Rust functions call Sand functions and await their results. Each async native function returns a Rust future that the VM stores on the calling task and polls.

**Status: unimplemented proposal.** Every cost figure in this document is an estimate. This design builds on the async-adapter outline from the callback study on `origin/claude/sandlang-native-callbacks-research-i6bf14`. That study proposed giving an async block VM access only between awaits and storing heap values in a rooted container.

## Why native functions cannot call Sand on `main`

Three VM rules prevent a native Rust function from calling a Sand function on `main`:

- **Any Sand function can suspend.** Under SPEC §11, Sand functions do not use `async` or `await` markers. To suspend a task, the interpreter returns to the scheduler. Active Rust frames on the call stack block that return.
- **The garbage collector ignores Rust stack handles.** The collector runs only at safe points in the interpreter loop and does not trace `VmValue` handles held in Rust variables.
- **Memory-limit retries restart native functions.** Under SPEC §16.3, if a native function reaches `max_memory`, the VM collects garbage and runs the function again from the start.

As `experiments/encode-decode-design-causes.md` explains, these three rules prevent a single fast codec. A native codec walker cannot call a custom Sand `encode` or `decode` method.

## Why the two tested callback designs fail for codecs

The callback study tested two designs for calling Sand from Rust:

- **Nested stack calls.** The native function calls the Sand function directly and waits on the Rust stack, as in Lua and CPython. If the Sand function suspends on I/O, the VM fails the call because an active Rust frame blocks the return to the scheduler. The callback study rejected nested stack calls for that reason.
- **Continuation calls.** Instead of calling Sand directly, the native function returns a call request and a continuation closure to the VM. The VM runs the Sand function on the current task and passes its result to the continuation closure. The callback study recommended this design because it preserves task suspension, garbage collection, and safe Rust.

Authors on `origin/encode-decode-traits` built and measured a continuation-based codec walker in `experiments/codec-traits/rejected-designs.md`. That walker paid three costs:

- **Callbacks drop the Rust stack frame.** To run a Sand method, the walker must return from Rust to the VM. Before returning, the walker copies its local state to the heap. After the Sand method finishes, the continuation rebuilds the walk from that heap state. Handling a custom field added about 270 ns to stop the walk, call Sand, and resume. The VM call itself took about 50 ns of that 270 ns.
- **Heap state slows down derived types.** A recursive Rust walker spreads its state across nested stack frames and cannot save that state mid-walk. The continuation walker therefore kept an explicit frame stack on the heap at all times. That heap stack made standard decoding 40% to 45% slower, even on inputs without custom types.
- **Retries require per-step rollback state.** After garbage collection, the VM repeats only the current continuation step. To return to the start of a step, the codec branch copied walker state and logged format changes at every step.

## Design goals

Authors write callback-capable native functions as `async` Rust functions. Inside the function, `context.call(sand_fn, args).await` runs `sand_fn` on the current task and returns its result. While `sand_fn` runs, four guarantees hold:

- `sand_fn` can suspend on async I/O.
- The garbage collector can run and traces every value that the native function roots.
- The VM stores suspended native futures on the heap rather than growing the Rust stack.
- `Engine` implements `Send` in 100% safe Rust.

## Why current async native functions cannot call Sand

SPEC §16.3 already defines async native functions for host I/O:

- An async native function returns a future that the calling Sand task awaits.
- The parameters, the future, and the future output must be owned `Send` types.
- VM handle types implement `!Send` so futures cannot hold them across waits.
- After the future completes, the scheduler passes `&mut Vm` to a finisher closure that converts the output into a `VmValue`.

An async native function on `main` can wait for host I/O, while only its finisher closure can access the VM heap.

SPEC §16.3 marks VM handles `!Send` to prevent stale handles after garbage collection rather than thread races. Pinning a future to the VM thread would still leave three problems:

- **Garbage collection invalidates unrooted handles.** While a future awaits a Sand call, the Sand code can trigger garbage collection. An untraced handle in the future can then point to a freed or reused heap slot. In safe Rust, reading that stale handle returns the wrong heap object instead of causing undefined behavior. The callback study reproduced this bug.
- **The VM needs exclusive access during awaits.** A future cannot hold `&mut Vm` across an `await` because the interpreter needs `&mut Vm` to run `sand_fn`.
- **`Engine` requires `Send`.** Under SPEC §16, `Engine` implements `Send`. Storing a `!Send` future inside the VM would make both `Vm` and `Engine` `!Send`. Restoring `Send` would require `unsafe impl Send`, which Sand's safe-Rust rule forbids.

## Part 1: Unify async native functions under a single future type

A new future type replaces `Suspension`, the type that async native functions return on `main`. Every async native function then uses the same scheduler path.

### Current scheduler path on `main`

On `main`, async futures wait on external events and access the VM only after completion:

1. An async native function returns a `Suspension` that wraps a boxed future.
2. The VM stores the `Suspension` in its `IoTable` of pending operations.
3. Each `IoTable` slot holds a waker. After I/O completes, the waker marks that slot ready.
4. Between slices of Sand bytecode, the scheduler polls ready futures in `IoTable::poll_ready` inside `crates/sandlang/src/vm/sched.rs`.
5. On completion, the future returns a finisher closure. The scheduler runs that closure with `&mut Vm` and resumes the task with the resulting value.

### Unified future type

Under the new design, a polled future returns `Pending` in two cases:

- **Waiting on an external event.** A host waker marks the task ready after I/O completes, as on `main`.
- **Requesting a Sand call.** The VM runs the requested Sand function on the task and then polls the future again.

Host I/O functions use the same future type. An I/O future awaits the host operation and then calls `context.with_vm` from Part 2 to build its return value. That final `context.with_vm` call replaces the finisher closure from `main`.

Each task stores its active native futures beside its Sand call frames rather than in `IoTable`. Keeping the future on the task lets the task run Sand frames while the future waits and lets the same future later wait on host I/O. `IoTable` maps only host wakers to waiting tasks.

Synchronous native functions keep their direct call path. Most native functions are synchronous and take about 10 ns per call. Only functions that await host I/O or Sand calls allocate a boxed future.

### Steps of a Rust-to-Sand call

1. A Sand task calls an async native function.
2. The VM stores the returned future on the heap inside the task, beside the Sand call frames.
3. The scheduler polls the future.
4. At `context.call(sand_fn, args).await`, the poll returns `Pending` with a request to call `sand_fn`.
5. The VM pushes a bytecode frame for `sand_fn` onto the task and runs `sand_fn`. If `sand_fn` suspends on I/O, the whole task suspends with the native future still stored on it.
6. After `sand_fn` returns, the VM stores the return value for the future and polls the future again.
7. After the future completes, the VM drops the future and passes its output to the Sand caller.

This layout also supports nested callbacks. If a custom `encode` method calls the native encoder again, the VM pushes a second future onto the task above the `encode` frame. The Rust call stack holds only the single future that the scheduler is polling. `max_call_depth` bounds total Sand and native nesting depth.

## Part 2: Share `&mut Vm` through a single-thread mutex

`Future::poll` accepts only `Pin<&mut Self>` and `&mut Context`, which provides a waker. Because of that fixed Rust signature, the scheduler cannot pass `&mut Vm` as a `poll` argument. The future also cannot store `&mut Vm` across awaits because the interpreter needs `&mut Vm` while Sand code runs.

An `Arc<Mutex<VmState>>` transfers VM access cleanly in safe Rust:

1. An `Arc<Mutex<VmState>>` holds the VM state. The scheduler holds the lock while running Sand bytecode.
2. Before polling native futures, the scheduler releases the lock.
3. Inside a poll, `context.with_vm(|vm| ...)` acquires the lock, runs the closure, and drops the guard.
4. After polling finishes, the scheduler acquires the lock again.

Because the scheduler and the future run on the same thread, acquiring this mutex never blocks.

Rust's type system prevents holding VM access across an `await`. `MutexGuard` implements `!Send`, so holding a guard across an `await` makes the future `!Send`. Because the VM requires `Send` futures, the Rust compiler rejects any future that keeps a `MutexGuard` across a wait. SPEC §16.3 uses the same `!Send` check for VM handles today.

## Part 3: Root VM values across awaits

Sand bytecode and garbage collection never run during a `Future::poll` call. Raw VM handles remain valid within a single poll. Across an `await`, Sand code can run and trigger garbage collection, so raw handles become unsafe to keep.

Each running future therefore owns a rooted value table:

- `context.root(value)` inserts a `VmValue` into the future's table and returns a `RootedValue` index.
- The garbage collector traces every entry in active future tables.
- Across awaits, the future stores only `RootedValue` indices. Each index wraps an integer and implements `Send`.
- The VM frees the future's table after the future completes or if the VM cancels the task.

In commit `eeec0f3`, the codec branch added a similar traced value list for native objects.

## Part 4: Await memory reservations and host permissions

After an async native function calls Sand code, the VM cannot safely restart that native function from the start because the Sand call may have caused side effects. Async native functions therefore await memory reservations instead of restarting:

1. Before allocating heap memory, the future calls `context.reserve(byte_count).await`.
2. If the heap exceeds `max_memory`, the poll returns `Pending` with a garbage-collection request.
3. Because Part 3 roots every value held across an `await`, the VM collects garbage in place without rewinding the native function.
4. The scheduler polls the future again. If collection freed enough space, the reservation succeeds; otherwise the VM fails the task with a memory error.

Async native functions request host permissions in the same way through `context.require(capability).await`.

Collecting garbage in place removes the step snapshots and rollback logs required on the codec branch. Synchronous native functions cannot call Sand code, so they keep the existing restart rule.

## Part 5: Sketch of an async codec walker

The untested sketch below illustrates a recursive async encoder:

```rust
async fn encode_value(
    context: &NativeContext,
    value: RootedValue,
    schema: &Schema,
    output: &mut String,
) -> Result<(), Ctl> {
    match schema {
        Schema::Custom(encode_fn) => {
            context.call(*encode_fn, [value]).await?;
        }
        Schema::Struct(fields) => {
            for (field_index, field_schema) in fields.iter().enumerate() {
                let field_value =
                    context.with_vm(|vm| context.root(vm.field(value, field_index)));
                Box::pin(encode_value(context, field_value, field_schema, output)).await?;
            }
        }
        _ => {}
    }
    Ok(())
}
```

The walker keeps its position in local variables and recursive calls. The Rust compiler generates the state machine that saves and restores local variables across each `await`.

## Estimated costs

Every cost below is an unmeasured estimate:

- **Heap allocation per recursive async call.** Rust requires `Box::pin` for recursive `async` calls. The walker can skip `Box::pin` on subtrees without custom codecs because the type schema marks those subtrees in advance. For purely derived subtrees, the walker can call a synchronous recursive function that keeps state in local variables instead of an explicit heap stack.
- **Mutex operations for VM access.** Even without contention, locking and unlocking `Mutex<VmState>` executes atomic instructions. A walker should batch multiple heap reads inside each `context.with_vm` closure.
- **Two mutex operations per poll batch.** The scheduler releases the mutex before polling ready futures and acquires it again afterward. `IoTable::poll_ready` already polls ready futures in batches, so a single release covers the whole batch. Host I/O latency dwarfs those two atomic operations.

## Open questions

- **Preventing raw `VmValue` across awaits.** `VmValue` implements `Send` on `main`, so the Rust compiler allows a future to keep an unrooted `VmValue` across an `await`. Current async native functions have the same gap.
- **Scope of the scheduler refactor.** On `main`, the interpreter is a `&mut Vm` method that borrows the whole VM across an execution slice. To suspend a task, the interpreter returns `FiberStop::Async` from `crates/sandlang/src/vm/interp.rs`. Moving VM state into `Arc<Mutex<VmState>>` so the scheduler can release it before polling requires an unmeasured refactor.
- **Migrating existing async native functions.** Replacing `Suspension` removes `Later::new` and its finisher closure. Every async native function in the standard library and host modules must migrate to the new future API.
- **Concurrent mutation during callbacks.** A custom Sand callback can mutate a collection while a native walker iterates over it. Following the callback study, each native caller must either fail on concurrent mutation like CPython or read from a snapshot.
- **Drop order on cancellation.** If the VM cancels a task or unwinds through `defer` blocks, it must drop interleaved native futures and Sand frames in reverse call order.

## First prototype and benchmark

To test this design, build a minimal prototype of Parts 1 through 4 with a single async native function that awaits a Sand call. Then measure two benchmarks:

- **Custom field overhead.** Time a single custom field inside a codec walk and compare it against the roughly 270 ns continuation hop on `origin/encode-decode-traits`.
- **Derived walk speed.** Time a walk over purely derived types and compare it against the recursive native decoder on `main`.
