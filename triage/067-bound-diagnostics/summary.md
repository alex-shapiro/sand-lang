# A wrong `Set` element type gives four errors, none at the line I must fix

TODO item 067 (Language): "Check that all written types satisfy constructor trait bounds…"

**Pain.** I write `Set<(float, float)>` in a signature and in a `let`. `sand check` does not reject the type I wrote. Instead it gives four errors in the body and at the call site. The first error points at the caller on line 13. One error says ``no method `insert` on type Set<(float, float)>; call it on `to_list()` ``, which sends me the wrong way. A library function such as `pub fn lookup(index: Index<float>)` compiles cleanly, but no caller can ever construct its argument. Also, `fn size<T>(b: Bag<T>)` does not infer `T: Hash`, so I must repeat a bound that the type already states.

**Why it matters.** An LLM fixes the line that the first error names. Here, that line (the call site) is correct, so the fix loop goes the wrong way. Runtime is sound: I found no path that puts a `float` or a list into a set or map key (`unique`, `to_set`, `group_by`, map literals, and generic `T: Hash` calls all fail at compile time).

**Reproduce.**
```
sand check repro.sand
error: (float, float) does not implement `Hash` ...  --> repro.sand:13:7   (call site)
error: no method `insert` on type Set<(float, float)>; call it on `to_list()`  --> 7:18
error: (float, float) does not implement `Hash` ...  --> 4:37   (`Set()`)
error: (float, float) does not implement `Hash` ...  --> 6:16   (`contains`)
sand run repro_signature.sand
library loaded                      # `Index<float>` is accepted
sand check aspirational.sand
error: type parameter `T` does not implement `Hash`; add `T: Hash`  --> 6:32
```

**Aspirational.** One error at `Set<(float, float)>` on line 3, and an error on `Index<float>` at its declaration. `aspirational.sand` compiles because `Bag<T>` implies `T: Hash`.
