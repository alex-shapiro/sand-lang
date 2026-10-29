# String error kinds allow misspelled `Error.kind` comparisons to compile without warnings

`Error.kind` is a `str` such as `"not_found"` or `"invalid"`. If a script
compares `e.kind` against a misspelled string such as `"notfound"`, `"NotFound"`,
or `"not-found"`, `sand check` and `sand lint` report nothing and the error
handler branch never executes.

Run:

```
$ sand check bug.sand && sand lint bug.sand
$ sand run bug.sand --allow='read ./' --no-prompt
error: not_found: cannot read config: not_found: missing-config.json: ...

$ sand run repro.sand --allow='read ./' --no-prompt
{}
```

Replacing string error kinds with an `ErrorKind` enum that includes an
`Other(str)` variant for custom error kinds turns misspelled error kinds into
compile-time errors, as in `aspirational.sand`.
