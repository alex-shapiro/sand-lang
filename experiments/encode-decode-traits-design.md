# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** Nothing in this document is implemented. The working tree of `encode-decode-traits-v2` holds an earlier draft. [What the draft keeps](#what-the-draft-keeps) lists the parts of that draft that this design reuses.

## Summary

`Encode` and `Decode` become ordinary traits with dictionaries. Their methods have **native default implementations**. The compiler derives an impl for a struct or enum when the components of the type implement the trait. A derived impl is an ordinary impl that uses the native defaults. A user impl replaces the derived impl for its type.

A native default walks a value in Rust. To find the impl of each component, it reads the **part dictionaries** of the impl, not the type of the value. If a part uses the same native default, the walk stays in Rust. If a part has a user impl, the walk calls that impl through an async native callback from commit `8a51c46`.

The VM has no code for any one trait. The mechanism applies to every trait that has native defaults. `Eq`, `Ord`, and `Hash` can move to the same mechanism later.

These goals from the task follow from the design:

- **`Value` can be encoded and decoded.** A single encode walker converts a Sand value to a `Value`. A single decode walker converts a `Value` to a Sand value.
- **JSON can be encoded and decoded.** JSON text converts to and from `Value` in plain Rust.
- **`Encode` and `Decode` are ordinary traits.** User impls, generic impls, bounds, and dictionaries follow the trait rules in SPEC §4.9.
- **`json.Json` uses the ordinary JSON encoder.** `Json` has an ordinary impl whose methods are native functions. The JSON writer has no special case for `Json`.

## The problem with the earlier draft

The earlier draft kept `Encode` and `Decode` as special cases in three places:

- **The checker.** The two traits declared methods but passed no dictionary. A `has_dict` exception named them.
- **The VM type table.** `StructRt` gained an `encode` slot and a `decode` slot. Each slot held the method of a custom impl.
- **The impl rules.** An impl of either trait had to target a struct or enum without type parameters, because the slots could not hold type arguments.

Each special case exists because the engine found impls through the type of a value, not through trait evidence. This design removes all three. Evidence comes from dictionaries, as for any other trait.

## Part 1: Native default methods

A trait method can have a native default implementation:

```sand
pub trait Encode {
    /// Writes `self` to `e` as a single value with `e.encode(x)`.
    native fn encode(self, e: Encoder)
}

pub trait Decode: Encode {
    /// Reads a value of this type from `d`, usually with `d.decode()`.
    native fn decode(d: Decoder) -> Result<Self>
}
```

A native default is a default method whose body is a Rust function. An impl that does not define the method uses the native default, as with any default method. A trait method already receives the dictionary of `Self` as its first hidden argument (`check/traits.rs`). The native default therefore receives the dictionary of the impl that it runs for.

## Part 2: Structural impls

A **structural impl** is an impl that uses the native defaults of its trait. It has two sources:

- **Explicit structural impls** for built-in types. The prelude declares them with empty bodies:

  ```sand
  impl Encode for int {}
  impl<T: Encode> Encode for [T] {}
  impl<K: Encode, V: Encode> Encode for [K: V] {}
  ```

- **Derived structural impls** for structs and enums. The checker creates an impl for each struct or enum whose components implement the trait. The fixed point in `check/derive.rs` already computes which type parameters must implement the trait. Those type parameters become the bounds of the derived impl. A user impl of the trait for the type suppresses the derived impl.

The prelude declares which traits the checker derives. Today the checker finds them by name through `Builtin`. [Open question 1](#open-questions) proposes a declaration for this property.

A derived impl is an `ImplDef` like any other. Evidence resolution then uses ordinary impl lookup for `Encode` and `Decode`, so `resolve_derived` no longer handles them.

## Part 3: Part dictionaries

A native default must reach the impl of each part of a value: each field of a struct, each payload of a variant, each element of a list. The impl dictionary on `main` holds the method closures, the supertrait dictionaries, and the dictionaries for the bounds of the impl. That is not enough. `struct User { email: Email }` has no type parameters, so its dictionary holds no dictionary for `Email`.

A structural impl therefore also has an **impl shape**. The impl shape is a compile-time constant with two fields:

- **The kind** of the type: `Int`, `Str`, `List`, `Map`, `Tuple(n)`, `Struct`, `Enum`, a native value type, or `Never`. For a struct, the kind holds the field names and the field defaults. For a native value type, the kind holds the parse function.
- **A part template** for each part. A part template describes how to build the dictionary of the part from the dictionaries of the impl bounds.

A part template has three forms:

```rust
enum PartTemplate {
    /// Program constant `k`, such as a constant dictionary.
    Const(u32),
    /// Dictionary `j` of the bounds of this impl.
    Bound(u32),
    /// A dictionary tuple built from its items.
    Tuple(Vec<PartTemplate>),
}
```

The dictionary of a structural impl holds its impl shape after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., impl shape, bound dictionaries...]
```

Trait dispatch reads only the methods and the supertrait dictionaries. Only the native defaults read the impl shape, so the layout of other impls does not change.

The checker builds each part template with the existing evidence machinery. It resolves `PartType: Trait` in the context of the impl, as it does for the body of an impl method. The result is an evidence expression made of `HKind::Dict` nodes, projections of the bound dictionaries, and constants. The compiler lowers that expression to a part template instead of to bytecode.

Examples:

| Impl | Parts | Part templates |
| --- | --- | --- |
| `impl Encode for User` with `email: Email` | `Email` | `Const(dict of Email)` |
| `impl<T: Encode> Encode for [T]` | `T` | `Bound(0)` |
| `impl<T: Encode> Encode for Pair<T>` with `items: [T]` | `[T]` | `Tuple([list method, list shape, Bound(0)])` |

## Part 4: Recursive types

**Part templates are instantiated lazily, at walk time.** That choice handles recursive types without cycles in constants or in runtime dictionaries.

Consider `struct Tree { kids: [Tree] }`. The shape of `Tree` refers to the dictionary of `[Tree]`. That dictionary refers to the dictionary of `Tree`, which holds the shape of `Tree`.

- **Constant dictionaries.** A part template refers to a constant by its index. The VM resolves the index at walk time, not when it creates the constants. The compiler reserves the constant index of a shape before it builds the part templates. A part template that reaches the same shape again uses the reserved index. The constants stay acyclic.
- **Runtime dictionaries.** For a generic `Tree<T>` in a generic function, the part template of `kids` is `Tuple([list method, list shape, Tuple([tree method, tree shape, Bound(0)])])`. The template is finite. The walker builds the dictionary of a part only when it descends into that part.
- **Polymorphic recursion.** For `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`, the part template of `Deep` builds `Nest<[T]>` from the dictionary of `T`. Each level of the value builds one more level of dictionary. The depth of the value bounds the work.

Two other schemes were considered:

| Scheme | Problem |
| --- | --- |
| Store the part dictionaries in each dictionary | Recursive types make cyclic dictionaries. Polymorphic recursion makes an infinite dictionary at compile time. |
| Generate a Sand function that returns the part dictionaries | The walker must call Sand code for each new dictionary. Each call costs an async callback of about 90 ns, even for a type with no user impls. |

For comparison, Swift instantiates the witness tables of generic conformances lazily and caches them. GHC builds dictionaries for recursive instances as lazy values. Both statements come from general knowledge, not from an audit.

The walker caches each instantiated part dictionary for the length of a walk. The cache key is the shape and the dictionary of the parent. A list of 10,000 `Tree<int>` values builds the dictionary of `Tree<int>` once.

## Part 5: The walkers

The walkers follow the draft, with one change: they carry dictionaries instead of reading `StructRt` slots.

**Encode** walks a heap value into a `Value` iteratively. Each open container holds its dictionary. For each part, the walker gets the part dictionary from the part template. If the method in the part dictionary is the native default, the walk continues in Rust. Otherwise, the walker records a **hole**: the path to the part, the value, and the part dictionary. After the walk, the native call calls the method of each hole with `[dictionary, value, encoder]` and fills the hole with the written `Value`.

**Decode** walks a `Value` with the kind of each shape. A part with a user impl becomes a hole, and so does a missing field with a computed default. If the first pass records holes, the engine calls each hole, and then a second pass builds the value with the results. This order runs computed defaults in post-order and only after the input decodes, as on `main`. It also lets sets and maps hash finished values.

If a walk records no holes, the native function returns at once. An immediately ready native call has no future and no callback.

## Part 6: Checker changes

The design needs these checker changes:

1. **Native default methods.** A trait can declare `native fn` methods. Today only impl methods can be native.
2. **Derived impls are `ImplDef`s.** `derive.rs` creates them. The derivation conditions stay as they are: a struct with `var` fields does not derive `Hash`, and a struct with `init` does not derive `Decode`. [Open question 1](#open-questions) covers how these conditions can leave the checker later.
3. **Part templates.** The checker resolves the evidence for the parts of each structural impl.
4. **`never`.** On `main`, `never` implements only traits without dictionaries (SPEC §5.1). `json.encode([])` needs `[never]: Encode`. `never` gets a structural impl of each derived trait, with kind `Never`. Its native default for `decode` fails, and its native default for `encode` never runs.
5. **Native functions read dictionaries.** `json.encode<T: Encode>` receives the dictionary of `T` as a hidden argument. A native parameter type `Dict<N>` reads the dictionary of the `N`-th bound with methods. `Num<N>` already reads the `Num` dictionary in the same way.
6. **`reflect` goes away.** The `Decode` dictionary carries the shape of the type, so `json.decode<T: Decode>` needs no `reflect` parameter. No other stdlib function uses `reflect`. The parser, the checker rules, `TypeDesc`, `DescTemplate`, `Op::Describe`, and `compile/reflect.rs` are deleted.

Two limits on dictionary traits stay in place:

- **A local function cannot require `Encode` or `Decode`.** The checker reports `local functions cannot require traits with methods`. On `main`, a local function can require them.
- **A struct field default cannot require `Encode` or `Decode`** (SPEC §4.2).

[Open question 2](#open-questions) asks whether to lift these limits in this branch.

## Part 7: JSON and `Value`

- **The JSON parser produces a `Value`.** Objects become maps with string keys, and `null` becomes `Value::Unit`.
- **The JSON writer reads a `Value`.** It holds the JSON rules: it flattens `Some`, writes `bytes` as Base64, and writes map keys and native value types as text.
- **The decode walker accepts the JSON form of each type.** For example, it accepts a Base64 string for `bytes` and a text key for a map with `int` keys. A `Value` from JSON text is therefore valid input.
- **`Json` has an ordinary impl with native methods:**

  ```sand
  impl Encode for Json { native fn encode(self, e: Encoder) }
  impl Decode for Json { native fn decode(d: Decoder) -> Result<Json> }
  ```

  These methods convert a whole `Json` tree to or from a `Value` in one call. `json.parse(text)` becomes `json.decode(text)` with `T = Json`.
- **`Vm::to_value` takes the `Encode` dictionary of the value.** A synchronous native function cannot run a user impl, so `Vm::to_value` fails with kind `"invalid"` if it reaches one. `NativeContext::to_value` runs user impls.

## Reinterpretation of SPLIT tasks 14 and 15

**Task 14** (`Value` and `Vm::to_value`) merged in #41. This design changes two of its decisions:

- `Vm::to_value` reads the `Encode` dictionary of the value, so that it finds user impls.
- The three heap walkers become a single encode walker. `json.encode` and `json.to_json` use that walker through `Value`.

**Task 15** becomes "`Encode` and `Decode` as ordinary traits", with this content:

- Parts 1 to 7 of this design.
- A single decode walker reads `Value`, and JSON text reaches it through `Value`.
- No `value::Decode` Rust trait. Task 16 adds `serde` support for `Value`, and a host converts `Value` to its Rust types through `serde`.

**Task 18** (host function calls) decodes each argument with the decode walker. Each parameter type of a script function is closed, so the compiler can emit a constant `Decode` dictionary for it.

## Costs and risks

- **Dictionary passing.** Each generic call with a `T: Encode` bound passes a dictionary. For a closed type, the dictionary is a constant. In generic code, the call builds a tuple at run time. The triage measured a generic trait call at about 12 ns more than a direct call.
- **Walk overhead.** Each part costs a dictionary read and a comparison. Closed types instantiate no dictionaries, because their part templates are constants.
- **The intermediate `Value` tree.** `json.encode` now builds a `Value` before it writes text. I expect a slowdown, and I will measure it rather than guess.
- **Memory accounting.** The walkers count the `Value` tree against `max_memory` while they build it. The tree and the root table stay outside the count during callbacks. `TODO.md` already records the general problem: "Charge boxed recursive futures against `max_memory`."

**Expected size.** The deletions are `compile/reflect.rs`, `TypeDesc` and `DescTemplate`, the typed JSON decoder, the JSON text encoder, `ToJsonConverter`, the computed-default fill prototypes, and `reflect` in the parser and checker. The additions are the two walkers, impl shapes and part templates, native default methods, structural impls for `never` and the built-in types, and `Dict<N>`. I expect the net line count to go down. I will report the measured count.

## Rejected alternatives

| Alternative | Reason |
| --- | --- |
| Look up user impls through the type of a value, in a table keyed by trait and type definition | It is a second dispatch mechanism beside dictionaries. Erased type arguments make generic and concrete-argument impls impossible, and `Decode` still needs a type description. |
| Generate Sand code for each derived impl, as Swift does | One code path, but each part costs a Sand call. The triage measured 2.3 to 7 times slower codecs. |
| Generate Sand code and keep a native fast path | Two codecs must agree on output, paths, and errors. This was the design of `origin/encode-decode-traits`. |

## What the draft keeps

The draft on `encode-decode-traits-v2` keeps these parts:

- `codec/encode.rs` and `codec/decode.rs`: the walkers, holes, the two-pass decode, and `Value` paths for error messages. Their dispatch changes from `StructRt` slots to dictionaries.
- The JSON parser and writer on `Value`, and the native `Json` impl methods.
- Computed defaults as holes. This deletes `struct_fill`, `set_fill`, and `FILL_PROTO`.

The draft drops these parts:

- `StructRt::encode` and `StructRt::decode`.
- `CodecImpls` on `TypeDef`.
- The `has_dict` exception for `Encode` and `Decode`.
- The rule that limits impls to struct and enum types without type parameters.

## Test and measurement plan

- **Existing behavior.** The `std_json.test` spec cases keep their output, except for deliberate message changes. This document will list each changed message.
- **New spec cases:**
  - user impls on structs and enums
  - generic impls, such as `impl<T: Encode> Encode for Box<T>`
  - recursive types and polymorphic recursion
  - `[never]`
  - a user impl inside another user impl
  - a user impl that waits on `task.sleep`
  - an impl that writes no value, and an impl that writes two values
  - error paths of user `Decode` impls
- **Robustness.** Run the tests with `Config::retry_stress`. Run the hostile probes from `hostile-probes-for-new-data`: RSS against the memory limit, deep values, many holes, and runs with a 512 KiB stack.
- **Speed.** Use `bench/run.py` and `benches/codec.rs` to compare `json.encode`, `json.decode`, `json.parse`, and `json.to_json` on `main` and on the branch. Also measure a generic call that passes an `Encode` dictionary.

## Open questions

1. **How does a trait declare that the checker derives it?** I propose a modifier, `pub derive trait Encode { ... }`, that marks a trait whose structural impl the checker creates for each eligible struct and enum. The two derivation conditions (`var` fields for `Hash`, `init` for `Decode`) remain checker rules until a later change moves them. The alternative is to keep the `Builtin` name list for this branch.
2. **Should this branch lift the limits on local functions and struct field defaults for dictionary traits?** I recommend keeping them, because they affect all traits with methods, not only codecs. Local functions that require `Encode` then become errors.
3. **Should this branch remove `reflect`?** I recommend yes. Without `reflect`, `Decode` evidence has one form, and the deletion is large.
