# A bounded generic Rust type cannot derive `SandStruct` if it nests or refers to itself

TODO item 018 (Embedding): "Support fields of type `Self` and nested generic types with…"

**Pain.** I bind a sorted collection `struct Sorted<T: Ord>` with `#[derive(SandStruct)]`. If another derived type holds a `Sorted<T>` field, or if the type has a `Vec<Self>` field, the crate does not compile. The error names `sandlang::native::Param<0>`, a type that I did not write.

**Why it matters.** `T: Ord` on a sorted or tree type is ordinary Rust. The error does not tell me the workaround: remove the bound from the struct.

**Reproduce.** In a scratch crate that depends on `sandlang`, `cargo build` with `repro.rs` (nested `Sorted<T>`) or `repro_self.rs` (`Tree<K: Ord>` with `children: Vec<Self>`). Both give:
```
error[E0277]: the trait bound `sandlang::native::Param<0>: std::cmp::Ord` is not satisfied
17 |     members: Sorted<T>,
   |              ^^^^^^^^^ the trait `std::cmp::Ord` is not implemented for `sandlang::native::Param<0>`
note: required by a bound in `Sorted`
```
These cases compile: `Vec<Self>` in a non-generic type, `Vec<Self>` in an unbounded generic type, and `Sorted<T: Ord>` alone.

**Aspirational.** Both files compile unchanged, and `Sorted<T: Ord>` becomes a Sand type with `T: Ord`.
