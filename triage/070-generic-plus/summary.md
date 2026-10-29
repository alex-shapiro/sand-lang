# A generic function can use `+` on numbers, but not on strings or lists

TODO item 070 (Language): "Replace the hardcoded `+` operand check (`require_plus` in…"

**Pain.** `"a" + "b"`, `[1] + [2]`, `b"a" + b"b"`, and `1s + 2s` all work on concrete types. In a generic function, `a + b` needs `T: Num`, which only `int` and `float` implement. When I follow the compiler hint and add `T: Num`, the call with strings fails with `str does not implement Num`. There is no bound that means "supports `+`". The workaround is a custom trait plus one `impl` for each type.

**Why it matters.** Generic concatenate, join, and fold helpers are common in Rust (`T: Add<Output = T>`), Swift, and Python, which all allow this. The compiler hint (`add T: Num`) leads to a second error and not to a fix.

**Reproduce.**
```
sand run aspirational.sand
error: unknown trait `Add`
error: type parameter `T` does not implement `Num`; add `T: Num`
# with `T: Num`:
error: str does not implement `Num`        (at the call with ["a", "b"])
sand run repro.sand          # custom `Concat` trait workaround
ab
[1, 2, 3]
3
```

**Aspirational.** `aspirational.sand`: a prelude `Add` trait (a supertrait of `Num`) covers every type that supports `+`, so `concat_all<T: Add>` works for `str`, lists, and `int`.
