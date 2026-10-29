# A host function or struct cannot use a Sand range

TODO item 022 (Embedding): "Implement `SandType`, `FromValue`, and `IntoValue` for `std::ops::Range<i64>`…"

**Pain.** I want a script to write `editor.View(lines: 10..20)` or `editor.count(1..=5)`. A `Range<i64>` field in a `SandStruct`, or a `RangeInclusive<i64>` parameter, does not compile. `Val<sandlang::native::Range>` is not a workaround, because its `bounds` method is private. Thus the host must use a tuple, and the script must write `(10, 20)` in place of `10..20`.

**Why it matters.** Ranges are the idiomatic Sand and Rust type for line spans, pages, and slices. LLM-authored scripts will pass `a..b` and get a type error.

**Reproduce.** `cargo build` with `repro.rs` in a scratch crate that depends on `sandlang`:
```
error[E0277]: the trait bound `std::ops::Range<i64>: SandType` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: IntoValue` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: FromValue` is not satisfied
error[E0277]: the trait bound `{closure@src/bin/range022.rs:15:25: 15:49}: HostFunction<_>` is not satisfied
```

**Aspirational.** `repro.rs` compiles. `lines` becomes a Sand `Range` field with the default `0..25`, and `count` takes a Sand `Range`.
