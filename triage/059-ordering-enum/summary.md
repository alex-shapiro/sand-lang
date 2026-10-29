# Comparing two time spans gives me a magic int that `match` cannot check

TODO item 059 (Language): "Add an `Ordering` enum to the prelude and return it…"

**Pain.** `Span` does not implement `Ord`, so `a.compare(b)` is the only way to order two spans. It returns `-1`, `0`, or `1`. A `match` on it needs a dead `_` arm, and a wrong constant (`== 2`) compiles and is always false. The Rust idiom `Less => … Equal => … Greater => …` binds `Less` as a variable and fails with `unreachable pattern`.

**Why it matters.** Rust and Swift users expect an `Ordering` value. With an int, the compiler cannot find a missing or impossible case.

**Reproduce.**
```
sand run repro.sand            # workaround
longer
longer (typo 2): false         # no warning from check or lint
sand check aspirational.sand
error: unreachable pattern     (at `Equal =>` and `Greater =>`)
```

**Aspirational.** `aspirational.sand`: `compare` returns `Ordering`, and the three-arm `match` is exhaustive. Today `Span.compare` is the only standard API that returns an ordering int, so the scope is small.
