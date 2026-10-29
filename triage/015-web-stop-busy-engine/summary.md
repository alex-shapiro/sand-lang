# Stopping a compute-bound engine in the browser terminates its worker and destroys the session

`Engine::interrupt` only takes effect if the engine is waiting on I/O, because a
compute loop never yields to the worker message queue. To stop a compute-bound
script, the browser playground calls `worker.terminate()`, which destroys the
REPL session and unloads the WebAssembly module.

Run:

```
wasm/build.sh nodejs target/wasm-node
PKG=$PWD/target/wasm-node node repro.mjs
```

`repro.mjs` schedules `engine.interrupt()` at 100 ms, but the synchronous run
continues until the 3 s time limit expires.

With cross-origin isolation, the engine can store the interrupt flag in a
`SharedArrayBuffer` so that `interrupt()` stops compute loops in place without
terminating the worker.
