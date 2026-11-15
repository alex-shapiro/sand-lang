# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** This document specifies the design before implementation. See [Reusable parts from the previous draft](#reusable-parts-from-the-previous-draft) for code kept from the previous `encode-decode-traits-v2` draft.

## Summary

We propose making `Encode` and `Decode` ordinary traits with native default methods. Users can implement both traits for concrete and generic types. Because the compiler erases type arguments, callers pass trait dictionaries to generic functions as hidden runtime arguments. See [Why `Encode` and `Decode` need dictionaries](#why-encode-and-decode-need-dictionaries) for a comparison with `main`.

Each trait method uses a native Rust walker as its default body. The compiler generates ordinary implementations for built-in types, structs, and enums. Generated implementations use these default methods. Users can override a generated implementation with a custom implementation.

During a walk, the walker looks up each part's implementation in trait dictionaries. If a part uses the native default, the walker processes that part in Rust. If a part uses a custom implementation, the walker runs a callback to the custom Sand method.

The VM runs `Encode` and `Decode` through a general mechanism for native default methods. Follow-up changes can migrate other derived traits to this mechanism.

This design meets four goals:

- **Host `Value` conversion.** The encode walker copies a Sand value into a `Value`. The decode walker builds a Sand value from a `Value`.
- **JSON encoding and decoding.** Plain Rust functions convert between JSON text and `Value`.
- **Ordinary trait rules.** The checker enforces standard SPEC §4.9 rules on trait bounds and custom implementations.
- **Standard `json.Json` handling.** `Json` has ordinary `Encode` and `Decode` implementations. `json.encode` and `json.decode` handle `Json` through those implementations.

## Terms

This document uses the following terms:

- **Trait evidence.** Trait evidence is a hidden runtime argument that supplies a trait implementation to a generic function. The compiler erases type arguments from functions. Callers pass trait evidence so generic functions can call trait methods.
- **Dictionary.** A dictionary is a runtime tuple that holds trait evidence for a trait with methods. On `main`, each dictionary stores three groups of items in order:
  1. The first group holds the method closures of the implementation.
  2. The second group holds supertrait dictionaries. For `Decode: Encode`, it holds the `Encode` dictionary for the same type.
  3. The third group holds bound dictionaries of the implementation. For `impl<T: Show> Show for [T]`, it holds the `Show` dictionary for `T`.

  For traits without methods, the checker verifies bounds at compile time. The compiler emits runtime dictionaries only if a trait or a supertrait declares methods. For example, `Hash` on `main` has zero methods.
- **Constant dictionary.** A constant dictionary is a dictionary of concrete types stored in the program constant pool. For example, `[int]: Show` uses a constant dictionary.
- **Runtime dictionary.** A runtime dictionary is a dictionary tuple that generic code builds at runtime from type-parameter dictionaries. For example, `fn print_all<T: Show>(items: [T])` builds the `[T]: Show` dictionary at runtime.
- **Derived trait.** A derived trait is a built-in trait that the compiler implements automatically. On `main`, the compiler derives five traits, which include `Encode` and `Decode`. Derived traits follow two rules on `main`:
  - A struct or enum implements a derived trait if every field and variant payload implements that trait.
  - The checker rejects custom implementations of derived traits.
- **Native function.** A native function is a Rust function that Sand code can call.
- **Native default method.** A native default method is a trait method whose default body is a native function. If an implementation leaves out the method body, the compiler uses the native default. This design adds native default methods to Sand.
- **Part.** A part is a direct child value inside a composite or container value:
  - Struct parts are field values.
  - Enum parts are variant payload values.
  - List and set parts are elements.
  - Map parts are keys and values.
- **Walker.** A walker is a native function that visits a value and its parts in turn. The encode walker reads a Sand value from the VM. The decode walker builds a Sand value in the VM.
- **`Value`.** `sandlang::value::Value` is a host-owned Rust type that holds a copy of a Sand value outside the VM. A Rust host can keep a `Value` after a VM run ends.
- **Callback.** A callback is a Sand function call made from an async native function. Commit `8a51c46` added callbacks. Benchmarks in `experiments/native-async-sand-calls.md` measured each callback round trip at about 90 ns.
- **Triage report.** The triage report is `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`. That document analyzes the performance and complexity problems in earlier codec designs.

## Why `Encode` and `Decode` need dictionaries

To support generic custom implementations, the compiler passes runtime dictionaries for `Encode` and `Decode`. On `main`, the prelude declares both traits with empty bodies. The type checker rejects custom implementations of both traits.

### `Encode` on `main`

On `main`, `json.encode` runs a single native encoder for all types. Heap objects store the structure that this encoder reads:

- Each heap object stores its kind tag.
- Each struct object stores the ID of its type definition.
- The VM type table maps each struct ID to its field identifiers.

The native encoder walks values directly from runtime tags. At compile time, the checker verifies `T: Encode` to reject functions, tasks, and channels. The compiler then erases the bound.

### `Decode` on `main`

A decoder reads raw text. Its caller must supply the target type. On `main`, `json.decode<reflect T: Decode>` receives a runtime type descriptor through its `reflect` type parameter. The compiler erases type arguments from functions. Callers pass this descriptor to supply the target type structure.

### Why custom implementations require dictionaries

Generic custom implementations require trait dictionaries for five reasons:

1. Users can write custom `Encode` implementations.
2. The native encoder must call custom methods for matching parts of a value.
3. Heap objects store type definitions and leave out type arguments. See [Why heap objects leave out type arguments](#why-heap-objects-leave-out-type-arguments).
4. Generic code for `Box<T>` must look up `T`'s `Encode` implementation at runtime.
5. Callers pass `T`'s implementation through a hidden trait dictionary.

## Why heap objects leave out type arguments

The VM leaves runtime type arguments off heap objects to avoid three costs. If every value stored its type arguments, the native encoder could look up generic implementations directly from values.

### Cost 1: Every generic call would pass runtime type arguments

With type erasure, generic functions run with zero hidden type descriptors. Consider this generic function:

```sand
fn empty_list<T>() -> [T] { [] }
```

The compiler emits a single copy of `empty_list` for all `T`. To record the element type inside the new list, `empty_list` would need `T` at runtime. Every caller would pass a runtime descriptor for `T`. Every generic function that builds a value would take hidden type descriptors.

On `main`, callers pass hidden arguments only for bounds on traits with methods. Callers of `fn identity<T>(value: T) -> T` pass only `value`.

### Cost 2: The VM stores immediate values inline

The VM stores small values directly in a 16-byte value slot outside the heap:

- `None`
- Unit enum variants, such as `Color.Red`
- Enum variants with small payloads, such as `Some(3)`

An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. To record type arguments on values, the VM would have to allocate a heap object for every `None`. Most Sand programs use `Option` heavily. Heap allocation for `None` would add large overhead.

### Cost 3: Extra memory per heap object

Storing an interned type-argument ID in every heap header would add about 4 bytes per object. This memory cost is smaller than Costs 1 and 2.

### Choices in other languages

Other languages trade off type erasure and runtime metadata in four ways. These summaries come from general knowledge:

- Java also erases type arguments at compile time.
- C# and .NET store generic type arguments on every object. They also pass runtime type metadata to generic code.
- Swift passes type metadata to generic functions as hidden arguments.
- Rust and C++ compile a separate copy of each generic function for each concrete type. The compiler knows every concrete type in each copy.

### Why dictionaries fit Sand

With dictionaries, callers pass runtime metadata only to functions with `Encode` or `Decode` bounds. On `main`, the `reflect` modifier follows the same principle and limits type descriptors to marked type parameters. Recording type arguments on values would add metadata arguments to every generic call. Functions unrelated to encoding would pay Cost 1 as well.

## Limits of the previous draft

In the previous draft, the walkers looked up custom implementations from runtime value types. That design worked only for concrete types. It also required four special cases:

- **Dictionary exception in the checker.** The checker skipped dictionaries for `Encode` and `Decode` despite their declared methods.
- **Method slots on type definitions.** Each type definition in the checker stored dedicated slots for custom `encode` and `decode` methods.
- **Method slots in the VM type table.** Each struct and enum record in the VM type table stored custom `encode` and `decode` methods.
- **Concrete-type rule.** The checker allowed custom `Encode` and `Decode` implementations only on concrete structs and enums. A fixed slot on a type definition stores a single method for all type arguments.

This design looks up implementations in standard dictionaries. That change removes all four special cases.

## Part 1: Native default methods

The prelude declares `Encode` and `Decode` with native default methods:

```sand
pub trait Encode {
    /// Writes `self` to `encoder` as a single value.
    /// Custom implementations call `encoder.encode(value)`.
    native fn encode(self, encoder: Encoder)
}

pub trait Decode: Encode {
    /// Decodes a value of `Self` from `decoder`.
    /// Custom implementations call `decoder.decode()`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

On `main`, callers pass `Self`'s dictionary to every trait method as its first hidden argument. A native default method receives the active implementation's dictionary in that slot. The walker reads that dictionary to find the implementation of each part.

## Part 2: Structural implementations

A structural implementation uses the native default methods of its trait. Rust setup code passes a list of supported type kinds as it registers each native default method. The type checker generates all structural implementations from that list. The prelude declares only the traits themselves.

### Type kinds

The Rust enum `TypeKind` lists the type categories that a native default can support:

- Built-in scalar kinds, such as `Int`, `Str`, and `Bytes`
- Built-in constructor kinds, such as `List`, `Map`, and `Tuple`
- `Struct` and `Enum`
- `NativeValue` for native value types, such as `time.PlainDate`
- `Never`

Each `ImplShape` constant in Part 3 stores a `TypeKind` variant.

### Registration

Rust setup code supplies a slice of `TypeKind` values with each native default registration. For example, prelude setup registers `Encode.encode` with every kind that the encode walker supports.

During trait declaration, the type checker processes each registered `TypeKind`:

- **Built-in kinds.** The checker creates a structural implementation for each built-in kind. It requires every type argument to implement the trait. For `List`, the checker creates `impl<T: Encode> Encode for [T] {}`.
- **`Struct` and `Enum` kinds.** If the list includes `Struct` or `Enum`, the checker derives a structural implementation for each eligible struct or enum. Its existing derivation pass computes the bounds on each type parameter from the field and payload types. For `struct Pair<A, B> { first: A, second: [B] }`, the checker generates `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.
- **Derivation conditions.** Rust setup code can attach an eligibility rule to any registered kind. The checker evaluates that rule before it derives an implementation. For `Decode`, the rule on `Struct` excludes structs with `init` constructors. The checker reads this rule from the registration entry instead of matching the trait identifier.

If a user writes a custom implementation for a type, the checker skips deriving that trait for the type.

The checker registers structural implementations as ordinary implementations. Its standard trait resolver checks `Encode` and `Decode` bounds directly.

### Synchronization

Two checks keep each registered kind list aligned with its walker:

- **Exhaustive `TypeKind` matching.** Each walker matches on the `TypeKind` stored in `ImplShape`. The Rust compiler requires that match to cover every `TypeKind` variant. If a developer adds a new variant, the Rust compiler reports an incomplete match in the walker.
- **Supertrait verification.** The checker verifies supertrait bounds on all structural and custom implementations. `Decode` requires `Encode` as a supertrait. Prelude setup must register every `Decode` kind for `Encode` as well. If prelude setup registers an extra kind only on `Decode`, the checker rejects the prelude in every test run.

### Completeness for built-in types

Under the orphan rule, a module can implement a prelude trait only if it is the prelude or declares the target type. The checker rejects user implementations of `Encode` and `Decode` on built-in types. Prelude registration alone determines which built-in types implement `Encode` and `Decode`.

### Scope: `Eq`, `Ord`, and `Hash`

We update only `Encode` and `Decode` in this branch. We leave `Eq`, `Ord`, and `Hash` on their current derivation path. The checker continues to read the built-in type table in `check/derive.rs` for those three traits. A follow-up pull request can migrate them to native default methods. That change will delete the table in `check/derive.rs`.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries store extra metadata

Native default methods look up each part's implementation in dictionaries. On `main`, dictionaries store bound dictionaries only for generic type parameters. Concrete field types have zero entries in a standard dictionary. For example, `struct User { email: Email }` has zero type parameters. A standard `User` dictionary stores entries only for `User` itself. It leaves out `Email`'s `Encode` dictionary.

### Implementation shape

The compiler emits a constant `ImplShape` for each structural implementation. It places `impl_shape` immediately after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., impl_shape, bound dictionaries...]
```

Ordinary trait calls read only the method closures and supertrait dictionaries. For custom implementations, the compiler emits the standard dictionary layout. Only native default methods read `impl_shape`.

Each `ImplShape` holds two fields:

- **Type kind.** This field stores a `TypeKind` variant from Part 2. The walker reads this variant to choose the value representation. `Struct` variants also store field identifiers and field defaults. `NativeValue` variants also store a text parser.
- **Part templates.** This field stores a `PartTemplate` for each part. The walker evaluates each template to get that part's dictionary.

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

- **Concrete field.** For field `email: Email` in `impl Encode for User`, the compiler emits `Constant(index)` for `Email`'s constant `Encode` dictionary. The walker loads that constant directly.
- **Direct type parameter.** In `impl<T: Encode> Encode for [T]`, the compiler emits `Bound(0)`. The walker reads `T`'s dictionary from bound slot `0`.
- **Nested generic field.** For field `items: [T]` in `impl<T: Encode> Encode for Page<T>`, the compiler emits `Tuple([list_method, list_shape, Bound(0)])`. The walker evaluates those three elements to build the `[T]` dictionary.

### How the compiler builds part templates

The compiler lowers `PartTemplate` values from the checker's existing trait-evidence trees. For method bodies on `main`, the checker builds an expression tree of dictionary tuples, bound reads, and constant loads. The compiler lowers that tree to bytecode on `main`. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same expression tree. The compiler then converts that tree into a `PartTemplate`.

### Worked example

With part templates, the native encoder reaches custom methods across erased type arguments. At runtime, a `Page<Email>` value stores zero record of `T = Email`. The walker reaches `Email.encode` through dictionaries.

Consider this program:

```sand
struct Email { address: str }

impl Encode for Email {
    fn encode(self, encoder: Encoder) { encoder.encode(self.address) }
}

struct Page<T> { items: [T] }

fn format_page<T: Encode>(page: Page<T>) -> str { json.encode(page) }

format_page(Page(items: [Email(address: "a@b.c")]))
```

`Email` defines a custom `Encode` implementation. `Page<T>` uses a derived structural implementation. Here, the native encoder is the native default method `Encode.encode`:

- **Step 1: Incoming bound dictionary.** The signature of `format_page` requires `T: Encode`. Its caller passes `Email`'s `Encode` dictionary to `format_page`. That dictionary is a 1-item tuple that holds `Email.encode`.
- **Step 2: Runtime `Page<T>` dictionary.** `format_page` calls `json.encode(page)`. For generic `T`, `format_page` builds the `Page<T>` dictionary at runtime as a 3-item tuple:
  1. Item 1 holds the native encoder method.
  2. Item 2 holds the constant `ImplShape` for `Page`.
  3. Item 3 holds `T`'s `Encode` dictionary from Step 1. In this call, that item is `Email`'s dictionary.
- **Step 3: `Page` part template.** `Page`'s `ImplShape` holds a `PartTemplate` for `items`. That template instructs the walker to build a 3-item `[T]` dictionary tuple:
  1. Item 1 holds the native encoder method.
  2. Item 2 holds the constant `ImplShape` for lists.
  3. Item 3 is `Bound(0)`, which refers to item 3 of the enclosing `Page` dictionary.
- **Step 4: Runtime walk.**
  1. `json.encode` calls the native encoder with `page` and the `Page` dictionary.
  2. The native encoder reads item 2 of the `Page` dictionary to get `Page`'s `ImplShape`.
  3. At `items`, the native encoder evaluates `items`'s part template to build the list dictionary. It copies `Email`'s dictionary from item 3 of the `Page` dictionary into item 3 of the list dictionary.
  4. Item 1 of the list dictionary is the native encoder itself. The walker processes the list directly in Rust.
  5. For each list element, the list `ImplShape` stores `Bound(0)`. The walker reads item 3 of the list dictionary, which is `Email`'s dictionary.
  6. Item 1 of `Email`'s dictionary is the custom Sand method `Email.encode`. The walker calls `Email.encode` through a callback.

## Part 4: Recursive types

### Cyclic dictionary references

Recursive types create cycles between dictionaries and implementation shapes. Consider `struct Tree { children: [Tree] }`. `Tree`'s `ImplShape` refers to the `[Tree]` dictionary. The `[Tree]` dictionary stores `Tree`'s dictionary as a bound. `Tree`'s dictionary stores `Tree`'s `ImplShape`.

### Building part dictionaries on demand

The walker breaks dictionary cycles by building each part's dictionary only if the walk visits that part:

- **Constant dictionaries.** `PartTemplate::Constant` stores an integer index into the constant pool. The VM resolves that index during the walk. Before lowering an `ImplShape`'s part templates, the compiler reserves the constant slot for that shape. For recursive references, the compiler emits that reserved index. The constant pool stays acyclic.
- **Runtime dictionaries.** For generic `Tree<T>`, the compiler emits the finite template `Tuple([list_method, list_shape, Tuple([tree_method, tree_shape, Bound(0)])])` for `children`. The walker builds `[Tree<T>]`'s dictionary at `children`. It then builds each child `Tree<T>` dictionary as it visits that child.
- **Polymorphic recursion.** A recursive type definition can wrap its type parameter in a nested type, as in `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. At each `Deep` layer, the walker evaluates `Deep`'s template to build `Nest<[T]>`'s dictionary from `T`'s dictionary. The walker builds only as many dictionaries as the runtime depth of the value.

### Per-walk dictionary cache

During a walk, the walker caches part dictionaries by template and parent dictionary. While encoding a list of 10,000 `Tree<int>` values, the walker builds the `Tree<int>` dictionary once.

### Rejected recursion schemes

We rejected two alternative schemes:

- **Store part dictionaries inside every dictionary.** For recursive types, the compiler would have to build cyclic tuples. On polymorphic recursion, the compiler would expand nested dictionaries forever.
- **Generate a Sand function that returns part dictionaries.** Each uncached dictionary lookup would run a Sand callback that takes about 90 ns. Types that use only native defaults would pay that cost as well.

Other languages use lazy dictionary construction as well. These comparisons come from general knowledge:

- Swift builds generic witness tables on demand at runtime. The Swift runtime then caches those tables.
- GHC builds recursive typeclass dictionaries as lazy thunks.

## Part 5: Native walkers

### Encode walker

The encode walker copies a Sand value into a host `Value`. It tracks open containers and their dictionaries on an explicit `Vec` stack. For each part, the walker resolves the part dictionary from its `PartTemplate`. It then checks the method in that dictionary:

- **Native default method.** The walker copies the part directly in Rust.
- **Custom Sand method.** The walker writes a placeholder into `Value`. It also records a hole with the target `Value` slot, the part value, and the part dictionary.

After the structural pass, the native function runs a callback for each hole. Each custom `encode` method writes a value to an `Encoder`. The walker encodes that value into the hole's slot.

### Decode walker

The decode walker builds a Sand value from a `Value` according to the type kind in each `ImplShape`. The walker records a hole in two cases:

- A part's type defines a custom `Decode` implementation.
- A struct field is absent from the input and declares a computed default expression. An example is `id: int = next_id()`.

If the walker records any holes, it runs in two passes:

1. In pass 1, the walker decodes and validates the input. It records every hole.
2. Between passes, the native function runs a callback for each hole. It saves the resulting Sand values.
3. In pass 2, the walker decodes the input again and inserts the saved results.

This two-pass order keeps two guarantees from `main`. First, the walker validates the full input before it runs any computed field defaults. It runs those defaults in post-order. Second, the walker hashes finished values as it inserts them into sets and maps.

### Synchronous fast path

If the walker records zero holes, the native function returns a synchronous result directly. Only walks with holes allocate a future. The VM takes about 10 ns for a synchronous native call. It takes about 112 ns to poll an immediately ready async native future.

## Part 6: Type checker and compiler changes

1. **Native default methods.** The checker treats a `native fn` method in a `trait` declaration as a native default method. On `main`, default trait methods must have Sand block bodies.
2. **Structural implementations from registered kinds.** The checker generates structural implementations from each native default's `TypeKind` list. It registers them as ordinary implementations. Prelude setup registers `Decode` with a rule that excludes structs with `init` constructors. For `Hash`, the checker keeps its existing rule that excludes structs with `var` fields.
3. **Part templates.** For each structural implementation, the checker builds trait-evidence trees for all parts. The compiler lowers those trees to `PartTemplate` constants.
4. **Structural implementations for `never`.** Under SPEC §5.1 on `main`, `never` implements only traits without methods. An expression such as `json.encode([])` requires `[never]: Encode`. With `TypeKind::Never` registered, the checker generates a structural implementation for `never` with shape kind `Never`. The decode walker always returns an error for `TypeKind::Never`. The type `never` has zero runtime values. Its `Encode` implementation exists solely to satisfy type bounds on empty containers.
5. **Trait dictionary parameters for native functions.** Generic native functions such as `json.encode<T: Encode>` receive `T`'s dictionary through a new `TraitDict<N>` parameter wrapper. A Rust native function accepts `TraitDict<N>` to read the `N`-th bound dictionary for traits with methods. This wrapper uses the same mechanism as `Num<N>`.
6. **Removal of `reflect`.** `Decode` dictionaries include `ImplShape` metadata. That metadata replaces `reflect` type descriptors in generic decoders. On `main`, only `json.decode` uses `reflect` in the standard library. We delete the `reflect` feature.

The checker applies its two existing rules for traits with methods to `Encode` and `Decode`:

- **Local functions.** For local functions, the checker rejects bounds on traits with methods. It reports `local functions cannot require traits with methods` for local `Encode` or `Decode` bounds.
- **Struct field defaults.** Under SPEC §4.2, the checker rejects bounds on traits with methods inside field default expressions.

See [Open question 1](#open-questions) on keeping both rules in this branch.

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
- **`Vm::to_value` and `NativeContext::to_value`.** Callers pass an `Encode` dictionary alongside the Sand value to `Vm::to_value`. `Vm::to_value` runs synchronously. If it reaches a custom `Encode` implementation, it returns an `"invalid"` error. A new async `NativeContext::to_value` method runs custom `Encode` implementations through callbacks.

## Updates to `SPLIT.md` tasks 14, 15, and 18

`SPLIT.md` divides the work of a source branch into numbered changesets:

- **Task 14: `Value` and `Vm::to_value`.** We merged Task 14 in pull request #41. This design updates two parts of Task 14:
  - Callers pass an `Encode` dictionary to `Vm::to_value`. That dictionary lets `Vm::to_value` detect custom `Encode` implementations.
  - The encode walker replaces the three separate walkers on `main`: `json.encode`, `json.to_json`, and `Vm::to_value`. `json.encode` writes its `Value` output as JSON text. `json.to_json` converts its `Value` output to `Json`.
- **Task 15: `Encode` and `Decode` as ordinary traits.** We rename Task 15 to this title. It covers Parts 1 through 7 of this design:
  - `json.decode` first parses JSON text into a `Value`. It then runs the decode walker.
  - Task 15 leaves `Value` decoding for Rust hosts to Task 16. In Task 16, we implement `serde` traits on `Value`. Rust hosts decode `Value` instances into Rust types through `serde`.
- **Task 18: Host function calls.** In Task 18, the engine runs the decode walker to convert each host `Value` argument. Host-callable script functions take concrete parameter types. The compiler emits a constant `Decode` dictionary for each parameter.

## Costs and risks

- **Dictionary passing.** Callers pass a dictionary tuple to generic functions with `T: Encode` bounds. For concrete types, callers pass a program constant. For generic types, callers build a tuple at runtime. Benchmarks in the triage report measured generic trait calls at about 12 ns above direct calls.
- **Per-part dispatch.** For each part, the walker resolves the part dictionary. It then checks the method pointer in that dictionary. For concrete types, the walker reads program constants directly from `Constant` templates.
- **Intermediate `Value` allocation.** `json.encode` builds a `Value` tree before writing JSON text. On `main`, `json.encode` writes JSON text directly from Sand values. We will benchmark `json.encode` to measure any slowdown.
- **Memory accounting across callbacks.** The walkers count `Value` memory against `max_memory` during each structural pass. During a Sand callback, the VM excludes the in-progress `Value` tree from `max_memory`. `TODO.md` tracks this gap under "Charge boxed recursive futures against `max_memory`."
- **Net codebase size.** We expect this design to reduce total code size. We will report the measured line delta in the implementation pull request.

  We delete:
  - `compile/reflect.rs` and `reflect` support in the parser and checker
  - Runtime type descriptors and their templates
  - The typed JSON decoder, the direct JSON encoder, and the `json.to_json` walker
  - Helper functions that run computed field defaults after decoding

  We add:
  - Native encode and decode walkers
  - `ImplShape` constants and `PartTemplate` lowering
  - Native default trait methods
  - `TypeKind` registration for native defaults
  - Compiler-generated structural implementations
  - The `TraitDict<N>` parameter wrapper for native functions

## Rejected alternatives

- **Runtime lookup table keyed by trait and type.** A runtime table would add a second dispatch mechanism alongside dictionaries. The compiler erases type arguments from generic values. Runtime lookup from values works only for concrete types. Callers of a decoder would also have to pass runtime type descriptors.
- **Generated Sand methods for all derived implementations.** Swift uses this model. Under this approach, compiler-generated Sand methods call a trait method for every part. Benchmarks in the triage report measured Sand-only codecs at 2.3 to 7 times slower than native walkers.
- **Dual generated Sand codecs and native fast paths.** With two codec paths, developers must keep output, error paths, and retry behavior identical across both. The triage report audited `origin/encode-decode-traits`. Over a quarter of its 8,210 new production lines synchronized those two paths.

## Reusable parts from the previous draft

We keep three components from the previous draft on `encode-decode-traits-v2`:

- **Encode and decode walkers.** The walkers in `codec/encode.rs` and `codec/decode.rs` implement holes, two-pass decoding, and JSON error paths such as `$.items[2].price`. We change only how those walkers look up implementations.
- **`Value`-based JSON parser and writer.** We keep both components and the native `Json` trait methods.
- **Computed field defaults as decode holes.** The decode walker evaluates computed field defaults through holes. That mechanism replaces the helper functions that run after decoding on `main`.

We drop all four special cases listed in [Limits of the previous draft](#limits-of-the-previous-draft).

## Test and measurement plan

- **Existing JSON behavior.** Keep existing test output in `tests/spec/std_json.test`. List every intentional error-message change in this document.
- **New spec tests:**
  - Custom `Encode` and `Decode` implementations on structs and enums
  - Generic custom implementations, such as `impl<T: Encode> Encode for Box<T>`
  - Direct recursive types and polymorphic recursion
  - `json.encode([])` with `[never]: Encode`
  - Composite values whose outer and inner types both define custom implementations
  - Async custom implementations that wait on `task.sleep`
  - Faulty `Encode` implementations that skip `encoder.encode` or call it twice
  - Error paths from failed custom `Decode` implementations
- **Robustness checks:**
  - Enable `Config::retry_stress` in tests to force retries at resource limits.
  - On large inputs, compare process resident memory against `max_memory`. Compare resident memory against equivalent Sand loops as well.
  - Encode and decode deeply nested values on a 512 KiB thread stack.
  - Encode values that contain many custom-encoded parts.
- **Benchmarks:**
  - Benchmark `json` functions against `main` with `bench/run.py` and `benches/codec.rs`.
  - Measure call overhead for generic functions with `Encode` bounds.

## Open questions

1. **Should this branch keep the existing rules on local functions and field defaults?** We recommend keeping both rules in this branch. The checker will reject local functions that require `Encode` or `Decode`. That behavior matches all other traits with methods.
2. **Should this branch delete `reflect`?** We recommend deleting `reflect` in this branch. `Decode` dictionaries then serve as the single source of runtime type metadata for decoding. Deleting `reflect` also removes `reflect` support from the parser, checker, compiler, and VM.
