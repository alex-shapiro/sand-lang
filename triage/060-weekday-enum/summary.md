# `day_of_week()` returns an `int` from `1` to `7` instead of a `Weekday` enum

`day_of_week()` in the `time` module returns an `int` from `1` for Monday to
`7` for Sunday. Scripts that assume `0`-based indexing from Python or
JavaScript compile without warnings and produce wrong results.

Run:

```
$ sand check repro.sand && sand lint repro.sand
$ sand run repro.sand
friday is weekend: true
sunday detected: false
friday is weekend (correct): false
```

Returning a `time.Weekday` enum from `day_of_week()` rejects numeric indexing
errors at compile time and enables exhaustive pattern matching over all seven
days, as in `aspirational.sand`.
