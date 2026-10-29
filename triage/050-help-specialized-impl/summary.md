# `help` says a type implements a trait that it does not implement

TODO item 050 (Diagnostics): "Display the target type arguments for specialized `impl` blocks…"

**Pain.** I implement `Describe` for `[int]` only. `help([str])` lists `Describe` in its `Implements` section without a condition, next to entries like `Eq where T: Eq`. I trust it, call `["a"].describe()`, and get `[str] does not implement Describe`.

**Why it matters.** LLMs use `help` to find out what they can call. Here `help` gives wrong information and causes a failed check.

**Reproduce.**
```
sand run repro.sand        # Implements: ... Close where T: Close / Describe
sand check repro-call.sand # error: [str] does not implement `Describe`
```
**Aspirational.** `help` lists the entry as `Describe for [int]`.
