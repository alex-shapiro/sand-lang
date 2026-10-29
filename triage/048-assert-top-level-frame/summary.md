# A failing `assert` on the last line of a script shows no script line

TODO item 048 (Diagnostics): "Include the `<top level>` stack frame if a failing `assert`…"

**Pain.** My script ends with an `assert`. When it fails, the trace shows only the prelude frame, so I cannot see which line of my script failed. The same `assert` followed by any other statement shows `at <top level> (repro.sand:3)`.

**Why it matters.** Scripts often end with checks, and an LLM that verifies its own script with `assert` needs the line to fix the right check.

**Reproduce.**
```
sand run repro.sand
error: assert: total is not 4
    at assert (<prelude>:271)
```
Append `print("done")` to the script, and the trace also shows `at <top level> (repro.sand:3)`.
