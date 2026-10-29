# An `import` in the middle of a file runs before the lines above it

TODO item 065 (Language): "Reject `import` statements after non-import items or statements…"

**Pain.** I can put `import` after other statements, and `sand check`, `sand lint`, and `sand fmt --check` all accept it. The import does not run at its position: the imported module runs first, and the name is usable on lines above the `import`. A Python user who writes `print("loading"); import "./plugin.sand"` expects the print first, but gets the plugin output first.

**Why it matters.** The source order does not match the run order, and the spec (§3.1) says this file is not valid. A reviewer of untrusted code who reads only the top of the file can miss the imports. The sandbox is not affected: the policy checks each import before any code runs, at any position (`--deny='import **'` stops the run before line 1).

**Reproduce.**
```
sand run repro.sand
side: module body runs      # runs before line 3
main: line 3
[1]                         # `json` used one line before `import json`
hi
```
Severity: low. Nothing breaks, but the order surprises the user.

**Aspirational.** `aspirational.sand`: all imports are at the top. `sand check` rejects `repro.sand` with an error at the first late `import`.
