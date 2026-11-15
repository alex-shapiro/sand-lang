# Native functions that await Sand calls

This file proposes a way for a native Rust function to call a Sand function and
continue after the result arrives. The native is a normal `async` Rust function.
The VM owns its future and polls it.

**Status: proposal.** Nobody has built or measured this design. Each claim
about its cost is speculative. The research report on the branch
`origin/claude/sandlang-native-callbacks-research-i6bf14` proposes the outline:
an adapter in which "the block must get VM access only between awaits", and "a
typed, rooted container" for heap state. This file fills in that outline.

## The problem

A native function cannot call a Sand function on `main`. Three facts cause this:

- Any Sand function can suspend. SPEC §11 states that Sand does not
  have `async`/`await` coloring. To suspend a task, the interpreter returns to the
  scheduler. It cannot return past a native frame on the Rust stack.
- The garbage collector does not trace a VM handle that Rust code holds. It
  collects only at safe points in the interpreter loop.
- If a native reaches the memory limit, the VM collects garbage and runs the
  native again from the start (SPEC §16.3).

`experiments/encode-decode-design-causes.md` explains why this blocks a regular
`Encode` and `Decode`. A native codec walker cannot call a hand-written `encode`
or `decode`.

## Why the two tested designs do not fit a codec

The research branch tested two designs.

**Nested call.** The native calls the Sand function directly, and waits for
the result on the Rust stack. Lua and CPython work this way. A Sand function
that suspends inside a nested call fails, because a native frame is on the
Rust stack. The research branch rejected this design for that reason.

**Continuation call.** The native does not call the Sand function. It returns a
request to the VM. The VM runs the Sand function as a normal frame on the
current task. Then the VM calls a Rust closure that the native supplied, with
the result. That closure is the _continuation_. The research branch recommends
this design, because it keeps every hard constraint that the branch tested.

The codec branch `origin/encode-decode-traits` built a native walker with
continuation calls, and measured it
(`experiments/codec-traits/rejected-designs.md`). The design has two costs:

- **Each callback destroys the walker's Rust frame.** To run the Sand function,
  the walker must return from its Rust function to the VM. A Rust function that
  returns loses its local variables. So the walker must copy its state to the
  heap before the call, and a continuation must rebuild the walk after it. A
  hand-written field cost about 270 ns to stop, call Sand, and resume. The VM
  call itself was about 50 ns of that.
- **The walker must keep its state on the heap at all times.** A recursive
  walker keeps its state across many nested Rust frames. It cannot copy that
  state out in the middle of a walk. So the walker used an explicit stack of
  frames on the heap instead of recursion. That made ordinary decode 40 to 45
  percent slower, including values that never call Sand.

The restart rule adds a third cost. With continuation calls, a retry after a
collection repeats only the current step. The walker must therefore be able to
return to its state at the start of each step. The codec branch kept a copy of
its state and a log of changes for each step.

## The goal

A native can be written as an `async` Rust function. Inside it,
`cx.call(f, args).await` runs the Sand function `f` and returns its result.
While `f` runs, these conditions hold:

- `f` can suspend on I/O.
- The collector can run, and every value that the native keeps stays valid.
- The Rust stack does not grow.
- `Engine` stays `Send`, and the code stays safe Rust.

## Why the current async natives cannot do this

Sand already supports async native functions. SPEC §16.3 describes them:

- An async native returns a future, and only the calling Sand task waits for it.
- Its parameters, its future, and the future's output must be owned `Send` types.
- VM handle types are `!Send`, so the future cannot hold a handle to a VM value.
- Only the _finisher closure_ receives `&mut Vm`. The finisher runs after the
  future completes, and converts its output into a Sand value.

So an async native can wait for host I/O, but it cannot touch the VM heap while
it waits, and it cannot ask the VM to run Sand code.

The rule against handles in futures does not exist because of threads. SPEC
§16.3 states the reason: "Garbage collection during a wait can free the heap
object that a VM handle refers to. Each VM handle type is therefore `!Send`."
The `!Send` rule is a compile-time way to keep handles out of code that waits.

A future pinned to the VM thread would therefore not be safe to give handles.
Two problems would remain:

- **A handle can become stale.** During an `await`, Sand code runs and can
  trigger a collection. The collector does not see the future's handle. It can
  free the object and give its slot to a new object. Sand is safe Rust, so the
  conflict results in incorrect rather than undefined behavior: the handle
  reads a different object than what was expected. The research branch reproduced
  this case.
- **The future cannot keep `&mut Vm` across an `await`.** The VM needs
  `&mut Vm` to run the Sand function during that `await`.

A future pinned to one thread also conflicts with SPEC §16, which promises that
`Engine` implements `Send`. A future that holds `!Send` handles is `!Send`. If
the VM stores that future, the VM and the `Engine` become `!Send`. Only
`unsafe impl Send` could restore `Send`, and the safe-Rust rule forbids it.

## Part 1: Replace the future type for async natives

The new future type replaces `Suspension`, the type that async natives return
on `main`. Each async native then follows one path through the scheduler.

### Existing future type

A future on `main` can wait on external events but cannot access the VM:

1. An async native returns a `Suspension`, which holds a boxed future.
2. The VM puts the `Suspension` in its `IoTable`, a table of pending
   operations.
3. Each slot of the table has a waker. An I/O event wakes the slot.
4. The scheduler polls each woken future between slices of Sand execution
   (`IoTable::poll_ready`, `crates/sandlang/src/vm/sched.rs`).
5. A completed future gives a _finisher_ closure. The scheduler runs the
   finisher with `&mut Vm`, and the task continues with the result.

### The new future type

The new future returns `Pending` for one of two reasons:

- **It is waiting for an external event**, such as I/O. A waker reports the
  event, as on `main`.
- **It asks the VM to run a Sand function.** The VM runs the function, then
  polls the future again.

An I/O native is a special case. Its future awaits the I/O, and then uses the
VM once at the end, through `cx.with_vm` (Part 2). That last `cx.with_vm` does
the work of the finisher on `main`, so the finisher closure is removed.

The futures live with their tasks, not in the `IoTable`. A future that calls
Sand must stay with its task while the task runs Sand code. The same future can
then wait for I/O. The `IoTable` keeps only the map from each waker to its
task.

Synchronous natives do not change. A synchronous native call costs about
10 ns, and most natives are synchronous. A boxed future for each call would add
an allocation. Only natives that wait, for I/O or for Sand code, use the future
type.

### How a call into Sand works

1. A Sand task calls an async native function.
2. The VM stores the future in the task, next to the task's Sand frames.
   The future is not on the Rust stack.
3. The VM polls the future.
4. If the future awaits `cx.call(f, args)`, the poll returns `Pending`, with a
   request to call `f`.
5. The VM pushes a frame for `f` on the task, and runs `f` as normal Sand code.
   If `f` suspends on I/O, the whole task suspends, and the future waits in the
   task.
6. `f` returns. The VM puts the result where the future can read it, and polls
   the future again.
7. The future completes. The VM drops it, and gives its output to the Sand
   caller.

Nested calls stay safe. Suppose a hand-written `encode` calls the codec native
again. The VM stores a second future in the same task, above the Sand frame of
`encode`. The Rust stack holds only the future that the VM polls at that
moment. The VM's call depth limit, `max_call_depth`, counts the nesting.

## Part 2: How the future reaches the VM during a poll

`Future::poll` has a fixed signature. It receives only a `Context`, which holds
a waker. The VM therefore cannot pass `&mut Vm` through `poll`. The future also
cannot store `&mut Vm`, because the VM needs `&mut Vm` to run `f`.

A lock that changes owner at each poll solves this in safe Rust:

1. The VM state lives in an `Arc<Mutex<...>>`. The scheduler holds the lock
   while Sand code runs.
2. Before a poll, the scheduler releases the lock.
3. Inside the future, `cx.with_vm(|vm| ...)` takes the lock, runs the closure,
   and releases the lock.
4. After the poll, the scheduler takes the lock again.

All of this happens within a single thread, so the lock is never contended.

The compiler enforces the rule against VM access across an `await`. A
`MutexGuard` is `!Send`. If a future keeps a guard across an `await`, the
future is `!Send`. The VM requires the future to be `Send`, so the compiler
rejects that future. This is the same mechanism that SPEC §16.3 uses today for
async natives.

## Part 3: A rooted table for values that cross an `await`

During a poll, Sand code does not run, so the collector does not run. A raw
handle that the future uses within one poll is therefore safe. A handle that
the future keeps across an `await` is not safe, because Sand code runs during
the `await`.

The fix is a rooted table for each running future:

- `cx.root(value)` puts a value in the table, and returns a `Rooted` index.
- The collector traces every value in the table.
- The future keeps only `Rooted` indices across an `await`. An index is a plain
  integer, so it is `Send`.
- The VM frees the whole table if the future completes, or if the VM cancels
  the task.

The codec branch built most of this table. It gave native objects a list of VM
values that the collector traces (commit `eeec0f3`, "Let native objects keep VM
values that the collector traces").

## Part 4: The restart rule becomes a wait

A future that has called Sand code cannot restart from the start, because that
call was a side effect. Memory checks therefore become `await` points:

1. The future calls `cx.reserve(bytes).await` before it allocates.
2. If the reservation fails, the poll returns `Pending`, with a request to
   collect garbage.
3. At that point, every value that the future keeps is in its rooted table,
   because Part 3 requires it at each `await`. So the VM can collect garbage at
   once, without a restart.
4. The VM polls the future again. The reservation then succeeds, or the run
   ends with a memory error.

A permission prompt works in the same way, through
`cx.require(capability).await`.

This part removes the copies of state and the logs of changes that the codec
branch needed for each step. Those existed only because the VM could run a
step again.

Synchronous natives keep the restart rule. They do not call Sand code, so a
restart from the start stays correct for them.

## Part 5: The shape of a codec walker

This sketch shows the shape of the code. It is not a tested API.

```rust
async fn walk(cx: &NativeCx, value: Rooted, schema: &Schema, out: &mut String) -> Result<(), Ctl> {
    match schema {
        Schema::HandWritten(encode) => {
            cx.call(*encode, [value]).await?;
        }
        Schema::Struct(fields) => {
            for (i, field_schema) in fields.iter().enumerate() {
                let field = cx.with_vm(|vm| cx.root(vm.field(value, i)));
                Box::pin(walk(cx, field, field_schema, out)).await?;
            }
        }
        _ => {}
    }
    Ok(())
}
```

The walker keeps its state as normal Rust code, in local variables and
recursion. The compiler writes the code that saves and restores that state at
each `await`.

## Costs

Each cost in this section is an estimate. Nobody has measured it.

- **One heap allocation for each nesting level.** Rust requires `Box::pin` for
  each recursive `async` call. The walker can avoid this cost for subtrees
  without hand-written parts. The schema tells the walker where those subtrees
  are. For each such subtree, the walker can call a plain recursive function
  that has no `await`. That function keeps its state in local variables, so
  these subtrees would also avoid the slowdown of an explicit stack.
- **One lock operation for each VM access.** The lock is uncontended, but it
  is not free. A walker should do many reads inside one `with_vm`.
- **Two lock operations for each batch of polls.** The scheduler releases the
  lock before it polls, and takes it again after. `IoTable::poll_ready`
  already polls woken futures in a batch, so one release can serve many
  futures. An I/O wait costs far more than these lock operations.

## Open questions

- **Raw handles across an `await`.** A raw `VmValue` is `Send` today. A future
  could keep one across an `await`, and the compiler would not stop it. Async
  natives have the same gap today.
- **The scheduler change.** On `main`, the interpreter is a method of `Vm`,
  and it holds `&mut Vm` for a whole slice of execution. Part 2 requires the
  scheduler to release the VM before each poll. The interpreter already returns
  to the scheduler for async natives (`FiberStop::Async` in
  `crates/sandlang/src/vm/interp.rs`). The size of the change to move the VM
  state behind a lock is not known.
- **The host API.** `Later::new` and its finisher closure go away. Each
  async native in the standard library and in host modules must move to the
  new future type.
- **Changes to inputs during a callback.** A Sand callback can change a value
  that the native reads, such as a list. The research report lists two
  choices: fail with an error, as CPython does, or work on a snapshot. Each
  native that calls Sand must choose one.
- **Unwinding.** If the VM cancels a task, or `defer` code runs, the VM must
  drop the futures of that task in the correct order.

## First measurement

Build the smallest version: one native that awaits one Sand call, with Parts 1
to 4. Then measure two cases:

- the cost of one hand-written field in a codec walk. The baseline is the
  roughly 270 ns of the continuation walker on the codec branch;
- the cost of a walk without hand-written parts. The baseline is the recursive
  native decoder on `main`.
