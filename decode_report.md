# The `Encode` and `Decode` Architecture in Sand

Sand is a statically typed, sandboxed bytecode interpreter implemented in safe Rust. A Sand program runs inside an isolated virtual machine. The virtual machine enforces hard limits on memory and execution fuel. The `Encode` trait converts an in-memory Sand value into a serialized form. The `Decode` trait builds an in-memory Sand value from a serialized form.

Sand solves the `Encode` and `Decode` problem with a dual-execution architecture. At the language level, `Encode` and `Decode` are regular traits with methods. These methods read and write through format-independent `Encoder` and `Decoder` objects. The type checker generates default method bodies for structs, enums, tuples, and native value types. Programmers can also write custom `impl Encode` and `impl Decode` blocks in Sand or in Rust.

At the execution level, the runtime uses a native fast path if a type uses only generated or built-in implementations. For those types, the compiler builds a structural description called a `Schema`. A native Rust walker executes that `Schema` in a single Rust function call. That call writes to or reads from the target format directly without allocating an `Encoder` or `Decoder` object on the virtual machine heap.

---

## High-Level Solution

### 1. Regular Trait Methods and Omitted Fields

`Encode` and `Decode` are traits declared in the standard library prelude at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L335-L396). `Encode` declares the `encode` method at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L335-L340). `Decode` declares the `decode` and `decode_missing` methods at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L386-L396):

```sand
pub trait Encode {
    fn encode(self, e: Encoder)
}

pub trait Decode {
    fn decode(d: Decoder) -> Result<Self>
    fn decode_missing() -> Self? { None }
}
```

#### Why `decode_missing` Exists

`decode_missing` exists so that a generic struct `struct Wrap<T> { v: T }` can decode an omitted field `"v"` if `T` is `Option<U>` and reject the omitted field if `T` is any other type.

If a field has a concrete type such as `v: int?` or `v: int`, the compiler already knows at compile time whether `"v"` can be omitted. For `v: int?`, the compiler generates code that sets `v` to `None` if `"v"` is omitted. For `v: int`, the compiler generates code that returns a missing-field error if `"v"` is omitted.

If a field has a generic type parameter `v: T`, the compiler compiles a single shared `Wrap.decode` function before `T` is known. At compile time, `Wrap.decode` cannot tell whether `T` is `Option<U>` or a required type such as `int`.

To solve this, `Decode` includes `decode_missing() -> Self?`:

- Every built-in type except `Option` uses the default implementation, which returns `None` to signal a missing-field error.
- `Option<T>` in [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L637-L643) overrides `decode_missing` to return `Some(None)`. The outer `Some` indicates that omitting the field is allowed. The inner `None` is the `Option<T>` value assigned to the field:

```sand
impl<T: Decode> Decode for T? {
    fn decode(d: Decoder) -> Result<T?> {
        Ok(if d.read_some()? { Some(T.decode(d)?) } else { None })
    }
    fn decode_missing() -> T?? { Some(None) }
}
```

If `Wrap.decode` reads `{}` and sees that `"v"` was omitted, it calls `T.decode_missing()`. If `T` is `int?`, `Option.decode_missing()` returns `Some(None)` and `Wrap.decode` sets `v` to `None`. If `T` is `int`, `int.decode_missing()` returns `None` and `Wrap.decode` returns a missing-field error.

Because `decode_missing` is a public method on `Decode`, a user-defined concrete type such as `Tags` in [codec_traits.test](crates/sandlang/tests/spec/codec_traits.test#L295-L305) can also override `decode_missing` to return `Some(default_value)` if omitted. In `Synth::missing` at [codec.rs](crates/sandlang/src/check/codec.rs#L1175-L1216) and `Checker::may_decode_missing` at [codec.rs](crates/sandlang/src/check/codec.rs#L739-L747), the compiler emits a call to `decode_missing()` only if a field lacks a default expression and its type is either a generic type parameter or a concrete type that overrides `decode_missing`. On the Rust host side, `sandlang::value::Decode::decode_missing_field` at [value.rs](crates/sandlang/src/value.rs#L492-L496) and [value.rs](crates/sandlang/src/value.rs#L692-L707) serves the same role for `#[derive(SandStruct)]` at [lib.rs](crates/sandlang-macros/src/lib.rs#L630).

#### Why `decode_missing` Is Not Required by the Type System

Sand does not need a second trait method to support omitted `Option` fields in generic structs. A single-method `Decode` trait with only `fn decode(d: Decoder) -> Result<Self>` can express the same behavior.

In `Option.decode(d)` at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L637-L640), the first call on `d` is `d.read_some()`. Every other type's `decode(d)` method starts with a value-reading call on `d`, such as `d.read_int()` or `d.read_str()`. If `Wrap.decode` placed `d` into a missing-field mode for an omitted field and called `T.decode(d)`, `d.read_some()` would return `Ok(false)`. `Option.decode(d)` would then return `Ok(None)`. Every other read method on `d` in missing-field mode would return the missing-field error. Sand instead uses a separate `decode_missing()` method rather than a missing-field state on `Decoder`.

### 2. Format-Independent `Encoder` and `Decoder` Objects

A programmer writes an `Encode` or `Decode` implementation once. That implementation works across all serialization formats. `Encoder` and `Decoder` are opaque native object types whose methods are declared in [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L418-L492). Their Rust struct definitions live in [mod.rs](crates/sandlang/src/codec/mod.rs#L318-L343).

An `Encoder` holds a boxed Rust trait object that implements `EncodeFormat` at [mod.rs](crates/sandlang/src/codec/mod.rs#L63-L163). A `Decoder` holds a boxed Rust trait object that implements `DecodeFormat` at [mod.rs](crates/sandlang/src/codec/mod.rs#L172-L310). A format is a backend implementation of `EncodeFormat` or `DecodeFormat`. Each format translates abstract codec operations into a concrete data representation.

Sand includes 3 built-in formats:

| Format     | Encoder        | Decoder        | Representation                |
| ---------- | -------------- | -------------- | ----------------------------- |
| JSON text  | `TextEncoder`  | `TextDecoder`  | UTF-8 JSON text `str`         |
| JSON tree  | `TreeEncoder`  | `TreeDecoder`  | In-memory `json.Json` enum    |
| Host value | `ValueEncoder` | `ValueDecoder` | Rust `sandlang::value::Value` |

Public Sand methods on `Encoder` and `Decoder` delegate to child values:

- Calling `e.encode(x)` at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L420-L422) calls `x.encode(e)`.
- Calling `d.decode()` at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L451-L453) calls `T.decode(d)`.
- Calling `d.finish(result)` at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L459-L461) calls `self.end_decode(result)` to verify that the decoder reached the end of the input. If `result` is `Err`, `d.finish(result)` prepends the failure's field and index path to the error message.

### 3. Automatic Derivation, `init` Constructors, and Private Fields

The type checker automatically derives `Encode` and `Decode` for structs, enums, tuples, and native value types. If a module provides a hand-written `impl` for a type, that hand-written `impl` replaces the derived implementation.

The fixed-point derivation pass in [derive.rs](crates/sandlang/src/check/derive.rs#L139-L155) determines which type parameters of a generic type must implement `Encode` or `Decode`. Mutually recursive types implement `Encode` or `Decode` if every component type satisfies the trait.

A Sand struct can declare a static `init` constructor method. A constructor must return either `Self` or `Result<Self, E>`. If a struct `T` declares `init`, `T(...)` calls outside `init` are resolved to `T.init` instead of field-by-field construction.

A generated `Decode` method must construct a struct directly from its fields, without calling `init`. Because direct field construction would bypass `init`, the derivation pass in [derive.rs](crates/sandlang/src/check/derive.rs#L218-L221) does not derive `Decode` for any struct that declares `init`. A struct with `init` still receives a derived `Encode` implementation, but the struct author must write an explicit `impl Decode` that calls `T(...)`, such as `Email` in [codec_traits.test](crates/sandlang/tests/spec/codec_traits.test#L84-L105):

```sand
struct Email {
    address: str

    fn init(address: str) -> Result<Email> {
        if not address.contains("@") {
            return Err(Error("invalid email: \(address)", kind: "invalid"))
        }
        Ok(Self(address: address))
    }
}

impl Decode for Email {
    fn decode(d: Decoder) -> Result<Email> {
        Email(d.decode()?)
    }
}
```

Field visibility also restricts `Decode`. If a struct has a private field, code outside the struct's defining module cannot construct that struct directly. `Checker::private_decoded_field` at [codec.rs](crates/sandlang/src/check/codec.rs#L118-L154) and `Checker::resolve_evidence` at [traits.rs](crates/sandlang/src/check/traits.rs#L1151-L1159) prevent external modules from using a generated `Decode` implementation for that struct.

### 4. Dual Execution: Driven Bytecode Path and Native Fast Path

Running a bytecode function call for every number, string, field, and list element adds approximately 10 nanoseconds of overhead per value. To avoid that cost on derived types while supporting arbitrary user code on custom types, Sand uses 2 execution paths:

1. **The Driven Bytecode Path**:
   Built-in primitive and collection types implement `Encode` and `Decode` in [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L494-L643). Those methods call private native methods on `Encoder` and `Decoder` in the prelude. For derived structs, enums, tuples, and native value types, the type checker declares derived methods on demand in `Checker::ensure_derived_methods` at [codec.rs](crates/sandlang/src/check/codec.rs#L292-L316). The type checker builds their High-Level Intermediate Representation, called `HIR`, in `Checker::derived_body` at [codec.rs](crates/sandlang/src/check/codec.rs#L773-L828). `HIR` is the compiler's typed expression tree produced before bytecode generation. If a type or any of its nested components has a hand-written `Encode` or `Decode` method, the virtual machine allocates an `Encoder` or `Decoder` on the heap. The virtual machine then runs the compiled bytecode methods.

2. **The Native Schema Fast Path**:
   `Checker::codec_is_native` at [codec.rs](crates/sandlang/src/check/codec.rs#L502-L545) checks 3 conditions to determine whether a type qualifies for the native fast path:
   - First, `Checker::derives_codec` at [codec.rs](crates/sandlang/src/check/codec.rs#L447-L451) requires the type and all of its component types to use derived or built-in implementations.
   - Second, every struct field default must be a constant scalar or an empty list, set, or map.
   - Third, the type nesting depth must be below `MAX_FAST_TYPE_DEPTH = 64` at [codec.rs](crates/sandlang/src/check/codec.rs#L18).

   A closed type is a type with zero free type parameters. If a closed type meets all 3 conditions, the checker builds a `Schema` in `Checker::schema_of` and `Checker::schema_node` at [codec.rs](crates/sandlang/src/check/codec.rs#L617-L682). A `Schema` is a recursive Rust data structure defined at [fast.rs](crates/sandlang/src/codec/fast.rs#L42-L67) that describes the runtime layout of a Sand type. The checker attaches that `Schema` to a fast implementation entry in `Checker::add_fast_impl` at [codec.rs](crates/sandlang/src/check/codec.rs#L595-L613).

3. **Single-Call Entry Dispatch**:
   Entry functions such as `json.encode` and `json.decode` at [json.sand](crates/sandlang/src/stdlib/json.sand#L127-L199) check for a `Schema` before allocating an `Encoder` or `Decoder`. A runtime trait dictionary is a tuple that holds the method closures and nested dictionaries for a trait implementation. Because the native start function `encode_text<T: Encode>` declares `<T: Encode>`, the compiler passes the runtime `Encode` dictionary for `T` as its first hidden argument. In Rust, `EncodeDict` at [mod.rs](crates/sandlang/src/codec/mod.rs#L781) and [ty.rs](crates/sandlang/src/native/ty.rs#L443-L488) extracts that hidden dictionary argument. `encode_text` at [json.rs](crates/sandlang/src/stdlib/json.rs#L153-L161) calls `fast_schema` at [mod.rs](crates/sandlang/src/codec/mod.rs#L788-L790) and `composed::dict_schema` at [composed.rs](crates/sandlang/src/codec/composed.rs#L154-L164) to check whether the dictionary has an associated `SchemaId`. A `SchemaId` is a 32-bit integer index that identifies a `Schema` at [fast.rs](crates/sandlang/src/codec/fast.rs#L32). If the dictionary has a `SchemaId`, `encode_text` executes the entire encode in 1 Rust call and returns `Encoding.Done(str)`. If the dictionary lacks a `SchemaId`, `encode_text` allocates an `Encoder` on the virtual machine heap and returns `Encoding.Driven(e)`. The Sand body of `json.encode` then calls `value.encode(e)` and `encoded_text(e)`.

4. **Runtime Schema Composition in Generic Code**:
   A generic function such as `fn read_list<T: Decode>(s: str) -> Result<[T]>` can call `json.decode(s)` on a constructed type `[T]` whose element type `T` is unknown until runtime. At compile time, `Checker::codec_template` at [codec.rs](crates/sandlang/src/check/codec.rs#L362-L428) creates a `SchemaTemplate` at [composed.rs](crates/sandlang/src/codec/composed.rs#L43-L53) for the generic `[T]` implementation. A `SchemaTemplate` is a list of `Schema` nodes with hole placeholders for the type parameters of the generic implementation. Inside `read_list`, the compiler emits a call to the native function `compose_codec_dict` at [mod.rs](crates/sandlang/src/compile/mod.rs#L2281-L2294) to build the runtime dictionary for `[T]`. If the caller's dictionary for `T` has a `SchemaId`, `compose` and `RuntimeSchemas::instantiate` at [composed.rs](crates/sandlang/src/codec/composed.rs#L101-L200) substitute that `SchemaId` into the `SchemaTemplate` at runtime. The resulting dictionary receives a runtime `SchemaId` and runs on the native fast path.

### 5. Unified Host Boundary

The host boundary between Rust code and the Sand virtual machine uses the same `Encode` and `Decode` traits. Every value that leaves the virtual machine as a `sandlang::value::Value` goes through the Sand `Encode` trait via `prelude.encode_value` at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L399-L407).

On the Rust side, the traits `sandlang::value::Encode` and `sandlang::value::Decode` at [value.rs](crates/sandlang/src/value.rs#L270-L497) convert between Rust types and `Value`. The procedural macros `#[derive(SandStruct)]`, `#[derive(SandEnum)]`, and `#[derive(SandValue)]` at [lib.rs](crates/sandlang-macros/src/lib.rs#L41-L86) generate Rust `value::Encode` and `value::Decode` implementations automatically. Those generated Rust implementations produce and consume the same `Value` structure as the derived Sand implementations.

---

## Low-Level Solution

### 1. Runtime Trait Dictionaries and Evidence Resolution

Generic values in Sand are type-erased at runtime. To dispatch trait methods on generic types, the compiler prepends a hidden dictionary parameter for each trait bound that declares methods. A runtime trait dictionary is a virtual machine tuple ([traits.rs](crates/sandlang/src/check/traits.rs#L3-L12)). The tuple contains 3 sections in order:

1. **Method closures**: 1 function reference for each method of the trait.
2. **Supertrait dictionaries**: 1 nested dictionary tuple for each direct supertrait that declares methods.
3. **Evidence dictionaries**: 1 nested dictionary tuple for each trait bound on the `impl` block's own type parameters.

`Encode` declares 1 method, `encode`, and has zero supertraits. Therefore, an `Encode` dictionary tuple contains 1 method slot followed by 1 evidence slot per required type parameter. `Decode` declares 2 methods, `decode` and `decode_missing`, and has zero supertraits. Therefore, a `Decode` dictionary tuple contains 2 method slots followed by 1 evidence slot per required type parameter.

In the type checker, checking a call to a generic function emits an evidence placeholder for each required dictionary in `Checker::evidence_hole` at [traits.rs](crates/sandlang/src/check/traits.rs#L996-L1000). After type inference finishes for the enclosing function, `Checker::resolve_holes` at [traits.rs](crates/sandlang/src/check/traits.rs#L1038-L1081) replaces each placeholder with a resolved dictionary expression. A resolved dictionary expression is a dictionary constructor, a local dictionary parameter, or a tuple field lookup.

`Checker::resolve_evidence` at [traits.rs](crates/sandlang/src/check/traits.rs#L1143-L1217) applies 2 rules for `Encode` and `Decode`:

- **The Uninhabited `never` Type**:
  `never` is Sand's uninhabited bottom type. An untyped empty list literal `[]` has the type `[never]`. For `json.encode([])` to compile, `never` must implement `Encode`. `Checker::never_dict` at [traits.rs](crates/sandlang/src/check/traits.rs#L1207-L1217) builds a dictionary for `never` if every method of the trait and its supertraits takes a `self` receiver. Because a runtime value never has the type `never`, a caller can never invoke a method that takes `self` on `never`. The compiler fills each method slot of the `never` dictionary with the unit value `()`. Because `Decode.decode` lacks a `self` parameter, `never_dict` returns `None` for `Decode`. Consequently, `never` does not implement `Decode`. Calling `json.decode("[]")` without a type annotation therefore produces a compile-time error.

- **Preference for Fast Schema Implementations**:
  A ground type is a type that contains zero type parameters. In `Checker::resolve_evidence` at [traits.rs](crates/sandlang/src/check/traits.rs#L1185-L1190), if the target type is a ground type and the trait is `Encode` or `Decode`, the checker calls `Checker::fast_codec_impl`. If `fast_codec_impl` finds a schema-backed implementation, `resolve_evidence` selects that fast implementation instead of the bytecode implementation.

### 2. On-Demand HIR Synthesis for Derived Implementations

The type checker generates `HIR` method bodies only for derived implementations that are used on the bytecode path. First, `Checker::synthesize_codec_impls` at [codec.rs](crates/sandlang/src/check/codec.rs#L158-L173) registers a derived implementation record with an empty method list. If trait resolution selects that derived implementation at [traits.rs](crates/sandlang/src/check/traits.rs#L1386), the checker calls `Checker::ensure_derived_methods` at [codec.rs](crates/sandlang/src/check/codec.rs#L292-L316) to declare its method identifiers.

During function body checking, `Checker::derived_body` at [codec.rs](crates/sandlang/src/check/codec.rs#L773-L828) runs the `Synth` builder at [codec.rs](crates/sandlang/src/check/codec.rs#L861-L1217) to construct typed `HIR` expression trees directly:

- **Structs**:
  - For `Encode`, `Synth::encode_body` at [codec.rs](crates/sandlang/src/check/codec.rs#L993-L1002) emits a call to `Encoder.begin_struct(e, def_id)`. For each field index `i`, it emits `Encoder.field(e, i)` followed by `Synth::encode` at [codec.rs](crates/sandlang/src/check/codec.rs#L935-L945). `Synth::encode` emits a direct scalar write for primitive fields, `Encoder.write_fast` for fields with a `SchemaId`, or a trait call `Encode.encode` for other fields. After the last field, `Synth::encode_body` emits `Encoder.end(e)`.
  - For `Decode`, `Synth::fields` at [codec.rs](crates/sandlang/src/check/codec.rs#L1056-L1063) and [codec.rs](crates/sandlang/src/check/codec.rs#L1129-L1170) declares a local value variable and a boolean `seen` flag for each field. It emits `Decoder.begin_struct(d, def_id)?`, followed by a loop that calls `Decoder.field(d)?` to read the next field index. If `Decoder.field(d)` encounters an unknown field name in the input, `next_field` at [mod.rs](crates/sandlang/src/codec/mod.rs#L1195-L1215) skips the unknown value and reads the next field. If `Decoder.field(d)` returns a negative index, the loop ends. If the index matches field `i`, the match arm decodes the field with `Synth::decode` at [codec.rs](crates/sandlang/src/check/codec.rs#L949-L962). The match arm then stores the decoded value and sets that field's `seen` flag to `true`. After the loop, `Synth::missing` at [codec.rs](crates/sandlang/src/check/codec.rs#L1175-L1216) fills each unseen field. Finally, `Synth::decode_body` constructs the struct and returns `Ok`.

- **Enums**:
  - For `Encode`, `Synth::encode_body` at [codec.rs](crates/sandlang/src/check/codec.rs#L1003-L1030) builds a `match self` expression with 1 arm per variant. An arm for a unit variant calls `Encoder.write_variant(e, rt_id)`. An arm for a payload variant calls `Encoder.begin_variant(e, rt_id)`, encodes each positional or named payload field, and calls `Encoder.end(e)`.
  - For `Decode`, `Synth::decode_body` at [codec.rs](crates/sandlang/src/check/codec.rs#L1064-L1090) calls `Decoder.variant(d, first_rt_id, count)?` to read the variant name and return its zero-based index. It then matches on that index. Unit variant arms construct the variant tag directly. Positional variant arms call `Synth::elements` at [codec.rs](crates/sandlang/src/check/codec.rs#L1104-L1116) followed by `Decoder.end_variant(d)?`. Named variant arms call `Synth::fields` followed by `Decoder.end_variant(d)?`.

- **Tuples**:
  In [codec.rs](crates/sandlang/src/check/codec.rs#L177-L191), `Checker::tuple_codec_impl` registers a derived implementation for a tuple of length `N` on first use. For `Encode` at [codec.rs](crates/sandlang/src/check/codec.rs#L981-L989), `Synth::encode_body` calls `Encoder.begin_tuple(e, N)`, encodes each tuple element in order, and calls `Encoder.end(e)`. For `Decode` at [codec.rs](crates/sandlang/src/check/codec.rs#L1047-L1052) and [codec.rs](crates/sandlang/src/check/codec.rs#L1104-L1116), `Synth::elements` calls `Decoder.begin_tuple(d, N)?`, reads each element after calling `Decoder.element(d)?`, calls `Decoder.end_tuple(d)?`, and constructs the tuple.

- **Native Value Types**:
  In [codec.rs](crates/sandlang/src/check/codec.rs#L1031-L1034) and [codec.rs](crates/sandlang/src/check/codec.rs#L1091-L1094), opaque Rust value types that implement `ValueType` call `Encoder.write_native(e, self)` for `Encode` and `Decoder.read_native(d, def_id)` for `Decode`.

### 3. Compile-Time Schemas and Runtime Schema Composition

#### Compile-Time `Schema` Construction

`Checker::fast_codec_impl` at [codec.rs](crates/sandlang/src/check/codec.rs#L480-L496) builds a `Schema` for any closed type if `codec_is_native` and `schema_of` succeed. `Schema` is defined at [fast.rs](crates/sandlang/src/codec/fast.rs#L42-L67):

```rust
pub enum Schema<R = SchemaId> {
    Unit,
    Bool,
    Int,
    Float,
    Duration,
    Str,
    Bytes,
    Range,
    Native(u32),
    Json,
    List(R),
    Set(R),
    Map(R, R),
    Option(R),
    Tuple(Box<[R]>),
    Struct(u32, Box<[R]>, Box<[Option<FieldDefault>]>),
    Enum(u32, Box<[Box<[R]>]>),
}
```

In `Schema::Struct(def, fields, defaults)`, `fields` stores each field's `SchemaId` and `defaults` stores each field's optional `FieldDefault`. `FieldDefault` at [fast.rs](crates/sandlang/src/codec/fast.rs#L72-L99) represents constant default values that the Rust walker can construct without running bytecode:

- `Scalar(VmValue)`: a scalar or unit enum variant value, such as `()`, `bool`, `int`, `float`, `duration`, or `None`.
- `Str(Box<str>)`: a constant string literal.
- `EmptyList`: an empty list `[]`.
- `EmptySet`: an empty set.
- `EmptyMap`: an empty map `[:]`.

If a struct field has a computed default expression such as a function call, `FieldDefault::new` returns `None`. `Checker::derives_codec` at [codec.rs](crates/sandlang/src/check/codec.rs#L447-L451) then disables the fast path for `Decode` on that struct. In that case, the struct uses its generated `HIR` method for `Decode` and still uses a `Schema` for `Encode`.

In [mod.rs](crates/sandlang/src/compile/mod.rs#L514-L523), the bytecode compiler builds a function prototype struct called `Proto` for each compiled function. If a function is a fast-schema method or a built-in scalar codec method recorded in `Checker::add_scalar_codec_schemas` at [codec.rs](crates/sandlang/src/check/codec.rs#L432-L442), the compiler stores its `SchemaId` in `Program::proto_schemas[proto_idx]`.

#### Runtime `SchemaTemplate` Instantiation

For generic implementations, `Checker::codec_template` at [codec.rs](crates/sandlang/src/check/codec.rs#L362-L428) builds a `SchemaTemplate` at [composed.rs](crates/sandlang/src/codec/composed.rs#L43-L53). In a `SchemaTemplate`, each type parameter is represented by a hole index `TemplateRef::Hole(h)` at [composed.rs](crates/sandlang/src/codec/composed.rs#L31-L38). `SchemaTemplate::holes[h]` stores the tuple index of that type parameter's evidence dictionary inside the outer dictionary tuple. If generic bytecode builds a dictionary for an implementation that has a `SchemaTemplate`, the compiler emits a call to the native function `compose_codec_dict(dict, method, template_id)` at [mod.rs](crates/sandlang/src/compile/mod.rs#L2281-L2294).

At runtime, `compose` at [composed.rs](crates/sandlang/src/codec/composed.rs#L178-L200) executes 5 steps:

1. It iterates over the dictionary indices in `template.holes` and calls `dict_schema(vm, arg)` at [composed.rs](crates/sandlang/src/codec/composed.rs#L154-L164) on each evidence dictionary.
2. `dict_schema` inspects the dictionary tuple. If the last element of the dictionary tuple is an integer `SchemaId` placed there by an earlier `compose` call, `dict_schema` returns that runtime `SchemaId`. Otherwise, if slot `0` is a closure whose `Proto` has a `SchemaId` in `Program::proto_schemas`, `dict_schema` returns that compile-time `SchemaId`.
3. If every hole's evidence dictionary provides a `SchemaId`, `RuntimeSchemas::instantiate` at [composed.rs](crates/sandlang/src/codec/composed.rs#L101-L126) instantiates the `SchemaTemplate`. It replaces each `TemplateRef::Hole(h)` with the `h`-th hole's `SchemaId` via `map_parts` at [composed.rs](crates/sandlang/src/codec/composed.rs#L129-L150).
4. `RuntimeSchemas` caches the instantiated `SchemaId` in `Built::by_holes` at [composed.rs](crates/sandlang/src/codec/composed.rs#L68). Repeated calls with the same type arguments reuse the cached `SchemaId`.
5. `compose` builds a new dictionary tuple. Slot `0` of the new tuple is the `ComposedEncode` or `ComposedDecode` method from [codec.rs](crates/sandlang/src/check/codec.rs#L802-L818). `compose` copies any remaining method slots such as `decode_missing` from `dict`. It then appends the integer `SchemaId` as the final tuple element.

Runtime schemas use indices starting at `RUNTIME_SCHEMA_BASE = 1 << 31` at [composed.rs](crates/sandlang/src/codec/composed.rs#L23). Each program execution allows at most `MAX_RUNTIME_SCHEMAS = 1024` runtime schema nodes at [composed.rs](crates/sandlang/src/codec/composed.rs#L27). If a program exceeds 1,024 runtime schema nodes, `instantiate` returns `None`, and `compose` returns the original bytecode dictionary `dict`.

### 4. Protocol Enforcement, Error Origin Tracking, and Rewinding

A hand-written `encode` or `decode` method can execute arbitrary Sand code. It might return early without reading or writing a value, read or write 2 values at the same nesting level, create a custom `Error`, or catch a failed `d.decode()` call and retry with a different type.

To enforce the codec rules without slowing down derived or built-in methods, `Checker::check_codec_protocol` at [codec.rs](crates/sandlang/src/check/codec.rs#L257-L265) wraps every user-written `Encode.encode` and `Decode.decode` method in a generated `CheckedEncode` or `CheckedDecode` wrapper at [codec.rs](crates/sandlang/src/check/codec.rs#L795-L827):

- **Checked Encode Wrapper**:
  Calls `Encoder.enter_impl(e, def_id)`, calls the user's `encode(self, e)` function, and calls `Encoder.leave_impl(e)`.
- **Checked Decode Wrapper**:
  Calls `Decoder.enter_impl(d, def_id)`, calls the user's `decode(d)` function to obtain `res`, and returns `Decoder.leave_impl(d, res)`.

`EncodeProtocol` and `DecodeProtocol` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L18-L486) implement the state machines for these wrappers.

#### Single-Value Enforcement

- `EncodeProtocol` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L52-L194) tracks the open container levels and a stack of `EncodeCall` entries at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L18-L25) for active hand-written `encode` calls. Each `EncodeCall` records the type definition, the nesting level, and the number of values already written at that level before the call began. Before every format write operation, `with_encoder` at [mod.rs](crates/sandlang/src/codec/mod.rs#L435-L468) checks `EncodeProtocol::takes_value` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L101-L108). If the call already wrote a value at its starting level, `EncodeProtocol::check_value` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L112-L121) aborts with `"<path>: Encode for <Type> wrote 2 values"`. In `Encoder.leave_impl`, `EncodeProtocol::leave` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L149-L165) verifies that the call closed any containers it opened and wrote 1 value. If the call wrote zero values, `leave_error` aborts with `"<path>: Encode for <Type> wrote no value"`.
- `DecodeProtocol` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L253-L486) maintains a stack of `DecodeCall` entries at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L211-L222) for active hand-written `decode` calls. Each `DecodeCall` records the type definition, the nesting level, the count of values read at that level, the input cursor mark, and the error count. If a hand-written `decode` method attempts a second read at its starting level, `DecodeProtocol::check_value` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L353-L359) records a protocol error `"<path>: Decode for <Type> read 2 values"`. If `Decoder.leave_impl` receives `Ok` while the read count is `0`, `DecodeProtocol::leave` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L411-L444) returns an `"invalid"` error with message `"<path>: Decode for <Type> read no value"`.

#### Error Origin Tracking

If a `Decode` method returns `Err(err)`, the runtime must determine the origin of `err`. If the `Decoder` created `err`, `err.message` already begins with the failure path, such as `$.items[1]: expected int`. If user code created `err`, such as `Email.init` returning `Error("invalid email", kind: "invalid")`, `err.message` lacks the prefix `$.email: `.

`keep_error` at [mod.rs](crates/sandlang/src/codec/mod.rs#L678-L682) and `result_error` at [mod.rs](crates/sandlang/src/codec/mod.rs#L709-L721) track error provenance without adding hidden fields to Sand's `Error` struct:

1. `VmValue` is the virtual machine's 64-bit value representation, which stores a heap index for heap-allocated objects. If a `Decoder` native function creates an `Error` object on the virtual machine heap, `keep_error` appends that `Error`'s `VmValue` handle to the `Decoder`'s traced value list. It then increments `protocol.errors` via `DecodeProtocol::add_error` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L289-L291).
2. In `leave_decode` at [mod.rs](crates/sandlang/src/codec/mod.rs#L726-L738) or `end_decode` at [mod.rs](crates/sandlang/src/codec/mod.rs#L761-L778), `result_error` compares the `VmValue` handle of a returned `Err(err)` against the last `protocol.errors` handles stored in the `Decoder`'s traced value list.
3. If the handle matches a stored decoder error, `result_error` classifies `err` as `ErrorOrigin::Decoder`. `DecodeProtocol::leave` returns `Ending::Result`, and `apply_ending` at [mod.rs](crates/sandlang/src/codec/mod.rs#L743-L756) returns `Err(err)` unchanged.
4. If the handle does not match a stored decoder error, `result_error` classifies `err` as `ErrorOrigin::Implementation`. `DecodeProtocol::leave` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L418-L423) truncates the path to the call's starting level and returns `Ending::Locate` with the formatted path string. `apply_ending` at [mod.rs](crates/sandlang/src/codec/mod.rs#L750-L752) prepends that path to `err.message`, preserves `err.kind`, and records the new `Error` handle via `keep_error`. Recording the new handle ensures that outer `leave_decode` wrappers treat the updated error as `ErrorOrigin::Decoder` and do not prepend a second path.

#### Automatic Format Rewinding on Retry

A hand-written `Decode.decode` method can try decoding 1 type, match on the `Result`, and decode a second type if the first decode returned `Err`:

```sand
impl Decode for Entry {
    fn decode(d: Decoder) -> Result<Entry> {
        let r: Result<lab.Reading> = d.decode()
        if let Ok(v) = r {
            return Ok(Entry.Reading(v))
        }
        Ok(Entry.Note(d.decode()?))
    }
}
```

This retry pattern works even if the first `d.decode()` call partially read a container before failing:

1. At the start of a hand-written `decode` call, `DecodeProtocol::enter` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L399-L405) calls `format.mark()` to save the current input cursor in `DecodeCall::mark`. `TextDecoder` saves its byte position and parser state at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L966-L970). `TreeDecoder` saves its container stack depth and cursor at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L436-L440). `ValueDecoder` saves its reader state at [value_format.rs](crates/sandlang/src/codec/value_format.rs#L692-L694) and [value_input.rs](crates/sandlang/src/codec/value_input.rs#L268-L275).
2. `DecodeProtocol` maintains 2 error counters at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L259-L264). `errors` is the total number of kept decoder errors. `settled` is the value of `errors` when the input cursor last sat at the start of a value.
3. If an inner decode fails, `self.errors` exceeds `self.settled`. `DecodeProtocol::failed` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L283-L285) checks that condition.
4. If the enclosing hand-written `decode` method then starts another read, `DecodeProtocol::before` calls `before_cold` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L335-L348) and `rewind` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L372-L382). `rewind` restores the format cursor via `format.rewind(mark)`, truncates the error path to the call's level, resets the read count at that level to `0`, and sets `self.settled = self.errors`.
5. At the end of the hand-written `decode` call, `DecodeProtocol::leave` releases the saved mark via `format.release(call.mark)` at [protocol.rs](crates/sandlang/src/codec/protocol.rs#L436).

### 5. Format Implementations and Specialized Schema Walkers

All 3 formats implement `EncodeFormat` and `DecodeFormat` at [mod.rs](crates/sandlang/src/codec/mod.rs#L63-L310). The general iterative `Schema` walker in [fast.rs](crates/sandlang/src/codec/fast.rs#L136-L872) drives any `EncodeFormat` or `DecodeFormat` using an explicit heap-allocated stack in `EncodeWalk` and `DecodeWalk`. Because those stacks live on the Rust heap rather than the call stack, they support up to `MAX_NESTING_DEPTH = 512` levels of nesting without risking a native thread stack overflow. In addition, the JSON text and host `Value` formats include specialized fast walkers for `Schema` execution.

#### Format 1: JSON Text

- **General Text Encoding and Decoding**:
  `TextEncoder` at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L63-L522) writes UTF-8 JSON bytes into a Rust `String`. It maintains a stack of open bracket states at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L29-L48) to insert commas, colons, and indentation. `TextDecoder` at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L537-L998) parses UTF-8 JSON slices using `Parser` at [json.rs](crates/sandlang/src/stdlib/json.rs#L714-L968) and a stack of open container states.
- **Specialized Schema Text Writer**:
  If `TextEncoder::write_fast` at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L230-L239) is called at the root of a value, it runs `TextEncoder::write_schema` at [walk.rs](crates/sandlang/src/stdlib/json/text/walk.rs#L57-L458) instead of the general `fast::encode` walker. `write_schema` traverses the `Schema` and `VmValue` together using a compact stack of `Step` items. To escape JSON strings quickly, `write_str` at [json.rs](crates/sandlang/src/stdlib/json.rs#L1039-L1071) calls `has_escape`. `has_escape` uses a 64-bit bitwise test across 8-byte words to check 8 bytes at a time for `"`, `\`, or control bytes below `0x20`. If a string has zero escaped bytes, `write_str` copies the entire string slice in a single `push_str` call.
- **Optimistic Schema Text Reader with Resumable Fallback**:
  `TextDecoder::read_fast` at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L708-L725) first calls `TextDecoder::read_schema` at [read.rs](crates/sandlang/src/stdlib/json/text/read.rs#L96-L535). `read_schema` runs an iterative `Reader` at [read.rs](crates/sandlang/src/stdlib/json/text/read.rs#L78-L205) that decodes common JSON types directly without maintaining a `DecodePath`. For struct and named-variant fields, `Frame::Fields` at [read.rs](crates/sandlang/src/stdlib/json/text/read.rs#L439-L463) tracks the next expected field index in `guess` and checks `names.get(guess)` first. Keys that appear in declaration order therefore match in constant time. If `Reader::open` encounters a schema variant that requires special decoding at [read.rs](crates/sandlang/src/stdlib/json/text/read.rs#L272-L308), such as `Bytes`, `Range`, `Native`, `Json`, `Set`, or a `Map` with non-string keys, it records that schema in `self.rare` and pauses at the current byte offset. `TextDecoder::read_with_rare_parts` at [read.rs](crates/sandlang/src/stdlib/json/text/read.rs#L121-L162) then calls the general walker `fast::decode` for that subvalue, passes the decoded value back into `Reader`, and resumes `Reader` from that byte offset. If `read_schema` encounters a syntax error or type mismatch, `read_fast` at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L720-L723) refunds the execution fuel spent and runs `fast::decode` from the start of the value to build the exact error message and `DecodePath`.

#### Format 2: In-Memory `json.Json` Tree

- **`TreeEncoder`**:
  Implemented at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L502-L940), `TreeEncoder` backs `json.to_json(value)`. It constructs `json.Json` enum variants on the virtual machine heap. To encode a Sand `str` into `Json.Str`, `TreeEncoder::str_value` at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L780-L783) reuses the existing `VmValue` string handle without copying its character buffer. `TreeEncoder::name` at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L653-L680) caches the heap string handles of struct field names and enum variant names in `self.names` so each name is allocated at most once per native call.
- **`TreeDecoder`**:
  Implemented at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L50-L473), `TreeDecoder` backs `json.from_json(value)`. It traverses an existing `json.Json` value in place on the virtual machine heap by storing cursor indices into list and map containers at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L27-L36). To decode a `Json.Str` variant or object key into a Sand `str`, `TreeDecoder::str` at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L282-L290) returns the inner `VmValue` string handle directly in constant time.

#### Format 3: Host `Value`

- **`ValueEncoder`**:
  Implemented at [value_format.rs](crates/sandlang/src/codec/value_format.rs#L33-L558), `ValueEncoder` converts a virtual machine value into a host `sandlang::value::Value`. `Value` preserves exact Sand types, including `Duration`, `Bytes`, `Tuple`, `Set`, `Range`, `Struct`, `Variant`, and `Native`. `StructRt` and `VariantRt` at [bytecode.rs](crates/sandlang/src/vm/bytecode.rs#L1219-L1255) cache shared `Arc<StructDesc>` and `Arc<VariantDesc>` descriptors in `OnceLock` cells so every encoded struct or variant instance shares the same descriptor allocation. If `value_encoder` has a fast-path `SchemaId`, `ValueEncoder::build` at [value_format.rs](crates/sandlang/src/codec/value_format.rs#L223-L398) builds the `Value` tree using flat value and frame stacks.
- **`ValueReader` and `ValueDecoder`**:
  Implemented at [value_input.rs](crates/sandlang/src/codec/value_input.rs#L1-L965) and [value_format.rs](crates/sandlang/src/codec/value_format.rs#L565-L772), `ValueReader` is the shared reader used by both Rust's `Value::decode::<T>()` and Sand's `ValueDecoder`. `Decoder::from_value` at [mod.rs](crates/sandlang/src/codec/mod.rs#L403-L405) constructs `ValueDecoder`. `ValueReader` accepts both native `Value` variants such as `Value::Struct` and JSON-shaped `Value` variants such as `Value::Map`. `ValueReader` maintains a stack of open containers at [value_input.rs](crates/sandlang/src/codec/value_input.rs#L253-L296), where each entry holds a `Cow<'v, Value>` and the next child index. This representation reads each child element in constant time at any nesting depth.

### 6. Garbage Collector Tracing and Zero-Copy Lending in Native Objects

On the driven bytecode path, an `Encoder` or `Decoder` is a heap-allocated native object stored as an `Obj::Native` entry on the virtual machine heap. That object lives across multiple bytecode instructions. A native Rust object must implement `Send` so a `Vm` can move between host threads. At the same time, any `VmValue` heap references held by that native object must be visible to the garbage collector during a collection cycle.

Sand solves this with `NativeCell<T>` at [value.rs](crates/sandlang/src/vm/value.rs#L1214-L1225):

```rust
pub(crate) type ObjectValues = smallvec::SmallVec<[VmValue; 1]>;

pub struct NativeCell<T> {
    pub(crate) values: ObjectValues,
    pub(crate) object: T,
}
```

Every `Obj::Native` slot on the virtual machine heap stores a `NativeCell<T>`. During garbage collection, `Obj::children` at [value.rs](crates/sandlang/src/vm/value.rs#L1285) traces every `VmValue` in `NativeCell::values`. If a native function appends values to a native object via `Heap::native_values_mut` at [heap.rs](crates/sandlang/src/vm/heap.rs#L1228-L1237), `get_mut_storing` applies the garbage collector write barriers for the newly stored `VmValue` handles. For temporary access during a single native call, `Heap::take_native_values` and `Heap::put_native_values` at [heap.rs](crates/sandlang/src/vm/heap.rs#L1200-L1216) move `NativeCell::values` out of the cell and restore it before returning.

`Encoder` and `Decoder` use `NativeCell::values` for 3 mechanisms:

1. **Zero-Copy JSON Text Lending**:
   If `decode_text` at [json.rs](crates/sandlang/src/stdlib/json.rs#L185-L193) allocates a driven `TextDecoder` for a Sand `str`, it stores the input `str` handle in slot `0` of the `Decoder`'s `ObjectValues` via `d.push_value(vm, text.raw())`. On each native call into `Decoder`, `with_input` at [mod.rs](crates/sandlang/src/codec/mod.rs#L517-L533) borrows the mutable string buffer inside the `Obj::Str` heap slot with `vm.heap.str_text_mut(input)`. It swaps that string buffer into `TextDecoder::text` via `swap_input` at [text.rs](crates/sandlang/src/stdlib/json/text.rs#L995-L997), runs the parser operation, and swaps the string buffer back into the `Obj::Str` heap slot before returning.

2. **GC-Safe `Json` Tree Lending**:
   `TreeEncoder` accumulates intermediate `VmValue` nodes across native calls, and `TreeDecoder` holds the root `json.Json` `VmValue` along with any owned key strings. `with_encoder` at [mod.rs](crates/sandlang/src/codec/mod.rs#L446-L453) and `lend` at [mod.rs](crates/sandlang/src/codec/mod.rs#L549-L573) take `NativeCell::values` via `take_native_values` and call `format.swap_values(&mut values)` at [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L466-L468) and [tree.rs](crates/sandlang/src/stdlib/json/tree.rs#L931-L935) at the start of each native call. They swap the values back into `NativeCell::values` via `put_native_values` before returning. Therefore, all `VmValue` handles reside inside `NativeCell::values` whenever the garbage collector can run.

3. **Traced Error Handles**:
   `keep_error` at [mod.rs](crates/sandlang/src/codec/mod.rs#L678-L682) appends every `Error` handle created by the `Decoder` to the end of `NativeCell::values`. This keeps those `Error` objects alive so `result_error` at [mod.rs](crates/sandlang/src/codec/mod.rs#L709-L721) can compare returned error handles by identity.

### 7. Host Boundary Mechanics: `Encoded<N>` and Custom Rust Codecs

#### Boundary Wrapper Synthesis for `Encoded<N>`

A Rust host function can accept any Sand value that implements `Encode` by using the extractor type `Encoded<N>` at [ty.rs](crates/sandlang/src/native/ty.rs#L371-L400). Here, `N` is the index of the type parameter `<T: Encode>` in the Sand function signature:

```rust
host.source("pub native fn keep<T: Encode>(value: T)");
host.function("keep", |value: Encoded<0>| {
    let v: Value = value.into_value();
});
```

A raw Rust native function cannot call back into the bytecode interpreter mid-call to run a hand-written Sand `impl Encode`. To solve this, if a native function has an `Encoded<N>` parameter, the type checker calls `Checker::add_native_boundary` at [mod.rs](crates/sandlang/src/check/mod.rs#L2831) and [codec.rs](crates/sandlang/src/check/codec.rs#L691-L698):

1. `add_native_boundary` hides the raw Rust native function `raw` and declares a new script wrapper function with the original signature and `FnDeclRef::Boundary(raw)`.
2. For the wrapper function body, `Checker::boundary_body` at [codec.rs](crates/sandlang/src/check/codec.rs#L704-L721) wraps each `Encoded<N>` argument in `Checker::encode_value` at [codec.rs](crates/sandlang/src/check/codec.rs#L725-L735), which emits a call to `prelude.encode_value(arg)` at [prelude.sand](crates/sandlang/src/stdlib/prelude.sand#L399-L407).
3. `prelude.encode_value` calls `value_encoder` at [mod.rs](crates/sandlang/src/codec/mod.rs#L860-L869), which runs the fast schema walk if available or returns a driven `Encoder` so `value.encode(e)` runs in bytecode.
4. `prelude.encode_value` returns the `Encoder` object holding the resulting `Arc<Value>`.
5. The wrapper function passes that `Encoder` object to the raw Rust native function.
6. Inside the raw native function, the `FromVmValue` trait implementation for `Encoded<N>` at [convert.rs](crates/sandlang/src/native/convert.rs#L666-L674) extracts the `Arc<Value>` from the `Encoder` object via `Encoder::encoded_value` at [mod.rs](crates/sandlang/src/codec/mod.rs#L385-L391). Calling `Encoded::into_value` at [ty.rs](crates/sandlang/src/native/ty.rs#L385-L387) uses `Arc::unwrap_or_clone` to take ownership of the `Value` without cloning.
7. Because the boundary function replaces the public function identifier, every use of the function—including direct calls, first-class function values, `defer` statements, and `spawn` expressions—runs `encode_value` before entering Rust.

#### Custom Rust `Encode` and `Decode` Registration

If a Rust struct or enum derives `#[derive(SandStruct)]` or `#[derive(SandEnum)]`, the macro sets `NamedType::VALUE_CODEC = true` at [lib.rs](crates/sandlang-macros/src/lib.rs#L285) and generates Rust `value::Encode` and `value::Decode` implementations by default. To prevent Sand code from defining a conflicting `impl Encode` or `impl Decode` that would disagree with Rust's `value::Encode` or `value::Decode`, `Checker::bound_value_codec_type` at [traits.rs](crates/sandlang/src/check/traits.rs#L501-L505) and `Checker::check_impl_head` at [traits.rs](crates/sandlang/src/check/traits.rs#L552-L562) reject any written Sand `impl Encode` or `impl Decode` for a bound Rust type whose `value_codec` flag is `true`.

If a bound Rust type needs a custom representation, such as `Reading { celsius: 21 }` encoding as `"21C"`, the programmer adds `#[sand(module = "...", no_value)]` to the Rust type. The programmer then implements `sandlang::value::Encode` and `sandlang::value::Decode` in Rust and registers the type with `module.bind::<Reading>().custom_codec::<Reading>()` at [mod.rs](crates/sandlang/src/native/mod.rs#L1020-L1040):

1. `custom_codec::<T>()` appends native `impl Encode for T` and `impl Decode for T` declarations to the module's Sand source and binds the native functions `T.encode` and `T.decode`.
2. The native `T.encode` function converts the `VmValue` into the Rust value `T` via `FromVmValue`, calls `x.encode()` to build a `Value`, and calls `VmVal::<Encoder>::write_value` at [mod.rs](crates/sandlang/src/codec/mod.rs#L1256-L1259) to write that `Value` into the active `EncodeFormat`.
3. The native `T.decode` function calls `VmVal::<Decoder>::read_value` at [mod.rs](crates/sandlang/src/codec/mod.rs#L1271-L1283) to read a `Value` from the active `DecodeFormat`, runs `value.decode::<T>()`, and on `Err(e)` calls `VmVal::<Decoder>::value_error` at [mod.rs](crates/sandlang/src/codec/mod.rs#L1287-L1291) to attach the outer `Decoder`'s path to the inner `DecodeError`.

### 8. Compiler and Inliner Optimizations for Trait Dispatch

Three compiler optimizations reduce dispatch overhead on the driven bytecode path:

1. **Static Devirtualization and Constant Dictionaries**:
   Each `impl` block has a compile-time identifier `ImplId`. If the concrete `ImplId` is known at a trait call site, `Compiler::static_target` at [mod.rs](crates/sandlang/src/compile/mod.rs#L2765-L2771) resolves the method slot directly to a function prototype or native function index. The compiler then emits a direct call instruction, `Op::CallDirectTo` or `Op::CallNativeTo`, instead of reading a closure from the dictionary tuple. Furthermore, if a dictionary expression depends only on static `impl` blocks and does not call `compose_codec_dict`, `Compiler::const_dict` at [mod.rs](crates/sandlang/src/compile/mod.rs#L2793-L2808) interns the entire nested tuple in the bytecode constant pool as a `Const::Tuple` entry. Loading that dictionary at runtime executes a single `Op::LoadConst` instruction.

2. **Entry-Block Dictionary Hoisting and Loop Method Hoisting**:
   During type checking, `Checker::hoist_dict` at [traits.rs](crates/sandlang/src/check/traits.rs#L1119-L1139) hoists non-constant dictionary expressions to local variable bindings in the function's entry block and deduplicates identical dictionaries with `same_dict`. During bytecode compilation, `Compiler::hoist_candidates` at [mod.rs](crates/sandlang/src/compile/mod.rs#L2988-L2993) detects trait method lookups inside loops and hoists the `Op::GetField` tuple read instruction before the loop.

3. **Inlining Generic Prelude Wrappers**:
   Entry functions and helper methods such as `json.encode`, `json.decode`, `Encoder.encode`, and `Decoder.decode` are small generic functions that accept hidden dictionary parameters. The `HIR` inliner in [inline.rs](crates/sandlang/src/compile/inline.rs#L1-L190) inlines non-recursive script functions whose bodies have at most `MAX_NODES = 12` expression nodes. In `inline_call`, the inliner binds hidden dictionary arguments alongside explicit arguments. Inlining `e.encode(self.x)` into a caller exposes the concrete dictionary of `self.x` at the call site, which enables `static_target` to devirtualize the inner `encode` call.
