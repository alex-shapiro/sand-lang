# Async native functions that call Sand

We propose letting async native Rust functions call Sand functions. Each async native function returns a Rust future. The VM stores that future on the calling task. The scheduler polls the future until it returns.

**Status: implemented on `async-native-sand-calls`.** See [Implementation](#implementation) for the final design refinements. See [Measurements](#measurements) for benchmark results. We based this design on the async-adapter outline from `origin/claude/sandlang-native-callbacks-research-i6bf14`. That study proposed lending VM access to an `async` block between `await` points. It also proposed keeping heap values in a rooted container.

## Precedent: piccolo async sequences

[piccolo](https://github.com/kyren/piccolo) is a stackless Lua VM in Rust. Its async sequences use the core of this design. An async sequence lets an `async` Rust block call Lua functions. The block awaits each result. The piccolo README describes async sequences as critical for complex callbacks.

Piccolo confirms two parts of this design:

- **The future requests VM actions.** At an `await` point, the future stores a Lua call request before returning `Pending`. The executor runs the Lua function. It then polls the future again. Part 1 uses the same mechanism.
- **Each call owns its rooted values.** A sequence owns a root set for values kept across `await` points. The future stores only handles into that root set. Part 3 uses the same ownership.

This design differs from piccolo in three ways:

- **Part 2 lends VM state in safe Rust.** Piccolo lends borrowed VM references to the future through `unsafe` code. Sand requires 100% safe Rust. Part 2 therefore lends owned VM state.
- **Part 1 uses a single future type for host I/O and Sand calls.** Piccolo restricts sequence futures to Lua requests. Part 1 handles both host I/O and Sand calls in the same future type.
- **Part 4 enforces hard memory limits inside native calls.** Piccolo leaves memory checks to the host between execution steps. In Part 4, native futures await memory reservations to stay within Sand's hard limit.

## Why `main` runs native functions as leaf calls

Three VM rules prevent a native Rust function from calling a Sand function on `main`:

- **The VM can suspend any Sand function.** Under SPEC §11, every Sand function uses synchronous call syntax. Any Sand function can wait on I/O. To suspend a task, the interpreter returns to the scheduler. Active Rust frames on the call stack block that return.
- **The garbage collector ignores Rust stack handles.** The collector runs only at safe points in the interpreter loop. The collector traces only VM registers and VM structures.
- **The VM restarts native functions on memory-limit retries.** Under SPEC §16.3, if a native function reaches `max_memory`, the VM collects garbage. It then runs the native function again from the start.

These three rules prevent a single fast codec. On `main`, a codec must either run in Sand or duplicate derived walks in Rust and Sand.

## Why the two tested callback designs fail for codecs

The callback study tested two designs for calling Sand from Rust:

- **Nested stack calls.** The native function calls Sand directly on the Rust stack, as in Lua and CPython. If the Sand callee waits on I/O, the active Rust frame blocks the interpreter from returning to the scheduler. The VM must then fail the call. The callback study therefore rejected this design.
- **Continuation calls.** The native function returns a call request and a continuation closure to the VM. The VM runs the Sand function on the current task. It then passes the result to the continuation closure. This design preserves task suspension, garbage collection, and safe Rust. The callback study therefore recommended it.

Authors on `origin/encode-decode-traits` built and measured a continuation-based codec walker in `experiments/codec-traits/rejected-designs.md`. That walker paid three costs:

- **Each callback returns to the VM.** To run a Sand method, the walker must return from Rust to the VM. Before returning, the walker copies its local state to the heap. After the VM finishes the Sand method, the continuation rebuilds the walk from that heap state. For each custom field, the walker spent about 270 ns to stop the walk, call Sand, and resume. The VM call itself took about 50 ns of that 270 ns.
- **Heap frame stacks slow derived walks.** A recursive Rust walker keeps its position in nested Rust stack frames. A return to the VM unwinds those frames. The continuation walker therefore kept an explicit frame stack on the heap. With walk frames on the heap, the walker decoded derived types 40% to 45% slower.
- **Retries require per-step rollback state.** After garbage collection, the VM repeats only the current continuation step. Before each step, the continuation walker copied its state. During the step, it logged format changes for rollback.

## Design goals

Authors write callback-capable native functions as `async` Rust functions. Inside the function, the author awaits `context.call(sand_fn, args)` to run `sand_fn` on the current task. That call returns `sand_fn`'s rooted result. While the VM runs `sand_fn`, it maintains four properties:

- The scheduler can suspend `sand_fn` on async I/O.
- The garbage collector traces every value rooted by the native function.
- The VM stores waiting native futures on the heap inside the task.
- `Engine` implements `Send` in 100% safe Rust.

## Limits of async native functions on `main`

SPEC §16.3 already defines async native functions for host I/O:

- An async native function returns a future that the calling Sand task awaits.
- The parameters, the future, and the future output must be owned `Send` types.
- VM handle types are `!Send`. The Rust compiler rejects any future that captures a VM handle across an `await` point.
- After the future returns `Poll::Ready`, the scheduler passes `&mut Vm` to a finisher closure. That closure converts the output into a `VmValue`.

On `main`, an async native future waits only on host I/O. Only its finisher closure accesses the VM heap.

Even on the VM thread, async futures must satisfy three constraints:

- **Garbage collection invalidates unrooted handles.** While a future awaits a Sand call, the Sand code can trigger garbage collection. An untraced handle in the future can then point to a freed or reused heap slot. In safe Rust, a read through that stale handle returns an unrelated heap object. The callback study reproduced this bug.
- **The interpreter needs `&mut Vm` during a Sand call.** The interpreter takes `&mut Vm` to run `sand_fn`. A future must release `&mut Vm` before each `await` point.
- **`Engine` requires `Send`.** Under SPEC §16, `Engine` implements `Send`. A `!Send` future inside `Vm` makes both `Vm` and `Engine` `!Send`. Only `unsafe impl Send` can restore `Send` on those types. Sand's 100% safe-Rust rule forbids `unsafe` code.

## Part 1: Unify async native functions under a single future type

A new future type replaces `Suspension`, the type that async native functions return on `main`. Every async native function then uses the same scheduler path.

### Current scheduler path on `main`

On `main`, async futures wait only on host I/O:

1. An async native function returns a `Suspension` that wraps a boxed future.
2. The VM stores the `Suspension` in its `IoTable` of pending operations.
3. Each `IoTable` slot stores a waker. After the host finishes the I/O operation, the waker marks that slot ready.
4. Between slices of Sand bytecode, the scheduler polls ready futures in `IoTable::poll_ready` inside `crates/sandlang/src/vm/sched.rs`.
5. On readiness, the future returns a finisher closure. The scheduler runs that closure with `&mut Vm`. It then resumes the task with the resulting value.

### Unified future type

Under the new design, a polled future returns `Pending` in two cases:

- **Waiting on an external event.** After the host finishes the operation, a waker marks the task ready, as on `main`.
- **Requesting a Sand call.** The VM runs the requested Sand function on the task. The scheduler then polls the future again.

The scheduler checks whether the poll stored a VM request. If the poll stored a request, the VM runs that request. Otherwise, the scheduler parks the task on its host waker. Each poll must either submit a VM request or register a host waker. Piccolo restricts sequence futures to Lua requests. Sand supports both host I/O and VM requests in the same future type. This single type replaces `Suspension`.

Host I/O functions use the same future type. An I/O future awaits the host operation. It then calls `context.with_vm` from Part 2 to build its return value. That final `context.with_vm` call replaces the finisher closure from `main`.

Each task stores its active native futures on its own fiber beside its Sand call frames. The task can then run Sand frames while a native future waits. The same future can later await host I/O. `IoTable` maps only host wakers to waiting tasks.

Synchronous native functions keep their direct call path. Each synchronous call takes about 10 ns. Only functions that await host I/O or Sand calls allocate a boxed future.

### Steps of a Rust-to-Sand call

1. A Sand task calls an async native function.
2. The VM stores the returned future on the heap inside the task, beside the Sand call frames.
3. The scheduler polls the future.
4. At `context.call(sand_fn, args).await`, the poll returns `Pending` with a request to call `sand_fn`.
5. The VM pushes a bytecode frame for `sand_fn` onto the task. The interpreter runs that frame. If `sand_fn` awaits host I/O, the scheduler suspends the task with the native future stored on it.
6. After `sand_fn` returns a value, the VM stores that result for the future. The scheduler then polls the future again.
7. After the future returns `Poll::Ready`, the VM drops the future. The VM then writes the output to the Sand caller.

This layout also supports nested callbacks. If a custom `encode` method calls the native encoder again, the VM pushes a second future onto the task above the `encode` frame. The Rust call stack stores only the single future that the scheduler is polling. `max_call_depth` bounds total Sand and native nesting depth.

## Part 2: Lend owned VM state to the future during a poll

`Future::poll` takes only `Pin<&mut Self>` and a waker `Context`. That fixed Rust signature prevents the scheduler from passing `&mut Vm` as a `poll` argument. The interpreter also needs exclusive access to `Vm` between polls.

### Design choice: lend owned state

This design wraps `Vm` in a shared owner. The scheduler lends that owner to `'static` futures in safe Rust. This `Vm` ownership change is the largest refactor in the proposal. On `main`, the interpreter borrows the entire `Vm` across each execution slice.

Every value captured by a `'static` future must satisfy `'static`. Two mechanisms can lend VM access during a poll:

- **Erase borrow lifetimes with `unsafe` code.** Piccolo stores raw pointers to borrowed VM references during each poll. Sand's safe-Rust rule forbids `unsafe` lifetime erasure.
- **Share owned state.** A shared container owns the VM state that native functions access. The scheduler and the polled future acquire the container in turn. Safe Rust accepts this ownership in a `'static` future.

### Mechanism

An `Arc<Mutex<Vm>>` transfers VM access in safe Rust:

1. An `Arc<Mutex<Vm>>` owns the VM state. The scheduler locks the mutex while running Sand bytecode.
2. Before polling native futures, the scheduler releases the lock.
3. Inside a poll, `context.with_vm(|vm| ...)` locks the mutex, runs the closure, and drops the guard.
4. After the poll returns, the scheduler locks the mutex again.

The engine polls the future on the scheduler thread. `context.with_vm` therefore acquires the uncontended lock immediately.

Rust's type system rejects any future that holds VM access across an `await` point. `MutexGuard` is `!Send`. A future that holds a guard across an `await` point is therefore `!Send`. The VM requires `Send` futures. The Rust compiler therefore rejects any future that keeps a `MutexGuard` across a wait. SPEC §16.3 uses the same `!Send` check for VM handles today.

## Part 3: Root VM values across `await` points

During a single `Future::poll` call, the engine runs only Rust code for that future. Across an `await` point, Sand code can trigger garbage collection. Collection invalidates unrooted handles.

Each running future therefore owns a rooted value table:

- `context.root(value)` appends `value` to the future's table. It returns a `RootedValue` index.
- The garbage collector traces every entry in active future tables.
- Across `await` points, the future stores only `RootedValue` indices. Each index is a `Send` integer handle.
- The VM frees the future's table after the future returns `Poll::Ready` or if the VM cancels the task.

In commit `eeec0f3`, `origin/encode-decode-traits` added a similar traced value list for native objects.

### Design choice: each call owns its root table

The object that owns the native future also owns the future's root table. The collector traces that table through the owning object:

- **The collector can free reference cycles.** The collector traces the table only while the owning object is reachable. If a rooted value points back to that owning object, the collector still reclaims both once the owning object is unreachable. Traced native objects on `origin/encode-decode-traits` follow the same rule. On `main`, the collector treats every live task as a root. Tasks gain cycle collection only if Sand later collects unreachable tasks.
- **The VM frees roots automatically at the end of the call.** After the future returns or the VM cancels the task, the VM drops the future and its root table.

In a global root table, each native function must remove its own entries explicitly. The piccolo docs warn that entries in its global registry can form uncollectable reference cycles.

## Part 4: Await memory reservations and host permissions

A Sand callback can perform side effects. A full restart of an async native function would repeat those side effects. Async native functions therefore await memory reservations in place:

1. Before allocating heap memory, the future calls `context.check_memory(byte_count).await`.
2. If the heap exceeds `max_memory`, the poll returns `Pending` with a garbage-collection request.
3. Part 3 roots every value stored across an `await` point. The VM therefore collects garbage in place while keeping the waiting future.
4. The scheduler polls the future again. If collection freed enough memory, `check_memory` returns `Poll::Ready`. Otherwise, the VM fails the task with a memory error.

Async native functions request host permissions in the same way through `context.require(capability).await`.

In-place garbage collection eliminates the step snapshots and rollback logs from `origin/encode-decode-traits`. Synchronous native functions execute in a single Rust call before returning to the VM. The VM still restarts them from the start on memory limits.

### Design choice: preserve hard memory limits

Part 4 enforces Sand's hard per-engine memory limit inside async native calls. Piccolo reports total memory for the host to check between execution steps. A single piccolo step can allocate past the host's target limit before returning control to the host. Soft limits would eliminate Part 4 at the cost of breaking Sand's hard memory guarantee.

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

The implementation follows Parts 1 to 4 with seven refinements:

- **`Later::new` builds the future directly.** `Later::new(wait_kind, |mut context| async move { ... })` passes a `NativeContext` to the builder closure. The engine converts the future's output directly into the Sand return value.
- **The scheduler shares `Vm` through `Arc<Mutex<Vm>>`.** The scheduler unlocks the VM only while polling a native future or awaiting a host operation. `NativeContext` panics if code calls `with_vm` or polls a VM request outside a poll of that call.
- **A trampoline prototype runs each callback.** In prototype 0 of every program, the interpreter executes `CallR` followed by `ResumeNative`. Regular Sand functions still return with `Ret`, `RetVariant`, and `RetUnit`.
- **`NativeContext::step` replaces `check_memory` and `require`.** `context.step(f)` runs `f` under the synchronous `&mut Vm` restart rule. After a memory or resource limit error, the engine collects garbage before running `f` again. After a permission request, the engine prompts the user before running `f` again from the start.
- **Each context allows a single active request.** `call` and `step` borrow `&mut NativeContext`. If the caller drops a request future before the scheduler takes it, `Drop` clears the pending VM request.
- **Each fiber reuses a single host-event waker.** A drop guard on each waiting call maintains per-kind host-wait counters for the scheduler's idle check.
- **The scheduler yields after exhausting the Tokio coop budget.** On `main`, 20,000 concurrent sleeps exhausted Tokio's coop budget. Exhausted coop budgets caused about 9.2 million polls. Explicit yields to Tokio cut that count to 40,000 polls.

## Measurements

We ran release benchmarks on an Apple M-series machine on 2026-11-15. Each number below is the fastest of several runs. We rebuilt the prototype walker from commit `1f7bb03` against the final API to encode 5,000 rows, each with a `Point` field:

| Walker | Time per row, ns |
| --- | ---: |
| Synchronous walker, as on `main` | 98 to 142 |
| Async walker, derived subtrees in a single `with_vm` call | 98 to 119 |
| Async walker, `with_vm` and a boxed future per node | 470 |
| Async walker, custom `Point` codec in Sand | 457 to 460 |

- **Derived walk speed.** On derived types, the async walker matches the speed of the synchronous walker.
- **Custom field overhead.** A Rust-to-Sand-to-Rust callback hop takes about 90 ns, down from about 270 ns for the continuation walker on `origin/encode-decode-traits`. Encoding a custom field with its Sand hook adds about 340 to 360 ns per row, down from about 435 ns.

Compared with `main`:

- The engine matches `main` speed on the `vm` benchmark, the `codec` benchmark, and the `bench/run.py` programs.
- The engine completes an immediately ready async native call in 112 ns, compared with 114 ns on `main`.
- Across 20,000 concurrent `task.sleep(1ms)` tasks, the engine spends about 1,000 ns per task, down from 2,880 ns on `main`.
- With 50,000 sleeping tasks, the engine completes 20,000 yielding async calls in 0.36 s, down from 0.77 s on `main`.

## Remaining problems

We recorded four follow-up items for async native walkers in `TODO.md`:

- Reject raw `VmValue` handles kept across waits.
- Let an async native function release roots during a call.
- Charge boxed recursive futures against `max_memory`.
- Detect collection changes made by a callback during iteration, or iterate a snapshot.
