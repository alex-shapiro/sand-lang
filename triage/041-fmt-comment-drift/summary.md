# `sand fmt` shifts line comments upward on each pass after nested tuple indexing

If a statement contains a nested tuple index such as `grid.0.0`, `sand fmt`
moves subsequent line comments above the preceding statement on each formatting
pass. As a result, `sand fmt` is not idempotent and detaches comments from the
statements they describe.

Run:

```
cp repro.sand /tmp/g.sand
for i in 1 2 3; do sand fmt /tmp/g.sand; done; cat /tmp/g.sand
```

After three passes, all three line comments move above `let a = grid.0.0`:

```
let grid = ((1, 2), (3, 4))
// d is bottom right
// c is bottom left
// b is top right
let a = grid.0.0

let b = grid.0.1
```

`sand fmt` must preserve comment positions and produce identical output on
repeated passes.
