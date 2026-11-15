# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** We have not implemented this design yet. An earlier code draft on `encode-decode-traits-v2` fails to compile. See [Reusable parts from the earlier draft](#reusable-parts-from-the-earlier-draft) for the parts that this design keeps.

## Summary

We propose making `Encode` and `Decode` ordinary traits with native default methods. Users can implement both traits, including generic implementations such as `impl<T: Encode> Encode for Box<T>`. The compiler erases type arguments at compile time. Callers therefore pass trait dictionaries to generic functions as hidden runtime arguments. See [Why `Encode` and `Decode` need dictionaries](#why-encode-and-decode-need-dictionaries) for the comparison with `main`.

Each trait method uses a native Rust default that walks values in Rust. The compiler derives `Encode` and `Decode` for structs and enums as ordinary implementations that inherit these defaults. A custom implementation on a type replaces its derived implementation.

During a walk, the native default finds each part's implementation through dictionaries rather than runtime value types. For derived parts, the walker stays in Rust. For parts with custom implementations, the walker calls the user Sand method through a callback.

The VM treats `Encode` and `Decode` like any other trait with native default methods. Other derived traits can use the same mechanism later.

This design meets four goals:

- **Host `Value` conversion.** The encode walker copies a Sand heap value into a `Value`. The decode walker builds a Sand heap value from a `Value`.
- **JSON encoding and decoding.** Plain Rust functions convert between JSON text and `Value`.
- **Ordinary trait rules.** Custom implementations, generic implementations, and trait bounds follow SPEC §4.9.
- **Standard `json.Json` handling.** `Json` implements `Encode` and `Decode` as ordinary trait implementations. The JSON writer formats `Json` through those implementations.

## Terms

Each term below keeps a single meaning throughout this document:

- **Trait evidence.** Generic functions such as `fn save<T: Encode>(item: T)` need the `Encode` implementation for `T`. Because the compiler erases type arguments, callers pass a hidden runtime argument called trait evidence.
- **Dictionary.** On `main`, trait evidence for a trait with methods is a tuple called a dictionary. Each dictionary stores three groups of items in order:
  1. Method closures of the implementation.
  2. Supertrait dictionaries. For `Decode: Encode`, this group holds the `Encode` dictionary for the same type.
  3. Bound dictionaries of the implementation. For `impl<T: Show> Show for [T]`, this group holds the `Show` dictionary for `T`.

  For a trait without methods, such as `Hash` on `main`, the checker checks the bound at compile time. The compiler omits a runtime dictionary.
- **Constant dictionary.** If every type in a dictionary is concrete, the compiler emits the dictionary once as a program constant. For example, `[int]: Show` uses a constant dictionary.
- **Runtime dictionary.** Inside generic code, a dictionary can depend on a type parameter's dictionary. Generated code then builds the dictionary tuple at runtime, as for `[T]: Show` inside `fn print_all<T: Show>(items: [T])`.
- **Derived trait.** The compiler implements five built-in traits automatically on `main`, including `Encode` and `Decode`:
  - A struct or enum implements a derived trait if all of its fields and variant payloads implement that trait.
  - Users cannot write custom implementations for derived traits on `main`.
- **Native function.** A native function is a Rust function that Sand code calls.
- **Native default method.** This design adds native default methods to traits. A native default method uses a native Rust function as its default body. Any implementation that omits the method body inherits the native default.
- **Part.** A part is a direct child value inside a container or composite value:
  - Struct parts are field values.
  - Enum parts are variant payload values.
  - List and set parts are elements.
  - Map parts are keys and values.
- **Walker.** A walker is a native function that visits a value and its parts in turn. The encode walker reads a value from the VM heap. The decode walker builds a value on the VM heap.
- **`Value`.** `sandlang::value::Value` is a host-owned Rust type that holds a copy of a Sand value outside the VM. A Rust host can keep a `Value` after a VM run ends.
- **Callback.** Commit `8a51c46` lets an async native function call a Sand function. This document calls that nested Sand call a callback. Each callback round trip takes about 90 ns, as measured in `experiments/native-async-sand-calls.md`.
- **Triage report.** The document `experiments/encode-decode-design-causes.md` on `encode-decode-design-causes` explains why earlier codec designs were slow or complex.

## Why `Encode` and `Decode` need dictionaries

Generic user implementations require runtime dictionaries for `Encode` and `Decode`. On `main`, the prelude declares both traits with empty bodies. The type checker rejects custom implementations of both traits.

### `Encode` on `main`

On `main`, `json.encode` runs a single native encoder for all types because heap values store their own structure:

- Each heap object stores its kind, such as list, map, or struct.
- Each struct object stores the ID of its type definition.
- The VM type table maps each struct ID to its field identifiers.

The native encoder walks heap values directly from these runtime tags. At compile time, the checker checks `T: Encode` to reject functions, tasks, and channels. Code generation then erases the bound.

### `Decode` on `main`

A decoder starts from raw text and must learn the target type from its caller. On `main`, `json.decode<reflect T: Decode>` receives a runtime type descriptor through its `reflect` type parameter. That hidden argument restores the type structure that type erasure removed.

### Why custom implementations require dictionaries

Supporting generic user implementations requires trait dictionaries:

1. Users can write custom `Encode` implementations.
2. The native encoder must call custom methods for matching parts of a value.
3. Heap values omit type arguments. See [Why heap values omit type arguments](#why-heap-values-omit-type-arguments).
4. A generic implementation such as `impl<T: Encode> Encode for Box<T>` needs `T`'s `Encode` implementation at runtime.
5. Callers pass `T`'s implementation through a hidden trait dictionary.

Generic user implementations, rather than trait methods alone, create the need for dictionaries.

## Why heap values omit type arguments

Sand omits runtime type arguments from heap values to avoid three costs. Recording type arguments on heap values would otherwise let the native encoder find generic implementations without dictionaries.

### Cost 1: Every generic call would pass runtime type arguments

Type erasure lets generic functions run without hidden type descriptors. Consider this generic function:

```sand
fn empty<T>() -> [T] { [] }
```

The compiler compiles `empty` once across all `T`. To record the element type inside the new list, `empty` would need `T` at runtime. Every caller would then pass a runtime descriptor for `T`. Every generic function that builds a value would take hidden type descriptors.

On `main`, generic calls pass hidden arguments only for bounds on traits with methods. Calling `fn identity<T>(value: T) -> T` passes only `value`.

### Cost 2: Immediate values live outside the heap

The VM stores small values inline in a 16-byte value slot instead of allocating a heap object:

- `None`
- Payload-less enum variants such as `Color.Red`
- Small-payload variants such as `Some(3)`

An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. To record type arguments on values, the VM would have to allocate a heap object for every `None`. Most Sand programs use `Option`, so heap allocation for `None` would add large overhead.

### Cost 3: Extra memory per heap object

Storing an interned type-argument ID in every heap header would add about 4 bytes per object. This memory cost is smaller than Costs 1 and 2.

### Choices in other languages

Other languages trade off type erasure and runtime metadata in four ways. We did not audit the source code of these languages:

- Java erases type arguments, as Sand does.
- C# and .NET store full generic type arguments on every object and pass runtime type metadata to generic code.
- Swift passes type metadata to generic functions as hidden arguments.
- Rust and C++ compile a separate copy of each generic function per concrete type. They therefore keep concrete types throughout compilation.

### Why dictionaries fit Sand

Dictionaries limit runtime metadata overhead to functions with `Encode` or `Decode` bounds. Storing type arguments on heap values would impose Cost 1 on every generic function, even in code that never encodes values. Sand's `reflect` modifier already makes runtime type descriptors opt-in per type parameter. Heap type tags would put that cost on every type parameter instead.

## Limits of the earlier draft

The earlier draft looked up custom implementations from runtime value types. That design needed four special cases and blocked generic implementations:

- **Type checker dictionary exception.** The checker skipped dictionaries for `Encode` and `Decode` even though both traits declared methods.
- **Checker type-definition slots.** Each type definition in the checker recorded custom `encode` and `decode` methods.
- **VM type-table slots.** Each struct and enum runtime record stored two extra fields for custom `encode` and `decode` methods.
- **Concrete-type rule.** The checker allowed custom `Encode` and `Decode` implementations only on non-generic structs and enums. The method slots could not hold generic type arguments.

Looking up implementations through standard dictionaries removes all four special cases.

## Part 1: Native default methods

The prelude declares `Encode` and `Decode` with native default methods:

```sand
pub trait Encode {
    /// Writes `self` to `encoder` as a single value with `encoder.encode(value)`.
    native fn encode(self, encoder: Encoder)
}

pub trait Decode: Encode {
    /// Reads a value of `Self` from `decoder` with `decoder.decode()`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

On `main`, every trait method receives `Self`'s dictionary as its first hidden argument. A native default method therefore receives the active implementation's dictionary. It reads that dictionary to find the implementation of each part.

## Part 2: Structural implementations

A structural implementation inherits the native default methods of its trait. The prelude and the type checker create structural implementations:

- **Explicit prelude implementations.** The prelude declares empty-body implementations for built-in types:

  ```sand
  impl Encode for int {}
  impl<T: Encode> Encode for [T] {}
  impl<K: Encode, V: Encode> Encode for [K: V] {}
  ```

- **Compiler-derived implementations.** The type checker generates a structural implementation for each eligible struct or enum. Its existing field-bound pass selects the type parameter bounds. For example, `struct Pair<A, B> { first: A, second: [B] }` gets `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.

If a user writes a custom implementation for a type, the checker skips deriving that trait for the type.

The checker registers derived implementations as ordinary implementations. Standard implementation lookup therefore resolves `Encode` and `Decode` bounds directly.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries store extra metadata

Native default methods must look up each part's implementation through dictionaries. On `main`, dictionaries store bound dictionaries only for generic type parameters. Concrete field types have zero bound entries in a standard dictionary. For example, `struct User { email: Email }` has zero type parameters, so a standard `User` dictionary omits the `Encode` dictionary for `Email`.

### Implementation shape

The compiler emits a constant `ImplShape` for each structural implementation. It places `impl_shape` right after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., impl_shape, bound dictionaries...]
```

Ordinary trait method calls read only the method closures and supertrait dictionaries. Non-structural dictionaries therefore keep their existing layout. Only native default methods read `impl_shape`.

Each `ImplShape` holds two fields:

- **Type kind.** The kind tells the walker how to represent the type. Struct kinds also store field identifiers and field defaults. Native value kinds also store their text parser.
- **Part templates.** This list holds a `PartTemplate` for each part so the walker can build that part's dictionary.

### Part templates

`PartTemplate` has three variants:

```rust
enum PartTemplate {
    /// Program constant index, such as a constant dictionary.
    Constant(u32),
    /// Bound dictionary index in the enclosing dictionary.
    Bound(u32),
    /// Dictionary tuple built from element templates.
    Tuple(Vec<PartTemplate>),
}
```

The walker interprets each variant as follows:

- **Concrete field:** In `impl Encode for User` with field `email: Email`, the walker reads `Constant(index)` and loads `Email`'s constant `Encode` dictionary.
- **Direct type parameter:** In `impl<T: Encode> Encode for [T]`, the walker reads `Bound(0)` to get `T`'s dictionary from bound slot `0`.
- **Nested generic field:** In `impl<T: Encode> Encode for Pair<T>` with field `items: [T]`, the walker reads `Tuple([list_method, list_shape, Bound(0)])` and builds the `[T]` dictionary from `T`'s bound dictionary.

### How the compiler builds part templates

The compiler lowers `PartTemplate` values from the checker's existing trait-evidence trees. For method bodies on `main`, the checker builds an expression tree of dictionary tuples, bound reads, and constant loads. Code generation compiles that tree to bytecode on `main`. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same expression tree. The compiler then converts that tree into a `PartTemplate`.

### Worked example

Part templates let the native encoder reach custom methods across erased type arguments. Type erasure removes `T = Email` from a `Page<Email>` heap value, so only dictionaries connect `page` to `Email.encode`.

Consider this program:

```sand
struct Email { address: str }

impl Encode for Email {
    fn encode(self, encoder: Encoder) { encoder.encode(self.address) }
}

struct Page<T> { items: [T] }

fn save<T: Encode>(page: Page<T>) -> str { json.encode(page) }

save(Page(items: [Email(address: "a@b.c")]))
```

`Email` defines a custom `Encode` implementation. `Page<T>` uses a derived structural implementation. Here, the native encoder is the native default method `Encode.encode`:

- **Step 1: Incoming bound dictionary.** `save` requires `T: Encode`, so the caller passes `Email`'s `Encode` dictionary to `save`. That dictionary is a single-item tuple that holds `Email.encode`.
- **Step 2: Runtime `Page<T>` dictionary.** `save` calls `json.encode(page)`, which needs the `Encode` dictionary for `Page<T>`. Because `T` is generic, `save` builds a 3-item tuple at runtime:
  1. The native encoder method.
  2. The constant `ImplShape` for `Page`.
  3. `T`'s `Encode` dictionary from Step 1. In this call, it is `Email`'s dictionary.
- **Step 3: `Page` part template.** `Page`'s `ImplShape` holds a `PartTemplate` for `items`. That template tells the walker to build a 3-item `[T]` dictionary tuple:
  1. The native encoder method.
  2. The constant `ImplShape` for lists.
  3. Item 3 of the enclosing `Page` dictionary.
- **Step 4: Runtime walk.**
  1. `json.encode` calls the native encoder with `page` and the `Page` dictionary.
  2. The native encoder reads item 2 of the `Page` dictionary to get `Page`'s `ImplShape`.
  3. At `items`, the native encoder runs `items`'s part template to build the list dictionary. It copies `Email`'s dictionary from item 3 of the `Page` dictionary into item 3 of the list dictionary.
  4. Item 1 of the list dictionary is the native encoder itself. The walk therefore stays in Rust.
  5. At the first list element, the list `ImplShape` selects item 3 of the list dictionary. That item is `Email`'s dictionary.
  6. Item 1 of `Email`'s dictionary is the custom Sand method `Email.encode`. The walker calls `Email.encode` through a callback.

## Part 4: Recursive types

### Cyclic dictionary references

Recursive types create cycles between dictionaries and implementation shapes. Consider `struct Tree { children: [Tree] }`. `Tree`'s `ImplShape` refers to the `[Tree]` dictionary. The `[Tree]` dictionary stores `Tree`'s dictionary as a bound. `Tree`'s dictionary stores `Tree`'s `ImplShape`.

### Building part dictionaries on demand

The walker breaks dictionary cycles by building each part's dictionary only if the walk visits that part:

- **Constant dictionaries.** `PartTemplate::Constant` stores a constant-pool index rather than an inline value. The VM reads that constant slot during the walk. Before compiling an `ImplShape`'s part templates, the compiler reserves the shape's constant index. Recursive references reuse that reserved index. Constant values therefore stay acyclic.
- **Runtime dictionaries.** For generic `Tree<T>`, `children` uses the finite template `Tuple([list_method, list_shape, Tuple([tree_method, tree_shape, Bound(0)])])`. The walker builds `[Tree<T>]`'s dictionary at `children`. It then builds each child `Tree<T>` dictionary as it visits that child.
- **Polymorphic recursion.** A generic type can recurse with a nested type argument, such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. `Deep`'s template builds `Nest<[T]>`'s dictionary from `T`'s dictionary at each `Deep` layer. The runtime depth of the value limits dictionary construction.

### Per-walk dictionary cache

During a walk, the walker caches part dictionaries by template and parent dictionary. Encoding a list of 10,000 `Tree<int>` values therefore builds `Tree<int>`'s dictionary once.

### Rejected recursion schemes

We rejected two eager or callback-based schemes:

- **Store part dictionaries inside every dictionary.** Recursive types form cyclic tuples. On polymorphic recursion, the compiler would expand nested dictionaries forever.
- **Generate a Sand function that returns part dictionaries.** Each uncached dictionary lookup would need a ~90 ns Sand callback. Derived types without custom methods would also pay that cost.

Other languages use lazy dictionary construction as well. We did not audit the Swift or GHC source code:

- Swift builds and caches generic witness tables on demand at runtime.
- GHC builds recursive typeclass dictionaries as lazy thunks.

## Part 5: Native walkers

### Encode walker

The encode walker copies a heap value into a host `Value`. Instead of recursing on the Rust call stack, it keeps a `Vec` stack of open containers and their dictionaries. For each part, the walker builds the part dictionary from its `PartTemplate` and checks its method:

- **Native default method.** The walker copies the part directly in Rust.
- **Custom Sand method.** The walker writes a placeholder into `Value`. It also records a hole with the target `Value` slot, the heap part, and the part dictionary.

After the structural pass, the native function runs a callback for each hole. Each custom `encode` method writes a value to an `Encoder`. The walker encodes that value into the hole's slot.

### Decode walker

The decode walker builds a heap value from a `Value` according to the type kind in each `ImplShape`. The walker records a hole in two cases:

- A part whose type defines a custom `Decode` implementation.
- A missing struct field with a computed default expression, such as `id: int = next_id()`.

If the walker records any holes, it runs in two passes:

1. Pass 1 validates and decodes the input while recording all holes.
2. The native function runs callbacks for all holes and saves their heap results.
3. Pass 2 decodes the input again and inserts the saved results at each hole.

Two-pass decoding keeps two guarantees. First, computed field defaults run in post-order after the walker validates the whole input, as on `main`. Second, sets and maps insert finished values with their final hashes.

### Synchronous fast path

If a walk records zero holes, the native function returns right away. It skips allocating a future and skips callbacks. Returning synchronously saves the ~112 ns cost of polling a ready async native future.

## Part 6: Type checker and compiler changes

1. **Native default methods.** Trait declarations accept `native fn` methods. On `main`, only `impl` blocks accept `native fn`.
2. **Ordinary derived implementations.** The checker registers derived implementations as ordinary structural implementations, as described in Part 2. The checker keeps both existing derivation exclusions: it skips `Hash` for structs with `var` fields, and it skips `Decode` for structs with `init` constructors.
3. **Part templates.** For each structural implementation, the checker builds trait-evidence trees for all parts. The compiler lowers those trees to `PartTemplate` constants.
4. **Structural implementations for `never`.** Under SPEC §5.1 on `main`, `never` implements only traits without methods. Expressions such as `json.encode([])` require `[never]: Encode`. In this design, `never` gets a structural implementation with shape kind `Never` for each derived trait. Its native default `decode` always returns an error. Because `never` has zero runtime values, its native default `encode` never runs.
5. **Trait dictionary parameters for native functions.** Generic native functions such as `json.encode<T: Encode>` receive `T`'s dictionary through a new `TraitDict<N>` parameter wrapper. `TraitDict<N>` reads the `N`-th method-bearing bound dictionary, just as `Num<N>` reads `Num` dictionaries.
6. **Removal of `reflect`.** `Decode` dictionaries include `ImplShape` metadata, which replaces `reflect` type descriptors in generic decoders. On `main`, only `json.decode` uses `reflect` in the standard library. We therefore delete the `reflect` feature.

Two existing restrictions on traits with methods now apply to `Encode` and `Decode`:

- **Local functions.** Local functions cannot require traits with methods. The checker rejects local `Encode` or `Decode` bounds with `local functions cannot require traits with methods`.
- **Struct field defaults.** Struct field default expressions cannot require traits with methods under SPEC §4.2.

See [Open question 2](#open-questions) on keeping both restrictions in this branch.

## Part 7: JSON and `Value`

- **JSON parsing.** The JSON parser converts JSON text to a `Value`. It maps JSON objects to string-keyed maps and JSON `null` to `Value::Unit`.
- **JSON writing.** The JSON writer formats a `Value` as JSON text under three rules:
  - It writes `Some(inner)` directly as `inner`.
  - It writes `bytes` values as Base64 strings.
  - It writes map keys and native value types as strings.
- **JSON-compatible decoding.** The decode walker accepts these JSON forms directly. For example, it accepts Base64 strings for `bytes` and string keys for `int`-keyed maps. Any `Value` parsed from valid JSON text is valid input for matching `Decode` types.
- **`json.Json` implementations.** `Json` implements `Encode` and `Decode` with dedicated native methods:

  ```sand
  impl Encode for Json { native fn encode(self, encoder: Encoder) }
  impl Decode for Json { native fn decode(decoder: Decoder) -> Result<Json> }
  ```

  Each method converts a full `Json` tree in a single call between `Json` and `Value`. `json.parse(text)` calls `json.decode<Json>(text)`.
- **`Vm::to_value` and `NativeContext::to_value`.** `Vm::to_value` takes an `Encode` dictionary alongside the heap value. `Vm::to_value` is synchronous and cannot run callbacks. If it reaches a custom `Encode` implementation, it fails with error kind `"invalid"`. A new async `NativeContext::to_value` method runs custom `Encode` implementations.

## Updates to `SPLIT.md` tasks 14, 15, and 18

`SPLIT.md` divides the work of a source branch into numbered changesets:

- **Task 14: `Value` and `Vm::to_value`.** Pull request #41 implemented Task 14. This design updates two of its choices:
  - `Vm::to_value` takes an `Encode` dictionary so it can detect custom `Encode` implementations.
  - The encode walker replaces the three separate heap walkers on `main`: `json.encode`, `json.to_json`, and `Vm::to_value`. `json.encode` writes its `Value` output as JSON text. `json.to_json` converts its `Value` output to `Json`.
- **Task 15: `Encode` and `Decode` as ordinary traits.** Task 15 takes this new title and covers Parts 1 through 7 of this design:
  - `json.decode` parses JSON text to a `Value` before running the decode walker.
  - Task 15 drops a separate `value::Decode` Rust trait. Instead, Task 16 implements `serde` traits on `Value` so hosts can decode into Rust types.
- **Task 18: Host function calls.** Task 18 uses the decode walker to convert each host `Value` argument. Host-callable script functions take concrete parameter types. The compiler therefore emits a constant `Decode` dictionary for each parameter.

## Costs and risks

- **Dictionary passing.** Generic calls with `T: Encode` bounds pass a dictionary tuple. For concrete types, callers pass a program constant. For generic types, callers build a tuple at runtime. The triage report measured generic trait calls at about 12 ns above direct calls.
- **Per-part dispatch.** For each part, the walker gets a part dictionary and checks its method pointer. Concrete types use `Constant` templates, so the walker reads program constants directly.
- **Intermediate `Value` allocation.** `json.encode` builds a `Value` tree before writing JSON text. On `main`, `json.encode` writes JSON text directly from heap values. We will benchmark `json.encode` to measure any slowdown.
- **Memory accounting across callbacks.** The walkers count `Value` memory against `max_memory` during each structural pass. During a Sand callback, the VM omits the in-progress `Value` tree from `max_memory`. `TODO.md` already tracks this issue class under "Charge boxed recursive futures against `max_memory`."
- **Net codebase size.** We expect total code size to shrink. The implementation pull request will report the measured line delta.

  This design deletes:
  - `compile/reflect.rs` and `reflect` support in the parser and checker
  - Runtime type descriptors and their templates
  - The typed JSON decoder, direct JSON text encoder, and `json.to_json` converter
  - Post-decode helper functions for computed field defaults

  This design adds:
  - The native encode and decode walkers
  - `ImplShape` constants and `PartTemplate` lowering
  - Native default trait methods
  - Structural implementations for built-in types and `never`
  - The `TraitDict<N>` native parameter wrapper

## Rejected alternatives

- **Runtime lookup table keyed by trait and type.** A runtime table duplicates dictionary lookup. Because the compiler erases type arguments, runtime value lookup fails on generic implementations. Decoding also starts from raw input and would still need runtime type descriptors.
- **Generated Sand methods for all derived implementations.** Swift uses this single-path model, where generated code calls a trait method for every part. The triage report measured Sand-only codecs at 2.3 to 7 times slower than native walkers.
- **Dual generated Sand codecs and native fast paths.** Two codec paths must keep identical output, error paths, and retry behavior. On `origin/encode-decode-traits`, the triage report found that at least a quarter of its 8,210 new production lines kept the two paths in sync.

## Reusable parts from the earlier draft

From the uncompiled draft on `encode-decode-traits-v2`, this design keeps three components:

- **The encode and decode walkers** in `codec/encode.rs` and `codec/decode.rs`. These files implement holes, two-pass decoding, and JSON error paths such as `$.items[2].price`. This design changes only how the walkers look up implementations.
- **The `Value`-based JSON parser and writer**, along with the native `Json` implementation methods.
- **Computed field defaults as decode holes.** This change replaces the post-decode helper functions on `main`.

This design drops all four special cases listed in [Limits of the earlier draft](#limits-of-the-earlier-draft).

## Test and measurement plan

- **Existing JSON behavior.** Keep existing test output in `tests/spec/std_json.test`. List every intentional error-message change in this document.
- **New spec tests:**
  - Custom `Encode` and `Decode` implementations on structs and enums
  - Generic custom implementations such as `impl<T: Encode> Encode for Box<T>`
  - Direct recursive types and polymorphic recursion
  - `json.encode([])` with `[never]: Encode`
  - Nested types where both outer and inner types define custom implementations
  - Async custom implementations that wait on `task.sleep`
  - Malformed `Encode` implementations that skip `encoder.encode` or call it twice
  - Error paths from failed custom `Decode` implementations
- **Robustness checks:**
  - Run tests with `Config::retry_stress` enabled to force limit-check retries.
  - Compare process resident memory against `max_memory` and against equivalent Sand loops on large inputs.
  - Encode and decode deeply nested values on a 512 KiB thread stack.
  - Encode values with many custom-implementation holes.
- **Benchmarks:**
  - Benchmark `json` codec functions against `main` with `bench/run.py` and `benches/codec.rs`.
  - Measure call overhead for generic functions with `Encode` bounds.

## Open questions

1. **How should traits opt into compiler derivation?** We propose a `derive` modifier on trait declarations, such as `pub derive trait Encode { ... }`, instead of matching trait identifiers in the checker. The checker keeps both existing derivation exclusions for now: it skips `Hash` for structs with `var` fields, and it skips `Decode` for structs with `init` constructors. Alternatively, this branch can keep identifier-based recognition.
2. **Should this branch keep the trait-method restrictions on local functions and struct field defaults?** We recommend keeping both restrictions in this branch. The checker will then reject local functions that require `Encode` or `Decode`, as it does for other traits with methods.
3. **Should this branch delete `reflect`?** We recommend deleting `reflect` in this branch. Deleting `reflect` leaves a single source of runtime type metadata for decoding. It also removes substantial compiler and VM code.
