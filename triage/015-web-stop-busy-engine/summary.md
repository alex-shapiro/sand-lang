# In the browser, stopping a compute-bound engine must kill its worker, losing the whole session

TODO item 015 (Web): "Stopping a busy engine in the browser terminates its worker, since Engine::interrupt only reaches…"

**Pain.** A playground user runs a tight loop and presses Stop. The page cannot
interrupt a busy engine: `Engine::interrupt` only takes effect when the engine
waits, but a compute loop never yields to the worker's message queue. The
playground's only recourse is `worker.terminate()`, which ends the REPL session
("Stopped: the session ended.") and discards every binding the user built up.

**Why it matters.** For an interactive playground or any browser embedder,
"stop the runaway code" should pause the work, not destroy the session. Killing
the worker also drops the loaded wasm module, so the next run pays the full
startup cost again.

**Reproduce.** The synchronous nature is visible even in Node:
    wasm/build.sh nodejs target/wasm-node   # if not already built
    PKG=/Users/alex/Projects/sandlang/target/wasm-node node repro.mjs
Observed: the run finishes at the 3s time limit ("time limit exceeded"); the
`interrupt()` scheduled for 100 ms only fires after the synchronous run
returns (~4000 ms). The flag is never seen mid-run. In the browser the page
runs on a separate thread, so the flag could be delivered, but the busy worker
never reads its message queue, so the page uses `worker.terminate()` instead
(site/playground/app.js, `stop()` -> `endSession()`).

**Aspirational.** With cross-origin isolation (COOP/COEP), keep the interrupt
flag in a `SharedArrayBuffer` the page writes and the running engine polls, so
Stop interrupts a busy engine in place and the session survives.
