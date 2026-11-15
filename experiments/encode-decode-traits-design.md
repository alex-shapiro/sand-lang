# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** This document proposes an unimplemented design. An earlier code draft on `encode-decode-traits-v2` fails to compile. [Reusable parts from the earlier draft](#reusable-parts-from-the-earlier-draft) lists the code from that draft that this design keeps.

## Summary

This design makes `Encode` and `Decode` ordinary traits with native default methods. Users can write custom implementations for both traits, including generic implementations such as `impl<T: Encode> Encode for Box<T>`. The compiler erases type arguments at compile time. Callers therefore pass trait dictionaries to generic implementations as hidden runtime arguments. [Why `Encode` and `Decode` need dictionaries](#why-encode-and-decode-need-dictionaries) explains this change from `main`.

Each trait method uses a Rust native default that walks values directly in Rust. The compiler derives `Encode` and `Decode` for qualifying structs and enums as ordinary implementations that inherit these native defaults. Defining a custom implementation for a type replaces its compiler-derived implementation.

During a walk, the native default looks up each part's implementation through dictionaries rather than runtime value types. Parts that use the native default stay in Rust. Parts with custom implementations invoke user Sand methods through callbacks.

The VM treats `Encode` and `Decode` like any other trait with native default methods. Other derived traits such as `Eq`, `Ord`, and `Hash` can adopt the same mechanism later.

This design meets four goals:

- **Host `Value` conversion.** The encode walker converts a Sand heap value to a `Value`. The decode walker converts a `Value` to a Sand heap value.
- **JSON encoding and decoding.** Plain Rust functions convert between JSON text and `Value`.
- **Ordinary trait rules.** Custom implementations, generic implementations, and trait bounds follow SPEC §4.9.
- **Standard `json.Json` handling.** `Json` implements `Encode` and `Decode` as ordinary implementations. The JSON writer formats `Json` through those implementations.

## Terms

Each term below keeps a single meaning throughout this document:

- **Trait evidence.** A generic function such as `fn save<T: Encode>(item: T)` requires the `Encode` implementation for `T`. Because the compiler erases type arguments, callers pass implementation metadata as a hidden runtime argument called trait evidence.
- **Dictionary.** On `main`, trait evidence for a trait with methods is a tuple called a dictionary. Each dictionary stores three groups of items in order:
  1. Method closures of the implementation.
  2. Supertrait dictionaries. For `Decode: Encode`, this group stores the `Encode` dictionary for the same type.
  3. Bound dictionaries of the implementation. For `impl<T: Show> Show for [T]`, this group stores the `Show` dictionary for `T`.

  For a trait without methods, such as `Hash` on `main`, the compiler checks the bound at compile time. It omits a runtime dictionary.
- **Constant dictionary.** If every type in a dictionary is concrete, the compiler emits the dictionary once as a program constant. For example, `[int]: Show` uses a constant dictionary.
- **Runtime dictionary.** Inside generic code, a dictionary can depend on a type parameter's dictionary. Generated code then builds the dictionary tuple at runtime, such as `[T]: Show` inside `fn print_all<T: Show>(items: [T])`.
- **Derived trait.** The compiler implements derived traits automatically for structs and enums whose fields and variant payloads implement the trait. On `main`, users cannot write custom implementations for derived traits.
- **Native function.** A native function is a Rust function that Sand code calls.
- **Native default method.** This design adds native default methods to traits. A native default method uses a native Rust function as its default body. Any implementation that omits the method body inherits the native default.
- **Part.** A part is a direct child value inside a composite or container value:
  - Struct parts are field values.
  - Enum parts are variant payload values.
  - List and set parts are elements.
  - Map parts are keys and values.
- **Walker.** A walker is a native function that visits a value and its parts in turn. The encode walker reads a value from the VM heap. The decode walker builds a value on the VM heap.
- **`Value`.** `sandlang::value::Value` is a host-owned Rust type that stores a copy of a Sand value outside the VM. A Rust host can retain a `Value` after a VM run ends.
- **Callback.** Commit `8a51c46` lets an async native function call a Sand function. This document calls that nested Sand call a callback. Each callback round trip takes about 90 ns, as reported in `experiments/native-async-sand-calls.md`.
- **Triage report.** The document `experiments/encode-decode-design-causes.md` on `encode-decode-design-causes` analyzes why earlier codec designs were slow or complex.

## Why `Encode` and `Decode` need dictionaries

Generic user implementations require runtime dictionaries for `Encode` and `Decode`. On `main`, the prelude declares both traits with empty bodies. The type checker rejects custom implementations of both traits.

### `Encode` on `main`

On `main`, `json.encode` runs a single native encoder for all types because heap values store their own structure:

- Each heap object stores its kind, such as list, map, or struct.
- Each struct object stores the ID of its type definition.
- The VM type table maps each struct ID to its field identifiers.

The native encoder walks heap values directly from these runtime tags. At compile time, the checker verifies `T: Encode` to reject functions, tasks, and channels. Code generation then erases the bound.

### `Decode` on `main`

A decoder starts from raw text and must learn the target type from its caller. On `main`, `json.decode<reflect T: Decode>` receives a runtime type descriptor through its `reflect` type parameter. That hidden argument restores the type structure that type erasure removed.

### Why custom implementations require dictionaries

Supporting generic user implementations requires trait dictionaries:

1. Users can write custom `Encode` implementations.
2. The native encoder must call custom methods for matching parts of a value.
3. Heap values omit type arguments, as explained in [Why heap values omit type arguments](#why-heap-values-omit-type-arguments).
4. A generic implementation such as `impl<T: Encode> Encode for Box<T>` requires `T`'s `Encode` implementation at runtime.
5. Callers provide `T`'s implementation through a hidden trait dictionary.

Generic user implementations, rather than trait methods alone, create the need for dictionaries.

## Why heap values omit type arguments

Sand omits runtime type arguments from heap values to avoid three costs. Recording type arguments on heap values would otherwise let the native encoder resolve generic implementations without dictionaries.

### Cost 1: Every generic call would pass runtime type arguments

Erasing type arguments lets generic functions run without hidden type descriptors. Consider this generic function:

```sand
fn empty<T>() -> [T] { [] }
```

The compiler compiles `empty` once across all `T`. Recording the element type inside the new list would require every caller to pass a runtime descriptor for `T`. Every generic function that constructs a value would then take hidden type descriptors.

On `main`, generic calls pass hidden arguments only for bounds on traits with methods. Calling `fn identity<T>(value: T) -> T` passes only `value`.

### Cost 2: Immediate values live outside the heap

The VM stores small values inline in a 16-byte value slot without allocating a heap object:

- `None`
- Payload-less enum variants such as `Color.Red`
- Small-payload variants such as `Some(3)`

An inline `None` of type `Email?` shares the exact bit pattern of an inline `None` of type `int?`. Recording type arguments on values would force heap allocation for every `None`. Most Sand programs use `Option`, so that change would add heavy allocation overhead.

### Cost 3: Extra memory per heap object

Storing an interned type-argument ID in every heap header would add about 4 bytes per object. This memory cost is smaller than Costs 1 and 2.

### Choices in other languages

Other languages trade off type erasure and runtime metadata in four ways, based on general language knowledge rather than a source audit:

- Java erases type arguments, as Sand does.
- C# and .NET store full generic type arguments on every object and pass runtime type metadata to generic code.
- Swift passes type metadata to generic functions as hidden arguments.
- Rust and C++ compile a separate copy of each generic function per concrete type, avoiding type erasure.

### Why dictionaries fit Sand

Dictionaries limit runtime metadata overhead to functions with `Encode` or `Decode` bounds. Storing type arguments on heap values would impose Cost 1 on every generic function, even in code that never encodes values. Sand's `reflect` modifier already makes runtime type descriptors opt-in per type parameter. Heap type tags would impose that cost on every type parameter instead.

## Limits of the earlier draft

The earlier draft resolved custom implementations from runtime value types. That approach required three special cases and blocked generic implementations:

- **Type checker exception.** `Encode` and `Decode` declared methods while skipping dictionaries through a hardcoded trait check.
- **VM type table slots.** Each struct and enum runtime record stored two extra fields for custom `encode` and `decode` methods.
- **Monomorphic implementation rule.** Custom `Encode` and `Decode` implementations worked only on non-generic structs and enums. The two method slots could not store generic type arguments.

Resolving implementations through standard dictionaries removes all three special cases.

## Part 1: Native default methods

The prelude declares `Encode` and `Decode` with native default methods:

```sand
pub trait Encode {
    /// Writes `self` to `encoder` as a single value with `encoder.encode(value)`.
    native fn encode(self, encoder: Encoder)
}

pub trait Decode: Encode {
    /// Reads a value of `Self` from `decoder`, typically with `decoder.decode()`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

On `main`, every trait method already receives `Self`'s dictionary as its first hidden argument. A native default method therefore receives the active implementation's dictionary and reads it to find part implementations.

## Part 2: Structural implementations

A structural implementation inherits the native default methods of its trait. Structural implementations come from two sources:

- **Explicit prelude implementations.** The prelude declares empty-body implementations for built-in types:

  ```sand
  impl Encode for int {}
  impl<T: Encode> Encode for [T] {}
  impl<K: Encode, V: Encode> Encode for [K: V] {}
  ```

- **Compiler-derived implementations.** The type checker generates a structural implementation for each qualifying struct or enum. Its existing field-bound analysis selects the type parameter bounds. For example, `struct Pair<A, B> { first: A, values: [B] }` receives `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.

Defining a custom implementation for a type suppresses automatic derivation of that trait for the type.

The type checker registers derived implementations as ordinary implementations. Standard implementation lookup therefore resolves `Encode` and `Decode` bounds without a separate derive-lookup path.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries store extra metadata

Native default methods must look up each part's implementation through dictionaries. Standard dictionaries on `main` store bound dictionaries only for generic type parameters. Concrete field types therefore lack entries in a standard dictionary. For example, `struct User { email: Email }` takes zero type parameters, so a standard `User` dictionary omits the `Encode` dictionary for `Email`.

### Implementation shape

The compiler emits a constant `ImplShape` for each structural implementation and places it right after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., impl_shape, bound dictionaries...]
```

Ordinary trait method calls read only the method closures and supertrait dictionaries. Non-structural dictionaries therefore keep their existing layout. Only native default methods read `impl_shape`.

Each `ImplShape` stores two fields:

- **Type kind.** Tells the walker how to represent the type, such as a primitive, container, composite type, native value type, or `never`. Struct kinds also store field identifiers and default values. Native value kinds also store their text-parsing function.
- **Part templates.** A `PartTemplate` for each part tells the walker how to build that part's dictionary.

### Part templates

A `PartTemplate` has three variants:

```rust
enum PartTemplate {
    /// Program constant at `constant_index`, such as a constant dictionary.
    Const(u32),
    /// Bound dictionary at `bound_index` in the enclosing dictionary.
    Bound(u32),
    /// Dictionary tuple built recursively from element templates.
    Tuple(Vec<PartTemplate>),
}
```

Three examples illustrate these variants:

- **Concrete field:** In `impl Encode for User` with field `email: Email`, `Const(constant_index)` loads the constant `Encode` dictionary for `Email`.
- **Direct type parameter:** In `impl<T: Encode> Encode for [T]`, `Bound(0)` reads `T`'s dictionary from bound slot `0`.
- **Nested generic field:** In `impl<T: Encode> Encode for Pair<T>` with field `items: [T]`, `Tuple([list_method, list_shape, Bound(0)])` constructs the `[T]` dictionary from `T`'s bound dictionary.

### How the compiler builds part templates

The compiler lowers `PartTemplate` values from the type checker's existing trait-evidence trees. For method bodies on `main`, the checker builds an expression tree of dictionary constructors, bound projections, and constant loads. Code generation compiles that tree to bytecode on `main`. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same expression tree. The compiler then converts that tree into a `PartTemplate`.

### Worked example

The program below traces runtime dictionaries and each step of the native encoder:

```sand
struct Email { address: str }

impl Encode for Email {
    fn encode(self, encoder: Encoder) { encoder.encode(self.address) }
}

struct Page<T> { items: [T] }

fn save<T: Encode>(page: Page<T>) -> str { json.encode(page) }

save(Page(items: [Email(address: "a@b.c")]))
```

`Email` defines a custom `Encode` implementation. `Page<T>` uses a compiler-derived structural implementation. Here, the native encoder is the native default method `Encode.encode`.

- **Step 1: Incoming bound dictionary.** Because `save` requires `T: Encode`, the caller passes `Email`'s `Encode` dictionary to `save`. That dictionary is a single-item tuple holding the custom method `Email.encode`.
- **Step 2: Runtime `Page<T>` dictionary.** Inside `save`, calling `json.encode(page)` requires the `Encode` dictionary for `Page<T>`. Because `T` is generic, `save` builds a 3-item tuple at runtime:
  1. The native encoder method.
  2. The constant `ImplShape` for `Page`.
  3. `T`'s `Encode` dictionary from Step 1, holding `Email`'s dictionary.
- **Step 3: `Page` part template.** `Page`'s `ImplShape` holds a `PartTemplate` for its `items` field. That template builds a 3-item `[T]` dictionary tuple:
  1. The native encoder method.
  2. The constant `ImplShape` for lists.
  3. Item 3 of the enclosing `Page` dictionary.
- **Step 4: Runtime walk.**
  1. `json.encode` calls the native encoder with `page` and the `Page` dictionary.
  2. The native encoder reads item 2 of the `Page` dictionary to get `Page`'s `ImplShape`.
  3. On reaching `items`, the native encoder evaluates `items`'s part template to build the list dictionary. Item 3 of that list dictionary receives `Email`'s dictionary.
  4. Item 1 of the list dictionary is the native encoder itself. The walk therefore continues in Rust.
  5. On reaching the first list element, the list `ImplShape` selects item 3 of the list dictionary, returning `Email`'s dictionary.
  6. Item 1 of `Email`'s dictionary is the custom Sand method `Email.encode`. The walker invokes `Email.encode` through a callback.

Type erasure removes `T = Email` from the `Page` heap value. Only dictionaries connect `page` to `Email.encode`. Part templates build each link of that chain during the walk: from the `Page` dictionary to the list dictionary, then to `Email`'s dictionary, and finally to `Email.encode`.

## Part 4: Recursive types

### Cyclic dictionary references

Recursive types create cycles between dictionaries and implementation shapes. Consider `struct Tree { children: [Tree] }`. `Tree`'s `ImplShape` references the `[Tree]` dictionary. The `[Tree]` dictionary stores `Tree`'s dictionary as a bound, and `Tree`'s dictionary stores `Tree`'s `ImplShape`.

### Building part dictionaries on demand

The walker breaks dictionary cycles by building each part's dictionary lazily only if the walk visits that part:

- **Constant dictionaries.** `PartTemplate::Const` stores a constant-pool index rather than an inline value. The VM dereferences that index during the walk. Before compiling an `ImplShape`'s part templates, the compiler reserves the shape's constant index. Recursive references reuse that reserved index without forming cyclic constants.
- **Runtime dictionaries.** For generic `Tree<T>`, `children` uses the finite template `Tuple([list_method, list_shape, Tuple([tree_method, tree_shape, Bound(0)])])`. The walker instantiates `[Tree<T>]`'s dictionary on visiting `children`. It instantiates each child `Tree<T>` dictionary on visiting that child.
- **Polymorphic recursion.** A generic type can recurse at a nested type argument, such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. `Deep`'s template constructs `Nest<[T]>`'s dictionary from `T`'s dictionary at each `Deep` layer. The value's runtime depth therefore bounds dictionary construction.

### Per-walk dictionary cache

During a walk, the walker caches resolved part dictionaries by `(part_template, parent_dictionary)`. Encoding a list of 10,000 `Tree<int>` values therefore builds `Tree<int>`'s dictionary once.

### Rejected recursion schemes

Two alternative approaches fail or add unnecessary overhead:

- **Store part dictionaries eagerly inside every dictionary.** Recursive types create cyclic tuples. Polymorphic recursion also expands infinitely at compile time.
- **Generate a Sand helper function that returns part dictionaries.** Each uncached dictionary lookup would require a ~90 ns Sand callback. That callback cost would slow down derived types as well.

Other languages use lazy dictionary construction as well, based on general language knowledge rather than a source audit. Swift builds and caches generic witness tables on demand at runtime. GHC builds recursive typeclass dictionaries as lazy thunks.

## Part 5: Native walkers

### Encode walker

The encode walker copies a heap value into a host `Value` without Rust stack recursion. Instead, it maintains an explicit `Vec` stack of open containers, and each entry retains its container dictionary. For each part, the walker resolves the part dictionary from its `PartTemplate` and checks the implementation method:

- **Native default method.** The walker copies the part directly in Rust.
- **Custom Sand method.** The walker writes a placeholder into `Value`. It also records a hole holding the target `Value` position, the heap part, and the part dictionary.

After finishing the structural pass, the native function runs a callback for each hole. Each custom `encode` method writes to an `Encoder`. The walker then encodes that output into the hole's placeholder slot.

### Decode walker

The decode walker constructs a heap value from a `Value` using the type kind in each `ImplShape`. Two cases record a hole:

- A part whose type defines a custom `Decode` implementation.
- A missing struct field with a computed default expression, such as `id: int = next_id()`.

If the input produces any holes, the decode walker runs in two passes:

1. Pass 1 validates and decodes the input while recording all holes.
2. The native function runs callbacks for all holes and keeps their resulting heap values.
3. Pass 2 decodes the input again and substitutes the callback results at each hole.

This two-pass order preserves two invariants. First, computed field defaults run in post-order after the walker validates the whole input, matching `main`. Second, sets and maps receive finalized values and hash their final contents.

### Synchronous fast path

For inputs without holes, the native function returns immediately without allocating a future or running callbacks. Returning synchronously avoids the ~112 ns cost of polling an immediately ready async native future.

## Part 6: Type checker and compiler changes

1. **Native default methods.** Trait declarations accept `native fn` methods. On `main`, only `impl` blocks accept `native fn`.
2. **Ordinary derived implementations.** The checker registers compiler-derived implementations as ordinary structural implementations, as described in Part 2. Existing derivation exclusions stay unchanged: structs with `var` fields skip `Hash`, and structs with `init` constructors skip `Decode`.
3. **Part templates.** For each structural implementation, the checker builds trait-evidence trees for all parts. The compiler lowers those trees to `PartTemplate` constants.
4. **Structural implementations for `never`.** Under SPEC §5.1 on `main`, `never` implements only traits without methods. Expressions such as `json.encode([])` require `[never]: Encode`. In this design, `never` receives a structural implementation with shape kind `Never` for each derived trait. Its native default `decode` always returns an error. Its native default `encode` is unreachable at runtime because `never` is uninhabited.
5. **Trait dictionary parameters for native functions.** Generic native functions such as `json.encode<T: Encode>` receive `T`'s dictionary through a new `TraitDict<N>` parameter wrapper. `TraitDict<N>` reads the `N`-th method-bearing bound dictionary, matching how `Num<N>` reads `Num` dictionaries.
6. **Removal of `reflect`.** `Decode` dictionaries include `ImplShape` metadata, replacing `reflect` type descriptors in generic decoders. On `main`, `json.decode` is the sole standard-library caller that uses `reflect`. Removing `reflect` deletes its syntax, checker pass, bytecode instruction, and runtime type descriptors.

Two existing restrictions on traits with methods now apply to `Encode` and `Decode`:

- **Local functions.** Local functions cannot bound type parameters by traits with methods. The checker rejects local `Encode` or `Decode` bounds with `local functions cannot require traits with methods`.
- **Struct field defaults.** Struct field default expressions cannot require traits with methods under SPEC §4.2.

[Open question 2](#open-questions) asks whether to keep both restrictions in this branch.

## Part 7: JSON and `Value`

- **JSON parsing.** The JSON parser converts JSON text to a `Value`. It maps JSON objects to string-keyed maps and JSON `null` to `Value::Unit`.
- **JSON writing.** The JSON writer formats a `Value` as JSON text under three representation rules:
  - It writes `Some(inner)` directly as `inner`.
  - It writes `bytes` values as Base64 strings.
  - It writes map keys and native value types as strings.
- **JSON-compatible decoding.** The decode walker accepts these JSON representations directly, including Base64 strings for `bytes` and string keys for `int`-keyed maps. Any `Value` parsed from valid JSON text is therefore valid input for matching `Decode` types.
- **`json.Json` implementations.** `Json` implements `Encode` and `Decode` with dedicated native methods:

  ```sand
  impl Encode for Json { native fn encode(self, encoder: Encoder) }
  impl Decode for Json { native fn decode(decoder: Decoder) -> Result<Json> }
  ```

  Each method converts a full `Json` tree in a single call between `Json` and `Value`. `json.parse(text)` delegates to `json.decode<Json>(text)`.
- **`Vm::to_value` and `NativeContext::to_value`.** `Vm::to_value` accepts an `Encode` dictionary alongside the heap value. Because `Vm::to_value` is synchronous and cannot run callbacks, reaching a custom `Encode` implementation fails with error kind `"invalid"`. A new async `NativeContext::to_value` method runs custom `Encode` implementations.

## Updates to `SPLIT.md` tasks 14, 15, and 18

`SPLIT.md` divides the embedding and codec roadmap into numbered tasks:

- **Task 14: `Value` and `Vm::to_value`.** Task 14 merged in pull request #41, and this design updates two of its decisions:
  - `Vm::to_value` takes an `Encode` dictionary so it can detect custom `Encode` implementations.
  - The encode walker replaces the three separate heap walkers on `main`: `json.encode`, `json.to_json`, and `Vm::to_value`. Both `json.encode` and `json.to_json` format its `Value` output.
- **Task 15: `Encode` and `Decode` as ordinary traits.** Task 15 adopts this new title and covers Parts 1 through 7 of this design:
  - `json.decode` parses JSON text to a `Value` before running the decode walker.
  - Task 15 omits a separate `value::Decode` Rust trait. Instead, Task 16 adds `serde` support on `Value` for converting to host Rust types.
- **Task 18: Host function calls.** Task 18 uses the decode walker to convert each host `Value` argument. Host-callable script functions take concrete parameter types, so the compiler emits a constant `Decode` dictionary for each parameter.

## Costs and risks

- **Dictionary passing.** Generic calls with `T: Encode` bounds pass a dictionary tuple. Concrete types pass a program constant, whereas generic callers build a tuple at runtime. The triage report measured generic trait calls at about 12 ns above direct calls.
- **Per-part dispatch.** For each part, the walker resolves a part dictionary and compares its method pointer. Concrete types use `Const` templates, so the walker reads program constants directly without allocating tuples.
- **Intermediate `Value` allocation.** `json.encode` builds a `Value` tree before formatting JSON text. On `main`, `json.encode` writes JSON text directly from heap values. Benchmarks will measure any slowdown in `json.encode`.
- **Memory accounting across callbacks.** The walkers count `Value` allocations against `max_memory` during each structural pass. While suspended in a Sand callback, the VM omits the in-progress `Value` tree from `max_memory`. `TODO.md` already tracks this class of issue under "Charge boxed recursive futures against `max_memory`."
- **Net codebase size.** Total line count should decrease. The implementation pull request will report the measured line delta.

  This design deletes:
  - `compile/reflect.rs` and `reflect` support in the parser and checker
  - Runtime type descriptors and their templates
  - The standalone typed JSON decoder, direct JSON text encoder, and `json.to_json` converter
  - Post-decode helper functions that fill computed field defaults

  This design adds:
  - The native encode and decode walkers
  - `ImplShape` constants and `PartTemplate` lowering
  - Native default trait methods
  - Structural implementations for built-in types and `never`
  - The `TraitDict<N>` native parameter wrapper

## Rejected alternatives

- **Runtime lookup table keyed by `(Trait, Type)`.** Adding a runtime table duplicates dictionary lookup. Because the compiler erases type arguments, runtime value lookup cannot resolve generic implementations. Decoding also starts without a heap value and would still require runtime type descriptors.
- **Compiler-generated Sand methods for all derived implementations.** Swift uses this single-path model, where every part runs a compiled trait call. The triage report measured Sand-only codecs at 2.3 to 7 times slower than native walkers.
- **Dual generated Sand codecs and native fast paths.** Maintaining two codec paths requires identical output, error paths, and retry behavior across both engines. On `origin/encode-decode-traits`, the triage report found that at least a quarter of its 8,210 new production lines kept the two paths consistent.

## Reusable parts from the earlier draft

From the uncompiled draft on `encode-decode-traits-v2`, this design keeps three components:

- **The encode and decode walkers** in `codec/encode.rs` and `codec/decode.rs`. That code includes holes, two-pass decoding, and JSON error paths such as `$.items[2].price`. Only implementation lookup changes to use dictionaries.
- **The `Value`-based JSON parser and writer**, along with the native `Json` implementation methods.
- **Computed field defaults as decode holes**, replacing the post-decode helper functions on `main`.

This design discards four special cases from the earlier draft:

- Custom `encode` and `decode` slots on VM struct and enum type records.
- Custom codec method tracking on type definitions in the type checker.
- The type checker exception that omitted dictionaries for `Encode` and `Decode`.
- The rule restricting custom implementations to non-generic structs and enums.

## Test and measurement plan

- **Existing JSON behavior.** Preserve existing test output in `tests/spec/std_json.test`. List every intentional error-message change in this document.
- **New spec tests:**
  - Custom `Encode` and `Decode` implementations on structs and enums
  - Generic custom implementations such as `impl<T: Encode> Encode for Box<T>`
  - Direct recursive types and polymorphic recursion
  - `json.encode([])`, exercising `[never]: Encode`
  - Nested types where both outer and inner types define custom implementations
  - Async custom implementations that suspend on `task.sleep`
  - Malformed `Encode` implementations that omit `encoder.encode` or call it twice
  - Error-path reporting from failing custom `Decode` implementations
- **Robustness checks:**
  - Run tests with `Config::retry_stress` enabled to force limit-check retries.
  - Compare process resident memory against `max_memory` and against equivalent Sand loops on large inputs.
  - Encode and decode deeply nested values on a 512 KiB thread stack.
  - Encode values with many custom-implementation holes.
- **Benchmarks:**
  - Compare `json` codec functions against `main` using `bench/run.py` and `benches/codec.rs`.
  - Measure call overhead for generic functions with `Encode` bounds.

## Open questions

1. **How should traits opt into compiler derivation?** I propose a `derive` modifier on trait declarations, such as `pub derive trait Encode { ... }`, instead of matching trait identifiers in the checker. Two derivation exclusions can remain checker rules for now: `var` fields skip `Hash`, and `init` constructors skip `Decode`. Alternatively, this branch can keep identifier-based recognition.
2. **Should this branch keep the trait-method restrictions on local functions and struct field defaults?** I recommend keeping both restrictions in this branch. Local functions with `Encode` or `Decode` bounds will then fail type checking like other method-bearing bounds.
3. **Should this branch delete `reflect`?** I recommend deleting `reflect` in this branch. Removing `reflect` leaves a single source of runtime type metadata for decoding and deletes substantial compiler and VM code.
