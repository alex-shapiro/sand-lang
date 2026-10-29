# A script that fits in its memory limit fails with "memory limit exceeded"

TODO item 052 (Performance): "Clear dead temporaries before native calls so that inlined allocations…"

**Pain.** I set `limit memory 20M` for a script that never holds more than one
12 MB string at a time. The second call fails, because the engine still keeps
the string of the first call. I must set the limit to double the real peak, or
guess which expressions keep old values.

**Why it matters.** Hosts set memory limits to stop untrusted scripts. If a
script needs 2x its real peak memory, a correct script fails, and the host
cannot choose a limit from what the script really uses.

**Reproduce.**

```
$ sand run repro.sand --policy=repro.policy --no-prompt
12000000
sand: memory limit exceeded          (exit 3)

$ sand run loop.sand --policy=repro.policy --no-prompt
12000000
sand: memory limit exceeded          (exit 3)
```

Each script runs to completion with `limit memory 24M`. A single
`print("y".repeat(12_000_000).len())` passes with `limit memory 14M`, so one
12 MB string fits in 20M.
