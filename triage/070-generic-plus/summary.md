# The `+` operator in generic functions requires `Num` and rejects strings, bytes, and lists

Concrete `str`, `bytes`, `duration`, and list types support `+`, but `+` on a
type parameter requires `T: Num`, which only `int` and `float` implement. To
write a generic function that combines values with `+`, a script must define a
custom trait and implement it for each type manually.

Run:

```
$ sand run aspirational.sand
error: unknown trait `Add`
error: type parameter `T` does not implement `Num`; add `T: Num`

$ sand run repro.sand
ab
[1, 2, 3]
3
```

Replacing the hardcoded `+` operand check with a prelude `Add` trait allows
generic functions to use `+` across numbers, durations, strings, bytes, and
lists, as in `aspirational.sand`.
