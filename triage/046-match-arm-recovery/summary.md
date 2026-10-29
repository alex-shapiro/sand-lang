# One bad `match` arm hides the next bad arm and adds a spurious error

TODO item 046 (Diagnostics): "Recover at the next `match` arm after an invalid pattern…"

**Pain.** I write Rust-style range arms (`90..100 =>`, `80..90 =>`). `sand check` reports only the first arm, plus `expected expression, found }` at the end of the function. I fix the first arm, run again, and only then see the error on the second arm. Struct patterns in Rust style (`Point { x: 0, y } =>`) behave the same way.

**Why it matters.** LLMs write these Rust patterns often. Each hidden error costs one more round trip, and the spurious `}` error suggests a brace problem that does not exist.

**Reproduce.**
```
sand check repro.sand
error: exclusive range patterns are not supported; use `..=`   --> repro.sand:3:11
error: expected expression, found `}`                          --> repro.sand:7:1
```
Line 4 (`80..90 =>`) has the same mistake but no error.
