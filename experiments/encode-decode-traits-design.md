# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** We specify this design before implementation. See [Reusable parts from the previous draft](#reusable-parts-from-the-previous-draft) for code that we keep from the previous `encode-decode-traits-v2` draft.

## Summary

We propose making `Encode` and `Decode` ordinary traits with native default methods. Users can implement both traits for concrete and generic types. The compiler erases type arguments from functions. Callers pass trait dictionaries to generic functions as hidden arguments. See [Why `Encode` and `Decode` need dictionaries](#why-encode-and-decode-need-dictionaries) for a comparison with `main`.

Each trait declares a native Rust walker as its default method. The type checker generates ordinary implementations for built-in types, structs, and enums. Those generated implementations use the default methods. Users can write custom implementations for their own types to override generated defaults.

During a walk, the walker looks up each part's implementation in trait dictionaries. For a part that uses the native default, the walker processes that part in Rust. For a part with a custom implementation, the walker runs a callback to the custom method.

The VM runs `Encode` and `Decode` like any other trait with native default methods. We meet four goals with this design:

- **Host `Value` conversion.** The encode walker copies a Sand value into a `Value`. The decode walker builds a Sand value from a `Value`.
- **JSON encoding and decoding.** Plain Rust functions convert between JSON text and `Value`.
- **Ordinary trait rules.** The checker enforces standard `docs/SPEC.md` §4.9 rules on trait bounds and custom implementations.
- **Standard `json.Json` handling.** We implement `Encode` and `Decode` on `Json` as ordinary trait implementations. Callers encode and decode `Json` through `json.encode` and `json.decode` like any other type.

## Terms

- **Trait evidence.** Callers pass a hidden argument called trait evidence to supply a trait implementation to a generic function.
- **Dictionary.** The VM represents trait evidence as a tuple called a dictionary. On `main`, the compiler builds each dictionary with three sections in order:
  1. Section 1 stores the method functions of the implementation.
  2. Section 2 stores supertrait dictionaries, such as the `Encode` dictionary inside a `Decode` dictionary.
  3. Section 3 stores bound dictionaries, such as `T`'s `Close` dictionary inside `impl<T: Close> Close for [T]`.

  The compiler emits dictionaries only if a trait or a supertrait declares methods. For traits with empty bodies and empty supertraits, the checker verifies bounds at compile time. For example, `Hash` on `main` declares only its trait header.
- **Constant dictionary.** The compiler stores a dictionary of concrete types in the constant pool. For example, `[fs.File]: Close` uses a constant dictionary.
- **Dynamic dictionary.** Generic code builds a dynamic dictionary tuple at runtime from type-parameter dictionaries. For example, `fn close_all<T: Close>(items: [T])` builds the `[T]: Close` dictionary at runtime.
- **Derived trait.** The type checker implements a derived trait automatically for qualifying types. On `main`, the checker derives five built-in traits, including `Encode` and `Decode`, under two rules:
  - The checker derives a trait for a qualifying struct or enum if every field and variant payload implements that trait.
  - The checker rejects custom implementations of derived traits.
- **Native function.** Rust host code registers a native function so Sand code can call it.
- **Native default method.** We add native default methods so a trait can use a native function as a method's default body. If an `impl` block has an empty body for that method, the checker uses the native default.
- **Structural implementation.** The checker generates a structural implementation that uses the native default methods of its trait.
- **Part.** A composite or container value holds direct child values called parts:
  - Struct parts are field values.
  - Enum parts are payload values of a variant.
  - List and set parts are elements.
  - Map parts are keys and values.
- **Walker.** A walker is a Rust routine that visits a value and its parts in turn. The encode walker reads a Sand value from the VM. The decode walker builds a Sand value in the VM.
- **Hole.** During a walk, the walker records a hole for any part or field default that requires a callback.
- **`Value`.** `sandlang::value::Value` is a host-owned Rust type that represents a Sand value outside the VM. A Rust host can build a `Value` or keep a `Value` after a VM run ends.
- **Callback.** An async native function can call a Sand function through a callback. We added callbacks in commit `8a51c46`. In `experiments/native-async-sand-calls.md`, we measured each callback round trip at about 90 ns.
- **Triage report.** In `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`, we analyzed the speed and complexity problems in earlier codec designs.

## Why `Encode` and `Decode` need dictionaries

To support generic custom implementations, the compiler passes dictionaries for `Encode` and `Decode`. On `main`, the prelude declares both traits with empty bodies. The type checker rejects custom implementations of both traits.

### `Encode` on `main`

On `main`, `json.encode` calls a single native encoder for all types. The VM stores runtime tags on values that this encoder reads:

- Immediate value slots tag scalars and inline enum variants.
- Heap objects tag their container or composite kind.
- Struct and enum objects store the ID of their type definition.
- The VM type table maps each struct and variant ID to its identifiers.

The native encoder walks values directly from these runtime tags. At compile time, the checker verifies `T: Encode`. That check rejects functions, tasks, channels, and native objects. The compiler then erases the bound.

### `Decode` on `main`

Callers of a decoder must supply the target type because the decoder starts from raw text. On `main`, callers pass a runtime type descriptor to `json.decode<reflect T: Decode>` through its `reflect` type parameter. That hidden argument supplies the type structure that the compiler erased.

### Why custom implementations require dictionaries

We pass dictionaries for `Encode` and `Decode` for five reasons:

1. Users can write custom `Encode` implementations.
2. The native encoder must call custom methods for matching parts of a value.
3. A heap object stores only the ID of its type definition. See [Why heap objects store only type definition IDs](#why-heap-objects-store-only-type-definition-ids).
4. In `impl<T: Encode> Encode for Box<T>`, the encoder must look up `T`'s `Encode` implementation at runtime.
5. Callers pass `T`'s implementation through a hidden trait dictionary.

## Why heap objects store only type definition IDs

The VM stores only type definition IDs on heap objects to avoid three runtime costs. If every value also stored its type arguments, the native encoder could look up generic implementations directly from values.

### Cost 1: Callers would pass type arguments at every generic call

With type erasure, callers pass only value arguments to unconstrained generic functions:

```sand
fn empty_list<T>() -> [T] { [] }
```

The compiler emits a single copy of `empty_list` for all `T`. To record the element type inside the new list, `empty_list` would need `T`'s type descriptor at runtime. Every caller would pass a descriptor for `T`. Callers would pass hidden type descriptors to every generic function that builds a value.

On `main`, callers pass hidden arguments only for `reflect` parameters and bounds on traits with methods. Callers of `fn identity<T>(value: T) -> T` pass only `value`.

### Cost 2: The VM stores immediate values inline

The VM stores immediate values directly in a 16-byte value slot outside the heap. Inline values include scalars, unit enum variants such as `None` and `Color.Red`, and single-payload variants such as `Some(3)`.

An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. To record type arguments on values, the VM would have to allocate a heap object for every `None`. Most Sand programs use `Option` heavily. Heap allocation for `None` would add large overhead.

### Cost 3: Extra memory per heap object

Storing an interned type-argument ID in each `Obj` slot on the heap would add about 4 bytes per heap object. This memory cost is smaller than Costs 1 and 2.

### Choices in other languages

From general knowledge, other languages balance type erasure and runtime metadata in four ways:

- The Java compiler also erases type arguments at compile time.
- The C# and .NET runtimes store generic type arguments on every object. Callers also pass runtime type metadata to generic code.
- The Swift compiler passes type metadata to generic functions as hidden arguments.
- The Rust and C++ compilers emit a separate copy of each generic function per concrete type. Each compiled copy knows all concrete types.

### Why dictionaries fit Sand

With dictionaries, callers pass metadata only to functions that require traits with methods. On `main`, we apply the same principle to `reflect` by passing type descriptors only for marked type parameters. Recording type arguments on values would add hidden metadata arguments to every generic call. Callers of generic functions that only manipulate values in memory would still pay Cost 1.

## Limits of the previous draft

By looking up implementations in standard dictionaries, we remove all four special cases from the previous draft. In the previous draft, the encode walker looked up custom implementations from runtime value types. That lookup worked only for concrete types and required four special cases:

- **Dictionary exception in the checker.** The checker skipped dictionaries for `Encode` and `Decode` despite their declared methods.
- **Method slots on type definitions.** The checker stored custom `encode` and `decode` method slots on each type definition.
- **Method slots in the VM type table.** The VM stored custom `encode` and `decode` method slots on each struct and enum record.
- **Concrete-type rule.** The checker allowed custom `Encode` and `Decode` implementations only on concrete structs and enums. Each type definition had a single fixed slot for all type arguments.

## Part 1: Native default methods

In the prelude, we declare `Encode` and `Decode` with native default methods:

```sand
/// Encodes values with `json.encode`.
pub trait Encode {
    /// Writes `self` to `encoder` as a single value.
    /// Implementors call `encoder.encode(value)`.
    native fn encode(self, encoder: Encoder)
}

/// Decodes values with `json.decode`.
pub trait Decode: Encode {
    /// Decodes a value of `Self` from `decoder`.
    /// Implementors call `decoder.decode()`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

A native default method finds part implementations through `Self`'s dictionary. On `main`, callers pass `Self`'s dictionary to every trait method as its first hidden argument. The walker reads that dictionary to look up each part's implementation.

## Part 2: Structural implementations

The type checker generates all structural implementations from the type kinds registered with each native default method. Rust setup code registers each native default with its supported type kinds. In the prelude, we declare only the traits themselves.

### Type kinds

We define the Rust enum `TypeKind` to list the type kinds that a native default can support:

- Built-in scalar kinds include `Int`, `Str`, and `Bytes`.
- Built-in constructor kinds include `List`, `Map`, and `Tuple`.
- User-defined composite kinds are `Struct` and `Enum`.
- `NativeValue` covers native value types such as `time.PlainDate`.
- `Never` covers the `never` type.

The compiler stores a `TypeKind` variant inside each `ImplShape` constant in Part 3.

### Registration

Rust setup code passes a slice of `TypeKind` values at native default registration. For example, prelude setup registers `Encode.encode` with every kind that the encode walker supports.

The checker processes each registered `TypeKind` to create structural implementations:

- **Built-in kinds.** During trait declaration, the checker creates a structural implementation for each built-in kind. It requires every type argument to implement the trait. For `List`, the checker creates `impl<T: Encode> Encode for [T] {}`.
- **`Struct` and `Enum` kinds.** If the list includes `Struct` or `Enum`, the checker derives a structural implementation for each qualifying struct or enum. Its derivation pass computes the bounds on each type parameter from the field and payload types. For `struct Pair<A, B> { first: A, second: [B] }`, the checker generates `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.
- **Derivation rules.** Rust setup code can attach a rule to any registered kind. The checker runs that rule before it derives an implementation. For `Decode`, the rule on `Struct` rejects structs with `init` constructors. The checker reads this rule from the registration entry rather than matching the trait name.

If a user writes a custom implementation for a type, the checker uses the custom implementation.

The checker registers structural implementations as ordinary implementations. Its trait resolver then checks `Encode` and `Decode` bounds with standard lookup.

### Consistency checks

Two compile-time checks keep registered `TypeKind` lists aligned with walkers:

- **Exhaustive `TypeKind` matching.** Each walker matches on the `TypeKind` field in `ImplShape`. The Rust compiler requires that match to cover every `TypeKind` variant. If a developer adds a variant, the Rust compiler reports an incomplete match in the walker.
- **Supertrait verification.** The checker verifies supertrait bounds on all structural and custom implementations. Because `Decode` has supertrait `Encode`, prelude setup must register every `Decode` kind on `Encode` too. If prelude setup registers an extra kind only on `Decode`, the checker rejects the prelude in every test run.

### Completeness for built-in types

Prelude registration alone determines which built-in types implement `Encode` and `Decode`. Under the orphan rule, a module can implement a prelude trait only for types declared in that module. Only the prelude can implement prelude traits for built-in types. The checker rejects user implementations on built-in types.

### Scope: `Eq`, `Ord`, and `Hash`

We update only `Encode` and `Decode` in this branch. We keep `Eq`, `Ord`, and `Hash` on their current derivation path. For those three traits, the checker continues to read the type table in `crates/sandlang/src/check/derive.rs`. We can migrate `Eq`, `Ord`, and `Hash` to native default methods in a follow-up pull request. At that point, we will delete the table in `crates/sandlang/src/check/derive.rs`.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries store extra metadata

Structural dictionaries store extra metadata so native default methods can look up concrete field and payload types. On `main`, the compiler places child dictionaries in a dictionary only for generic type parameters. For `struct User { email: Email }`, `User` takes concrete field `Email` directly. A standard `User` dictionary stores only `User`'s method. The walker needs another record of `Email`'s `Encode` dictionary.

### Implementation shape

The compiler emits a constant `ImplShape` for each structural implementation. It places the implementation shape right after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., implementation shape, bound dictionaries...]
```

Ordinary trait calls read only the method functions and supertrait dictionaries. For custom implementations, the compiler emits the standard dictionary layout. Only native default methods read the implementation shape.

Each `ImplShape` struct has two fields:

- **Type kind.** The compiler stores a `TypeKind` variant from Part 2 in this field. The walker reads this variant to choose the value representation. On `Struct` and `Enum` variants, the compiler also stores field or variant identifiers and field defaults. On `NativeValue` variants, the compiler stores a text parser.
- **Part templates.** The compiler stores a `PartTemplate` for each part in this field. The walker evaluates each template to get that part's dictionary.

### Part templates

We define `PartTemplate` with three variants:

```rust
enum PartTemplate {
    /// Loads a constant dictionary or shape from the constant pool.
    Constant(u32),
    /// Reads a bound dictionary from a slot in the enclosing dictionary.
    Bound(u32),
    /// Builds a dictionary tuple from element templates.
    Tuple(Vec<PartTemplate>),
}
```

The compiler and walker handle each variant by part form:

- **Concrete field.** For field `email: Email` in `impl Encode for User`, the compiler emits `Constant(index)` for `Email`'s constant `Encode` dictionary. The walker loads that constant directly.
- **Direct type parameter.** In `impl<T: Encode> Encode for [T]`, the compiler emits `Bound(0)`. The walker reads `T`'s dictionary from the first bound slot of the enclosing dictionary.
- **Nested generic field.** For field `items: [T]` in `impl<T: Encode> Encode for Page<T>`, the compiler emits `Tuple([list_method, list_shape, Bound(0)])`. The walker evaluates those three elements. It then builds the `[T]` dictionary from them.

### How the compiler builds part templates

The compiler lowers `PartTemplate` values from the checker's trait-evidence trees. For method bodies on `main`, the checker builds an expression tree of dictionary tuples, slot reads, and constant loads. The compiler lowers that tree to bytecode on `main`. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same expression tree. The compiler then converts that tree into a `PartTemplate`.

### Worked example

With part templates, the native encoder reaches custom methods across erased type arguments. At runtime, a `Page<Email>` object records only `Page`. The walker reaches `Email.encode` through dictionaries:

```sand
struct Email { address: str }

impl Encode for Email {
    fn encode(self, encoder: Encoder) { encoder.encode(self.address) }
}

struct Page<T> { items: [T] }

fn format_page<T: Encode>(page: Page<T>) -> str { json.encode(page) }

format_page(Page(items: [Email(address: "a@b.c")]))
```

Here, the user defines a custom `Encode` implementation on `Email`. The checker derives a structural implementation on `Page<T>`. The native encoder is the native default method `Encode.encode`:

- **Step 1: Incoming bound dictionary.** Because `format_page` requires `T: Encode`, its caller passes `Email`'s `Encode` dictionary. That dictionary is a 1-item tuple containing `Email.encode`.
- **Step 2: Dynamic `Page<T>` dictionary.** Inside `format_page`, the call `json.encode(page)` needs `Page<T>: Encode`. For generic `T`, `format_page` builds a 3-item `Page<T>` dictionary tuple at runtime:
  1. Slot 0 stores the native encoder method.
  2. Slot 1 stores the constant `ImplShape` for `Page`.
  3. Slot 2 stores `T`'s `Encode` dictionary from Step 1. In this call, slot 2 holds `Email`'s dictionary.
- **Step 3: `Page` part template.** `Page`'s `ImplShape` stores a `PartTemplate` for `items`. That template tells the walker how to build a 3-item `[T]` dictionary tuple:
  1. Slot 0 stores the native encoder method.
  2. Slot 1 stores the constant `ImplShape` for lists.
  3. Slot 2 is `Bound(0)`. The walker resolves `Bound(0)` to slot 2 of the enclosing `Page` dictionary.
- **Step 4: Runtime walk.**
  1. `json.encode` calls the native encoder with `page` and the `Page` dictionary.
  2. The native encoder reads slot 1 of the `Page` dictionary to get `Page`'s `ImplShape`.
  3. At `items`, the native encoder evaluates `items`'s part template to build the list dictionary. It copies `Email`'s dictionary from slot 2 of the `Page` dictionary into slot 2 of the list dictionary.
  4. Slot 0 of the list dictionary holds the native encoder itself. The walker processes the list directly in Rust.
  5. For each list element, the walker reads `Bound(0)` from the list `ImplShape`. It loads slot 2 of the list dictionary to get `Email`'s dictionary.
  6. Slot 0 of `Email`'s dictionary holds the custom method `Email.encode`. The walker calls `Email.encode` through a callback.

## Part 4: Recursive types

### Cyclic dictionary references

Recursive type definitions create reference cycles between dictionaries and implementation shapes. In `struct Tree { children: [Tree] }`, `Tree`'s `ImplShape` refers to the `[Tree]` dictionary. The `[Tree]` dictionary stores `Tree`'s dictionary as a bound. `Tree`'s dictionary stores `Tree`'s `ImplShape`.

### Building part dictionaries on demand

The walker breaks dictionary cycles by building a part's dictionary only if the walk visits a value of that part:

- **Constant dictionaries.** `PartTemplate::Constant` stores an integer index into the constant pool. The VM resolves that index during the walk. Before lowering an `ImplShape`'s part templates, the compiler reserves the constant slot for that shape and emits `Tree`'s constant dictionary tuple. For recursive references, the compiler emits the index of `Tree`'s dictionary. Every loaded tuple in the constant pool stays acyclic.
- **Dynamic dictionaries.** For generic `Tree<T>`, the compiler emits the finite template `Tuple([list_method, list_shape, Tuple([tree_method, tree_shape, Bound(0)])])` for `children`. The walker builds `[Tree<T>]`'s dictionary at `children`. As the walker visits each child element, it builds that child's `Tree<T>` dictionary.
- **Polymorphic recursion.** A recursive type can wrap its type parameter in a nested type, as in `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. At each `Deep` value, the walker evaluates `Deep`'s template. That evaluation builds `Nest<[T]>`'s dictionary from `T`'s dictionary. The walker builds a single dictionary per `Deep` layer in the runtime value.

### Per-walk dictionary cache

During a walk, the walker caches dynamic part dictionaries by shape ID, part index, and parent dictionary. While encoding a list of 10,000 `Tree<T>` values inside a generic function, the walker builds the `Tree<T>` dictionary once.

### Rejected recursion schemes

We rejected two alternative schemes:

- **Store part dictionaries inside every dictionary.** For recursive types, the compiler would have to build cyclic tuples. On polymorphic recursion, the compiler would expand nested dictionaries forever.
- **Generate a Sand function that returns part dictionaries.** For each uncached lookup, the walker would run a Sand callback of about 90 ns. Callers encoding types that use only native defaults would still pay that cost.

From general knowledge, other language runtimes also build dictionaries on demand:

- The Swift runtime builds generic witness tables on demand and caches them.
- GHC compiles recursive typeclass dictionaries as lazy thunks.

## Part 5: Native walkers

### Encode walker

The encode walker copies a Sand value into a host `Value`. It tracks open containers and their dictionaries on an explicit `Vec` stack. For each part, the walker resolves the part dictionary from its `PartTemplate`. It then checks the method in that dictionary:

- **Native default method.** The walker copies the part directly in Rust.
- **Custom method.** For any custom Sand or native method, the walker writes a placeholder into `Value`. It also records a hole with the target `Value` slot, the rooted part value, and the rooted part dictionary.

After the structural pass, the native function runs a callback for each hole. Each custom `encode` method writes a value to an `Encoder`. The walker encodes that value into the hole's slot.

### Decode walker

The decode walker builds a Sand value from a `Value` according to the `TypeKind` in each `ImplShape`. The walker records a hole in two cases:

- The target part's type defines a custom `Decode` implementation.
- A struct field declares a computed default, such as `id: int = next_id()`. The walker records a hole if the input lacks that field.

If the walker records any holes, it runs in two passes:

1. In pass 1, the walker decodes and validates the input. It records every hole.
2. Between passes, the native function runs a callback for each hole and roots the resulting Sand values.
3. In pass 2, the walker decodes the input again and inserts the saved results.

With two passes, we keep two guarantees from `main`. First, the walker validates the full input before running any computed field defaults. It then runs custom `Decode` holes and computed field defaults in post-order. Second, the walker hashes finished values before inserting elements into sets and maps.

### Synchronous return

If every visited part uses the native default, the native function returns a synchronous result directly. The engine allocates a future only if the walker records any holes. The VM takes about 10 ns for a synchronous native call. It takes about 112 ns to poll an async native future that finishes on its first poll.

## Part 6: Type checker and compiler changes

1. **Native default methods.** The checker treats a `native fn` method in a `trait` declaration as a native default method. On `main`, default trait methods must have Sand block bodies.
2. **Custom and structural implementations.** We allow user `impl` blocks on `Encode` and `Decode` under `docs/SPEC.md` §4.9. The checker generates structural implementations from each native default's `TypeKind` list, as Part 2 specifies. For `Hash`, the checker keeps its existing rule for `var` fields.
3. **Part templates.** The checker builds part trait-evidence trees. The compiler lowers them to `PartTemplate` constants, as Part 3 specifies.
4. **Structural implementations for `never`.** By registering `TypeKind::Never`, we give `never` a structural implementation with shape kind `Never`. Under `docs/SPEC.md` §5.1 on `main`, `never` implements only traits with empty bodies. Expressions such as `json.encode([])` require `[never]: Encode`. The decode walker always returns an error for `TypeKind::Never`. Because `never` is uninhabited, `[never]: Encode` only meets type bounds on empty containers.
5. **Dictionary parameters for native functions.** Generic native functions such as `json.encode<T: Encode>` receive `T`'s dictionary through a new `TraitDict<N>` parameter wrapper. A Rust native function takes `TraitDict<N>` to read a bound dictionary. `TraitDict<N>` follows the design of `Num<N>`.
6. **Removal of `reflect`.** `Decode` dictionaries store `ImplShape` metadata. Decoders read target type structure from those dictionaries instead of `reflect` type descriptors. On `main`, only the `json` module uses `reflect` in the standard library. We delete `reflect` from the language.

The checker applies its two existing rules for traits with methods to `Encode` and `Decode`:

- **Local functions.** On local functions, the checker rejects bounds on traits with methods. It reports `local functions cannot require traits with methods; declare the function at the top level` for local `Encode` or `Decode` bounds.
- **Struct field defaults.** Under `docs/SPEC.md` §4.2, the checker rejects calls inside field defaults that use a struct type parameter's bound on a trait with methods.

See [Open questions](#open-questions) on keeping both rules and deleting `reflect` in this branch.

## Part 7: JSON and `Value`

- **JSON parsing.** The JSON parser converts JSON text to a `Value`. It maps JSON objects to string-keyed maps. It maps JSON `null` to `Value::Unit`.
- **JSON writing.** The JSON writer formats a `Value` as JSON text. For example:
  - It writes `Some(inner)` directly as `inner`.
  - It writes `bytes` values as Base64 strings.
  - It writes map keys and native value types as strings.
- **JSON-compatible decoding.** The decode walker accepts JSON-formatted `Value` trees directly. It decodes Base64 strings into `bytes` and parses string keys for `int`-keyed maps. Every `Value` produced from valid JSON text is valid input for matching `Decode` types.
- **`json.Json` implementations.** We implement `Encode` and `Decode` on `Json` with dedicated native methods:

  ```sand
  impl Encode for Json { native fn encode(self, encoder: Encoder) }
  impl Decode for Json { native fn decode(decoder: Decoder) -> Result<Json> }
  ```

  Each method converts an entire `Json` tree to or from `Value` in a single native call. Inside `json.parse(text)`, we call `json.decode<Json>(text)`.
- **`Vm::to_value` and `NativeContext::to_value`.** Callers pass an `Encode` dictionary alongside the Sand value to `Vm::to_value`. `Vm::to_value` runs synchronously. If it reaches a custom `Encode` implementation, it returns an `"invalid"` error. We add an async `NativeContext::to_value` method to run custom `Encode` implementations through callbacks.

## Updates to `SPLIT.md` changesets 14, 15, and 18

In `SPLIT.md`, we divide the work of a source branch into numbered changesets:

- **Changeset 14: `Value` and `Vm::to_value`.** We merged Changeset 14 in pull request #41. Here, we update two of its parts:
  - Callers pass an `Encode` dictionary to `Vm::to_value` so it can detect custom `Encode` implementations.
  - We replace the three separate walkers on `main` with the single encode walker. `json.encode` formats the `Value` output as JSON text. `json.to_json` converts the `Value` output to `Json`.
- **Changeset 15: `Encode` and `Decode` as ordinary traits.** We rename Changeset 15 to this title to cover Parts 1 through 7 of this design:
  - In `json.decode`, we parse JSON text into a `Value` and then run the decode walker.
  - We defer host-side `Value` decoding to Changeset 16. Changeset 16 adds `serde` traits on `Value` for Rust hosts.
- **Changeset 18: Host function calls.** In Changeset 18, the engine decodes each host `Value` argument with the decode walker. Host-callable script functions take concrete parameter types. The compiler emits a constant `Decode` dictionary for each parameter.

## Costs and risks

- **Dictionary passing.** Callers pass a dictionary to generic functions with `T: Encode` bounds. Callers pass a constant for concrete types. For constructed generic types, callers build a tuple at runtime. In the triage report, we measured generic trait calls at about 12 ns above direct calls.
- **Per-part dispatch.** For each part, the walker resolves the part dictionary and checks its method function. For concrete types, the walker loads program constants directly from `Constant` templates.
- **Intermediate `Value` allocation.** Both `json.encode` and `json.decode` build an intermediate `Value` tree. On `main`, `json.encode` writes JSON text directly from Sand values. Likewise, `json.decode` on `main` builds Sand values directly from JSON text. We will benchmark both functions to measure the time difference.
- **Memory accounting across callbacks.** The walkers count `Value` memory against `max_memory` during each structural pass. During a Sand callback, the VM counts callback roots against `max_memory`. The in-progress `Value` tree stays outside `max_memory` until the callback returns. In `TODO.md`, we track callback root growth under "Let an async native function release roots during a call." We also track sub-future memory under "Charge boxed recursive futures against `max_memory`."
- **Net codebase size.** We expect this design to reduce total code size. We will report the measured line delta in the pull request.

  We delete four subsystems:
  - We delete `crates/sandlang/src/compile/reflect.rs` and `reflect` support in the parser, checker, and VM.
  - We delete runtime type descriptors and their templates.
  - We delete the typed JSON decoder, the direct JSON encoder, and the `json.to_json` walker.
  - We delete the helper functions that run computed field defaults after decoding.

  We add six components:
  - We add the native encode and decode walkers.
  - We add `ImplShape` constants and `PartTemplate` lowering.
  - We add native default trait methods.
  - We add `TypeKind` registration for native defaults.
  - We generate structural implementations in the type checker.
  - We add the `TraitDict<N>` parameter wrapper for native functions.

## Rejected alternatives

- **Runtime lookup table.** A runtime table indexed by trait and type would duplicate dictionary dispatch. Because the compiler erases type arguments, runtime lookup from values works only for concrete types. Callers of a decoder would also have to pass runtime type descriptors.
- **Generated Sand methods for all derived implementations.** Swift uses this model. Under it, compiler-generated methods call a trait method for every part. In the triage report, Sand-only codecs took 2.3 to 7 times as long as native walkers.
- **Dual generated Sand codecs and native walkers.** With two codec paths, developers must keep output, error paths, and retry behavior identical across both. In the triage report, we audited `origin/encode-decode-traits`. Over a quarter of its 8,210 added lines kept those two paths aligned.

## Reusable parts from the previous draft

We keep three parts from our previous uncommitted draft for `encode-decode-traits-v2`:

- **Encode and decode walkers.** We keep the walkers from that draft, including holes, two-pass decoding, and JSON error paths such as `$.items[2].price`. We change only implementation lookup to read trait dictionaries.
- **`Value`-based JSON parser and writer.** We keep the JSON parser, the JSON writer, and the native `Json` trait methods.
- **Computed field defaults as decode holes.** The decode walker evaluates computed field defaults through holes instead of running separate helper functions after decoding.

We drop all four special cases from [Limits of the previous draft](#limits-of-the-previous-draft).

## Test and measurement plan

- **Existing JSON behavior.** We keep existing test output in `crates/sandlang/tests/spec/std_json.test`. We will list every planned error-message change in this document.
- **New spec tests:**
  - We test custom `Encode` and `Decode` implementations on structs and enums.
  - We test generic custom implementations, such as `impl<T: Encode> Encode for Box<T>`.
  - We test direct recursive types and polymorphic recursion.
  - We test `json.encode([])` with `[never]: Encode`.
  - We test composite values whose outer and inner types both define custom implementations.
  - We test async custom implementations that wait on `task.sleep`.
  - We test faulty `Encode` implementations that skip `encoder.encode` or call it twice.
  - We test error paths from failed custom `Decode` implementations.
- **Robustness checks:**
  - We enable `Config::retry_stress` in tests to force retries at resource limits.
  - On large inputs, we compare resident memory against `max_memory` and against matching Sand loops.
  - We encode and decode deeply nested values on a 512 KiB thread stack.
  - We encode values with many custom-encoded parts.
- **Benchmarks:**
  - We benchmark `json` functions against `main` with `bench/run.py` and `crates/sandlang/benches/codec.rs`.
  - We measure call overhead for generic functions with `Encode` bounds.

## Open questions

1. **Should we keep the existing rules on local functions and field defaults in this branch?** Yes. Keeping both rules makes `Encode` and `Decode` match all other traits with methods.
2. **Should we delete `reflect` in this branch?** Yes. With `reflect` deleted, `Decode` dictionaries are the single source of runtime type metadata. Deleting `reflect` also removes `reflect` code from the parser, checker, compiler, and VM.
