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

## A process stdin error looks like an end of input

**Problem.** If `sand run` or `sand test` cannot read its standard input,
`forward_stdin` (`cli/src/main.rs`) writes the error to stderr and ends the
feed. The script gets `None` or a short `read_all`, and it cannot tell cut-off
input from complete input. On main, the script got an `Err` with a kind.

**Options.**

1. Add back `StdinFeed::fail(io::Error)`: after the queued bytes, every read
   fails with the error. Cost: one more feed method and an error state, about
   25 lines with its CLI test. The deletion pass removed it to shrink the API.
2. Keep the current behavior. Such errors are rare (for example, stdin is a
   directory), and the user sees the message on stderr.
