# Struct field defaults cannot call trait methods on the struct type parameters

Default expressions on struct fields do not receive trait dictionaries, so a
generic struct such as `struct Stats<T: Num>` cannot use `T.zero()` as a field
default. Without the field default, callers must use a custom constructor
function instead of `Stats()`, and `json.decode` fails if the JSON input omits
that field.

Run:

```
$ sand run aspirational.sand
error: field defaults cannot use `Num`, which type parameter `T` requires, because defaults receive no trait dictionaries

$ sand run repro.sand
Stats(total: 2.5, count: 1)
Err(Error(message: "$: missing field `total`", kind: "parse"))
```

Passing trait dictionaries to default-value helpers and `json.decode` field
initializers allows `aspirational.sand` to construct `Stats()` and decode JSON
objects that omit `total`.
