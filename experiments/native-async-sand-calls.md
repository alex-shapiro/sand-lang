# Async native functions that call Sand

We propose letting async native Rust functions call Sand functions. Each async native function returns a Rust future. The VM stores that future on the calling task and polls it.

**Status: implemented on `async-native-sand-calls`.** [Implementation](#implementation) lists where the code differs from this proposal, and [Measurements](#measurements) replaces the cost estimates. This design builds on the async-adapter outline from the callback study on `origin/claude/sandlang-native-callbacks-research-i6bf14`. That study proposed lending VM access to an `async` block only between `await` points. It also proposed storing heap values in a rooted container.

## Precedent: piccolo async sequences

[piccolo](https://github.com/kyren/piccolo) is a stackless Lua VM in Rust that uses the core of this design. Its async sequences let an `async` Rust block call Lua functions and await their results. The piccolo README describes async sequences as critical for complex callbacks.

Piccolo confirms two parts of this design:

- **The future requests VM actions.** At an `await` point, the future stores a Lua call request and returns `Pending`. The executor runs the Lua function and polls the future again. Part 1 uses the same mechanism.
- **Each call owns its rooted values.** A sequence owns a root set for values kept across `await` points. The future stores only handles into that root set. Part 3 uses the same ownership.

This design differs from piccolo in three ways:

- **Part 2 lends VM state in safe Rust.** Piccolo lends borrowed VM references to the future through `unsafe` code. Sand requires 100% safe Rust, so Part 2 lends owned VM state instead.
- **Part 1 uses a single future type for host I/O and Sand calls.** Piccolo restricts sequence futures to Lua requests. Part 1 handles both host I/O and Sand calls in the same future type.
- **Part 4 enforces hard memory limits inside native calls.** Piccolo leaves memory checks to the host between execution steps. Part 4 awaits memory reservations so the VM keeps native calls within Sand's hard limit.

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

An `Arc<Mutex<Vm>>` transfers VM access in safe Rust:

1. An `Arc<Mutex<Vm>>` owns the VM state. The scheduler locks the mutex while running Sand bytecode.
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

The object that owns the native future also owns the future's root table. The collector traces that table through the owning object rather than as a global root set:

- **The collector can free reference cycles.** The collector traces the table only while the owning object is reachable. If a rooted value points back to that owning object, the collector still reclaims both once the owning object is unreachable. Traced native objects on `origin/encode-decode-traits` follow the same rule. On `main`, the collector treats every live task as a root, so this benefit applies only if Sand later collects unreachable tasks.
- **The VM drops roots automatically with the call.** Dropping the finished or cancelled future frees its root table. Each call therefore releases all of its roots upon return.

In a global root table, each native function must remove its own entries explicitly. The piccolo docs warn that entries in its global registry can form uncollectable reference cycles.

## Part 4: Await memory reservations and host permissions

A Sand callback can perform side effects, so the VM cannot safely restart an async native function from the start. Async native functions therefore await memory reservations in place:

1. Before allocating heap memory, the future calls `context.check_memory(byte_count).await`.
2. If the heap exceeds `max_memory`, the poll returns `Pending` with a garbage-collection request.
3. Because Part 3 roots every value stored across an `await` point, the VM collects garbage in place and keeps the waiting future.
4. The scheduler polls the future again. If collection freed enough memory, the reservation returns `Poll::Ready`; otherwise, the VM fails the task with a memory error.

Async native functions request host permissions in the same way through `context.require(capability).await`.

Collecting garbage in place eliminates the step snapshots and rollback logs from `origin/encode-decode-traits`. Synchronous native functions run only Rust code, so the VM still restarts them on memory limits.

### Design choice: preserve hard memory limits

Part 4 enforces Sand's hard per-engine memory limit inside async native calls. Piccolo instead leaves memory checks to the host between execution steps. A single piccolo step can exceed the limit by a bounded amount, so piccolo never stops a native call to enforce the limit. Soft limits would eliminate Part 4 at the cost of breaking Sand's hard memory guarantee.

### Design choice: charge future state against `max_memory`

The VM must charge the size of each native future against `max_memory`. A native future stores local variables and boxed recursive frames outside the VM heap. Deeper script inputs force a recursive walker to allocate more boxed futures. Under `docs/INTERNALS.md`, `max_memory` covers every allocation whose size depends on script input. Piccolo leaves Rust future state outside its memory accounting.

## Part 5: Sketch of an async codec walker

The sketch below illustrates a recursive async encoder. Commit `1f7bb03` benchmarked a complete version:

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

## Implementation

The engine implements Parts 1 to 4 with these differences from the proposal:

- **`Later` keeps its name.** `Later::new(wait_kind, |mut cx| async move { ... })` replaces the finisher closure.
- **The engine owns `Arc<Mutex<Vm>>`.** The drive releases the lock only to poll a native future or to await host events. `with_vm` and every request work only while the engine polls the call.
- **A trampoline frame runs each callback.** Proto 0 of every program calls the callee with `CallR` and then runs the new `ResumeNative` instruction, so ordinary returns do not change.
- **`step` replaces `reserve` and `require`.** `context.step(f)` runs `f` under the restart rule of a synchronous native function: after a memory or resource limit error or a prompt, the engine collects garbage or asks the user, and runs `f` again.
- **Requests are exclusive.** `call` and `step` take `&mut self`, so a future waits on at most one request. A request that a losing `select!` branch drops withdraws itself.
- **Each fiber has one waker.** A drop guard on the waiting call counts host waits, so the idle check reads two counters.
- **The engine yields when the host spends its tokio budget.** On `main`, 20,000 concurrent sleeps took about 9.2 million polls instead of 40,000.

## Measurements

Release builds on an Apple M-series machine, 2026-11-15, fastest of several runs. The prototype walker (commit `1f7bb03`, rebuilt against the final API) encodes 5,000 rows with one `Point` field each:

| Walker | ns per row |
| --- | --- |
| synchronous, as on `main` | 98 to 142 |
| async, derived subtrees inside one `with_vm` | 98 to 119 |
| async, one `with_vm` and one boxed future per node | 470 |
| async, custom `Point` codec in Sand | 457 to 460 |

- **Derived walk speed.** An async walk of derived types runs as fast as the synchronous walker.
- **Custom field overhead.** A Rust-to-Sand-to-Rust hop costs about 90 ns, compared with about 270 ns for the continuation walker on `origin/encode-decode-traits`. A custom field adds about 340 to 360 ns per row, including the Sand hook, compared with about 435 ns.

Against `main`:

- The `vm` and `codec` benches and the `bench/run.py` programs show no change.
- An async call that completes on its first poll takes 112 ns, compared with 114 ns.
- 20,000 concurrent `task.sleep(1ms)` calls take about 1,000 ns per task, compared with 2,880 ns.
- With 50,000 tasks waiting on long sleeps, 20,000 yielding async calls take 0.36 s, compared with 0.77 s.

## Remaining problems

`TODO.md` tracks raw `VmValue` across waits, root tables that only grow, and uncounted boxes of nested futures. A callback can also change a collection that a native walker iterates, so each walker must fail on mutation or read a snapshot.
