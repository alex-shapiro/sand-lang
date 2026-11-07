# Tradeoffs

## Fixed stdin and a feed act differently across VMs of one engine

**Problem.** Each new VM of an engine gets a fresh copy of the
`Config::stdin_bytes` input. All VMs of an engine share one feed stream. So
after `Session::reset`, or in the next test of `run_tests`, fixed input starts
again from the first byte, but feed input continues where the last read
stopped. `StdinConfig` (`src/engine.rs`) and the match in `Engine::new_vm`
exist only to keep this difference.

**Options.**

1. Make stdin one stream for each engine, from any source. `Config` holds a
   `VmStdin` and `new_vm` clones it. `StdinConfig` and its match go away. Cost:
   each test of a file no longer gets the same fixed input, and a host that runs
   several scripts on one engine with `stdin_bytes` gets only the rest of the
   input.
2. Keep the current behavior. Fixed input acts like a file that each run opens
   again. A feed acts like a stream, which a run cannot replay.
