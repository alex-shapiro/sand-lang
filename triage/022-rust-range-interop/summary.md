# Rust `Range<i64>` and `RangeInclusive<i64>` do not implement `SandType`

`std::ops::Range<i64>` and `RangeInclusive<i64>` do not implement `SandType`,
`FromVmValue`, or `IntoVmValue`. `VmVal<sandlang::native::ty::Range>::bounds` is
also private. Host functions and `#[derive(SandStruct)]` structs must therefore
use tuples instead of Sand `Range` values.

Building `repro.rs` fails with:

```
error[E0277]: the trait bound `std::ops::Range<i64>: SandType` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: IntoVmValue` is not satisfied
error[E0277]: the trait bound `std::ops::Range<i64>: FromVmValue` is not satisfied
error[E0277]: the trait bound `{closure@src/bin/range022.rs:15:25: 15:49}: HostFunction<_>` is not satisfied
```

Implementing `SandType`, `FromVmValue`, and `IntoVmValue` for `Range<i64>` and
`RangeInclusive<i64>` maps both types to the Sand `Range` type so that
`repro.rs` compiles.
