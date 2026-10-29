# Rust `Range<i64>` and `RangeInclusive<i64>` do not implement `SandType`

Host functions and `#[derive(SandStruct)]` structs cannot accept or return
`std::ops::Range<i64>` or `RangeInclusive<i64>` because neither type implements
`SandType`, `FromValue`, or `IntoValue`. `Val<sandlang::native::Range>` is not
a usable alternative because its `bounds` method is private, which forces host
APIs to use tuples instead of Sand `Range` values.

Building `repro.rs` fails with:

```
error[E0277]: the trait bound `std::ops::Range<i64>: SandType` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: IntoValue` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: FromValue` is not satisfied
error[E0277]: the trait bound `{closure@src/bin/range022.rs:15:25: 15:49}: HostFunction<_>` is not satisfied
```

Implementing `SandType`, `FromValue`, and `IntoValue` for `Range<i64>` and
`RangeInclusive<i64>` allows `repro.rs` to compile and map both types to the
Sand `Range` type.
