# `sand fmt` moves comments up one statement on each pass after nested tuple indexing

TODO item 041 (Tooling): "Make `sand fmt` idempotent when a multi-line expression contains line…"

**Pain.** I write ordinary code that reads nested tuples (`grid.0.0`) and puts a comment above the next line. Each `sand fmt` pass moves each comment up by one statement. After three passes, all comments sit at the top of the file, and they describe the wrong lines.

**Why it matters.** The formatter silently destroys the meaning of comments, and `sand fmt --check` fails again after every `sand fmt`. The trigger is the nested tuple index, not a multi-line expression: comments in multi-line calls, chains, lists, and binary expressions stay stable in my tests.

**Reproduce.**
```
cp repro.sand /tmp/g.sand
for i in 1 2 3; do sand fmt /tmp/g.sand; done; cat /tmp/g.sand
```
Observed after pass 3 (passes 1 and 2 also give different output):
```
let grid = ((1, 2), (3, 4))
// d is bottom right
// c is bottom left
// b is top right
let a = grid.0.0

let b = grid.0.1
...
```
One pass on `print(p.0.1)\n// done\np.1.print()` also moves `// done` into the `print(...)` argument list.
