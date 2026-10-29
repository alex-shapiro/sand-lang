# A generic struct cannot default a field to `T.zero()`

TODO item 068 (Language): "Allow struct field defaults to call methods from traits…"

**Pain.** I want a generic accumulator `struct Stats<T: Num> { var total: T = T.zero() }` so that `Stats()` and `json.decode` both start from zero. Sand rejects the default with an error about "trait dictionaries", which is a compiler term. The workaround is a static `new()`. Every caller must remember to use `Stats.new()` and not `Stats()`, and `json.decode` fails on input that omits `total`.

**Why it matters.** Swift (`var total: T = .zero`) and Python dataclasses allow this pattern. The workaround changes the public API of the struct and removes default-on-decode, which is the main reason to declare a field default.

**Reproduce.**
```
sand run aspirational.sand
error: field defaults cannot use `Num`, which type parameter `T` requires, because defaults receive no trait dictionaries
sand run repro.sand          # `new()` workaround
Stats(total: 2.5, count: 1)
Err(Error(message: "$: missing field `total`", kind: "parse"))
```

**Aspirational.** `aspirational.sand`: `Stats()` gives `total: 0.0`, and decoding `{"count": 0}` gives `Ok(Stats(total: 0.0, count: 0))`.
