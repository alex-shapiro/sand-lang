# `time.Span.compare` returns `-1`, `0`, or `1` instead of an `Ordering` enum

`time.Span` does not implement `Ord`, and `time.Span.compare` returns `-1`,
`0`, or `1` as an `int`. Because the return type is `int`, a `match` expression
requires a wildcard `_` arm, and comparing the result against an invalid
integer constant such as `2` compiles without warnings.

Run:

```
$ sand run repro.sand
longer
longer (typo 2): false

$ sand check aspirational.sand
error: unreachable pattern
```

Adding a prelude `Ordering` enum with `Less`, `Equal`, and `Greater` variants
and returning it from `time.Span.compare` makes three-way comparisons
exhaustive without a wildcard arm, as in `aspirational.sand`.
