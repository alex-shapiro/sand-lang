# The mixed-type list hint suggests a struct or tuple when only an enum works

TODO item 047 (Diagnostics): "Suggest an enum in addition to a struct or tuple…"

**Pain.** I put a `Circle` and a `Square` in one list. The error tells me to "use a struct or a tuple for mixed values". Neither one lets me iterate a list of different shapes. The spec says to use an enum, but the error does not mention it.

**Why it matters.** LLMs follow the hint literally. Here the hint sends them to a wrong design and costs at least one more round trip.

**Reproduce.**
```
sand check repro.sand
error: type mismatch: expected Circle, found Square; all elements of a list have one
       type, so use a struct or a tuple for mixed values
```
**Aspirational.** The hint also suggests an enum. `aspirational.sand` shows the enum version, which runs and prints `3.14` and `4.0`.
