# Task lifetime and cancellation

Run on 2026-11-09 against commit `73fdfb8`.

## Question

When should a Sand task stop running? We started from a working assumption: a task should stay alive only if it is the main task or another task is waiting for it. We also had two open decisions about `task.race` and `task.timeout`:

1. If the caller of a race or a timeout is cancelled, what happens to the tasks it started or is waiting for?
2. If another task cancels a task in a race while the race is waiting, what should the race do?

## Conclusions

The assumption does not describe any system that we tested. A task with no waiter and no kept handle finished in every system that allows such a task. Ownership, not waiting, decides whether a task stops.

Sand made these changes:

- `task.timeout` cancels its work if its caller is cancelled.
- `task.race` continues if a task is cancelled during the race and another task can still finish.

Sand kept these behaviors:

- A spawned task runs whether or not any task waits for it.
- A plain `task.race` leaves its tasks running if its caller is cancelled.

## Method

We ran the same scenarios in each system. In each scenario, worker tasks sleep and record whether they finished or were cancelled. A slow worker sleeps for 80 ms. The faster worker in a race sleeps for 10 to 50 ms. Most cancellations occur about 10 ms after the workers start. Each program waits long enough for the worker to finish.

| System | Version | Tasks tested |
| --- | --- | --- |
| Python asyncio | 3.15.2 | `create_task`, `wait`, `wait_for`, `TaskGroup` |
| Trio | 0.35.0 | nurseries, cancel scopes, `move_on_after` |
| Rust Tokio | 1.54.1 | `spawn`, `select!`, `JoinSet`, `timeout` |
| Swift | 6.4.2 | `Task`, `async let`, task groups |
| Kotlin coroutines | Kotlin 2.4.21, coroutines 1.11.0 | `GlobalScope`, child coroutines, `select`, `withTimeout` |
| Java | 25.0.5, preview enabled | threads, `StructuredTaskScope` |
| Go | 1.27.3 | goroutines, `context` |
| Node | 26.13.0 | promises, `AbortController` |

Each program ran 10 times per system, and each run produced the same output. Each Sand candidate ran each scenario 5 times, with the same result each time. We did not test Haskell or Erlang, because they were not installed.

## Results from other systems

"Finishes" means that the task ran to its end. "Cancelled" means that the task stopped early and ran its cleanup. "Killed" means that the process ended before the task finished.

### Nobody waits for the task and its handle is gone

The task finished in asyncio, Tokio, Swift, Kotlin global tasks, Java, Go, and Node. Trio has no such task, because the block that starts a task waits for that task to end.

In one additional asyncio test, the task waited on a future that only the task could reach. The garbage collector destroyed the task while it was still pending.

### The main task returns while another task is running

| Outcome | Systems |
| --- | --- |
| Cancelled | Sand, asyncio, Tokio (dropped at runtime shutdown) |
| Killed | Go, Swift, Kotlin global tasks, Java virtual threads |
| Main waits | Trio, Kotlin child tasks, Java platform threads, Node |

### The only waiter of a task is cancelled

| Outcome | Systems |
| --- | --- |
| Finishes | Sand, Tokio spawned tasks, Swift `Task`, Kotlin global tasks, Java, Go and Node unless cancellation is passed to the task |
| Cancelled | asyncio, Trio, Tokio plain futures, Swift `async let`, Kotlin child tasks |

If one of two waiters was cancelled, asyncio cancelled the task, and the other waiter received `CancelledError`. Swift and Kotlin kept the task running, and the other waiter received its result.

### The caller of a race is cancelled

| Outcome | Systems |
| --- | --- |
| Tasks finish | Sand, asyncio `wait`, Tokio `select!` on spawned tasks, Kotlin `select` on global tasks, Node `Promise.race` |
| Tasks cancelled | asyncio `TaskGroup`, Trio, Tokio `JoinSet`, Swift task groups, Kotlin child tasks, Java `StructuredTaskScope`, Go derived contexts |

### The caller of a timeout is cancelled

The work was cancelled in asyncio, Trio, Tokio, Kotlin, and Go. Before this change, Sand was the only tested system in which the work finished. We did not test this case in Swift, Java, or Node.

### A task in a race is cancelled during the race

| Outcome | Systems |
| --- | --- |
| The other task wins | Trio (a race built on a nursery), Java (the task failed instead of being cancelled), Node `Promise.any` |
| The race ends with an error that the program can handle | Tokio, Kotlin, Node `Promise.race` |
| The race returns the cancelled task | asyncio `wait` |
| The whole program ends and the script cannot prevent it | Sand before this change |

We did not test this case in Swift or Go.

## Sand measurements

### How scripts use tasks

We ran 50 Sand scripts that spawn tasks, from the language specs, the examples, and the guide. They started 1,068 tasks. Of these scripts, 34 had at least one task that never had a waiter while it was running. This count includes tasks whose results a script read after they finished. One test starts 1,000 of the tasks. The specs emphasize cancellation, so these numbers are only a rough guide.

`examples/chat_server.sand` starts a task for each client and never waits for it. The rule in our assumption would stop these tasks.

The 34 recorded model transcripts in the `ash` project contained no model-written use of `spawn`. They give no evidence about how models use tasks.

### Candidates

Speed is the change in retired instructions for 200,000 spawn and wait pairs, against the unchanged engine. Cancel time is the same measure for cancelling 9,000 sleeping tasks. "Failing tests" counts failures out of 1,135 tests. Differences of about 0.5% or less are within run-to-run variation: the cancel baseline moved by about 1.4% between sessions.

| Candidate | Effect | Failing tests | Speed | Cancel time |
| --- | --- | --- | --- | --- |
| `task.timeout` cancels its work | The work stops with the caller | 0 | +0.04% | +0.45% |
| A race cancels its tasks with its caller | Another waiter on one of those tasks ends the program | 8 | -0.01% | +0.13% |
| Cancelling a task cancels the tasks it started | Fixes timeouts, but not plain races | 0 | +1.04% | 7.6 times as long |
| Cancel a task when its only waiter is cancelled | A later waiter finds the task cancelled and the program ends | 9 | +0.02% | 7.0 times as long |
| A race ignores a task cancelled during the race | The other task wins | 0 | +0.03% | -0.20% |
| A race fails if any of its tasks is cancelled | One cancelled task ends the program | 0 | +0.01% | -0.26% |

The 8 failures of the race candidate and 8 of the 9 failures of the sole-waiter candidate are two spec cases that assert the earlier behavior, repeated across test modes. The ninth failure is `cancelled_waits_do_not_accumulate`.

The slow cancel times come from prototypes that scan every task. A better data structure might remove this cost. We did not build one.

We did not prototype a rule that stops tasks when garbage collection finds their handles unreachable. The asyncio result above, and the requirement that replay depends only on the program, are the reasons to avoid that rule.

### Final implementation

The race prototype scanned the race list on each cancellation, which could allow unmetered quadratic work. The final implementation counts the tasks that can still finish the race, so each cancellation costs constant time. Against the unchanged engine, retired instructions changed by -0.01% for a channel ping-pong, +0.01% for spawn and wait pairs, and +0.21% for cancelling 9,000 tasks. A repeat measurement gave +0.01%, 0.00%, and -0.57%, so these differences are within run-to-run variation. All 1,135 tests passed.

## Open follow-up

Owner-scoped tasks match the original assumption most closely: a block owns the tasks it starts, cannot exit until they end, and cancels them if it is cancelled. Trio, Swift task groups, Kotlin scopes, and Java's `StructuredTaskScope` all use this model. We did not build it for Sand.
