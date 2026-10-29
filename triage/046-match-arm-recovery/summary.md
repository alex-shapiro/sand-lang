# Invalid `match` arm patterns suppress errors on later arms and report a spurious `}` error

If a `match` arm contains an invalid pattern such as an exclusive range
`90..100 =>` or a Rust-style struct pattern `Point { x: 0, y } =>`, the parser
fails to recover at the next `match` arm. As a result, it skips diagnostics on
subsequent invalid arms and reports `expected expression, found }` at the end
of the `match`.

Run:

```
$ sand check repro.sand
error: exclusive range patterns are not supported; use `..=`   --> repro.sand:3:11
error: expected expression, found `}`                          --> repro.sand:7:1
```

The parser should recover at the next `match` arm so that `sand check` reports
both `90..100` and `80..90` without a trailing brace error.
