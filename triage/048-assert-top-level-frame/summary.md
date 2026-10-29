# A failing `assert` on the final line of a script omits the `<top level>` stack frame

If a failing `assert` is the final top-level statement in a script, the stack
trace includes only the `<prelude>` frame and omits the `<top level>` frame
with the script file path and line number. If another statement follows the
`assert`, the stack trace includes `at <top level> (repro.sand:3)`.

Run:

```
$ sand run repro.sand
error: assert: total is not 4
    at assert (<prelude>:271)
```

The VM should include the `<top level>` frame even if `assert` is in tail
position at the end of a script.
