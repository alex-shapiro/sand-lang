# Design: `Encode` and `Decode` as Ordinary Traits

We propose to make `Encode` and `Decode` ordinary traits that users can implement. Each trait declares a native Rust default method for derived implementations. Users can use the default methods or write custom implementations.

## Goals

Design goals:

- `Encode` and `Decode` follow the same rules as every other trait.
- `Encode` and `Decode` implementations are composable and nestable.
- A user can implement `Encode` and `Decode` for concrete, generic, and native types.
- A script can encode any `Encode` value as JSON text. It can decode JSON text into any `Decode` type.
- Host code can encode any `Encode` value into a `Value`. It can decode a `Value` into any `Decode` type.
- The walkers encode and decode `json.Json` under the same rules as every other type.
- The checker, the compiler, and the VM use the same mechanism for every trait with a native default method.
- Encoding and decoding stay close to the speed of `main`.

## Architecture

### Static trait dictionaries over runtime type inspection

The compiler resolves `Encode` and `Decode` from static types. Callers pass trait dictionaries to generic functions. A `Decode` dictionary describes the structure of the target type. A host function that decodes into a type parameter requires `Decode`.

The walkers cannot resolve generic implementations or `Decode` implementations from heap value tags. The VM omits type arguments from runtime values. An empty `[Email]` list has the same heap representation as an empty `[int]` list. An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. If the VM recorded type arguments on every value, every generic call would pass hidden descriptors. The VM would also allocate heap objects for inline values such as `None`. The decode walker also cannot inspect a value tag to select a `Decode` implementation. The decode walker reads raw input before any Sand value exists.

With trait dictionaries, callers pass a hidden argument only to generic functions with an `Encode` or `Decode` bound.

### Native walkers with callbacks over generated Sand codecs

Each trait declares a native Rust default method that walks structural values. The walker processes derived parts in Rust. It awaits a Sand callback for custom methods, `init` constructors, and computed field defaults.

During a callback, the walker keeps its position in an explicit stack of frames on the heap. Only walks with a callback below the root use this stack. See [Cached plans over per-part dictionary reads](#cached-plans-over-per-part-dictionary-reads).

We rejected four alternatives:

- **Recursive async walkers.** A recursive async walker keeps its position in nested futures. Each poll recurses once for each nesting level. A prototype overflowed a 512 KiB thread stack at 500 levels.
- **Generated Sand methods.** In benchmarks, generated Sand `encode` and `decode` methods took 2.3 to 7 times as long as native walkers.
- **Dual native and Sand codecs.** With both generated Sand codecs and native walkers, the engine duplicates output formatting, error paths, and memory-limit retries. In an earlier prototype, over a quarter of the new code kept the two paths aligned.
- **Two-pass walks with placeholders.** In a two-pass design, the walker builds an intermediate value tree with placeholders on every JSON call. That tree uses extra memory. Without format rules, the walker also cannot distinguish Base64 bytes from plain strings in an untyped first pass.

The decode walker can invoke a callback before rejecting a later field. The decode walker runs custom `decode` methods, `init` constructors, and computed defaults as it reaches each part.

### Trait-agnostic native default methods over codec support in the compiler

The checker automatically derives every trait with a native default method for Sand structs and enums.

The compiler records a shape for each derived implementation. The walkers read a shape to find the parts of a value and the dictionary of each part. A shape holds the type constructor and a part template for each part. A part template builds the part's dictionary from the outer dictionary. A trait builds values if its native default method is a static method with `Self` in its return type, as in `Decode.decode`. For a trait that builds values, a shape also holds constructor data: the `init` call and the default value of each parameter or field.

The walkers form a standard library module.

We rejected one alternative:

- **Codec metadata in the compiler and the bytecode.** If the compiler built shapes only for `Encode` and `Decode`, the VM would depend on one standard library module. Each new derived trait would also need compiler changes.

### Format-agnostic sinks and sources over per-format walkers

The encode walker writes to an `EncodeSink` interface. The decode walker reads from a `DecodeSource` interface. JSON text and host `Value` each provide a sink and a source.

Each sink and source applies its own format rules. For example, the JSON sink and source represent `bytes` as Base64 strings. They also represent map keys as strings. The `Value` sink and source accept only exact Sand types.

We rejected two alternatives:

- **Separate walkers for each format.** Per-format walkers would duplicate callback suspension, heap rooting, and resume logic across formats.
- **JSON conventions inside the `Value` source.** If the `Value` source accepted Base64 strings for `bytes`, host callers would have to follow JSON rules. Other libraries such as `serde_json` encode byte strings differently. Hosts with JSON input can call `json.decode` directly.

### Cached plans over per-part dictionary reads

Structural dictionaries store part templates instead of child dictionaries. Before a walk, the walker resolves the templates below the root dictionary into a plan: a Rust tree with a node for each distinct dictionary. The walker then visits values by the plan without dictionary reads.

A plan has a size limit. A polymorphically recursive type such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }` exceeds it. If a dictionary below the root runs Sand code, or if a plan exceeds its size limit, the walker reads each part dictionary as it reaches the part.

The walkers cache plans across calls in per-VM storage for native modules. That storage maps a Rust type to a value, so two modules cannot collide. Each VM has its own map, so engines in one process do not share caches. The cache keys plans by constant root dictionaries, whose handles last for the life of the VM.

We rejected three alternatives:

- **Per-part dictionary reads.** In release-build benchmarks of small and large `json.encode` and `json.decode` calls, reading each part's dictionary and shape made calls take 1.7 to 2.3 times as long as on `main`.
- **Eagerly nested dictionaries.** If parent dictionaries stored child dictionaries directly, the compiler would need cyclic constants for recursive types such as `Tree`. On a polymorphically recursive type such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`, the compiler would expand nested dictionaries without bound.
- **Generated Sand lookup functions.** Each call to a generated Sand helper function costs about 90 ns per uncached child dictionary. Generic code builds dictionaries at run time, and the walker builds a new plan for each of them on every call. Each of those plans would pay that cost for each part.

## Language Design

### Independent `Encode` and `Decode` traits

`Encode` and `Decode` are independent traits without a supertrait link. `Num` keeps `Encode` as a supertrait. Every numeric type implements `Encode`.

If `Decode` required `Encode`, users could not implement `Decode` alone for request payloads or configuration structs. Both `serde` and Swift keep the two traits separate.

### Automatic derivation with overrides

Built-in data types and `never` implement `Encode` and `Decode`. The checker also derives both traits for every Sand struct or enum whose parts satisfy the required bounds. The checker excludes functions, tasks, and channels. Those types represent active runtime state.

A custom `impl Encode` or `impl Decode` block replaces the derived implementation for its type constructor under standard trait coherence rules. For example, `impl Encode for Wrapper<int>` replaces the derived `impl<T: Encode> Encode for Wrapper<T>`. `Wrapper<str>` then does not implement `Encode`.

We rejected four alternatives:

- **Explicit `derive` annotations.** Automatic derivation matches `Eq`, `Ord`, and `Hash` in Sand.
- **Field attributes.** Custom `impl` blocks already express field renames and omissions. We can add field attributes later if needed.
- **Rejecting concrete type arguments on custom `impl` blocks.** If the checker rejected `impl Encode for Wrapper<int>`, codec traits would diverge from standard trait coherence. Users also could not write custom codecs for concrete type instantiations.
- **Excluding `never`.** With `Encode` and `Decode` on `never`, scripts can use empty generic types such as `[never]` and `Result<int, never>`. The checker excludes `never` from other traits with methods. The encode walker never visits a `never` value. The decode walker returns a parse error for `never`.

### Single-value streaming through `Encoder` and `Decoder`

Trait methods write a value through `Encoder` or read a value through `Decoder`:

```sand
/// Converts a value to an external format such as JSON.
pub trait Encode {
    /// Writes `self` to `encoder` as a single value.
    /// Returns an error if `self` cannot be encoded.
    native fn encode(self, encoder: Encoder) -> Result<()>
}

/// Decodes a value from an external format such as JSON.
pub trait Decode {
    /// Reads a value of type `Self` from `decoder`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

An `Encoder` accepts a single value during its callback. The engine returns an `"invalid"` error if the callback skips the write, writes a second value, or uses the `Encoder` after returning.

A `Decoder` reads at most a single value during its callback. Before reading, a custom `decode` method can call `decoder.kind()` to check the next input kind. The method can then branch on untagged inputs such as `int` or `str`. If the method returns without reading, the decode walker skips the unread input value. If a read fails, the decode walker returns that read error.

We rejected four alternatives:

- **Intermediate `Json` or `Value` trees.** If the walkers converted every value through `Json` or `Value`, they would allocate intermediate trees. They would also tie both traits to a single data model. With direct writes and reads, each sink and source preserves format-specific types such as raw `bytes` in `Value`.
- **Infallible `encode`.** With `Result<()>`, custom `encode` methods and format sinks can return errors for invalid values.
- **Rewindable `Decoder` retries.** If a read fails partway through a value, input rewinding cannot undo side effects from nested callbacks.
- **Omitting `decoder.kind()`.** Without `decoder.kind()` or retries, a Sand `decode` method could request only a fixed type. It could not decode untagged unions across formats.

### Trait bounds in local functions and field defaults

Local generic functions and struct field defaults can use trait bounds with methods across all traits. `Encode` and `Decode` declare methods. Without this change, the old checker rules for traits with methods would reject both traits in local functions and field defaults. For a derived `Decode` implementation on a struct, the checker also requires any trait bounds used in its field defaults.

We rejected two alternatives:

- **Keeping either restriction.** If the checker kept the local-function rule, scripts could not declare local generic codec helpers. If the checker kept the field-default rule, generic struct defaults could not call trait methods.
- **Lifting the restrictions only for `Encode` and `Decode`.** If the checker lifted these rules only for `Encode` and `Decode`, codec traits would differ from user-defined traits.

### Structs with `init` constructors

For a struct with an `init` constructor, the compiler derives `Decode` from `init`'s parameters. At runtime, the decode walker reads those parameters and calls `init`. The derived implementation includes `init`'s trait bounds. The walker passes their dictionaries to `init`. The walker sets missing `Option` parameters to `None`. For other missing parameters with defaults, the walker evaluates their defaults. The walker ignores `init`'s visibility. Field-based decoding likewise populates private fields.

For a trait that builds values, such as `Decode`, the derived implementation visits a struct's `init` parameters. A trait reads values if its native default method takes `self`, as in `Encode.encode`. For a trait that reads values, the derived implementation visits the struct's fields. The checker derives a trait that reads values for a struct with `init` only if the `init` parameters match the fields by name and type. The compiler accepts the struct definition in both cases.

We rejected six alternatives:

- **Omitting derived `Decode` for structs with `init`.** Without derived `Decode`, every validated struct would need a manual `Decode` implementation.
- **Omitting derived `Decode` if `init` requires trait dictionaries.** Because the derived implementation includes `init`'s bounds, generic validated structs keep derived `Decode`.
- **Writing fields directly without calling `init`.** If the walker wrote fields directly, it would bypass `init`'s validation.
- **Requiring `pub` on `init`.** If the checker required `pub` on `init`, authors would have to expose private validated constructors. Field-based decoding already populates private fields.
- **Deriving `Encode` even if `init` parameters differ from the fields.** The derived decoder would reject the output of derived `Encode`. Derived `Encode` writes struct fields. Derived `Decode` reads `init` parameters.
- **Rejecting the struct definition if `init` parameters differ from the fields.** If the checker reported an error at the struct definition, it would reject asymmetric structs that never use `Encode` or `Decode`. The checker runs derivation automatically on every struct.

### Rejecting duplicate struct fields

The decode walker returns a `"parse"` error if a struct field or `init` parameter appears more than once. For maps, the decode walker overwrites the earlier entry on a duplicate key.

If struct decoding overwrote earlier fields, it would hide malformed input. It would also run wasted callbacks for overwritten fields. `serde` likewise rejects duplicate struct fields.

## Host and Standard Library Design

### Requiring `Encode` for host `Value` conversion

Every conversion between a Sand value and a host `Value` runs inside an active `Vm`. A `Value` stays valid after the run ends:

- Every conversion to `Value` requires `T: Encode`.
- Every conversion from `Value` requires `T: Decode`.
- Native functions can also take `Encoded<N>` arguments that the engine converts before the call.
- The engine rejects host calls to functions whose return type does not implement `Encode`.
- If a top-level script result does not implement `Encode`, the engine returns `Value::Unit`.

We rejected two alternatives:

- **Outside-VM codec traits on Rust types.** Host code cannot run Sand's codec rules through outside-VM Rust traits alone. The engine executes custom Sand methods, `init` constructors, and computed defaults inside an active `Vm`.
- **Keeping `Value::Opaque` and an untyped synchronous `Vm::to_value`.** With an untyped synchronous `Vm::to_value`, the engine would ignore custom `Encode` implementations. It would also require a second heap walker. An unbounded type parameter lacks an `Encode` dictionary. A synchronous call also cannot await a Sand callback.

### Explicit opt-in and native methods for native types

A module author opts a native Rust type into a derived trait by name, as in `#[sand(derive(Encode, Decode))]`. The walkers then encode and decode the type structurally. The derive macros reject `derive(...)` on generic native types and on `SandObject` types.

For a custom implementation, the module source declares an ordinary `impl` block with a native method, as in `impl Encode for Money { native fn encode(self, encoder: Encoder) -> Result<()> }`. The Rust body takes `VmVal<Self>` and reads the VM heap in place. It writes through the sink of the `Encoder` handle, or reads through the source of the `Decoder` handle. If a custom method is a native function, the walker calls it directly through a general VM call for native function values, without an async callback.

We rejected four alternatives:

- **Automatic derivation for all bound Rust types.** If the checker derived codecs automatically for all bound types, it would expose internal Rust struct layouts as wire formats.
- **Writing custom codecs for native types in Sand.** Native methods run directly in the walker without async VM callbacks.
- **Registering Rust codec functions on the type binding.** The checker and the compiler would then carry codec data in type definitions and shapes.
- **Native methods over owned Rust values.** If native methods took `Self` instead of `VmVal<Self>`, the engine would copy heap values such as `Json` trees into Rust on every call.

### Fallible `json.encode` and infallible `json.to_json`

`json.encode` returns `Result<str>`. It returns an `"invalid"` error for non-finite floats and for map keys without a text form.

`json.to_json` keeps `Json` as its return type and applies the same JSON rules. If conversion fails, `json.to_json` stops the run.

We rejected two alternatives:

- **Keeping `str` as the return type of `json.encode`.** With `Result<str>`, scripts can recover from non-finite floats. Successful calls always return valid RFC 8259 JSON. If `json.encode` stopped the run, scripts could not recover. If `json.encode` emitted `null`, it would silently corrupt numeric data.
- **Returning `Result<Json>` from `json.to_json`.** If `json.to_json` returned `Result<Json>`, callers would have to handle `Result` on every use of the `json!` macro.

### `json.Json` as an ordinary native type

`json.Json` implements `Encode` and `Decode` with native methods in `json.sand`, as an ordinary native type. The walkers contain zero `Json`-specific branches.

If the walkers branched on `Json`, `Json` would diverge from the rules for other types.

The `Value` encoder converts a `Json` value into plain `Value` data such as `Value::Int(3)` instead of `Value::Variant` wrappers.
