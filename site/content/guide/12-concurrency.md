+++
title = "Concurrency"
description = "Run concurrent tasks, coordinate work, and pass messages."
+++

A Sand engine multiplexes concurrent tasks on a single OS thread. If a task blocks on I/O, a timer, or another task, the scheduler switches to another ready task. Functions do not use `async` or `await` annotations; any function can suspend and resume transparently.

## Suspending functions

I/O functions suspend only the calling task while the operation completes. Other ready tasks will run while the suspended task waits:

```sand
import fs

fn save(name: str, text: str) -> Result<int> {
    fs.write_text(name, text)?
    Ok(text.len())
}

print(save("notes.txt", "buy sand")?)
```

## Tasks

`spawn expr` creates a concurrent task that evaluates `expr` and returns a `Task<T>` handle. Call `wait()` on the handle to wait for the task result:

```sand
fn slow_square(n: int) -> int {
    task.sleep(10ms)
    n * n
}

let a = spawn slow_square(3)
let b = spawn slow_square(4)
print(a.wait() + b.wait())
```

Both tasks sleep concurrently, so the program finishes in roughly 10 ms. A newly spawned task begins execution at the next scheduling point. A `spawn` block can contain multiple statements and can read and mutate variables in the enclosing scope:

```sand
var count = 0
let t = spawn {
    count += 5
    count * 2
}
print(t.wait())
print(count)
```

`Task<T>` provides three methods:

- `wait()` suspends the calling task until the target task completes and returns its result. Subsequent calls to `wait()` return the same result.
- `is_done()` returns `true` if the task has completed or been cancelled.
- `cancel()` cancels the task if it is still running.

Cancelling a task marks it done immediately and schedules its pending `defer` blocks (see [Resources](/guide/resources/)):

```sand
let t = spawn {
    defer print("cleanup")
    task.sleep(1s)
    print("not printed")
}
task.yield_now()
t.cancel()
print(t.is_done())
task.sleep(1ms)
```

Calling `wait()` on a cancelled task aborts the run with `task cancelled`. An unhandled failure in any task aborts the entire program; return a `Result` from a spawned task to propagate recoverable errors to the caller of `wait()`.

When the entry module finishes top-level execution, Sand cancels any remaining tasks.

## Coordinating multiple tasks

The prelude `task` module provides combinators for waiting on multiple tasks:

- `task.all(tasks)` waits for every task in `tasks` and returns their results in order.
- `task.race(tasks)` returns the result of the first task to complete and cancels the remaining tasks.
- `task.timeout(d, f)` runs `f()` in a task and returns `Some(result)` if it completes within duration `d`, or cancels the task and returns `None`.
- `task.sleep(d)` suspends the calling task for duration `d`.

```sand
fn after(d: duration, value: str) -> str {
    task.sleep(d)
    value
}

let tasks = [30ms, 10ms, 20ms].map(d => spawn after(d, "\(d)"))
print(task.all(tasks))

print(task.race([spawn after(50ms, "slow"), spawn after(5ms, "fast")]))

print(task.timeout(100ms, () => after(1ms, "in time")))
print(task.timeout(5ms, () => after(1s, "too late")))
```

## Channels

A `Channel<T>` is a FIFO message queue between tasks. Create a channel with `task.channel()`. `send(v)` enqueues a value, and `recv()` dequeues the oldest value, suspending while the channel is empty. Once the channel is closed and drained, `recv()` returns `None`.

Iterating over a channel with `for` receives values until the channel is closed and empty:

```sand
let ch = task.channel()
let producer = spawn {
    for word in ["red", "green", "blue"] {
        ch.send(word)
    }
    ch.close()
}
for word in ch {
    print(word)
}
producer.wait()
```

Close a channel after the last message is sent so that consumers waiting in `for` or `recv()` can finish.

By default, channels are unbounded. `task.channel(capacity: n)` creates a bounded channel whose `send` method suspends the sender while the buffer holds `n` unread messages. `send` returns `false` if the channel is closed. `try_recv()` polls the channel without suspending, and `len()` returns the number of queued messages. See the [`task` reference](/std/task/) for all channel methods.

## Deadlock detection

If all tasks are suspended and zero pending I/O operations or timers remain to wake them, Sand detects the deadlock and aborts the run:

```sand fails
let ch: Channel<int> = task.channel()
print(ch.recv())
```

## Preemption

The VM counts bytecode instructions executed by each task and yields to the scheduler after a fixed limit. This prevents a CPU-bound task from starving other ready tasks:

```sand
var stop = false
let busy = spawn {
    var spins = 0
    while not stop {
        spins += 1
    }
    spins > 0
}
task.sleep(5ms)
stop = true
print("busy task stopped: \(busy.wait())")
```

For the complete concurrency specification, see [§11 of the specification](/spec/#11-concurrency-and-asynchronous-io).
