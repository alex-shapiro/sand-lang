# Unchecked constructor trait bounds on type annotations produce cascading errors away from the invalid type

The type checker does not verify constructor trait bounds on function
signatures or local type annotations, and it does not infer implied trait
bounds on functions from their parameter types. In `repro.sand`, writing
`Set<(float, float)>` produces four errors at call sites and method calls
instead of rejecting the type annotation directly. In `repro_signature.sand`,
`pub fn lookup(index: Index<float>, ...)` compiles even though `Index<K: Hash>`
cannot be instantiated with `float`.

Run:

```
$ sand check repro.sand
error: (float, float) does not implement `Hash` ...  --> repro.sand:11:7
error: no method `insert` on type Set<(float, float)>; call it on `to_list()`  --> 5:18
error: (float, float) does not implement `Hash` ...  --> 2:37
error: (float, float) does not implement `Hash` ...  --> 4:16

$ sand run repro_signature.sand
library loaded

$ sand check aspirational.sand
error: type parameter `T` does not implement `Hash`; add `T: Hash`  --> 3:32
```

Verifying constructor trait bounds on all written types reports a single error
at `Set<(float, float)>` and `Index<float>`, and inferring implied bounds on
functions allows `aspirational.sand` to compile without repeating `T: Hash`.
