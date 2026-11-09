# Task lifetime and cancellation

Run on 2026-11-09 against commit `73fdfb8`.

## Question

When should a Sand task stop running? We started from the assumption that a task should run only if it is the main task or another task waits for it. We also had two open decisions about `task.race` and `task.timeout`:

1. If the caller of a race or a timeout is cancelled, what happens to the tasks it started or waits for?
2. If another task cancels a task in a race while the race waits, what should the race do?

## Conclusions

Ownership, not waiting, decides whether a task stops. In every system that allows a task without a waiter or handle, that task runs to completion.

Sand made two changes:

- `task.timeout` cancels its work if its caller is cancelled.
- `task.race` continues if a task is cancelled during the race and another task can still finish.

Sand kept two behaviors:

- A spawned task runs even if no task waits for it.
- `task.race` leaves its tasks running if its caller is cancelled.

## Method

We ran the same scenarios across eight concurrency runtimes:

- Python 3.15.2 (`asyncio`)
- Trio 0.35.0
- Rust Tokio 1.54.1
- Swift 6.4.2
- Kotlin 2.4.21 (`kotlinx.coroutines` 1.11.0)
- Java 25.0.5 (with preview features)
- Go 1.27.3
- Node 26.13.0

In each scenario, worker tasks sleep and record whether they finish or are cancelled. A slow worker sleeps for 80 ms. The faster worker in a race sleeps for 10 to 50 ms. Most cancellations occur about 10 ms after the workers start. Each program waits long enough for the worker to finish.

Each program ran 10 times per system, and every run produced the same output. Each Sand candidate ran each scenario 5 times with the same result.

## Results from other systems

In the results below:

- "Finishes" means the task ran to completion.
- "Cancelled" means the task stopped early and ran its cleanup.
- "Killed" means the process exited before the task finished.

### Unowned tasks without waiters

The task finished in every system that allows an unowned task. In Trio, the block that starts a task always waits for that task to end.

In one extra asyncio test, a task waited on a future that only the task could reach. The garbage collector destroyed the pending task.

### The main task returns while another task is running

- **Cancelled**: Sand, asyncio, and Tokio.
- **Killed**: Go, Swift, Kotlin global tasks, and Java virtual threads.
- **Main waits**: Trio, Kotlin child tasks, Java platform threads, and Node.

### The only waiter of a task is cancelled

- **Finishes**: Sand and unowned tasks in other systems. Go and Node tasks also finish unless the caller passes a cancellation token to the task.
- **Cancelled**: asyncio futures and tasks, and scoped child tasks in other systems.

If one of two waiters was cancelled, asyncio cancelled the task and sent `CancelledError` to the second waiter. Swift and Kotlin kept the task running and sent its result to the second waiter.

### The caller of a race is cancelled

- **Tasks finish**: Sand and races over unowned tasks.
- **Tasks cancelled**: Scoped task groups and races that share a Go `context`.

### The caller of a timeout is cancelled

All five tested systems cancelled the timed work. Before this change, Sand was the only tested system where the timed work finished.

### A task in a race is cancelled during the race

- **Another task wins**: Trio, Java, and Node `Promise.any`.
- **The race returns a catchable error**: Tokio, Kotlin, and Node `Promise.race`.
- **The race returns the cancelled task**: asyncio `wait`.
- **The program aborts**: Sand before this change.

## Sand measurements

### How scripts use tasks

In 50 Sand scripts that spawn tasks, 34 scripts had at least one task that ran without a waiter. Across the specs, examples, and guide, these scripts started 1,068 tasks; one test started 1,000 of those tasks. This count includes tasks whose results a script read after they finished. Because the specs emphasize cancellation, these numbers are only a rough guide.

`examples/chat_server.sand` starts a task for each client and never waits for it. Requiring a waiter would stop these client tasks.

None of the 34 recorded model transcripts in the `ash` project used `spawn`.

### Candidates

In the table below, "Speed" is the change in retired instructions for 200,000 spawn-and-wait pairs compared to the baseline engine. "Cancel time" is the change for cancelling 9,000 sleeping tasks. "Failing tests" is the failure count out of 1,135 tests. Changes within 0.5% are noise; the cancel baseline shifted by 1.4% between sessions.

| Candidate | Failing tests | Speed | Cancel time |
| --- | --- | --- | --- |
| `task.timeout` cancels its work | 0 | +0.04% | +0.45% |
| A race cancels its tasks with its caller | 8 | -0.01% | +0.13% |
| Cancelling a task cancels the tasks it started | 0 | +1.04% | 7.6x |
| Cancelling the sole waiter cancels the task | 9 | +0.02% | 7.0x |
| A race ignores a cancelled task | 0 | +0.03% | -0.20% |
| A race fails if any task is cancelled | 0 | +0.01% | -0.26% |

The candidates have these tradeoffs:

- **`task.timeout` cancels its work**: The work stops with the caller.
- **A race cancels its tasks with its caller**: If another waiter waits for one of those tasks, the program aborts.
- **Cancelling a task cancels the tasks it started**: This rule fixes timeouts, but not plain races.
- **Cancelling the sole waiter cancels the task**: If a later waiter waits for the task, the program aborts.
- **A race ignores a cancelled task**: Another task can still win the race.
- **A race fails if any task is cancelled**: A single cancelled task aborts the program.

Two spec tests caused the 8 failures in the race and sole-waiter candidates across test modes. The ninth failure in the sole-waiter candidate was `cancelled_waits_do_not_accumulate`.

The 7x cancel times came from prototypes that scanned every task. An index of tasks by owner or waiter could avoid the scan.

Stopping tasks when garbage collection reclaims their handles would make execution depend on collection timing and could drop pending tasks as in asyncio.

### Final implementation

The final implementation tracks the number of tasks that can still finish a race, so each cancellation takes constant time. The initial race prototype scanned the race list on each cancellation, which allowed unmetered quadratic work.

All 1,135 tests passed, and instruction counts stayed within run-to-run noise. Across two runs against the baseline engine, retired instructions changed by:

- -0.01% and +0.01% for channel ping-pong
- +0.01% and 0.00% for spawn-and-wait pairs
- +0.21% and -0.57% for cancelling 9,000 tasks

## Open follow-up

Owner-scoped tasks match the original assumption most closely. An owner block cannot exit until its tasks end, and cancelling the block cancels its tasks. Several runtimes use this model.
