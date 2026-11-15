# Design: `Encode` and `Decode` as Ordinary Traits

We propose to make `Encode` and `Decode` ordinary traits that users can implement. Each trait declares a native Rust default method that provides the derived implementation. Users can rely on the default methods or write custom implementations for their own types.

## Goals

The design has six goals:

- `Encode` and `Decode` follow the same rules as every other trait.
- `Encode` and `Decode` implementations are composable and nestable.
- A user can define `Encode` and `Decode` for concrete, generic, and native types.
- A script can encode any `Encode` value as JSON text and decode JSON text into any `Decode` type.
- A script can encode any `Encode` value as a host `Value` and decode a host `Value` as a `Decode` type.
- `json.Json` is encoded and decoded under the same rules as every other type.

## Architecture

### Static trait dictionaries over runtime type inspection

The compiler resolves `Encode` and `Decode` from static types and passes trait dictionaries to generic functions. We delete the `reflect` keyword because `Decode` dictionaries supply the target type structure instead. A host function that decodes into a type parameter requires `Decode` instead of `reflect`.

An alternative approach is to look up custom implementations at runtime from heap value tags. That approach fails for generic types. Today, heap values erase type arguments. For example, an empty `[Email]` list and an inline `None` of type `Email?` do not record `Email` at runtime. Recording type arguments on every value would add hidden descriptors to every generic call. It would also allocate heap objects for inline values like `None`. Runtime value tags also cannot guide `Decode`, which starts from raw input before any value exists.

Passing dictionaries adds a hidden argument only to generic calls with an `Encode` or `Decode` bound.

### Native walkers with callbacks over generated Sand codecs

Each trait uses a native Rust default method to traverse structural values. The walker processes default parts directly in Rust. It suspends for an async Sand callback at custom methods, `init` constructors, and computed field defaults.

We considered three alternatives for executing structural codecs:

- **Generated Sand methods.** Generating Sand `encode` and `decode` methods for every structural type ran 2.3 to 7 times as long as native walkers in benchmarks.
- **Dual native and Sand codecs.** Running generated Sand codecs beside a native walker requires matching output, error paths, and memory-limit retries across two engines. In an earlier prototype, keeping the two paths aligned took over a quarter of the new code.
- **Two-pass walks with placeholders.** Building an intermediate value tree to avoid callbacks allocates extra memory on every JSON call. It also forces format rules such as Base64 byte decoding into the structural walker.

Running callbacks in place changes callback order during decoding. Previously, `json.decode` ran computed field defaults after parsing the entire input. Under this design, custom `decode` methods and computed defaults run as the walker reaches each part. A callback can therefore run before a later field fails to decode.

### Format-agnostic sinks and sources over per-format walkers

A single encode walker writes to an `EncodeSink` interface, and a single decode walker reads from a `DecodeSource` interface. JSON text, `Json` trees, and host `Value`s each provide their own sink or source.

Format-specific encodings stay inside each format's sink and source. For example, the JSON sink and source represent `bytes` as Base64 strings and map keys as strings. The `Value` sink and source accept only exact Sand types.

We rejected two alternatives:

- **Separate walkers for each format.** Separate walkers would duplicate callback suspension, heap rooting, and resume logic across three encoders and two decoders.
- **JSON conventions inside the `Value` source.** Accepting Base64 strings for `bytes` in `Value` would bake JSON rules into host interop. Other libraries such as `serde_json` treat byte strings differently. Hosts with JSON input can call the JSON decoder directly.

### On-demand part dictionaries over eager nested dictionaries

Structural dictionaries store declarative templates that build child dictionaries only as the walker visits each child. The walker caches constructed child dictionaries for the duration of the walk.

We rejected two alternatives for supplying child dictionaries to the walker:

- **Eagerly nested dictionaries.** Storing child dictionaries inside parent dictionaries creates cyclic constants for recursive types such as `Tree`. It also expands without bound for polymorphically recursive types such as `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`.
- **Generated Sand lookup functions.** Calling a Sand helper function to fetch child dictionaries adds about 90 ns per uncached child dictionary. Types that use only native defaults would still pay that callback cost.

## Language Design

### Independent `Encode` and `Decode` traits

`Encode` and `Decode` are independent traits without a supertrait link between them. `Num` keeps `Encode` as a supertrait because every numeric type is encodable.

Requiring `Encode` as a supertrait of `Decode` would block decode-only types such as request payloads and configuration structs. Both `serde` and Swift keep the two traits separate.

### Automatic derivation with overrides

The checker derives `Encode` and `Decode` automatically for built-in data types, `never`, and every Sand struct or enum whose parts satisfy the required bounds. Functions, tasks, and channels are excluded because they represent active runtime state rather than data.

A custom `impl Encode` or `impl Decode` block replaces the derived implementation for its type constructor under standard trait coherence rules. For example, `impl Encode for Wrapper<int>` replaces the derived `impl<T: Encode> Encode for Wrapper<T>`, so `Wrapper<str>` does not implement `Encode`.

We rejected four alternatives:

- **Explicit `derive` annotations.** Automatic derivation matches `Eq`, `Ord`, and `Hash` in Sand.
- **Field attributes.** Custom `impl` blocks already express field renames and omissions without adding attribute syntax. Field attributes can be added later if needed.
- **Rejecting concrete type arguments on custom `impl` blocks.** Banning `impl Encode for Wrapper<int>` would add a codec-only exception to trait rules and block instantiation-specific codecs.
- **Excluding `never`.** Other traits with methods exclude `never`. Including `never` in `Encode` and `Decode` lets scripts use empty generic containers such as `[never]` and `Result<int, never>`. Encoding `never` is unreachable, and decoding `never` returns a parse error.

### Single-value streaming through `Encoder` and `Decoder`

Methods read and write a single value through `Encoder` and `Decoder` handles:

```sand
/// Converts a value into an external format, such as JSON text.
pub trait Encode {
    /// Writes `self` to `encoder` as a single value.
    /// Returns an error if `self` cannot be encoded.
    native fn encode(self, encoder: Encoder) -> Result<()>
}

/// Builds a value from an external format, such as JSON text.
pub trait Decode {
    /// Reads a value of type `Self` from `decoder`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

An `Encoder` accepts a single value during its callback. Omitting a write, writing a second value, or using the `Encoder` after the callback returns produces an `"invalid"` error.

A `Decoder` reads at most a single value during its callback. Before reading, a custom `decode` method can call `decoder.kind()` to inspect the kind of the next input value and branch on untagged inputs such as `int` or `str`. If the method omits a read, the source skips the unread input value. If a read fails, the entire decode fails with that read error.

We rejected four alternatives:

- **Intermediate `Json` or `Value` trees.** Converting through `Json` or `Value` allocates intermediate trees and ties the traits to a single data model. Streaming directly preserves format-specific types such as raw `bytes` in `Value`.
- **Infallible `encode`.** Returning `Result<()>` lets custom `encode` methods and format sinks report invalid values cleanly.
- **Rewindable `Decoder` retries.** If a read fails partway through a value, rewinding the input cannot undo side effects from nested callbacks that already ran.
- **Omitting `decoder.kind()`.** Without a kind query or retries, a Sand `decode` method could read only a single expected type and could not decode untagged inputs across formats.

### Trait bounds in local functions and field defaults

Local generic functions and struct field defaults can use trait bounds with methods across all traits. Because `Encode` and `Decode` now declare methods, existing checker restrictions on traits with methods would otherwise block them in both places. A derived `Decode` implementation also requires any trait bounds that the struct's field defaults use.

We rejected two alternatives:

- **Keeping either restriction.** Keeping the local-function restriction would break existing scripts with local generic codec helpers. Keeping the field-default restriction would prevent generic structs from calling trait methods inside defaults.
- **Lifting the restrictions only for `Encode` and `Decode`.** Special-casing the codec traits would violate the goal that `Encode` and `Decode` follow the same rules as every other trait.

### Structs with `init` constructors

For structs that declare an `init` constructor, the compiler derives `Decode` from `init`'s parameters and calls `init`. The derived implementation also requires `init`'s trait bounds and passes their dictionaries to `init`. Missing `Option` parameters default to `None`, and missing parameters with defaults run their defaults. The decoder ignores `init`'s visibility, matching field-based decoding.

If `init`'s parameter names or types differ from the struct's fields, the compiler omits derived `Encode`. Asymmetric structs still compile, and callers get a compile error if they encode the struct without a custom `Encode` implementation.

We rejected six alternatives:

- **Omitting derived `Decode` for structs with `init`.** Requiring manual `Decode` implementations adds repetitive code to every validated type.
- **Omitting derived `Decode` if `init` requires trait dictionaries.** Propagating `init`'s bounds lets generic validated structs derive `Decode` without manual implementations.
- **Writing fields directly without calling `init`.** Skipping `init` breaks validation rules that the constructor enforces.
- **Requiring `pub` on `init`.** Field-based decoding already populates private fields, so requiring `pub` on `init` would penalize constructor validation.
- **Deriving `Encode` even if `init` parameters differ from the fields.** Derived `Encode` writes fields while derived `Decode` reads `init` parameters, so the derived decoder would reject the encoded output.
- **Rejecting the struct definition if `init` parameters differ from the fields.** Because derivation is automatic, a definition-site error would also reject structs that never use `Encode` or `Decode`.

### Rejecting duplicate struct fields

During decoding, a duplicate struct field or `init` parameter returns a `"parse"` error. Duplicate keys in a map still overwrite earlier values.

Overwriting earlier struct fields would hide malformed input and run wasted callbacks for overwritten fields. `serde` also rejects duplicate struct fields.

## Host and Standard Library Design

### Requiring `Encode` for host `Value` conversion

A `Value` can outlive a run, but encoding to or decoding from `Value` happens only inside an active `Vm`. Each conversion to `Value` requires `T: Encode`. We delete `Value::Opaque`, synchronous `Vm::to_value`, and outside-VM `value::Encode`. Async native functions convert values asynchronously. Synchronous native functions take pre-converted `Value` parameters bounded by `Encode`. The engine rejects host calls to functions whose return type does not implement `Encode`. A top-level script whose result does not implement `Encode` returns `Value::Unit`.

We rejected two alternatives:

- **Outside-VM codec traits on Rust types.** Custom Sand codecs, constructors, and defaults require an active `Vm` to execute Sand code. Outside-VM Rust traits therefore cannot honor Sand's codec rules.
- **Keeping `Value::Opaque` and an untyped synchronous `Vm::to_value`.** An unbounded type parameter lacks an `Encode` dictionary, and a synchronous call cannot await a Sand callback. An untyped `Vm::to_value` would therefore ignore custom `Encode` implementations and require a second heap walker.

### Explicit opt-in and Rust traits for native types

Native Rust types opt in to each trait through `#[sand(encode, decode)]` for derived codecs or `#[sand(encode = "manual", decode = "manual")]` for hand-written Rust implementations. Opaque `SandObject` types support only the manual form because the macro cannot inspect their fields.

Manual implementations implement the Rust traits `native::Encode` and `native::Decode` over `&mut Vm` and `VmVal<Self>`. They read and write the VM heap in place through `EncodeSink` and `DecodeSource` without copying into Rust values. Derived native types use the VM walker directly and do not generate Rust codec code. Native types with type parameters cannot implement `Encode` or `Decode`.

We rejected four alternatives:

- **Automatic derivation for all bound Rust types.** Explicit opt-in avoids exposing internal Rust struct layouts as unintended wire formats.
- **Writing custom codecs for native types in Sand.** Running manual codecs in Rust avoids async VM callbacks.
- **Defining `native::Encode` and `native::Decode` over owned Rust values.** Operating on `&Self` and `Self` instead of `VmVal<Self>` would copy heap values such as `Json` trees into Rust on every manual codec call.
- **Supporting generic native types.** Encoding or decoding erased Sand type parameters from Rust would force trait dictionaries and async VM callbacks into the Rust trait signatures. The standard library defines zero public generic native types.

### Fallible `json.encode` and infallible `json.to_json`

`json.encode` returns `Result<str>`. It returns an `"invalid"` error for non-finite floats and for map keys without a text form.

`json.to_json` writes directly to a `Json` sink under the same JSON rules and keeps `Json` as its return type. If conversion fails, `json.to_json` stops the run.

We rejected two alternatives:

- **Keeping `str` as the return type of `json.encode`.** Returning `Result<str>` lets scripts handle untrusted float data while guaranteeing valid RFC 8259 JSON. Stopping the run prevents recovery, and emitting `null` silently corrupts numbers.
- **Returning `Result<Json>` from `json.to_json`.** Keeping `Json` avoids forcing `Result` handling onto every use of the `json!` macro.

### `json.Json` as an ordinary native type

`json.Json` implements `Encode` and `Decode` manually in Rust as an ordinary native type. Its implementation reads and writes the VM heap in place through `EncodeSink` and `DecodeSource` rather than relying on `Json` branches inside the walkers.

Keeping dedicated `Json` branches in the walkers would violate the goal that `Json` follows the same rules as every other type.

Converting a `Json` value to a host `Value` now produces plain `Value` data such as `Value::Int(3)` instead of `Value::Variant` wrappers.
