# `help` displays types from other modules without their module prefix

`help(fs.Metadata)` prints `modified: Instant?` instead of
`modified: time.Instant?`, and `help(http.Request)` prints `conn: Conn` instead
of `conn: net.Conn`. Using the unqualified type name in a script that imports
only `fs` or `http` fails with `unknown type`.

Run:

```
$ sand check repro.sand
error: unknown type `Instant`   --> repro.sand:3:32
```

`help` should qualify types defined in other modules with their module name so
that signatures copied from `help` output are valid as written, as in
`aspirational.sand`.
