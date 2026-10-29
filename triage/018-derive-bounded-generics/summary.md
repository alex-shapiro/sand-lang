# `#[derive(SandStruct)]` fails on bounded generic types that nest or reference `Self`

`#[derive(SandStruct)]` and `#[derive(SandEnum)]` fail to compile if a field
uses a bounded generic type such as `Sorted<T>` where `Sorted<T: Ord>`, or
`Self` inside a bounded generic type such as `Vec<Self>` in `Tree<K: Ord>`. The
derive macro substitutes `sandlang::native::Param<N>` for each type parameter
to describe field types, and `Param<N>` does not implement Rust traits such as
`Ord`.

Building `repro.rs` or `repro_self.rs` fails with:

```
error[E0277]: the trait bound `sandlang::native::Param<0>: std::cmp::Ord` is not satisfied
17 |     members: Sorted<T>,
   |              ^^^^^^^^^ the trait `std::cmp::Ord` is not implemented for `sandlang::native::Param<0>`
note: required by a bound in `Sorted`
```

Both files should compile without requiring the user to remove trait bounds
from the struct definition.
