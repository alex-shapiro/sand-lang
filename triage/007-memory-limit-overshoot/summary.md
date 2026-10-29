# Peak RSS exceeds the configured memory limit

Peak resident memory exceeds `max_memory` by 1.32x on the standard computed
field defaults workload, and by up to 1.8x on wider structs. Because the
overshoot depends on the workload, a host cannot choose a single `max_memory`
limit that bounds process memory reliably.

Run:

```
/usr/bin/time -l target/release/sand run repro.sand --policy=repro.policy --no-prompt
```

With a 128 MiB limit, `repro.sand` fails with `memory limit exceeded` at
182–192 MiB peak RSS, which is 1.32x the limit after subtracting the 14 MiB
baseline. A 16-field struct with a 512 MiB limit reaches 842 MiB, or 1.6x the
limit.

The engine should keep peak RSS within a fixed, documented multiplier of
`max_memory` across all workloads.
