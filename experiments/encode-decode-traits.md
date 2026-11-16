# Design: `Encode` and `Decode` as Ordinary Traits

We propose to make `Encode` and `Decode` ordinary traits that users can implement. Each trait declares a native Rust default method for derived implementations. Users can use the default methods or write custom implementations.

## Goals

The design has six goals:

- `Encode` and `Decode` follow the same rules as every other trait.
- `Encode` and `Decode` implementations are composable and nestable.
- A user can implement `Encode` and `Decode` for concrete, generic, and native types.
- A script can encode any `Encode` value as JSON text. It can decode JSON text into any `Decode` type.
- Host code can encode any `Encode` value into a `Value`. It can decode a `Value` into any `Decode` type.
- The walkers encode and decode `json.Json` under the same rules as every other type.

## Architecture

### Static trait dictionaries over runtime type inspection

The compiler resolves `Encode` and `Decode` from static types. Callers pass trait dictionaries to generic functions. We delete the `reflect` keyword. Callers pass `Decode` dictionaries with the target type structure instead. A host function that decodes into a type parameter requires `Decode` instead of `reflect`.

The walkers cannot resolve generic implementations or `Decode` implementations from heap value tags. The VM omits type arguments from runtime values. An empty `[Email]` list has the same heap representation as an empty `[int]` list. An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. If the VM recorded type arguments on every value, every generic call would pass hidden descriptors. The VM would also allocate heap objects for inline values such as `None`. The decode walker also cannot inspect a value tag to select a `Decode` implementation. The decode walker reads raw input before any Sand value exists.

With trait dictionaries, callers pass a hidden argument only to generic functions with an `Encode` or `Decode` bound.

### Native walkers with callbacks over generated Sand codecs

Each trait declares a native Rust default method that walks structural values. The walker processes derived parts in Rust. It awaits a Sand callback for custom methods, `init` constructors, and computed field defaults.

We rejected three alternatives:

- **Generated Sand methods.** In benchmarks, generated Sand `encode` and `decode` methods took 2.3 to 7 times as long as native walkers.
- **Dual native and Sand codecs.** With both generated Sand codecs and native walkers, the engine duplicates output formatting, error paths, and memory-limit retries. In an earlier prototype, over a quarter of the new code kept the two paths aligned.
- **Two-pass walks with placeholders.** In a two-pass design, the walker builds an intermediate value tree with placeholders on every JSON call. That tree uses extra memory. Without format rules, the walker also cannot distinguish Base64 bytes from plain strings in an untyped first pass.

The decode walker can invoke a callback before rejecting a later field. The decode walker runs custom `decode` methods, `init` constructors, and computed defaults as it reaches each part. Previously, `json.decode` ran computed field defaults only after it parsed the entire input.

### Format-agnostic sinks and sources over per-format walkers

The encode walker writes to an `EncodeSink` interface. The decode walker reads from a `DecodeSource` interface. JSON text and host `Value` each provide a sink and a source.

Each sink and source applies its own format rules. For example, the JSON sink and source represent `bytes` as Base64 strings. They also represent map keys as strings. The `Value` sink and source accept only exact Sand types.

We rejected two alternatives:

- **Separate walkers for each format.** Per-format walkers would duplicate callback suspension, heap rooting, and resume logic across formats.
- **JSON conventions inside the `Value` source.** If the `Value` source accepted Base64 strings for `bytes`, host callers would have to follow JSON rules. Other libraries such as `serde_json` encode byte strings differently. Hosts with JSON input can call `json.decode` directly.

### On-demand part dictionaries over eager nested dictionaries

Structural dictionaries store part templates instead of child dictionaries. The walker builds each child dictionary from its template only if it visits that child. It caches built child dictionaries for the rest of the walk.

We rejected two alternatives:

- **Eagerly nested dictionaries.** If parent dictionaries stored child dictionaries directly, the compiler would need cyclic constants for recursive types such as `Tree`. On a polymorphically recursive type such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`, the compiler would expand nested dictionaries without bound.
- **Generated Sand lookup functions.** Each call to a generated Sand helper function costs about 90 ns per uncached child dictionary. The walker would pay that callback cost even on derived types.

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

If `init`'s parameter names or types differ from the struct's fields, the compiler omits derived `Encode`. The compiler still accepts the struct definition. It reports a compile error only if code requires `Encode` without a custom `Encode` implementation.

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
- We delete `Value::Opaque`, synchronous `Vm::to_value`, and `value::Encode`.
- `Later::new` futures convert values through `NativeContext::to_value` and `NativeContext::from_value`.
- Native functions can also take `Encoded<N>` arguments that the engine converts before the call.
- The engine rejects host calls to functions whose return type does not implement `Encode`.
- If a top-level script result does not implement `Encode`, the engine returns `Value::Unit`.

We rejected two alternatives:

- **Outside-VM codec traits on Rust types.** Host code cannot run Sand's codec rules through outside-VM Rust traits alone. The engine executes custom Sand methods, `init` constructors, and computed defaults inside an active `Vm`.
- **Keeping `Value::Opaque` and an untyped synchronous `Vm::to_value`.** With an untyped synchronous `Vm::to_value`, the engine would ignore custom `Encode` implementations. It would also require a second heap walker. An unbounded type parameter lacks an `Encode` dictionary. A synchronous call also cannot await a Sand callback.

### Explicit opt-in and Rust traits for native types

A module author opts a native Rust type into `Encode` and `Decode` with `#[sand(encode, decode)]` or `#[sand(encode = "manual", decode = "manual")]`. With `#[sand(encode, decode)]`, the VM walker encodes and decodes the type structurally. With `"manual"`, the walker calls a custom Rust implementation. The `SandObject` macro accepts only the manual form. The macro cannot inspect the Rust fields of an opaque reference type.

Manual codecs implement `native::codec::Encode` and `native::codec::Decode` over `&mut Vm` and `VmVal<Self>`. They read and write the VM heap in place through `EncodeSink` and `DecodeSource`. They avoid copying heap data into owned Rust values. Derived native types use the VM walker directly without generated Rust codec code. The derive macros reject `encode` and `decode` on generic native types.

We rejected four alternatives:

- **Automatic derivation for all bound Rust types.** If the checker derived codecs automatically for all bound types, it would expose internal Rust struct layouts as wire formats.
- **Writing custom codecs for native types in Sand.** Rust manual codecs run directly in the walker without async VM callbacks.
- **Defining `native::codec::Encode` and `native::codec::Decode` over owned Rust values.** If those traits took `&Self` and `Self` instead of `VmVal<Self>`, the engine would copy heap values such as `Json` trees into Rust on every call.
- **Supporting generic native types.** To support generic native types, the Rust codec traits would need trait dictionaries and async VM callbacks. The standard library defines zero public generic native types.

### Fallible `json.encode` and infallible `json.to_json`

`json.encode` returns `Result<str>`. It returns an `"invalid"` error for non-finite floats and for map keys without a text form.

`json.to_json` keeps `Json` as its return type and applies the same JSON rules. If conversion fails, `json.to_json` stops the run.

We rejected two alternatives:

- **Keeping `str` as the return type of `json.encode`.** With `Result<str>`, scripts can recover from non-finite floats. Successful calls always return valid RFC 8259 JSON. If `json.encode` stopped the run, scripts could not recover. If `json.encode` emitted `null`, it would silently corrupt numeric data.
- **Returning `Result<Json>` from `json.to_json`.** If `json.to_json` returned `Result<Json>`, callers would have to handle `Result` on every use of the `json!` macro.

### `json.Json` as an ordinary native type

`json.Json` implements `Encode` and `Decode` manually in Rust as an ordinary native type. Its Rust implementation reads and writes the VM heap in place through `EncodeSink` and `DecodeSource`. The walkers contain zero `Json`-specific branches.

If the walkers branched on `Json`, `Json` would diverge from the rules for other types.

The `Value` encoder converts a `Json` value into plain `Value` data such as `Value::Int(3)` instead of `Value::Variant` wrappers.
