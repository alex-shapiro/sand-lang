# Async native functions that call Sand

We propose letting async native Rust functions call Sand functions. Each async native function returns a Rust future. The VM stores that future on the calling task and polls it.

**Status: unimplemented proposal.** Every cost figure in this document is an estimate. This design builds on the async-adapter outline from the callback study on `origin/claude/sandlang-native-callbacks-research-i6bf14`. That study proposed lending VM access to an `async` block only between `await` points. It also proposed storing heap values in a rooted container.

## Precedent: piccolo async sequences

[piccolo](https://github.com/kyren/piccolo) is a stackless Lua VM in Rust that uses the core of this design. Its async sequences let an `async` Rust block call Lua functions and await their results. The piccolo README describes async sequences as critical for complex callbacks.

Piccolo confirms two parts of this design:

- **The future requests VM actions.** At an `await` point, the future stores a Lua call request and returns `Pending`. The executor runs the Lua function and polls the future again. Part 1 uses the same mechanism.
- **Each call owns its rooted values.** A sequence owns a root set for values kept across `await` points. The future stores only handles into that root set. Part 3 uses the same ownership.

This design differs from piccolo in three ways:

- **Part 2 lends VM state in safe Rust.** Piccolo lends borrowed VM references to the future through `unsafe` code. Sand requires 100% safe Rust, so Part 2 lends owned VM state instead.
- **Part 1 uses a single future type for host I/O and Sand calls.** Piccolo restricts sequence futures to Lua requests. Part 1 handles both host I/O and Sand calls in the same future type.
- **Part 4 enforces hard memory limits inside native calls.** Piccolo checks memory only between execution steps. Part 4 awaits memory reservations so the VM keeps native calls within Sand's hard limit.

## Why native functions cannot call Sand on `main`

Three VM rules prevent a native Rust function from calling a Sand function on `main`:

- **The VM can suspend any Sand function.** Under SPEC §11, every Sand function uses synchronous call syntax and may wait on I/O. To suspend a task, the interpreter returns to the scheduler. Active Rust frames on the call stack block that return.
- **The garbage collector ignores Rust stack handles.** The collector runs only at safe points in the interpreter loop. It traces only VM registers and VM structures, leaving `VmValue` variables on the Rust stack untraced.
- **Memory-limit retries restart native functions.** Under SPEC §16.3, if a native function reaches `max_memory`, the VM collects garbage. It then runs the native function again from the start.

As `experiments/encode-decode-design-causes.md` explains, these three rules prevent a single fast codec. A native codec walker cannot call a custom Sand `encode` or `decode` method.

## Why the two tested callback designs fail for codecs

The callback study tested two designs for calling Sand from Rust:

- **Nested stack calls.** The native function calls Sand directly on the Rust stack, as in Lua and CPython. If the Sand callee waits on I/O, the active Rust frame blocks the interpreter from returning to the scheduler. The VM must then fail the call, so the callback study rejected this design.
- **Continuation calls.** Instead of calling Sand directly, the native function returns a call request and a continuation closure to the VM. The VM runs the Sand function on the current task and passes its result to the continuation closure. The callback study recommended this design because it preserves task suspension, garbage collection, and safe Rust.

Authors on `origin/encode-decode-traits` built and measured a continuation-based codec walker in `experiments/codec-traits/rejected-designs.md`. That walker paid three costs:

- **Callbacks drop the Rust stack frame.** To run a Sand method, the walker must return from Rust to the VM. Before returning, the walker copies its local state to the heap. After the Sand method finishes, the continuation rebuilds the walk from that heap state. Handling a custom field added about 270 ns to stop the walk, call Sand, and resume. The VM call itself took about 50 ns of that 270 ns.
- **Heap state slows down derived types.** A recursive Rust walker stores its position across nested Rust stack frames. Because a continuation call unwinds those frames, the continuation walker kept an explicit frame stack on the heap. That heap stack made standard decoding 40% to 45% slower, even for derived types.
- **Retries require per-step rollback state.** After garbage collection, the VM repeats only the current continuation step. To return to the start of a step, the continuation walker copied its state and logged format changes at every step.

## Design goals

Authors write callback-capable native functions as `async` Rust functions. Inside the function, `context.call(sand_fn, args).await` runs `sand_fn` on the current task and returns its result. While `sand_fn` runs, the VM maintains four properties:

- The scheduler can suspend `sand_fn` on async I/O.
- The garbage collector traces every value rooted by the native function.
- The VM stores waiting native futures on the heap rather than on the Rust stack.
- `Engine` implements `Send` in 100% safe Rust.

## Why current async native functions cannot call Sand

SPEC §16.3 already defines async native functions for host I/O:

- An async native function returns a future that the calling Sand task awaits.
- The parameters, the future, and the future output must be owned `Send` types.
- VM handle types implement `!Send` so futures cannot store them across `await` points.
- After the future returns `Poll::Ready`, the scheduler passes `&mut Vm` to a finisher closure. That closure converts the output into a `VmValue`.

On `main`, an async native future waits only on host I/O. Only its finisher closure accesses the VM heap.

SPEC §16.3 marks VM handles `!Send` to prevent stale handles after garbage collection rather than thread races. Pinning a future to the VM thread would still leave three problems:

- **Garbage collection invalidates unrooted handles.** While a future awaits a Sand call, the Sand code can trigger garbage collection. An untraced handle in the future can then point to a freed or reused heap slot. In safe Rust, reading that stale handle returns an unrelated heap object. The callback study reproduced this bug.
- **The interpreter needs `&mut Vm` during a Sand call.** The interpreter takes `&mut Vm` to run `sand_fn`. A waiting future therefore cannot store `&mut Vm` across an `await` point.
- **`Engine` requires `Send`.** Under SPEC §16, `Engine` implements `Send`. Storing a `!Send` future inside the VM would make both `Vm` and `Engine` `!Send`. Restoring `Send` would require `unsafe impl Send`, which Sand's safe-Rust rule forbids.

## Part 1: Unify async native functions under a single future type

A new future type replaces `Suspension`, the type that async native functions return on `main`. Every async native function then uses the same scheduler path.

### Current scheduler path on `main`

On `main`, async futures wait on external events and access the VM only after returning `Poll::Ready`:

1. An async native function returns a `Suspension` that wraps a boxed future.
2. The VM stores the `Suspension` in its `IoTable` of pending operations.
3. Each `IoTable` slot stores a waker. After the host finishes the I/O operation, the waker marks that slot ready.
4. Between slices of Sand bytecode, the scheduler polls ready futures in `IoTable::poll_ready` inside `crates/sandlang/src/vm/sched.rs`.
5. On readiness, the future returns a finisher closure. The scheduler runs that closure with `&mut Vm` and resumes the task with the resulting value.

### Unified future type

Under the new design, a polled future returns `Pending` in two cases:

- **Waiting on an external event.** After the host finishes the operation, a waker marks the task ready, as on `main`.
- **Requesting a Sand call.** The VM runs the requested Sand function on the task and then polls the future again.

The scheduler checks whether the poll stored a VM request. If the poll stored a request, the VM runs that request. Otherwise, the task waits for a host waker. The VM treats a poll that both stores a request and waits on a host event as an error. Piccolo avoids this case by restricting sequence futures to Lua requests. Sand allows both kinds of wait in the same future so the new type can replace `Suspension`.

Host I/O functions use the same future type. An I/O future awaits the host operation and then calls `context.with_vm` from Part 2 to build its return value. That final `context.with_vm` call replaces the finisher closure from `main`.

Each task stores its active native futures beside its Sand call frames rather than in `IoTable`. The task can then run Sand frames while a native future waits. The same future can later await host I/O. `IoTable` maps only host wakers to waiting tasks.

Synchronous native functions keep their direct call path. Most native functions are synchronous and take about 10 ns per call. Only functions that await host I/O or Sand calls allocate a boxed future.

### Steps of a Rust-to-Sand call

1. A Sand task calls an async native function.
2. The VM stores the returned future on the heap inside the task, beside the Sand call frames.
3. The scheduler polls the future.
4. At `context.call(sand_fn, args).await`, the poll returns `Pending` with a request to call `sand_fn`.
5. The VM pushes a bytecode frame for `sand_fn` onto the task and runs it. If `sand_fn` awaits host I/O, the scheduler suspends the whole task with the native future still stored on it.
6. After `sand_fn` returns, the VM stores the return value for the future and polls the future again.
7. After the future returns `Poll::Ready`, the VM drops the future and passes its output to the Sand caller.

This layout also supports nested callbacks. If a custom `encode` method calls the native encoder again, the VM pushes a second future onto the task above the `encode` frame. The Rust call stack stores only the single future that the scheduler is polling. `max_call_depth` bounds total Sand and native nesting depth.

## Part 2: Lend owned VM state to the future during a poll

`Future::poll` takes only `Pin<&mut Self>` and a waker `Context`. That fixed Rust signature prevents the scheduler from passing `&mut Vm` as a `poll` argument. The future also cannot store `&mut Vm` across `await` points because the interpreter needs `&mut Vm` to run Sand code.

### Design choice: lend owned state

This design moves VM state into a shared owner so safe Rust can lend it to `'static` futures. Refactoring `Vm` ownership is the largest structural cost of the proposal. On `main`, the interpreter borrows the entire `Vm` across each execution slice.

A `'static` future requires lifetime-free types, leaving two ways to lend VM access during a poll:

- **Erase borrow lifetimes with `unsafe` code.** Piccolo stores raw pointers to borrowed VM references during each poll. Sand's safe-Rust rule forbids `unsafe` lifetime erasure.
- **Share owned state.** A shared container owns the VM state that native functions access. The scheduler and the polled future acquire exclusive access in turn, which satisfies `'static` in safe Rust.

### Mechanism

An `Arc<Mutex<VmState>>` transfers VM access in safe Rust:

1. An `Arc<Mutex<VmState>>` owns the VM state. The scheduler locks the mutex while running Sand bytecode.
2. Before polling native futures, the scheduler releases the lock.
3. Inside a poll, `context.with_vm(|vm| ...)` locks the mutex, runs the closure, and drops the guard.
4. After polling finishes, the scheduler locks the mutex again.

The scheduler and the future run on the same thread, so `context.with_vm` acquires the lock immediately.

Rust's type system prevents keeping VM access across an `await` point. `MutexGuard` implements `!Send`, so keeping a guard across an `await` point makes the future `!Send`. Because the VM requires `Send` futures, the Rust compiler rejects any future that keeps a `MutexGuard` across a wait. SPEC §16.3 uses the same `!Send` check for VM handles today.

## Part 3: Root VM values across `await` points

Within a single `Future::poll` call, only Rust code runs, so raw VM handles point to live heap slots. Across an `await` point, Sand code can trigger garbage collection and invalidate unrooted handles.

Each running future therefore owns a rooted value table:

- `context.root(value)` inserts a `VmValue` into the future's table and returns a `RootedValue` index.
- The garbage collector traces every entry in active future tables.
- Across `await` points, the future stores only `RootedValue` indices. Each index wraps an integer and implements `Send`.
- The VM frees the future's table after the future returns `Poll::Ready` or if the VM cancels the task.

In commit `eeec0f3`, `origin/encode-decode-traits` added a similar traced value list for native objects.

### Design choice: each call owns its root table

The task object that owns the native future also owns the future's root table. The collector traces that table through the owning object rather than as a global root set:

- **The collector can free reference cycles.** The collector traces the table only while the owning object is reachable. If a rooted value points back to that owning object, the collector still reclaims both once the owning object is unreachable. Traced native objects on `origin/encode-decode-traits` follow the same rule.
- **The VM drops roots automatically with the call.** Dropping the finished or cancelled future frees its root table. Each call therefore releases all of its roots upon return.

In a global root table, each native function must remove its own entries explicitly. The piccolo docs warn that entries in its global registry can form uncollectable reference cycles.

## Part 4: Await memory reservations and host permissions

A Sand callback can perform side effects, so the VM cannot safely restart an async native function from the start. Async native functions therefore await memory reservations in place:

1. Before allocating heap memory, the future calls `context.reserve(byte_count).await`.
2. If the heap exceeds `max_memory`, the poll returns `Pending` with a garbage-collection request.
3. Because Part 3 roots every value stored across an `await` point, the VM collects garbage in place and keeps the waiting future.
4. The scheduler polls the future again. If collection freed enough memory, the reservation returns `Poll::Ready`; otherwise, the VM fails the task with a memory error.

Async native functions request host permissions in the same way through `context.require(capability).await`.

Collecting garbage in place eliminates the step snapshots and rollback logs from `origin/encode-decode-traits`. Synchronous native functions run only Rust code, so the VM still restarts them on memory limits.

### Design choice: preserve hard memory limits

Part 4 enforces Sand's hard per-engine memory limit inside async native calls. Piccolo instead checks memory only between execution steps. A single piccolo step can exceed the limit by a bounded amount, so piccolo runs each native call to completion. Soft limits would eliminate Part 4 at the cost of breaking Sand's hard memory guarantee.

### Design choice: charge future state against `max_memory`

The VM must charge the size of each native future against `max_memory`. A native future stores local variables and boxed recursive frames outside the VM heap. Deeper script inputs force a recursive walker to allocate more boxed futures. Under `docs/INTERNALS.md`, `max_memory` covers every allocation whose size depends on script input. Piccolo leaves Rust future state outside its memory accounting.

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

The walker keeps its position in local variables and recursive calls. The Rust compiler generates the state machine that saves and restores local variables across each `await` point.

## Estimated costs

Every cost below is an unmeasured estimate:

- **Heap allocation per recursive async call.** Rust requires `Box::pin` for recursive `async` calls. The type schema marks purely derived subtrees in advance, so the walker can skip `Box::pin` for them. A synchronous recursive helper can walk those subtrees using local stack variables.
- **Mutex operations for VM access.** Locking and unlocking an uncontended `Mutex<VmState>` still executes atomic instructions. A walker should batch multiple heap reads inside each `context.with_vm` closure.
- **Two mutex operations per poll batch.** The scheduler releases the mutex before polling ready futures and locks it again afterward. `IoTable::poll_ready` already polls ready futures in batches, so a single release covers the whole batch. Host I/O takes far longer than two atomic operations.

## Open questions

- **Preventing raw `VmValue` across `await` points.** `VmValue` implements `Send` on `main`. The Rust compiler therefore allows a future to keep an unrooted `VmValue` across an `await` point. Current async native functions have the same gap.
- **Scope of the scheduler refactor.** On `main`, the interpreter is a `&mut Vm` method that borrows the whole VM across an execution slice. To suspend a task, the interpreter returns `FiberStop::Async` from `crates/sandlang/src/vm/interp.rs`. Part 2 moves VM state into `Arc<Mutex<VmState>>` so the scheduler can release the lock before polling. We have not yet measured the size of that refactor.
- **Migrating existing async native functions.** Replacing `Suspension` removes `Later::new` and its finisher closure. Authors must update every async native function in the standard library and host modules to use the new future API.
- **Concurrent mutation during callbacks.** A custom Sand callback can mutate a collection while a native walker iterates over it. As in the callback study, each native caller must either fail on concurrent mutation like CPython or read from a snapshot.
- **Drop order on cancellation.** Cancelling a task or unwinding through `defer` blocks drops active frames. The VM must drop interleaved native futures and Sand frames in reverse call order.

## First prototype and benchmark

To test this design, build a minimal prototype of Parts 1 through 4. Include a single async native function that awaits a Sand call, and run two benchmarks:

- **Custom field overhead.** Time a single custom field inside a codec walk. Compare that time against the roughly 270 ns continuation hop on `origin/encode-decode-traits`.
- **Derived walk speed.** Time a walk over purely derived types. Compare that time against the recursive native decoder on `main`.
