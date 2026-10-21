+++
title = "Concurrency"
description = "Run work in tasks with spawn, wait for results, and pass messages between tasks with channels."
+++

A Sand engine runs one task at a time on one thread. When a task waits for I/O, a timer or another task, the engine runs a different task. You do not mark functions as `async` and you do not write `await`: any function can wait.

## Functions that wait

This function writes a file. The task waits while the engine writes the file, and other ready tasks run. The caller sees an ordinary function call.

```sand
import fs

fn save(name: str, text: str) -> Result<int> {
    fs.write_text(name, text)?
    Ok(text.len())
}

print(save("notes.txt", "buy sand")?)
```

Because no function is "async", you can call any function from any other function. To do two things at the same time, start a task.

## Tasks

`spawn e` starts a task that evaluates the expression `e`. It returns a `Task<T>`, where `T` is the type of `e`. Call `wait()` to get the result.

```sand
fn slow_square(n: int) -> int {
    task.sleep(10ms)
    n * n
}

let a = spawn slow_square(3)
let b = spawn slow_square(4)
print(a.wait() + b.wait())
```

Both tasks sleep at the same time, so the program takes about 10 ms, not 20 ms. A task starts at the next scheduling point, not at the `spawn`. A `spawn` block can contain statements, and it can read and assign the variables in scope:

```sand
var count = 0
let t = spawn {
    count += 5
    count * 2
}
print(t.wait())
print(count)
```

A `Task<T>` has three methods:

- `wait()` makes the current task wait until `t` is done, then returns its value. A second `wait()` returns the same value.
- `is_done()` tells whether `t` is done.
- `cancel()` stops `t` if it is still running.

A cancelled task is done at once. Then it runs its pending `defer` blocks (see [Resources](/guide/resources/)):

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

`task.yield_now()` lets the task start before the cancellation. If you call `wait()` on a cancelled task, the run fails with `task cancelled`.

An error in any task stops the whole run. Return a `Result` from a task to give an error to the task that waits.

When the top-level code of the main module finishes, Sand ends the run and cancels the tasks that are not done.

## Waiting for many tasks

The `task` module is always in scope. Its functions combine tasks:

- `task.all(tasks)` waits for all tasks and returns their results in the same order.
- `task.race(tasks)` returns the result of the first task to finish and cancels the others.
- `task.timeout(d, f)` runs the function `f` in a task. It returns `None` and cancels the task if `f` does not finish within `d`.
- `task.sleep(d)` makes the current task wait for the duration `d`.

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

A channel is a queue of messages between tasks. `task.channel()` creates one. `send(v)` adds a message and `recv()` removes the oldest one. `recv()` waits while the channel is empty, and returns `None` after the channel is closed and empty.

A `for` loop over a channel receives messages until the channel is closed and empty:

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

Always close a channel when no more messages will come. Otherwise the loop waits for more messages, and the run can end in a deadlock.

By default a channel has no size limit. `task.channel(capacity: n)` creates a bounded channel: `send` waits while it holds `n` messages. Use a bounded channel to stop a fast producer from getting too far ahead of a slow consumer.

`send` returns `false` if the channel is closed. `try_recv()` receives without waiting, and `len()` gives the number of queued messages. For all methods, see the [`task` reference](/std/task/).

## Deadlocks

If every task waits and no I/O or timer can wake one of them, no task can continue. Sand detects this and stops the run:

```sand fails
let ch: Channel<int> = task.channel()
print(ch.recv())
```

## Preemption

The engine counts the work that each task does. After a fixed amount of work, the engine lets another ready task run. Thus a busy loop in one task cannot block the other tasks:

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

The main task sleeps while `busy` runs its loop. When the sleep ends, the engine switches from `busy` to the main task, which sets `stop`.

[§11 of the specification](/spec/#11-concurrency-and-asynchronous-io) gives the full rules.
