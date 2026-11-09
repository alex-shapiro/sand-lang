# Tradeoffs

## Cancelling a task that waits in `task.race`

**Problem:** when a task in `task.race` is cancelled, the other tasks of the race keep running. A `task.timeout` caller that is cancelled can leave its work running for the same reason. The guide says only that a race cancels the remaining tasks when the first task completes.

**Options:**

- **Keep the tasks running.** This is the current behavior. A task can pass a task to a race and still own it. The cost is that a cancelled caller can leave work running with no error.
- **Cancel the tasks of the race.** Cancellation then reaches all the work that the caller started, as `asyncio.wait_for` does. The cost is that a task can stop because another task cancelled a race that it did not start.

## A task that is cancelled during `task.race`

**Problem:** `task.race([a, b])` returns the result of `b` if `a` was cancelled before the race. If `a` is cancelled during the race, the run stops with `cancelled: task cancelled`.

**Options:**

- **Fail only if the cancelled task is the last one.** A race then gives the same result for both orders. The cost is that a script does not see the cancellation of one task.
- **Fail in both orders.** Each cancellation is then visible. The cost is that a race over a list with a cancelled task always fails.
- **Keep both behaviors.** No script changes. The result continues to depend on timing.
