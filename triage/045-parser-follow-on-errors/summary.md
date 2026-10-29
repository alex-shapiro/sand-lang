# A single syntax mistake gives extra errors that point at the wrong lines

TODO item 045 (Diagnostics): "Suppress follow-on parser errors after an initial syntax error…"

**Pain.** I forget one closing quote on line 2. `sand check` gives three errors on lines 6, 7, and 11. No error points at line 2, and the last error (`expected }`) tells me to add a brace that is not missing. A missing `:` in a parameter also gives a spurious `unexpected }`.

**Why it matters.** An LLM that iterates on compiler errors fixes what the errors say. Here, the follow-on errors send it to the wrong lines and suggest a wrong fix.

**Reproduce.**
```
sand check repro.sand
error: expected newline or `;`, found identifier   --> repro.sand:6:18
error: unterminated string                         --> repro.sand:7:20
error: expected `}`, found end of file             --> repro.sand:11:1

sand check repro2.sand
error: expected `:`, found identifier   --> repro2.sand:1:11
error: unexpected `}`                   --> repro2.sand:1:39
```
**Aspirational.** One error for each mistake. For the unterminated string, the error points at the opening quote on line 2. Suppressing follow-on errors alone does not move the first error to line 2.
