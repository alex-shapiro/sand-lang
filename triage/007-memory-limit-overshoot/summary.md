# A memory limit does not bound the process: peak RSS runs ~1.3x the limit, worse for wide structs

TODO item 007 (Test coverage): "peak_rss_stays_near_the_memory_limit in tests/peak_rss.rs failed…"

**Pain.** A host that sets `limit memory 128M` to cap untrusted code still sees
the process reach ~182 MiB resident, 1.32x the limit. The "decoded computed
defaults" workload (JSON decoding into a struct with computed field defaults)
is the worst of the standard cases, and wider structs push it further (a
16-field struct at 512M reached 842 MiB, 1.6x).

**Why it matters.** Hosts embed Sand to run untrusted code within a fixed
memory budget. If the real resident set routinely exceeds the configured limit
by a third or more, the host must guess a lower limit to stay safe, which makes
the limit hard to choose and undermines the point of setting it.

**Reproduce.**
    cd triage/007-memory-limit-overshoot
    /usr/bin/time -l /Users/alex/Projects/sandlang/target/release/sand run repro.sand --policy=repro.policy --no-prompt
Observed (3 runs): "memory limit exceeded", peak RSS 182-192 MiB against the
128 MiB limit; an empty program peaks near 14 MiB, so the ratio is ~1.32. This
matches the 1.32 the TODO reports. At a 512 MiB limit the same script peaks
~660 MiB (1.26x); a wider struct reaches 1.6-1.8x.

**Aspirational.** Keep peak RSS within a small, documented factor of
`max_memory` for every growth shape, or state the real worst-case multiplier so
hosts can size the limit. The current factor depends on the workload, so no
single limit reliably bounds the process.
