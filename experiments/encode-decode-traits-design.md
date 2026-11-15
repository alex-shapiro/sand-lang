# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** We specify this design before implementation. See [Reusable parts from the previous draft](#reusable-parts-from-the-previous-draft) for code that we keep from the previous `encode-decode-traits-v2` draft.

## Summary

We propose making `Encode` and `Decode` ordinary traits with native default methods. Users can implement both traits for concrete and generic types. The compiler erases type arguments from functions. Callers pass trait dictionaries to generic functions as hidden arguments. See [Why `Encode` and `Decode` need dictionaries](#why-encode-and-decode-need-dictionaries) for a comparison with `main`.

Each trait declares a native Rust walker as its default method. The type checker generates ordinary implementations for built-in types, structs, and enums. Those generated implementations use the default methods. Users can write custom implementations for their own types to override generated defaults.

During a walk, the walker looks up each part's implementation in trait dictionaries. For a part that uses the native default, the walker processes that part in Rust. For a part with a custom implementation, the walker runs a callback to the custom method.

The VM runs `Encode` and `Decode` like any other trait with native default methods. We meet four goals with this design:

- **Host `Value` conversion.** The encode walker writes a Sand value to a `Value` sink. The decode walker builds a Sand value from a `Value` source.
- **JSON encoding and decoding.** The encode walker writes JSON text directly to a JSON sink. The decode walker reads JSON text directly from a JSON source.
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
- **Walker.** A walker is a Rust routine that visits a value and its parts in turn. The encode walker reads a Sand value from the VM. The decode walker builds a Sand value in the VM. If a walker reaches a part that needs Sand code, it waits for a callback at that point and then continues.
- **Encode sink.** An encode sink is a Rust object that receives output from the encode walker. The walker makes requests such as "write bytes" or "begin a struct with these fields." Each output format has its own encode sink.
- **Decode source.** A decode source is a Rust object that supplies input to the decode walker. The walker asks the source for a value of a specific kind, such as bytes or a map key of type `int`. Each input format has its own decode source.
- **`Value`.** `sandlang::value::Value` is a host-owned Rust type that represents a Sand value outside the VM. A Rust host can build a `Value` or keep a `Value` after a VM run ends.
- **Callback.** An async native function can call a Sand function through a callback. We added callbacks in commit `8a51c46`. In `experiments/native-async-sand-calls.md`, we measured each callback round trip at about 90 ns.
- **Triage report.** In `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`, we analyzed the speed and complexity problems in earlier codec designs.

## Why `Encode` and `Decode` need dictionaries

To support generic custom implementations, the compiler passes dictionaries for `Encode` and `Decode`. On `main`, the prelude declares both traits with empty bodies. The type checker rejects custom implementations of both traits.

### `Encode` on `main`

On `main`, `json.encode` calls a single native encoder for all types. The VM stores runtime tags on values that this encoder reads:

- Immediate value slots tag scalars and inline enum variants.
- Heap objects tag their container or composite kind.
- Struct objects store the ID of their type definition.
- Enum values store the ID of their variant.
- The VM type table maps each struct and variant ID to its identifiers.

The native encoder walks values directly from these runtime tags. At compile time, the checker verifies `T: Encode`. That check rejects functions, tasks, channels, and native objects. The compiler then erases the bound.

### `Decode` on `main`

Callers of a decoder must supply the target type because the decoder starts from raw text. On `main`, callers pass a runtime type descriptor to `json.decode<reflect T: Decode>` through its `reflect` type parameter. That hidden argument supplies the type structure that the compiler erased.

### Why custom implementations require dictionaries

We pass dictionaries for `Encode` and `Decode` for five reasons:

1. Users can write custom `Encode` implementations.
2. The native encoder must call custom methods for matching parts of a value.
3. A heap object omits its type arguments. See [Why heap objects omit type arguments](#why-heap-objects-omit-type-arguments).
4. In `impl<T: Encode> Encode for Box<T>`, the encoder must look up `T`'s `Encode` implementation at runtime.
5. Callers pass `T`'s implementation through a hidden trait dictionary.

## Why heap objects omit type arguments

The VM omits type arguments from heap objects to avoid three runtime costs. If every value also stored its type arguments, the native encoder could look up generic implementations directly from values.

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
- `Struct` and `Enum` cover structs and enums that Sand source declares, including prelude types such as `Option`.
- `NativeValue` covers native value types such as `time.PlainDate`.
- `Never` covers the `never` type.

The compiler stores a `TypeKind` variant inside each `ImplShape` constant in Part 3.

### Registration

Rust setup code passes a slice of `TypeKind` values at native default registration. For example, prelude setup registers `Encode.encode` with every kind that the encode walker supports.

The checker processes each registered `TypeKind` to create structural implementations:

- **Built-in kinds.** During trait declaration, the checker creates a structural implementation for each built-in kind. It requires every type argument to implement the trait. For `List`, the checker creates `impl<T: Encode> Encode for [T] {}`.
- **`Struct` and `Enum` kinds.** If the list includes `Struct` or `Enum`, the checker derives a structural implementation for each qualifying struct or enum. Its derivation pass computes the bounds on each type parameter from the field and payload types. For `struct Pair<A, B> { first: A, second: [B] }`, the checker generates `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.
- **Structs that declare `init`.** For `Decode`, the `Struct` kind also covers structs that declare `init`. The decode walker constructs such a struct through its `init` method. See [Decoding a struct that declares `init`](#decoding-a-struct-that-declares-init). The checker derives `Decode` for such a struct if every parameter type of `init` implements `Decode`.

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

- **Type kind.** The compiler stores a `TypeKind` variant from Part 2 in this field. The walker reads this variant to choose the value representation. On `Struct` and `Enum` variants, the compiler also stores field or variant identifiers and field defaults. For a struct that declares `init`, the `Decode` shape stores the parameters of `init` instead of the fields. It also stores the `init` function. On `NativeValue` variants, the compiler stores a text parser.
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

Each walker visits a value in a single pass. If the walker reaches a part that needs Sand code, it waits for a callback at that point. After the callback, the walker continues from the same position. This design matches the architecture of `serde`: walkers write to a sink or read from a source, and each format supplies the sink or the source.

### Walker state across callbacks

Each walker keeps an explicit stack of open containers in a `Vec`. It does not recurse on the Rust stack. During a callback, Sand code can run the garbage collector. The collector does not trace heap values that a Rust variable holds. The walker therefore keeps its heap values in a walk object on the VM heap. The collector traces that object. The walk object holds three groups of values:

- The containers that the walker is visiting
- The partly built containers of the decode walker
- The dictionaries of the open containers

The heap does not move objects. A heap value in the walk object therefore keeps its handle across a callback.

A callback can also change a container that the walker is visiting. For example, a custom `encode` method can remove an element from a list that contains its value. The walker reads each child at the time it reaches that child. Rooting keeps every container alive, so a changed container gives changed output, not invalid memory access. `TODO.md` already tracks detection of such changes: "Detect collection changes during an async native walk."

### Synchronous start

A native function runs its walker synchronously first. If the walk never needs a callback, the native function returns its result directly. It allocates no future. The VM takes about 10 ns for a synchronous native call. It takes about 112 ns for an async native call whose future finishes on its first poll.

If the walker needs a callback, it moves its state into a future at that point. The explicit stack makes that move possible: the walker state is data, not Rust stack frames. In `experiments/native-async-sand-calls.md`, a walker of this form ran derived types at the speed of the synchronous walker on `main`. A custom field added about 340 to 360 ns per row.

### Encode walker

The encode walker reads a Sand value and writes it to an encode sink. For each part, the walker gets the part dictionary from its `PartTemplate`. It then checks the method in that dictionary:

- **Native default method.** The walker writes the part to the sink in Rust.
- **Custom method.** The walker creates an `Encoder` that wraps the same sink. It calls the custom method through a callback. The custom method writes a value to the `Encoder` with `encoder.encode(value)`. That call runs the encode walker on `value` with the same sink. The value therefore appears in the output at the position of the part.

An `Encoder` accepts a single value. If a custom method writes no value or a second value, the encode fails with kind `"invalid"`. [Open question 4](#open-questions) asks whether a return value should replace this runtime check.

### Encode sink interface

The encode sink has a method for each kind of value in the data model:

- Scalars: unit, `bool`, `int`, `float`, `str`, `bytes`, and `duration`
- Sequences: begin a list, set, or tuple with its length, then end it
- Maps: begin a map with its length, then end it
- Structs: begin a struct with its type name and field names, then end it
- Enum values: write a variant with its type name, variant name, and payload form
- Options: write `None`, or begin `Some`
- Native value types: write a value with its type name and text

The walker identifies `Option` by the ID of the prelude `Option` definition. The sink therefore receives option calls without comparing type names.

This design has two encode sinks:

- **The JSON sink** writes JSON text. It holds the JSON rules. For example, it writes `Some(inner)` as `inner`, `bytes` as a Base64 string, and map keys and native value types as strings.
- **The `Value` sink** builds a `Value`. It writes `Option` as a `Value::Variant` of the prelude type `Option`, as `Vm::to_value` does on `main`.

### Decode walker

The decode walker builds a Sand value according to the `TypeKind` in each `ImplShape`. It reads its input from a decode source. It waits for a callback in three cases:

- **Custom `Decode` implementation.** The walker creates a `Decoder` that wraps the same source at the current position. It calls the custom method through a callback. The method reads its input with `decoder.decode()`.
- **Computed field default.** A struct field declares a computed default, such as `id: int = next_id()`. If the input lacks that field, the walker calls the default function through a callback after it reads the other fields of the struct.
- **Struct that declares `init`.** See [Decoding a struct that declares `init`](#decoding-a-struct-that-declares-init).

Each callback finishes before the walker uses its result. Sets and maps therefore insert finished values and hash them correctly.

A `Decoder` reads a single value from its input. If a read fails, the source returns to the start of the part. The custom method can then try to read the part as another type. After the callback returns, the source skips any part of the input that the method did not read.

### Order of callbacks

The decode walker runs each callback at the point where it reaches the part. This order changes a guarantee from `main`. On `main`, `json.decode` runs computed field defaults only after the whole input decodes. In this design, a default or a custom method can run before a later part of the input fails to decode.

The spec test "decode runs the defaults that are not constants, as a construction does" checks the `main` guarantee. Its last line prints the number of calls after a failed decode. That number changes from 5 to 6, because the default for `inner.a` runs before the walker reaches the invalid `items` field. [Open question 5](#open-questions) asks whether to accept this change.

### Decode source interface

The decode source has a method for each kind of value that the walker can request:

- Scalars: unit, `bool`, `int`, `float`, `str`, `bytes`, and `duration`
- Sequences: begin a list, set, or tuple, read each element, and end it
- Maps: begin a map, read each key with the requested key kind, read each value, and end it
- Structs: begin a struct, read each field name and value, and end it
- Enum values: read a variant name, then its payload
- Options: read `None`, or begin `Some`
- Native value types: read the text of a value
- Position: save a position, return to it, and skip a value
- Error paths: format the path of the current position, such as `$.items[2].price`

This design has two decode sources:

- **The JSON source** reads JSON text as the walker makes requests. It holds the JSON rules. For example, it decodes a Base64 string if the walker requests bytes. It parses a string key as an `int` if the walker requests an `int` key.
- **The `Value` source** reads a `Value`. It accepts only the exact form of each kind. For example, it accepts `Value::Bytes` for bytes, and it rejects `Value::Str`.

The JSON rules therefore stay in the JSON sink and the JSON source. The walkers contain no format rules.

### Decoding a struct that declares `init`

On `main`, a struct that declares `init` does not implement `Decode`. A derived decoder would build the struct field by field. That construction would skip the validation in `init`. The native decoder on `main` cannot call `init`, because `init` is Sand code. Commit `8a51c46` removed that limit.

In this design, the decode walker constructs such a struct through `init`. The walker reads the parameters of `init` from the input, in the same way that it reads the fields of a struct. For example, `struct Port { n: int, fn init(n: int) -> Result<Port> }` decodes from the JSON object `{"n": 80}`. The walker then runs these steps:

1. It fills each missing argument. A missing argument of an `Option` type becomes `None`. A missing argument with a default uses that default. The compiler lowers a parameter default to an inline expression or to a helper function of the earlier parameters. The walker calls a helper function through a callback.
2. It calls `init` with the arguments through a callback.
3. It converts the result of `init`. If `init` returns `Self` or `Ok`, the walker uses the value. If `init` returns `Err(e)`, the decode fails at the path of the struct. If `e` is an `Error`, the decode error keeps its kind. For any other error type, the decode error uses kind `"invalid"` and the display text of `e`.

The derived `Encode` implementation of the struct still writes the fields. If the parameters of `init` match the fields, a value survives a round trip through JSON. If they differ, the user writes a custom `Encode` or `Decode` implementation.

From general knowledge, other libraries also decode through validating code:

- In Rust, the `serde` attribute `#[serde(try_from = "Type")]` decodes a value through a fallible conversion.
- In Kotlin, `kotlinx.serialization` runs the `init` blocks of a class during decoding.

## Part 6: Type checker and compiler changes

1. **Native default methods.** The checker treats a `native fn` method in a `trait` declaration as a native default method. On `main`, default trait methods must have Sand block bodies.
2. **Custom and structural implementations.** We allow user `impl` blocks on `Encode` and `Decode` under `docs/SPEC.md` §4.9. The checker generates structural implementations from each native default's `TypeKind` list, as Part 2 specifies. A struct that declares `init` derives `Decode` if every parameter type of `init` implements `Decode`. On `main`, such a struct never derives `Decode`. For `Hash`, the checker keeps its existing rule for `var` fields.
3. **Part templates.** The checker builds part trait-evidence trees. The compiler lowers them to `PartTemplate` constants, as Part 3 specifies.
4. **Structural implementations for `never`.** By registering `TypeKind::Never`, we give `never` a structural implementation with shape kind `Never`. Under `docs/SPEC.md` §5.1 on `main`, `never` implements only traits with empty bodies. Expressions such as `json.encode([])` require `[never]: Encode`. The decode walker always returns an error for `TypeKind::Never`. Because `never` is uninhabited, `[never]: Encode` only meets type bounds on empty containers.
5. **Dictionary parameters for native functions.** Generic native functions such as `json.encode<T: Encode>` receive `T`'s dictionary through a new `TraitDict<N>` parameter wrapper. A Rust native function takes `TraitDict<N>` to read a bound dictionary. `TraitDict<N>` follows the design of `Num<N>`.
6. **Removal of `reflect`.** `Decode` dictionaries store `ImplShape` metadata. Decoders read target type structure from those dictionaries instead of `reflect` type descriptors. On `main`, only the `json` module uses `reflect` in the standard library. We delete `reflect` from the language.

The checker applies its two existing rules for traits with methods to `Encode` and `Decode`:

- **Local functions.** On local functions, the checker rejects bounds on traits with methods. It reports `local functions cannot require traits with methods; declare the function at the top level` for local `Encode` or `Decode` bounds.
- **Struct field defaults.** Under `docs/SPEC.md` §4.2, the checker rejects calls inside field defaults that use a struct type parameter's bound on a trait with methods.

See [Open questions](#open-questions) on keeping both rules and deleting `reflect` in this branch.

## Part 7: JSON and `Value`

- **`json.encode`.** The encode walker writes the value to the JSON sink. `json.encode` builds no intermediate tree.
- **`json.decode`.** The decode walker reads the JSON text from the JSON source. `json.decode` builds no intermediate tree.
- **`json.Json` implementations.** We implement `Encode` and `Decode` on `Json` with dedicated native methods:

  ```sand
  impl Encode for Json { native fn encode(self, encoder: Encoder) }
  impl Decode for Json { native fn decode(decoder: Decoder) -> Result<Json> }
  ```

  `Json.encode` writes a whole `Json` tree to the sink of the `Encoder` in a single native call. `Json.decode` reads a whole JSON value from the source of the `Decoder` in a single native call. With the `Value` source, `Json.decode` converts the `Value` with the rules of the JSON sink. `json.parse(text)` calls `json.decode<Json>(text)`.
- **`json.to_json`.** `json.to_json(value)` writes the value with the JSON sink, and then parses the text into a `Json` tree. This path is slower than a direct conversion. It keeps the JSON rules in the JSON sink only.
- **`Vm::to_value` and `NativeContext::to_value`.** These functions run the encode walker with the `Value` sink. Callers pass an `Encode` dictionary alongside the Sand value. `Vm::to_value` runs synchronously. If it reaches a custom `Encode` implementation, it returns an `"invalid"` error. The async `NativeContext::to_value` method runs custom `Encode` implementations through callbacks. [Open question 3](#open-questions) asks whether to keep the synchronous function.

## Updates to `SPLIT.md` changesets 14, 15, and 18

In `SPLIT.md`, we divide the work of a source branch into numbered changesets:

- **Changeset 14: `Value` and `Vm::to_value`.** We merged Changeset 14 in pull request #41. Here, we update two of its parts:
  - Callers pass an `Encode` dictionary to `Vm::to_value` so it can detect custom `Encode` implementations.
  - On `main`, three separate walkers read Sand values: `json.encode`, `json.to_json`, and `Vm::to_value`. We replace them with the single encode walker and two encode sinks. Changeset 14 rejected a shared walker with a sink. That design "required about 300 extra lines and `#[inline(always)]` across two trait layers." Callbacks in the middle of a walk now require a single walker, so we accept that cost.
- **Changeset 15: `Encode` and `Decode` as ordinary traits.** We rename Changeset 15 to this title to cover Parts 1 through 7 of this design. Its original plan already used a decode walker that reads from a JSON source and a `Value` source. We defer host-side `Value` decoding to Changeset 16. Changeset 16 adds `serde` traits on `Value` for Rust hosts.
- **Changeset 18: Host function calls.** In Changeset 18, the engine decodes each host `Value` argument with the decode walker and the `Value` source. Host-callable script functions take concrete parameter types. The compiler emits a constant `Decode` dictionary for each parameter.

## Costs and risks

- **Dictionary passing.** Callers pass a dictionary to generic functions with `T: Encode` bounds. Callers pass a constant for concrete types. For constructed generic types, callers build a tuple at runtime. In the triage report, we measured generic trait calls at about 12 ns above direct calls.
- **Per-part dispatch.** For each part, the walker resolves the part dictionary and checks its method function. For concrete types, the walker loads program constants directly from `Constant` templates.
- **Sink and source dispatch.** The walkers call the sink and the source through Rust generics. The Rust compiler therefore builds a copy of each walker for each sink and each source.
- **Walker state across callbacks.** Before the first callback, the walker moves its state into a future, and it keeps its heap values in a walk object. The walk object grows with the nesting depth of the value, and the VM counts it against `max_memory` as a heap object. `TODO.md` tracks the memory of the future itself under "Charge boxed recursive futures against `max_memory`."
- **Callback order.** A custom method or a computed default can run before a later part of the input fails to decode. See [Order of callbacks](#order-of-callbacks).
- **Net codebase size.** We expect this design to reduce total code size. We will report the measured line delta in the pull request.

  We delete four subsystems:
  - We delete `crates/sandlang/src/compile/reflect.rs` and `reflect` support in the parser, checker, and VM.
  - We delete runtime type descriptors and their templates.
  - We delete the three separate encode walkers and the typed JSON decoder.
  - We delete the helper functions that run computed field defaults after decoding.

  We add these components:
  - We add the native encode and decode walkers.
  - We add the encode sink and decode source interfaces, with a JSON implementation and a `Value` implementation of each.
  - We add `ImplShape` constants and `PartTemplate` lowering.
  - We add native default trait methods.
  - We add `TypeKind` registration for native defaults.
  - We generate structural implementations in the type checker.
  - We add the `TraitDict<N>` parameter wrapper for native functions.

## Rejected alternatives

- **Runtime lookup table.** A runtime table indexed by trait and type would duplicate dictionary dispatch. Because the compiler erases type arguments, runtime lookup from values works only for concrete types. Callers of a decoder would also have to pass runtime type descriptors.
- **Generated Sand methods for all derived implementations.** Swift uses this model. Under it, compiler-generated methods call a trait method for every part. In the triage report, Sand-only codecs took 2.3 to 7 times as long as native walkers.
- **Dual generated Sand codecs and native walkers.** With two codec paths, developers must keep output, error paths, and retry behavior identical across both. In the triage report, we audited `origin/encode-decode-traits`. Over a quarter of its 8,210 added lines kept those two paths aligned.
- **Deferred callbacks through an intermediate tree.** An earlier version of this design never waited for a callback inside a walk. The encode walker built a `Value` tree with placeholders and filled them after the walk. The decode walker decoded its input twice. This version built an intermediate tree for every `json.encode` and `json.decode` call. It also needed placeholders, child-index paths, and a second decode pass. It put JSON rules into the format-neutral decode walker, because a JSON parser that runs without the target type cannot tell bytes from text.

## Reusable parts from the previous draft

We keep three parts from our previous uncommitted draft for `encode-decode-traits-v2`:

- **The JSON text parser and string escaping.** The JSON source reuses the parser. The JSON sink reuses the string escaping.
- **The `Value` copy logic.** The `Value` sink reuses the logic that copies native value types, type names, and struct descriptors.
- **Error path formatting.** The decode sources reuse the formatting of paths such as `$.items[2].price`.

The draft walkers recorded holes and decoded twice. This design replaces them. We drop all four special cases from [Limits of the previous draft](#limits-of-the-previous-draft).

## Test and measurement plan

- **Existing JSON behavior.** We keep existing test output in `crates/sandlang/tests/spec/std_json.test`, except for the callback order change in [Order of callbacks](#order-of-callbacks). We will list every planned error-message change in this document.
- **New spec tests:**
  - We test custom `Encode` and `Decode` implementations on structs and enums.
  - We test generic custom implementations, such as `impl<T: Encode> Encode for Box<T>`.
  - We test direct recursive types and polymorphic recursion.
  - We test `json.encode([])` with `[never]: Encode`.
  - We test composite values whose outer and inner types both define custom implementations.
  - We test async custom implementations that wait on `task.sleep`.
  - We test faulty `Encode` implementations that skip `encoder.encode` or call it twice.
  - We test error paths from failed custom `Decode` implementations.
  - We test a custom `Decode` implementation that tries a second type after a failed read.
  - We test decoding through `init`, including missing arguments with defaults and `init` errors.
  - We test the `Value` source with input in JSON form, such as `Value::Str` for `bytes`. The decode must fail.
  - We test a custom `encode` method that changes a list that the walker is visiting.
- **Robustness checks:**
  - We enable `Config::retry_stress` in tests to force retries at resource limits.
  - On large inputs, we compare resident memory against `max_memory` and against matching Sand loops.
  - We encode and decode deeply nested values on a 512 KiB thread stack.
  - We encode and decode values with many custom-encoded parts, and we force a garbage collection in each callback.
- **Benchmarks:**
  - We benchmark `json` functions against `main` with `bench/run.py` and `crates/sandlang/benches/codec.rs`.
  - We measure call overhead for generic functions with `Encode` bounds.

## Open questions

1. **Should we keep the existing rules on local functions and field defaults in this branch?** We recommend yes. Keeping both rules makes `Encode` and `Decode` match all other traits with methods.
2. **Should we delete `reflect` in this branch?** We recommend yes. With `reflect` deleted, `Decode` dictionaries are the single source of runtime type metadata. Deleting `reflect` also removes `reflect` code from the parser, checker, compiler, and VM.
3. **Should we delete the synchronous `Vm::to_value`?** We recommend yes. Its result depends on script code. A host call works until a script author adds a custom `Encode` implementation, and then the call fails at runtime. With a single async function, every conversion can run custom implementations.
4. **Should a custom `encode` method return its value instead of writing it?** With `fn encode(self) -> Encoded`, a method returns exactly one value, so the type checker enforces the rule. `Encoded` would be an opaque prelude type built from any `Encode` value, as in `Encoded(self.address)`. This change removes the `Encoder` type and its two runtime errors. We recommend yes.
5. **Should we accept the callback order change?** We recommend yes. Callbacks then run in the same order as Sand code that builds the same value. The alternative is a syntax check of the whole JSON text before the walk. That check would still let a type error after a callback leave the callback's side effects in place.
