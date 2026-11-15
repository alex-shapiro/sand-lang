# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** This document specifies the design before implementation. See [Reusable parts from the earlier draft](#reusable-parts-from-the-earlier-draft) for the parts kept from the earlier `encode-decode-traits-v2` draft.

## Summary

We propose making `Encode` and `Decode` ordinary traits with native default methods. Users can implement both traits for concrete and generic types, such as `Box<T>`. The compiler erases type arguments. Callers therefore pass trait dictionaries to generic functions as hidden runtime arguments. See [Why `Encode` and `Decode` need dictionaries](#why-encode-and-decode-need-dictionaries) for a comparison with `main`.

Each trait method defaults to a native Rust walker. The compiler generates ordinary implementations for structs and enums that inherit these defaults. A custom implementation replaces the derived implementation for that type.

During a walk, the walker looks up each part's implementation in trait dictionaries. It processes derived parts directly in Rust. For parts with custom implementations, the walker runs a callback to the custom Sand method.

The VM treats `Encode` and `Decode` like all other traits with native default methods. Other derived traits can use the same mechanism later.

This design meets four goals:

- **Host `Value` conversion.** The encode walker copies a Sand heap value into a `Value`. The decode walker builds a Sand heap value from a `Value`.
- **JSON encoding and decoding.** Plain Rust functions convert between JSON text and `Value`.
- **Ordinary trait rules.** The checker enforces standard SPEC §4.9 rules on trait bounds and custom implementations.
- **Standard `json.Json` handling.** `Json` has ordinary `Encode` and `Decode` implementations. `json.encode` and `json.decode` handle `Json` through those implementations.

## Terms

This document uses the following terms:

- **Trait evidence.** Trait evidence is a hidden runtime argument that provides a trait implementation to a generic function. Callers pass trait evidence because the compiler erases type arguments.
- **Dictionary.** A dictionary is a runtime tuple that provides trait evidence for a trait with methods. On `main`, each dictionary stores three groups of items in order:
  1. The first group holds the method closures of the implementation.
  2. The second group holds supertrait dictionaries. For `Decode: Encode`, it holds the `Encode` dictionary for the same type.
  3. The third group holds bound dictionaries of the implementation. For `impl<T: Show> Show for [T]`, it holds the `Show` dictionary for `T`.

  For traits without methods, such as `Hash` on `main`, the checker verifies bounds at compile time. Code generation omits runtime dictionaries for those traits.
- **Constant dictionary.** A constant dictionary is a dictionary of concrete types stored as a program constant. For example, `[int]: Show` uses a constant dictionary.
- **Runtime dictionary.** A runtime dictionary is a dictionary tuple that generic code builds at runtime from type-parameter dictionaries. For example, `fn print_all<T: Show>(items: [T])` builds the `[T]: Show` dictionary at runtime.
- **Derived trait.** A derived trait is a built-in trait that the compiler implements automatically. On `main`, the compiler derives five traits, including `Encode` and `Decode`:
  - A struct or enum implements a derived trait if every field and variant payload implements that trait.
  - On `main`, the checker rejects custom implementations of derived traits.
- **Native function.** A native function is a Rust function that Sand code can call.
- **Native default method.** A native default method is a trait method whose default body is a native function. Any implementation that omits the method body inherits the native default. This design adds native default methods to Sand.
- **Part.** A part is a direct child value inside a composite or container value:
  - Struct parts are field values.
  - Enum parts are variant payload values.
  - List and set parts are elements.
  - Map parts are keys and values.
- **Walker.** A walker is a native function that visits a value and its parts in turn. The encode walker reads a value from the VM heap. The decode walker builds a value on the VM heap.
- **`Value`.** `sandlang::value::Value` is a host-owned Rust type that holds a copy of a Sand value outside the VM. A Rust host can keep a `Value` after a VM run ends.
- **Callback.** A callback is a Sand function call made from an async native function. Commit `8a51c46` added support for callbacks. Each callback round trip takes about 90 ns, as measured in `experiments/native-async-sand-calls.md`.
- **Triage report.** The triage report is `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`. It analyzes the performance and complexity problems in earlier codec designs.

## Why `Encode` and `Decode` need dictionaries

Generic user implementations require runtime dictionaries for `Encode` and `Decode`. On `main`, the prelude declares both traits with empty bodies. The type checker rejects custom implementations of both traits.

### `Encode` on `main`

On `main`, `json.encode` uses a single native encoder for all types. Heap values store the structure that this encoder reads:

- Each heap object stores its kind, such as list, map, or struct.
- Each struct object stores the ID of its type definition.
- The VM type table maps each struct ID to its field identifiers.

The native encoder walks heap values directly from these runtime tags. At compile time, the checker verifies `T: Encode` to reject functions, tasks, and channels. Code generation then erases the bound.

### `Decode` on `main`

A decoder reads raw text. It therefore needs the target type from its caller. On `main`, `json.decode<reflect T: Decode>` receives a runtime type descriptor through its `reflect` type parameter. That hidden argument restores the type structure that type erasure removed.

### Why custom implementations require dictionaries

Generic custom implementations require trait dictionaries for five reasons:

1. Users can write custom `Encode` implementations.
2. The native encoder must call custom methods for matching parts of a value.
3. Heap values omit type arguments. See [Why heap values omit type arguments](#why-heap-values-omit-type-arguments).
4. A generic implementation such as `impl<T: Encode> Encode for Box<T>` needs `T`'s `Encode` implementation at runtime.
5. Callers pass `T`'s implementation through a hidden trait dictionary.

## Why heap values omit type arguments

Sand omits runtime type arguments from heap values to avoid three costs. Recording type arguments on heap values would otherwise let the native encoder find generic implementations without dictionaries.

### Cost 1: Every generic call would pass runtime type arguments

Type erasure lets generic functions run without hidden type descriptors. Consider this generic function:

```sand
fn empty<T>() -> [T] { [] }
```

The compiler emits a single copy of `empty` for all `T`. To record the element type inside the new list, `empty` would need `T` at runtime. Every caller would then pass a runtime descriptor for `T`. Every generic function that builds a value would take hidden type descriptors.

On `main`, generic calls pass hidden arguments only for bounds on traits with methods. Calling `fn identity<T>(value: T) -> T` passes only `value`.

### Cost 2: The VM stores immediate values inline

The VM stores small values inline in a 16-byte slot without heap allocation:

- `None`
- Enum variants without payloads, such as `Color.Red`
- Enum variants with small payloads, such as `Some(3)`

An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. To record type arguments on values, the VM would have to allocate a heap object for every `None`. Most Sand programs use `Option` heavily. Allocating `None` on the heap would add large overhead.

### Cost 3: Extra memory per heap object

Storing an interned type-argument ID in every heap header would add about 4 bytes per object. This memory cost is smaller than Costs 1 and 2.

### Choices in other languages

Other languages trade off type erasure and runtime metadata in four ways. These summaries come from general knowledge:

- Java erases type arguments, as Sand does.
- C# and .NET store full generic type arguments on every object and pass runtime type metadata to generic code.
- Swift passes type metadata to generic functions as hidden arguments.
- Rust and C++ compile a separate copy of each generic function per concrete type. They therefore keep concrete types throughout compilation.

### Why dictionaries fit Sand

Dictionaries limit runtime metadata overhead to functions with `Encode` or `Decode` bounds. Storing type arguments on heap values would impose Cost 1 on every generic function. Code that never encodes values would still pay that cost. Sand's `reflect` modifier on `main` restricts runtime type descriptors to marked type parameters. Heap type tags would put that cost on every type parameter instead.

## Limits of the earlier draft

The earlier draft looked up custom implementations from runtime value types. That approach blocked generic implementations. It also required four special cases:

- **Type checker dictionary exception.** The checker omitted dictionaries for `Encode` and `Decode` despite their declared methods.
- **Type-definition method slots.** Each type definition in the checker recorded custom `encode` and `decode` methods.
- **VM type-table slots.** Each struct and enum record in the VM type table stored custom `encode` and `decode` methods.
- **Concrete-type rule.** The checker allowed custom `Encode` and `Decode` implementations only on concrete structs and enums. Fixed method slots cannot represent generic type arguments.

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

A structural implementation inherits the native default methods of its trait. The Rust code that registers each native default method passes a list of supported type kinds. The type checker generates all structural implementations from that list. The prelude declares only the traits themselves.

### Type kinds

The Rust enum `TypeKind` identifies each category of type that a native default can support:

- Built-in scalar kinds, such as `Int`, `Str`, and `Bytes`
- Built-in constructor kinds, such as `List`, `Map`, and `Tuple`
- `Struct` and `Enum`
- `NativeValue` for native value types such as `time.PlainDate`
- `Never`

Each `ImplShape` constant in Part 3 stores a `TypeKind` variant.

### Registration

Each native default registration supplies a slice of `TypeKind` values. For example, prelude setup registers `Encode.encode` with every kind that the encode walker supports.

During trait declaration, the type checker processes each registered `TypeKind`:

- **Built-in kinds.** The checker creates a structural implementation for each built-in kind. It requires every type argument to implement the trait. For `List`, the checker creates `impl<T: Encode> Encode for [T] {}`.
- **`Struct` and `Enum` kinds.** If the list includes `Struct` or `Enum`, the checker derives a structural implementation for each eligible struct or enum. Its existing derivation pass computes the bounds on each type parameter from the field and payload types. For `struct Pair<A, B> { first: A, second: [B] }`, the checker generates `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.
- **Derivation conditions.** Each registered kind can include an eligibility rule that the checker evaluates. For `Decode`, the rule on `Struct` skips structs with `init` constructors. Because registration supplies the rule, the checker selects the rule from registration data, not from the trait name.

If a user writes a custom implementation for a type, the checker skips deriving that trait for the type.

The checker registers structural implementations as ordinary implementations. Its standard trait resolver then checks `Encode` and `Decode` bounds directly.

### Synchronization

Two checks keep the registration and the walker in agreement:

- **Exhaustive `TypeKind` matching.** Each walker matches on the `TypeKind` stored in `ImplShape`. Rust requires that match to cover every `TypeKind` variant. If a developer adds a new variant, the Rust compiler reports an incomplete match in the walker.
- **Supertrait verification.** The checker verifies supertrait bounds on all implementations, including structural implementations. Because `Decode` requires `Encode`, every kind registered for `Decode` must also be registered for `Encode`. If `Decode` registers an extra kind, the checker rejects the prelude during every test run.

### Completeness for built-in types

The orphan rule allows an implementation of a prelude trait only in the prelude or in the module that declares the type. The checker therefore rejects user implementations of `Encode` on built-in types. The registered `TypeKind` list defines the complete set of `Encode` implementations for built-in types.

### Scope: `Eq`, `Ord`, and `Hash`

This branch updates only `Encode` and `Decode`. We leave `Eq`, `Ord`, and `Hash` on their current derivation path. The checker continues to read the built-in type table in `check/derive.rs` for those three traits. We can migrate them to native default methods in a follow-up pull request. That migration will also delete the table in `check/derive.rs`.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries store extra metadata

Native default methods must look up each part's implementation through dictionaries. On `main`, dictionaries store bound dictionaries only for generic type parameters. Standard dictionaries omit entries for concrete field types. For example, `struct User { email: Email }` has zero type parameters. A standard `User` dictionary therefore omits the `Encode` dictionary for `Email`.

### Implementation shape

The compiler emits a constant `ImplShape` for each structural implementation. It places `impl_shape` immediately after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., impl_shape, bound dictionaries...]
```

Ordinary trait method calls read only the method closures and supertrait dictionaries. Custom implementations keep the standard dictionary layout. Only native default methods read `impl_shape`.

Each `ImplShape` holds two fields:

- **Type kind.** This field stores a `TypeKind` variant from Part 2. The variant tells the walker how to represent the value. `Struct` variants also store field identifiers and field defaults. `NativeValue` variants also store a text parser.
- **Part templates.** This field stores a `PartTemplate` for each part. The walker uses each template to obtain that part's dictionary.

### Part templates

`PartTemplate` has three variants:

```rust
enum PartTemplate {
    /// Index of an entry in the program constant pool.
    Constant(u32),
    /// Slot index of a bound dictionary in the enclosing dictionary.
    Bound(u32),
    /// Dictionary tuple built from element templates.
    Tuple(Vec<PartTemplate>),
}
```

The walker evaluates each variant as follows:

- **Concrete field:** For field `email: Email` in `impl Encode for User`, `Constant(index)` points to `Email`'s constant `Encode` dictionary. The walker loads that constant directly.
- **Direct type parameter:** In `impl<T: Encode> Encode for [T]`, `Bound(0)` selects `T`'s dictionary from bound slot `0`.
- **Nested generic field:** For field `items: [T]` in `impl<T: Encode> Encode for Pair<T>`, the compiler emits `Tuple([list_method, list_shape, Bound(0)])`. The walker evaluates those three elements to build the `[T]` dictionary.

### How the compiler builds part templates

The compiler lowers `PartTemplate` values from the checker's existing trait-evidence trees. For method bodies on `main`, the checker builds an expression tree of dictionary tuples, bound reads, and constant loads. Code generation compiles that tree to bytecode on `main`. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same expression tree. The compiler then converts that tree into a `PartTemplate`.

### Worked example

Part templates let the native encoder reach custom methods across erased type arguments. Type erasure removes `T = Email` from the `Page<Email>` heap value. Only dictionaries connect `page` to `Email.encode`.

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

- **Step 1: Incoming bound dictionary.** `save` requires `T: Encode`. Its caller passes `Email`'s `Encode` dictionary to `save`. That dictionary is a 1-item tuple that holds `Email.encode`.
- **Step 2: Runtime `Page<T>` dictionary.** `save` calls `json.encode(page)`. For generic `T`, `save` builds the `Page<T>` dictionary at runtime as a 3-item tuple:
  1. Item 1 holds the native encoder method.
  2. Item 2 holds the constant `ImplShape` for `Page`.
  3. Item 3 holds `T`'s `Encode` dictionary from Step 1. In this call, that item is `Email`'s dictionary.
- **Step 3: `Page` part template.** `Page`'s `ImplShape` holds a `PartTemplate` for `items`. That template tells the walker to build a 3-item `[T]` dictionary tuple:
  1. Item 1 holds the native encoder method.
  2. Item 2 holds the constant `ImplShape` for lists.
  3. Item 3 copies item 3 of the enclosing `Page` dictionary.
- **Step 4: Runtime walk.**
  1. `json.encode` calls the native encoder with `page` and the `Page` dictionary.
  2. The native encoder reads item 2 of the `Page` dictionary to get `Page`'s `ImplShape`.
  3. At `items`, the native encoder runs `items`'s part template to build the list dictionary. It copies `Email`'s dictionary from item 3 of the `Page` dictionary into item 3 of the list dictionary.
  4. Item 1 of the list dictionary is the native encoder itself. The walker therefore processes the list in Rust.
  5. At the first list element, the list `ImplShape` selects item 3 of the list dictionary. That item is `Email`'s dictionary.
  6. Item 1 of `Email`'s dictionary is the custom Sand method `Email.encode`. The walker calls `Email.encode` through a callback.

## Part 4: Recursive types

### Cyclic dictionary references

Recursive types create cycles between dictionaries and implementation shapes. Consider `struct Tree { children: [Tree] }`. `Tree`'s `ImplShape` refers to the `[Tree]` dictionary. The `[Tree]` dictionary stores `Tree`'s dictionary as a bound. `Tree`'s dictionary stores `Tree`'s `ImplShape`.

### Building part dictionaries on demand

The walker breaks dictionary cycles by building each part's dictionary only if the walk visits that part:

- **Constant dictionaries.** `PartTemplate::Constant` stores an integer index into the constant pool. The VM resolves that index during the walk. Before lowering an `ImplShape`'s part templates, the compiler reserves the constant slot for that shape. Recursive references emit that reserved index. The constant pool therefore stays acyclic.
- **Runtime dictionaries.** For generic `Tree<T>`, `children` uses the finite template `Tuple([list_method, list_shape, Tuple([tree_method, tree_shape, Bound(0)])])`. The walker builds `[Tree<T>]`'s dictionary at `children`. It then builds each child `Tree<T>` dictionary as it visits that child.
- **Polymorphic recursion.** A generic type can recurse with a nested type argument, such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. `Deep`'s template builds `Nest<[T]>`'s dictionary from `T`'s dictionary at each `Deep` layer. The runtime depth of the value limits dictionary construction.

### Per-walk dictionary cache

During a walk, the walker caches part dictionaries by template and parent dictionary. Encoding a list of 10,000 `Tree<int>` values therefore builds the `Tree<int>` dictionary once.

### Rejected recursion schemes

We rejected two eager or callback-based schemes:

- **Store part dictionaries inside every dictionary.** Recursive types form cyclic tuples. On polymorphic recursion, the compiler would expand nested dictionaries forever.
- **Generate a Sand function that returns part dictionaries.** Each uncached dictionary lookup would need a Sand callback of about 90 ns. Derived types without custom methods would also pay that cost.

Other languages use lazy dictionary construction as well. These comparisons come from general knowledge:

- Swift builds and caches generic witness tables on demand at runtime.
- GHC builds recursive typeclass dictionaries as lazy thunks.

## Part 5: Native walkers

### Encode walker

The encode walker copies a heap value into a host `Value`. It tracks open containers and their dictionaries on an explicit `Vec` stack. For each part, the walker resolves the part dictionary from its `PartTemplate`. It then checks the method in that dictionary:

- **Native default method.** The walker copies the part directly in Rust.
- **Custom Sand method.** The walker writes a placeholder into `Value`. It also records a hole with the target `Value` slot, the heap part, and the part dictionary.

After the structural pass, the native function runs a callback for each hole. Each custom `encode` method writes a value to an `Encoder`. The walker encodes that value into the hole's slot.

### Decode walker

The decode walker builds a heap value from a `Value` according to the type kind in each `ImplShape`. The walker records a hole in two cases:

- A part whose type defines a custom `Decode` implementation
- A missing struct field with a computed default expression, such as `id: int = next_id()`

If the walker records any holes, it runs in two passes:

1. Pass 1 decodes and validates the input. It records every hole.
2. The native function runs a callback for each hole. It saves the resulting heap values.
3. Pass 2 decodes the input again using the saved results.

Two-pass decoding preserves two guarantees. First, the walker validates the entire input before it runs any computed field defaults. It runs those defaults in post-order, as on `main`. Second, sets and maps insert finished values with their final hashes.

### Synchronous fast path

If a walk records zero holes, the native function returns immediately. It returns a synchronous result without allocating a future. An async native call whose future is ready at its first poll costs about 112 ns. A synchronous native call costs about 10 ns.

## Part 6: Type checker and compiler changes

1. **Native default methods.** The checker treats a `native fn` method in a `trait` declaration as a native default method. On `main`, a trait declaration cannot give a method a native default body.
2. **Structural implementations from registered kinds.** The checker generates structural implementations from each native default's `TypeKind` list, as Part 2 describes. It registers them as ordinary implementations. Prelude registration for `Decode` excludes structs with `init` constructors. For `Hash`, the checker continues to exclude structs with `var` fields on its existing derivation path.
3. **Part templates.** For each structural implementation, the checker builds trait-evidence trees for all parts. The compiler lowers those trees to `PartTemplate` constants.
4. **Structural implementations for `never`.** Under SPEC §5.1 on `main`, `never` implements only traits without methods. Expressions such as `json.encode([])` require `[never]: Encode`. Registering `TypeKind::Never` gives `never` a structural implementation with shape kind `Never`. The decode walker always returns an error for `TypeKind::Never`. Because `never` has zero runtime values, the encode walker never visits a `Never` part.
5. **Trait dictionary parameters for native functions.** Generic native functions such as `json.encode<T: Encode>` receive `T`'s dictionary through a new `TraitDict<N>` parameter wrapper. `TraitDict<N>` reads the `N`-th bound dictionary for traits with methods. It follows the same mechanism as `Num<N>`.
6. **Removal of `reflect`.** `Decode` dictionaries include `ImplShape` metadata. That metadata replaces `reflect` type descriptors in generic decoders. On `main`, only `json.decode` uses `reflect` in the standard library. We therefore delete the `reflect` feature.

Two existing restrictions on traits with methods now apply to `Encode` and `Decode`:

- **Local functions.** For local functions, the checker rejects bounds on traits with methods. It reports `local functions cannot require traits with methods` for local `Encode` or `Decode` bounds.
- **Struct field defaults.** Under SPEC §4.2, the checker rejects bounds on traits with methods inside field default expressions.

See [Open question 1](#open-questions) on keeping both restrictions in this branch.

## Part 7: JSON and `Value`

- **JSON parsing.** The JSON parser converts JSON text to a `Value`. It maps JSON objects to string-keyed maps. It maps JSON `null` to `Value::Unit`.
- **JSON writing.** The JSON writer formats a `Value` as JSON text under three rules:
  - It writes `Some(inner)` directly as `inner`.
  - It writes `bytes` values as Base64 strings.
  - It writes map keys and native value types as strings.
- **JSON-compatible decoding.** The decode walker accepts these JSON forms directly. For example, it decodes Base64 strings into `bytes`. It also parses string keys for `int`-keyed maps. Any `Value` parsed from valid JSON text is valid input for matching `Decode` types.
- **`json.Json` implementations.** `Json` implements `Encode` and `Decode` with dedicated native methods:

  ```sand
  impl Encode for Json { native fn encode(self, encoder: Encoder) }
  impl Decode for Json { native fn decode(decoder: Decoder) -> Result<Json> }
  ```

  Each method converts an entire `Json` tree to or from `Value` in a single native call. `json.parse(text)` calls `json.decode<Json>(text)`.
- **`Vm::to_value` and `NativeContext::to_value`.** `Vm::to_value` takes an `Encode` dictionary alongside the heap value. Because `Vm::to_value` is synchronous, it cannot run Sand callbacks. If it reaches a custom `Encode` implementation, it returns an `"invalid"` error. A new async `NativeContext::to_value` method runs custom `Encode` implementations.

## Updates to `SPLIT.md` tasks 14, 15, and 18

`SPLIT.md` divides the work of a source branch into numbered changesets:

- **Task 14: `Value` and `Vm::to_value`.** Pull request #41 implemented Task 14. This design updates two of its choices:
  - `Vm::to_value` takes an `Encode` dictionary to detect custom `Encode` implementations.
  - The encode walker replaces the three separate heap walkers on `main`: `json.encode`, `json.to_json`, and `Vm::to_value`. `json.encode` writes its `Value` output as JSON text. `json.to_json` converts its `Value` output to `Json`.
- **Task 15: `Encode` and `Decode` as ordinary traits.** We rename Task 15 to this title. It covers Parts 1 through 7 of this design:
  - `json.decode` first parses JSON text into a `Value`. It then runs the decode walker.
  - We omit a separate `value::Decode` Rust trait from Task 15. In Task 16, we implement `serde` traits on `Value` so Rust hosts can decode values.
- **Task 18: Host function calls.** Task 18 uses the decode walker to convert each host `Value` argument. Host-callable script functions take concrete parameter types. The compiler therefore emits a constant `Decode` dictionary for each parameter.

## Costs and risks

- **Dictionary passing.** Generic calls with `T: Encode` bounds pass a dictionary tuple. For concrete types, callers pass a program constant. For generic types, callers build a tuple at runtime. The triage report measured generic trait calls at about 12 ns above direct calls.
- **Per-part dispatch.** For each part, the walker resolves the part dictionary. It then checks the method pointer in that dictionary. For concrete types, `Constant` templates let the walker read program constants directly.
- **Intermediate `Value` allocation.** `json.encode` builds a `Value` tree before writing JSON text. On `main`, `json.encode` writes JSON text directly from heap values. We will benchmark `json.encode` to measure any slowdown.
- **Memory accounting across callbacks.** The walkers count `Value` memory against `max_memory` during each structural pass. During a Sand callback, the VM omits the in-progress `Value` tree from `max_memory`. `TODO.md` already tracks this issue class under "Charge boxed recursive futures against `max_memory`."
- **Net codebase size.** We expect this design to reduce total code size. We will report the measured line delta in the implementation pull request.

  This design deletes:
  - `compile/reflect.rs` and `reflect` support in the parser and checker
  - Runtime type descriptors and their templates
  - The typed JSON decoder, the direct heap-to-JSON encoder, and the `json.to_json` heap walker
  - Post-decode helper functions for computed field defaults

  This design adds:
  - The native encode and decode walkers
  - `ImplShape` constants and `PartTemplate` lowering
  - Native default trait methods
  - `TypeKind` registration for native defaults and compiler-generated structural implementations
  - The `TraitDict<N>` native parameter wrapper

## Rejected alternatives

- **Runtime lookup table keyed by trait and type.** A runtime table duplicates dictionary lookup. Because the compiler erases type arguments, a runtime table cannot resolve generic implementations. A decoder also starts from raw input. It would still require runtime type descriptors.
- **Generated Sand methods for all derived implementations.** Swift uses this model. Compiler-generated methods call a trait method for every part. The triage report measured Sand-only codecs at 2.3 to 7 times slower than native walkers.
- **Dual generated Sand codecs and native fast paths.** Maintaining two codec paths requires matching output, error paths, and retry behavior across both. The triage report examined `origin/encode-decode-traits`. At least a quarter of its 8,210 new production lines existed solely to keep both paths consistent.

## Reusable parts from the earlier draft

This design keeps three components from the earlier draft on `encode-decode-traits-v2`:

- **Encode and decode walkers.** The walkers in `codec/encode.rs` and `codec/decode.rs` implement holes, two-pass decoding, and JSON error paths such as `$.items[2].price`. This design changes only how they look up implementations.
- **`Value`-based JSON parser and writer.** We keep both components and the native `Json` trait methods.
- **Computed field defaults as decode holes.** This approach replaces the post-decode helper functions on `main`.

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
  - On large inputs, compare process resident memory against `max_memory`. Also compare resident memory against equivalent Sand loops.
  - Encode and decode deeply nested values on a 512 KiB thread stack.
  - Encode values that contain many custom-encoded parts.
- **Benchmarks:**
  - Benchmark `json` codec functions against `main` with `bench/run.py` and `benches/codec.rs`.
  - Measure call overhead for generic functions with `Encode` bounds.

## Open questions

1. **Should this branch keep the existing restrictions on local functions and field defaults?** We recommend keeping both restrictions in this branch. The checker will then reject local functions that require `Encode` or `Decode`, as it does for other traits with methods.
2. **Should this branch delete `reflect`?** We recommend deleting `reflect` in this branch. Deleting `reflect` leaves a single source of runtime type metadata for decoding. It also removes substantial compiler and VM code.
