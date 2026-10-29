# `help` shows type names that I cannot use as written

TODO item 051 (Diagnostics): "Qualify types from other modules in `help` output…"

**Pain.** `help(fs.Metadata)` shows `modified: Instant?`, and `help(http.Request)` shows `conn: Conn` and `-> Result<Json, Error>`. I copy `Instant?` into my function signature and get `unknown type Instant`. The type is `time.Instant`, and the error gives no hint.

**Why it matters.** LLMs copy signatures from `help` output into their code. Unqualified names from other modules cause a failed check and a guess about the correct module.

**Reproduce.**
```
sand check repro.sand
error: unknown type `Instant`   --> repro.sand:4:32
```
**Aspirational.** `help` prints `modified: time.Instant?`, so the copied type resolves (`aspirational.sand` passes `sand check`).
