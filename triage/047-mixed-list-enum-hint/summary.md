# Mixed-type list diagnostics suggest structs and tuples but omit enums

If a list literal contains elements of different types such as
`[Circle(r: 1.0), Square(s: 2.0)]`, the type mismatch diagnostic suggests using
a struct or a tuple, neither of which supports iterating over a heterogeneous
collection.

Run:

```
$ sand check repro.sand
error: type mismatch: expected Circle, found Square; all elements of a list have one
       type, so use a struct or a tuple for mixed values
```

The diagnostic hint should also suggest wrapping the variants in an enum, as
shown in `aspirational.sand`.
