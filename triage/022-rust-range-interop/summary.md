# Rust `Range<i64>` and `RangeInclusive<i64>` do not implement `SandType`

Host functions and `#[derive(SandStruct)]` structs cannot accept or return
`std::ops::Range<i64>` or `RangeInclusive<i64>` because neither type implements
`SandType`, `FromVmValue`, or `IntoVmValue`. The `bounds` method on
`VmVal<sandlang::native::ty::Range>` is private. Host APIs must therefore use
tuples instead of Sand `Range` values.

Building `repro.rs` fails with:

```
error[E0277]: the trait bound `std::ops::Range<i64>: SandType` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: IntoVmValue` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: FromVmValue` is not satisfied
error[E0277]: the trait bound `{closure@src/bin/range022.rs:15:25: 15:49}: HostFunction<_>` is not satisfied
```

Implementing `SandType`, `FromVmValue`, and `IntoVmValue` for `Range<i64>` and
`RangeInclusive<i64>` allows `repro.rs` to compile and map both types to the
Sand `Range` type.
