# Design: `Encode` and `Decode` as Ordinary Traits

This document expands `experiments/encode-decode-traits.md` with implementation details. If the two documents disagree, follow `experiments/encode-decode-traits.md`. Extra details in this document are candidate designs, not requirements.

## Summary

We propose to make `Encode` and `Decode` ordinary traits that users can implement. Each trait declares a native Rust default method for derived implementations. Users can keep the default methods or write custom implementations.

The design has six goals:

- `Encode` and `Decode` follow the same rules as every other trait.
- `Encode` and `Decode` implementations are composable and nestable.
- A user can implement `Encode` and `Decode` for concrete, generic, and native types.
- A script can encode any `Encode` value as JSON text. It can decode JSON text into any `Decode` type.
- Host code can encode any `Encode` value into a `Value`. It can decode a `Value` into any `Decode` type.
- The walkers encode and decode `json.Json` under the same rules as every other type.

## Terms

- **Dictionary.** A dictionary is a tuple that supplies a trait implementation to generic code at runtime. The checker selects the dictionary from static types. The caller passes it as a hidden argument. As documented in `crates/sandlang/src/check/traits.rs:3-12`, a dictionary has three sections in order:
  1. The method functions of the implementation.
  2. The dictionaries of supertraits that have dictionaries.
  3. The dictionaries required by the bounds of the implementation, such as `T: Close` in `impl<T: Close> Close for [T]`.

  A trait has a dictionary only if the trait or a supertrait declares a method. See `crates/sandlang/src/check/traits.rs:382` and `408-411`. The checker verifies methodless traits such as `Hash` only at compile time.

- **Trait evidence.** Trait evidence is the checker's expression for a dictionary. It can be a hidden parameter, a slot read from another dictionary, or a dictionary constructor.
- **Constant dictionary.** A constant dictionary is a concrete type's dictionary stored in the constant pool. The compiler deduplicates constant dictionaries in `dict_consts` at `crates/sandlang/src/compile/mod.rs:2747-2762`.
- **Dynamic dictionary.** Generic code builds a dynamic dictionary at runtime from the dictionaries of its type parameters. For example, `fn close_all<T: Close>(items: [T])` builds the `[T]: Close` dictionary at runtime.
- **Derived trait.** On `main`, the checker automatically implements `Eq`, `Ord`, `Hash`, `Encode`, and `Decode`. In `crates/sandlang/src/check/derive.rs:200-203`, the checker derives a trait for a struct or enum if every field and payload type implements that trait. It makes two exceptions. It omits `Hash` for structs with `var` fields. It omits `Decode` for structs that declare `init`. In `crates/sandlang/src/check/traits.rs:492-497`, the checker rejects user `impl` blocks for derived traits.
- **Native function.** A native function is a Rust function that Sand code calls.
- **Native default method.** A native default method is a trait method whose default body is a native function. This design adds native default methods.
- **Structural implementation.** A structural implementation is a trait implementation that uses a native default method. The checker generates structural implementations from registrations and type definitions. Users can also write bodyless `impl` blocks that use the native default method.
- **Custom implementation.** A custom implementation is an `impl` block with a user-written method body.
- **Part.** A part is a direct child of a container:
  - A struct field.
  - A variant payload value.
  - A list, set, or tuple element.
  - A map key or map value.
- **Type constructor.** A type constructor is a type without its type arguments, such as `List` for `[int]`. On `main`, the checker enum `Ctor` represents type constructors. This design renames `Ctor` to `TypeConstructor`.
- **Walker.** A walker is a Rust routine that visits a value and its parts. The encode walker reads a Sand value. The decode walker builds a Sand value.
- **Encode sink.** An encode sink is a Rust object that receives output from the encode walker. Each output format provides its own encode sink.
- **Decode source.** A decode source is a Rust object that supplies input to the decode walker. Each input format provides its own decode source.
- **Implementation shape.** An implementation shape is metadata that describes a single level of a type to a walker. The compiler stores a shape ID in each structural dictionary.
- **Part template.** A part template is an expression that the walker evaluates to build a part's dictionary.
- **`Value`.** `sandlang::value::Value` is a Rust type that represents a Sand value outside the VM. Host code can keep a `Value` after a VM run ends.
- **Callback.** A callback is an async call from a native function to a Sand function through `NativeContext::call` in `crates/sandlang/src/native/context.rs:169`. Commit `8a51c46` added callbacks.

## Why `Encode` and `Decode` need dictionaries

### The two traits on `main`

On `main`, `crates/sandlang/src/stdlib/prelude.sand:313` and `318` declare `Encode` and `Decode` without methods. Only traits with methods have runtime dictionaries. The checker verifies `T: Encode` at compile time. The compiler then erases the bound.

`json.encode` inspects runtime VM tags:

- The VM tags each scalar and inline variant in its value slot.
- The VM tags each heap object with its kind.
- The VM stores a type definition ID on each struct object and a variant ID on each heap variant.
- The VM type table maps those IDs to field and variant names.

`json.decode` must know the target type before it reads input. `crates/sandlang/src/stdlib/json.sand:144` declares its type parameter with `reflect`. Callers pass a runtime `TypeDesc` descriptor as a hidden argument.

### Custom implementations need dictionaries

Trait dictionaries supply erased type arguments to custom generic implementations. Consider `impl<T: Encode> Encode for Box<T>` applied to `Box<Email>`. To encode `Box<Email>`, the walker must find the `Encode` implementation for `Email`. Because the VM omits type arguments from heap objects, the walker cannot recover `Email` from the value alone. Sand callers already pass trait dictionaries to supply erased trait implementations.

### Why heap objects do not record type arguments

If the VM recorded type arguments on runtime values, every program would pay two costs:

- **Every generic call would pass type descriptors.** The compiler emits a single function body for all instantiations of a generic function. To store `T` on the new list, `fn empty_list<T>() -> [T] { [] }` would need a descriptor for `T`. Every caller would have to pass that descriptor. On `main`, callers pass hidden arguments only for `reflect` parameters and for bounds on traits with methods. See `crates/sandlang/src/check/traits.rs:3-6`.
- **Inline values would need heap objects.** A `VmValue` occupies 16 bytes in `crates/sandlang/src/vm/value.rs:60`. In `crates/sandlang/src/vm/value.rs:26-56`, the VM packs scalars, payload-free variants such as `None`, single-payload variants such as `Some(3)`, and integer pairs inline. An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. To store type arguments, the VM would have to allocate a heap object for every `None`.

With trait dictionaries, callers pass hidden arguments only to functions that require traits with methods.

## Part 1: Trait declarations

The prelude declares both traits with native default methods:

```sand
/// Converts a value into an external format such as JSON text.
pub trait Encode {
    /// Writes `self` to `encoder` as a single value.
    /// Returns an error if `self` cannot be encoded.
    native fn encode(self, encoder: Encoder) -> Result<()>
}

/// Builds a value from an external format such as JSON text.
pub trait Decode {
    /// Reads a value of type `Self` from `decoder`.
    native fn decode(decoder: Decoder) -> Result<Self>
}
```

A custom `encode` method writes its value through the `Encoder` argument. It returns `Result<()>`. See [Design decisions](#design-decisions).

Native default methods follow the same rules as other default methods. If an `impl` block omits the method body, the compiler uses the native default method. Callers pass the `Self` dictionary to every trait method as its first hidden argument. See `crates/sandlang/src/check/traits.rs:9-11`. The native default walker reads the implementation shape and part dictionaries from that `Self` dictionary.

### Independent traits

`Encode` and `Decode` are independent traits without a supertrait link. In commit `49d4022` on `main`, the supertrait `Decode: Encode` restricted `json.decode` to `Encode` types without `init`. Under this design, users implement `Decode` directly. That restriction is obsolete. Both `serde` and Swift also keep the two traits separate:

- In `serde_core` 1.0.229, `src/de/mod.rs:554` declares `pub trait Deserialize<'de>: Sized` without a `Serialize` supertrait.
- In the Swift repository, `stdlib/public/core/Codable.swift:43-47` declares `Encodable` and `Decodable` as separate protocols. It defines `Codable` as "a type alias for the `Encodable` and `Decodable` protocols".

### `Num` keeps `Encode`

`Num` keeps `pub trait Num: Ord + Encode` in `crates/sandlang/src/stdlib/prelude.sand:321`. On `main`, in `crates/sandlang/tests/spec/std_json.test:554`, the spec test `"data implements Encode, and Num implies it"` passes a `T: Num` value to a function that requires `T: Encode`.

The `Encode` supertrait on `Num` adds a single supertrait slot to two constant dictionaries. Existing `Num` method offsets stay unchanged. `crates/sandlang/src/check/traits.rs:491-497` restricts `Num` implementations to the prelude. `crates/sandlang/src/stdlib/prelude.sand:328-337` implements `Num` only for `int` and `float`. `native::Num` reads `zero` and `one` by slot index in `crates/sandlang/src/check/traits.rs:466-474`. Dictionary layout places method slots before supertrait slots. `zero` and `one` stay at slots 0 and 1.

## Part 2: Structural implementations

### `TypeConstructor`

We rename `Ctor` in `crates/sandlang/src/check/mod.rs:82-97` to `TypeConstructor`. We move it to `crates/sandlang/src/types.rs` beside `TypeDefKind`. We also rename related functions such as `ctor_of` in `crates/sandlang/src/check/mod.rs:3350`. In the parser, `PatternKind::Ctor` represents a pattern constructor. It keeps its name.

We document `TypeConstructor` as follows:

```rust
/// The constructor of a type without its type arguments, such as `List` for
/// `[int]`. The checker indexes trait implementations by type constructor.
pub enum TypeConstructor {
    Int, Float, Bool, Str, Bytes, Duration, Unit, Range,
    List, Map, Set, Tuple, Fn,
    /// A struct, enum, or opaque type identified by its type definition ID.
    Def(TypeDefId),
}
```

`impl_key` in `crates/sandlang/src/check/traits.rs:119-127` indexes trait implementations by trait, type constructor, and tuple arity. `TypeDefKind` in `crates/sandlang/src/types.rs:700-709` classifies each type definition as a struct, enum, or opaque type. `Opaque::Value` identifies a native value type such as `time.PlainDate`.

### Registration

Rust setup code registers each native default method with its supported types:

- Built-in type constructors such as `List` and `Map`.
- Type definition categories: structs, enums, and native value types.
- Support for `never`.

Setup code registers type categories rather than type definition IDs. The engine runs Rust setup before the checker reads source files.

Each walker module defines a single exhaustive `match` function over type constructors and categories. That function maps each variant to a walker handler or marks it unsupported. Registration calls the same function. A single definition governs both registration and dispatch. Because the `match` covers every variant, the Rust compiler requires an explicit choice for each new constructor or category.

Both walkers support the types that derive `Encode` on `main` in `crates/sandlang/src/check/derive.rs:34-37` and `196-199`. Those supported types include `never`, structs, enums, native value types, and every built-in type constructor except `Fn`. Both walkers exclude tasks, channels, and native reference objects. The decode walker also supports structs that declare `init`.

A Rust type bound in a native module implements `Encode` or `Decode` only if its derive attribute declares the trait. See [Native types](#native-types).

### How the checker builds structural implementations

**Built-in type constructors.** For each registered built-in constructor, the checker generates a structural implementation that requires the trait on every type argument. For `List`, it generates `impl<T: Encode> Encode for [T] {}`. The checker indexes tuple implementations by arity. At the first lookup of a tuple arity, the checker generates a tuple implementation with a type parameter per element.

**Structs and enums.** The checker computes the bounds of each structural implementation through ordinary trait resolution:

1. The checker declares the headers of all custom implementations.
2. For each struct or enum without a custom implementation, the checker resolves the trait for each part type. It treats the type parameters of the struct or enum as unknowns. It records every required bound on a type parameter across all traits.
3. During resolution, the checker assumes that in-progress structural implementations satisfy the trait. It repeats step 2 until the recorded bounds reach a fixed point.

For `struct Pair<A, B> { first: A, second: [B] }`, this pass produces `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.

The derivation pass on `main` cannot propagate custom trait bounds from part types. Consider a `Box<T>` field under a custom `impl<T: Describe> Encode for Box<T>`. The enclosing struct must require `T: Describe`. On `main`, the derivation pass misses that bound for three reasons:

- `Derived::Needs` in `crates/sandlang/src/check/derive.rs:14-19` records only parameter indices for the same trait.
- The pass consults only its own derivation table for type definitions in `crates/sandlang/src/check/derive.rs:28-33`.
- The checker runs the pass before declaring module `impl` headers in `crates/sandlang/src/check/mod.rs:1328-1330`, `1365`, and `1366-1374`.

We reuse `propagate_to_fixed_point` in `crates/sandlang/src/check/derive.rs:166-190`. As `crates/sandlang/src/check/derive.rs:123-125` states, greatest fixed-point iteration ensures "that mutually recursive types implement a trait unless a component type refutes it". We replace its per-type index list with type-parameter and trait pairs. The checker runs this pass after declaring `impl` headers and before `check_impl_supertraits` in `crates/sandlang/src/check/mod.rs:1375-1377`.

**Structs that declare `init`.** For a struct with `init`, the checker derives `Decode` from `init`'s parameter types instead of the struct's fields. See [Decoding a struct that declares `init`](#decoding-a-struct-that-declares-init). The decode walker ignores `init` visibility. The structural `Decode` implementation also requires `init`'s trait bounds. The walker passes those bound dictionaries to `init`.

**`never`.** If a trait registration includes `never`, the checker generates a structural implementation for `never`. `never` implements zero other traits with methods. See `crates/sandlang/src/check/traits.rs:1123` and `crates/sandlang/src/check/verify.rs:228`. Both `serde` and Swift also implement codec traits for `never` independently:

- In `serde_core` 1.0.229, `src/de/impls.rs:42-49` returns `Err(Error::custom("cannot deserialize `!`"))` from `Deserialize` for `!`. `src/ser/impls.rs:351-357` implements `Serialize` for `!` with `*self`.
- Swift SE-0396 conforms `Never` to `Encodable` and `Decodable`. For `Decodable`, SE-0396 states that "the implementation throws a `DecodingError.typeMismatch` error if decoding is attempted."

### Custom implementations

The checker accepts user `impl` blocks for `Encode` and `Decode` under standard trait rules.

A custom implementation replaces the structural implementation for its type constructor. See `crates/sandlang/src/check/traits.rs:537-550`. The checker rejects a second implementation for a trait, type constructor, and tuple arity if it can unify their trait arguments. Because `Encode` and `Decode` take zero trait arguments, an `impl Encode for W<int>` block replaces the structural `Encode` implementation for every `W<T>`. `W<str>` then does not implement `Encode`. [Open question 4](#open-questions) asks whether to keep this rule.

If an `impl` block omits the method body, the checker builds a structural implementation with the block's declared bounds. The checker verifies that every part implements the trait under those bounds. For example, `impl<T: Encode> Encode for Box<T> {}` is valid if every part of `Box<T>` implements `Encode` under `T: Encode`.

Only the prelude can implement `Encode` or `Decode` for built-in types. Under the orphan rule in `crates/sandlang/src/check/traits.rs:527-535`, a module can implement a trait only if it declares either the trait or the type. That check assigns built-in types to the prelude.

### Scope: `Eq`, `Ord`, and `Hash`

This branch updates only `Encode` and `Decode`. The checker continues to derive `Eq`, `Ord`, and `Hash` through `crates/sandlang/src/check/derive.rs`. Until a later change migrates those three traits, the checker keeps two derivation mechanisms.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries need more data

A walker needs the dictionary of every part, even for concrete field types. Standard dictionaries store bound dictionaries only for type parameters. For `struct User { email: Email }`, a standard `User` dictionary stores only `User`'s method. The walker reads `Email`'s `Encode` dictionary from an implementation shape.

### Dictionary layout

A structural dictionary uses this tuple layout:

```text
[native default method, shape ID, bound dictionaries...]
```

Because neither trait has supertraits, slot 1 follows the method slot directly. A custom dictionary keeps the standard `[method, bound dictionaries...]` layout. Ordinary trait calls read only slot 0. The walker checks slot 0 to distinguish structural and custom dictionaries. If slot 0 holds the native default method, the walker reads the shape ID from slot 1.

### The shape table

The compiler stores shapes in a side table on `Program`. It places the integer shape index in slot 1 of each structural dictionary. `Program` already stores type descriptor templates in `desc_templates` at `crates/sandlang/src/vm/bytecode.rs:1335`.

Integer shape IDs keep the constant pool acyclic. In `crates/sandlang/src/vm/mod.rs:740-752`, the VM builds constant tuples in index order. Each constant tuple can reference only earlier constants. Direct references for a recursive `Tree` would form a constant cycle between `Tree`'s dictionary, its shape, and `[Tree]`'s dictionary. An integer shape ID breaks that cycle at the shape. A part template can reference a dictionary with the same shape ID because the compiler allocates the shape ID first.

Each shape stores three groups of fields:

- **Constructor.** This field stores the `TypeConstructor` or a `never` marker. For `TypeConstructor::Def`, the checker's type definition ID equals the VM type table index because `crates/sandlang/src/compile/mod.rs:418-419` appends VM type records in checker order. The walker reads field names and variants from that VM table, just as `json.encode` does on `main`.
- **Part templates.** This slice stores a `PartTemplate` for each part.
- **Decode metadata.** A `Decode` shape stores three extra items absent from the VM type table:
  - Struct field defaults: each entry holds a constant or the field's default helper function from `crates/sandlang/src/check/mod.rs:1610-1632`. On `main`, `FieldDefault::Computed` in `crates/sandlang/src/vm/value.rs:1135-1145` points to a post-decode fill function instead.
  - Constructor metadata for structs with `init`: the `init` function, its parameters, and their defaults.
  - Native value parser: the parse function from `ValueTypeInfo` in `crates/sandlang/src/types.rs:721-725`.

Shapes are codec metadata rather than general trait metadata. Only native default methods read shapes. In this branch, only `Encode` and `Decode` declare native default methods.

### Part templates

Part templates replace `DescTemplate` from `main`. A `DescTemplate` builds an entire `TypeDesc` at runtime from type argument descriptors. A `PartTemplate` builds only the dictionary of a single part:

```rust
enum PartTemplate {
    /// Loads a constant dictionary, method function, or shape ID.
    Constant(u32),
    /// Reads a bound dictionary from the enclosing dictionary.
    Bound(u32),
    /// Builds a dictionary tuple from element templates.
    Tuple(Vec<PartTemplate>),
}
```

The compiler emits a template for each part kind:

- **Concrete part.** For `email: Email` in `impl Encode for User`, the compiler emits `Constant(k)`. Here `k` indexes `Email`'s constant `Encode` dictionary.
- **Type parameter.** For elements in `impl<T: Encode> Encode for [T]`, the compiler emits `Bound(0)`. The walker reads the first bound dictionary from the enclosing dictionary.
- **Generic part.** For `items: [T]` in `impl<T: Encode> Encode for Page<T>`, the compiler emits `Tuple([Constant(native_encode), Constant(list_shape), Bound(0)])`. The walker evaluates those three elements to build the `[T]` dictionary.

The walker charges fuel during part template evaluation. It also enforces a template depth limit. On `main`, `op_describe` in `crates/sandlang/src/vm/interp.rs:1952-1954` charges fuel per template instantiation. `DescTemplate::instantiate` in `crates/sandlang/src/vm/value.rs:1384-1389` enforces the depth limit.

### How the compiler builds part templates

For generic function bodies, the checker already builds trait evidence trees of dictionary constructors, slot reads, and constant loads. The compiler lowers those trees to bytecode. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same kind of evidence tree. The compiler converts that tree into a `PartTemplate` instead of bytecode.

### Worked example

This example traces how the walker reaches a custom method across an erased type argument. At runtime, the VM stores only the `Page` type ID on a `Page<Email>` heap object:

```sand
struct Email { address: str }

impl Encode for Email {
    fn encode(self, encoder: Encoder) -> Result<()> { encoder.encode(self.address) }
}

struct Page<T> { items: [T] }

fn format_page<T: Encode>(page: Page<T>) -> Result<str> { json.encode(page) }

format_page(Page(items: [Email(address: "a@b.c")]))
```

The user writes a custom `Encode` implementation for `Email`. The checker generates a structural implementation for `Page<T>` that requires `T: Encode`:

1. **The caller passes the `Email` dictionary.** `format_page` requires `T: Encode`. Its caller passes `Email`'s `Encode` dictionary. Because `Email`'s custom implementation has zero bounds, this dictionary is a 1-element tuple `[Email.encode]`.
2. **`format_page` builds the `Page<T>` dictionary.** `json.encode(page)` requires `Page<T>: Encode`. Because `T` is generic, `format_page` builds a 3-element tuple at runtime:
   - Slot 0 holds the native default `Encode.encode`.
   - Slot 1 holds `Page`'s shape ID.
   - Slot 2 holds `T`'s bound dictionary. In this call, slot 2 holds `Email`'s dictionary.
3. **The walker visits `page`.** `json.encode` starts the walker with `page` and the `Page<T>` dictionary. Slot 0 holds the native default method. The walker reads `Page`'s shape ID from slot 1.
4. **The walker builds the `[T]` dictionary.** `Page`'s shape holds `Tuple([Constant(native_encode), Constant(list_shape), Bound(0)])` for `items`. `Bound(0)` reads slot 2 of the `Page<T>` dictionary. Evaluation builds `[Encode.encode, list_shape, Email_dict]`.
5. **The walker visits the list.** Slot 0 of the `[T]` dictionary holds the native default method. The walker processes the list in Rust. The list shape's `Bound(0)` template reads slot 2 of the `[T]` dictionary to get `Email`'s dictionary.
6. **The walker calls `Email.encode`.** Slot 0 of `Email`'s dictionary holds `Email.encode` rather than the native default method. The walker calls `Email.encode` through a callback.

`Decode` dictionaries use the same layout. Because `Decode` is independent of `Encode`, a `Decode` dictionary omits any `Encode` supertrait slot.

## Part 4: Recursive types

### Dictionaries on demand

The walker builds a part's dictionary only if it visits a value of that part. With on-demand construction, the walker terminates on all three forms of recursive types:

- **Direct recursion.** In `struct Tree { children: [Tree] }`, `Tree`'s shape holds `Constant(k)` for `children`. Here `k` indexes the constant `[Tree]` dictionary. That `[Tree]` dictionary holds `Tree`'s dictionary in its bound slot. See [The shape table](#the-shape-table) for why these constants stay acyclic.
- **Generic recursion.** For `Tree<T>`, the compiler emits the finite template `Tuple([native, list_shape, Tuple([native, tree_shape, Bound(0)])])` for `children`. At a `children` field, the walker builds the `[Tree<T>]` dictionary. From that dictionary, it builds the inner `Tree<T>` dictionary.
- **Polymorphic recursion.** In `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`, each `Deep` value wraps `T` in another list layer. At each `Deep` value, the walker builds a single `Nest<[T]>` dictionary from `T`'s dictionary. Each `Deep` wrapper also adds a value nesting level. The 512-level limit in `crates/sandlang/src/vm/container_children.rs:15` bounds dictionary construction.

Go's `encoding/json` likewise builds and caches type handlers at runtime. In `src/encoding/json/encode.go:386-400` of the Go repository, it breaks cycles with an indirection: "To deal with recursive types, populate the map with an indirect func before we build it".

### Per-walk dictionary cache

During a walk, the walker caches each dynamic part dictionary by shape ID, part index, and enclosing dictionary. Consider a list of 10,000 `Tree<T>` values in a generic function. The walker builds `Tree<T>`'s `children` dictionary once per enclosing dictionary rather than once per element. The walker roots cached dictionaries alongside its other heap values.

## Part 5: Native walkers

### Format-independent walkers

Both walkers visit Sand value structure and delegate all format rules to sinks and sources. The encode walker writes to an `EncodeSink`. The decode walker reads from a `DecodeSource`. Each walker is generic over its sink or source. In `serde_core` 1.0.229, `src/ser/mod.rs:263-265` likewise makes `Serialize` generic over the format with `fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer`.

Unlike `serde`'s generated per-type code, the walkers read implementation shapes at runtime. Go's `encoding/json` similarly reads runtime type metadata.

This shared walker reverses a decision in `SPLIT.md` changeset 14. Changeset 14 rejected a shared sink walk because it "required about 300 extra lines and `#[inline(always)]` across two trait layers." With callbacks inside a walk, three separate walkers would duplicate callback suspension, rooting, and resumption. A single walker with three sinks implements that logic once.

### Walker state across callbacks

Each walker keeps an explicit stack of open containers instead of recursing on the Rust stack. Stack entries hold indices and rooted handles rather than heap borrows. During a callback, Sand code can run the garbage collector. The collector traces rooted handles rather than Rust local variables. A suspended Rust `Future` also cannot hold `&Heap` borrows across a callback.

Both walkers need new stack code:

- **Encode walker.** `Vm::to_value` and the JSON text encoder already walk with explicit stacks. However, `ContainerChildren` in `crates/sandlang/src/vm/container_children.rs:32-43` holds borrowed slice iterators. The encode walker replaces `ContainerChildren` with index-based stack entries.
- **Decode walker.** The typed JSON decoder in `crates/sandlang/src/stdlib/json.rs:1007-1133` recurses on the Rust stack. We rewrite it to use an explicit stack. `TODO.md:124-127` tracks this conversion with the requirement "without regressing throughput". In `crates/sandlang/src/stdlib/json.rs:567`, `Parser<'a>` borrows its input text. In `crates/sandlang/src/stdlib/json.rs:883-886`, `Seg::Key(Cow<'a, str>)` borrows keys for error paths. The JSON source replaces those borrows with a rooted input handle and a byte offset.

Each async native call owns a root table. `NativeContext::root` in `crates/sandlang/src/native/context.rs:143-145` registers heap values for garbage-collector tracing. The scheduler in `crates/sandlang/src/vm/sched.rs:514` counts root table capacity against `max_memory`. Before each callback, the walker roots three sets of values:

- Open containers on the walk stack.
- Decoded child values buffered for unfinished containers.
- Dictionaries of open containers.

Rooted handles remain valid across callbacks because the collector in `crates/sandlang/src/vm/heap.rs:1-3` never moves heap objects.

This design lets an async native call release roots before the call returns. `TODO.md:46-53` tracks this change: "Let an async native function release roots during a call." On `main`, the engine only appends to the root table during a call. Without mid-call release, a walk with many callbacks would retain every rooted value until the walk ends.

Before each callback, the decode walker closes its active `Staging` closure. After the callback returns, it opens a fresh `Staging` closure. As `crates/sandlang/src/vm/heap.rs:447-450` documents, staged heap values become valid only after their staging closure returns.

A callback can mutate an open container. For example, a custom `encode` callback can remove an element from an open list. The walker reads each child as it reaches that index. Because the walker roots every open container, the collector keeps mutated containers live. A mutating callback changes only the output. `TODO.md:42-45` tracks mutation detection.

### Synchronous start

Each native codec function starts its walker synchronously. If the walker finishes without callbacks, the function returns `Later::ready` in `crates/sandlang/src/native/func.rs:411-413` without allocating a `Future`. At the first callback, the walker moves its state into a `Future` with `Later::new` in `crates/sandlang/src/native/func.rs:403-408`. Between callbacks, the walker processes derived parts inside a single `NativeContext::with_vm` call rather than calling `with_vm` per part.

Before the first callback, the engine collects garbage on a memory-limit error. It then restarts the native function from the start. See `crates/sandlang/src/vm/mod.rs:927-929`. A full restart is safe before the walker invokes a Sand callback. After the first callback, a full restart would repeat callback side effects. The walker instead runs post-callback allocations inside `NativeContext::step` in `crates/sandlang/src/native/context.rs:179-199`. On a memory-limit error, `NativeContext::step` collects garbage. It then retries only its own closure.

### Speed of the walker

Before building the rest of this design, we will prototype and benchmark per-part dispatch on derived values. We will add a derived-subtree fast path only if that benchmark shows a regression.

We have not yet measured the proposed walker. The closest measurements appear in the "Measurements" section of `experiments/native-async-sand-calls.md`. Those benchmarks ran the prototype walker from commit `1f7bb03` across 5,000 rows:

- The synchronous walker on `main` took 98 to 142 ns per row.
- The async prototype took 98 to 119 ns per row. Its static `Schema::is_derived` check sent derived subtrees to a synchronous walk inside a single `with_vm` call.
- Without `Schema::is_derived`, a per-node `with_vm` call and boxed `Future` took 470 ns per row.

The proposed walker differs from both prototypes. It avoids per-node boxed futures. However, it evaluates a part template and reads slot 0 of a part dictionary at every part. If that per-part work adds measurable cost, the walker can skip it inside subtrees without custom implementations. A precomputed flag on each dictionary would mark subtrees without custom parts. The compiler can compute that flag for constant dictionaries. For a dynamic dictionary, the walker can compute the flag from bound dictionaries during construction and store it in the per-walk cache.

### Encode walker

The encode walker reads a Sand value and writes to an `EncodeSink`. At each part, it builds the part dictionary from the part template and checks slot 0:

- **Native default method.** The walker writes the part directly to the sink in Rust.
- **Custom method.** The walker creates an `Encoder` handle to the shared sink. It calls the custom method through a callback. Inside the callback, `encoder.encode(value)` runs the encode walker on `value` with the same sink. The nested walker writes `value` at the part's output position.

`encoder.encode` returns `Result<()>`. If the nested walk fails, `encoder.encode` returns that error for propagation with `?`. If a custom method returns `Err`, the outer walker stops. It returns that error.

Both the outer walker and `Encoder` hold handles to the shared sink. During a callback, the outer walker waits for the callback to finish. Only the active nested walk writes to the sink.

An `Encoder` accepts a single value during its callback. The engine returns an `"invalid"` error in three cases:

- The custom method returns without writing a value.
- The custom method writes a second value.
- Sand code calls `encoder.encode` after the callback returns.

Each nested `encoder.encode` call increments the nesting depth. If a custom `encode` method writes `self`, the walker stops it at the 512-level limit.

### Encode sink interface

`EncodeSink` provides methods for each value kind:

- Scalars: write unit, `bool`, `int`, `float`, `str`, `bytes`, or `duration`.
- Sequences: begin a list, set, or tuple with its length, then end it.
- Maps: begin a map with its length, then end it.
- Structs: begin a struct with its type name and field names, then end it.
- Enums: write a variant with its type name, variant name, and payload form.
- Options: write `None` or begin `Some`.
- Native value types: write a value with its type name and text.

The walker matches `Some` and `None` by their variant IDs in `RtIds` at `crates/sandlang/src/vm/bytecode.rs:1300-1305`. Sinks receive dedicated option calls without comparing type names.

Three encode sinks implement `EncodeSink`:

- **JSON sink.** Writes JSON text under the rules in `crates/sandlang/src/stdlib/json.sand:1-14`. It writes `Some(inner)` as `inner` and `bytes` as a Base64 string. It writes map keys and native value types as strings. It writes unit variants as strings and payload variants as single-key objects. It returns an `"invalid"` error for non-finite floats and for map keys without a text form. If nesting exceeds 512 levels, the walker stops the run.
- **`Value` sink.** Builds a host `Value`. It writes `Option` as a `Value::Variant` of the prelude `Option` type, just as `Vm::to_value` does on `main`.
- **`Json` sink.** Builds a `Json` tree for `json.to_json`. It shares the JSON rules of the JSON sink.

### Decode walker

The decode walker builds a Sand value from each dictionary's shape. It reads input from a `DecodeSource`. It awaits a callback in three cases:

- **Custom `Decode` implementation.** The walker creates a `Decoder` handle to the shared source at the current position. It calls the custom method through a callback. The method reads its input with `decoder.decode()`.
- **Computed field default.** A struct field can declare a computed default such as `id: int = next_id()`. If the input omits that field, the walker calls the field's default helper after reading the present fields.
- **Struct with `init`.** See [Decoding a struct that declares `init`](#decoding-a-struct-that-declares-init).

Sets and maps hash and insert only finished values because each callback finishes before the walker inserts its result.

Both the outer walker and `Decoder` share the source. A `Decoder` reads at most a single value during its callback. If the custom method returns without reading, the decode walker skips that unread value. If `decoder.decode()` fails, the decode walker returns that read error regardless of what the custom method returns. After a failed read, the walker stops. A failed read can leave the source partway through a value. A custom method cannot retry a failed read as another type. In this draft, a Sand `decode` method also cannot query the next input kind. [Open question 3](#open-questions) asks whether `Decoder` should expose that query.

If a struct field or `init` parameter appears twice in the input, the decode walker returns a `"parse"` error. Callers can handle that error without stopping the run. For duplicate map keys, the decode walker overwrites the earlier entry with the later value.

### Order of callbacks

Because the decode walker awaits each callback in place, it can invoke a custom method or computed default before rejecting a later field. This order differs from `main`. On `main`, `crates/sandlang/src/stdlib/json.sand:137-138` runs computed field defaults "in post-order after parsing succeeds". In `serde_derive` 1.0.229, `src/de.rs:766-779` likewise calls a `default = "path"` function as soon as it finds a missing field.

On `main`, in `crates/sandlang/tests/spec/std_json.test:634-660`, the spec test `"decode runs the defaults that are not constants, as a construction does"` prints the default call count after a failed decode. Under this design, the test prints 6 instead of 5. The walker evaluates `inner.a`'s default before reaching the invalid `items` field.

### Decode source interface

`DecodeSource` provides methods for each walker request:

- Scalars: read unit, `bool`, `int`, `float`, `str`, `bytes`, or `duration`.
- Sequences: begin a list, set, or tuple, read elements, and end the sequence.
- Maps: begin a map, read typed keys and values, and end the map.
- Structs: begin a struct, read field names and values, and end the struct.
- Enums: read a variant name, then read its payload.
- Options: read `None` or begin `Some`.
- Native value types: read the text of a value.
- Next kind: report the kind of the next input value, such as `int`, list, or map. Untyped readers such as `Json`'s `Decode` implementation call this method to select the next read.
- Skip: skip the next unread value.
- Error paths: format the current input path, such as `$.items[2].price`.

Two decode sources implement `DecodeSource`:

- **JSON source.** Parses JSON text on demand and applies JSON conversion rules. It decodes a Base64 string if the walker requests `bytes`. It parses a string key as an `int` if the walker requests an `int` map key.
- **`Value` source.** Reads a host `Value` and accepts only exact `Value` variants. For example, it accepts `Value::Bytes` for `bytes` and rejects a Base64 `Value::Str`.

All JSON rules stay inside the JSON sink and JSON source. The walkers inspect only Sand value structure.

### Native types

Native modules bind Rust types to Sand types with `NativeModule::bind::<T>()` in `crates/sandlang/src/native/mod.rs:904-905` and four derive macros in `crates/sandlang-macros/src/lib.rs`:

- `SandStruct` binds a Rust struct to a Sand struct.
- `SandEnum` binds a Rust enum to a Sand enum.
- `SandValue` binds a `ValueType` Rust type to an opaque Sand value type. As `crates/sandlang/src/vm/value.rs:879-880` notes, the runtime "encodes and decodes values in JSON as strings".
- `SandObject` binds a Rust type to an opaque Sand reference type such as `fs.File` or `net.Conn`.

`bind` reads `T::binding()` and registers the type declaration with the checker. The checker builds the type definition in `crates/sandlang/src/check/mod.rs:1684-1720`.

#### Opt-in

A native type implements `Encode` or `Decode` only if its `#[sand(...)]` derive attribute declares the trait:

- `#[sand(encode, decode)]` selects derived implementations for both traits.
- `#[sand(encode = "manual")]` declares `Encode` without a derived implementation. The author implements the Rust trait `sandlang::native::Encode` manually. `#[sand(decode = "manual")]` works the same way for `Decode`.

The `SandObject` macro accepts only `"manual"` because opaque reference types hide their internal structure. The derive macros reject `encode` and `decode` on generic native types. Every public native type in the standard library is concrete.

On `main`, the checker in `crates/sandlang/src/check/derive.rs:196-199` automatically derives `Encode` for every bound struct, enum, and value type. Under this design, a native type implements `Encode` or `Decode` only if its `#[sand(...)]` attribute declares the trait. Standard library data types add explicit codec attributes where needed.

Manual codecs still require the macro attribute because `bind` is a generic function. Generic code in stable Rust cannot test whether `T: Encode` holds. Rust trait specialization remains unstable. See `https://github.com/rust-lang/rust/issues/31844`. `#[sand(manual_conversion)]` in `crates/sandlang-macros/src/lib.rs:9` already uses this explicit opt-in to omit derived `FromVmValue` and `IntoVmValue` implementations. `json::Json` uses `manual_conversion` in `crates/sandlang/src/stdlib/json.rs:36`. For a `"manual"` codec declaration, the derive macro emits a compile-time assertion that the Rust trait implementation exists.

#### The Rust traits

These illustrative signatures show the Rust codec traits:

```rust
pub trait Encode {
    fn encode(&self, sink: &mut dyn EncodeSink) -> Result<(), HostError<Fail>>;
}

pub trait Decode: Sized {
    fn decode(source: &mut dyn DecodeSource) -> Result<Self, HostError<Fail>>;
}
```

`EncodeSink` and `DecodeSource` expose the [encode sink interface](#encode-sink-interface) and [decode source interface](#decode-source-interface) as Rust traits. In `serde_core` 1.0.229, `src/ser/mod.rs:247-259` likewise passes a `Serializer` to `Serialize::serialize`. Because these methods take trait objects, the compiler emits a single Rust function per manual trait rather than a copy per sink or source. Only manual implementations pay for dynamic dispatch.

`native::Encode` replaces `value::Encode` in `crates/sandlang/src/value.rs:243-275`. A helper converts a Rust value into a `Value` by running its `native::Encode` implementation into the `Value` sink. In `serde_json` 1.0.154, `to_value` in `src/value/mod.rs:995-998` similarly runs `Serialize` into a value builder.

For example, this manual implementation encodes `Money` as integer cents instead of text:

```rust
/// An amount of money in cents, such as `1250`.
#[derive(Clone, PartialEq, Eq, Hash, sandlang::SandValue)]
#[sand(module = "bank", encode = "manual", decode = "manual")]
pub struct Money(u64);

impl sandlang::native::Encode for Money {
    fn encode(&self, sink: &mut dyn EncodeSink) -> Result<(), HostError<Fail>> {
        let cents = i64::try_from(self.0).map_err(|_| HostError::new("invalid", "amount too large"))?;
        sink.write_int(cents)
    }
}

impl sandlang::native::Decode for Money {
    fn decode(source: &mut dyn DecodeSource) -> Result<Money, HostError<Fail>> {
        let cents = source.read_int()?;
        u64::try_from(cents).map(Money).map_err(|_| HostError::new("parse", "negative amount"))
    }
}
```

#### How the walkers run a native implementation

Each type binding records whether each declared codec trait is derived or manual:

- **Derived.** A derived native type has two implementations of a single encoding: the VM walker for heap values and a macro-generated Rust implementation for host values. For Sand calls, the checker builds a structural implementation from struct fields, enum variants, or `ValueType` text. The VM walker reads or builds the heap value directly without calling Rust codec methods. For host `Value` conversion, the derive macro also generates `native::Encode` from the same field list. On `main`, `crates/sandlang/src/value.rs:250-251` states the same rule for `value::Encode`: a derived `encode` "returns the same `Value` that [`Vm::to_value`] copies from the VM". Tests compare the two outputs. [Open question 1](#open-questions) asks how to avoid maintaining two implementations.
- **Manual.** The derive macro generates a Rust wrapper function for each manual trait. It stores that wrapper in the binding as the Sand trait method. If the walker reaches that wrapper method, it calls the wrapper directly in the active Rust walk with the current sink or source. The walker skips Sand callbacks and executes zero Sand bytecode:
  - For `SandObject`, the `Encode` wrapper borrows `&T` from the heap with `VmVal::get` in `crates/sandlang/src/native/mod.rs:571` and calls `T::encode`. For other native types, the wrapper converts the heap value to `T` with `FromVmValue` before calling `T::encode`.
  - The `Decode` wrapper calls `T::decode` and converts the result into a heap value with `IntoVmValue`.

  Except on `SandObject`, each manual wrapper copies the value between the VM heap and Rust. [Open question 1](#open-questions) asks how to avoid that copy.

Manual Rust implementations follow the same rules as custom Sand implementations. Each call writes or reads a single value. Any returned `HostError` becomes the encode or decode error.

### Decoding a struct that declares `init`

On `main`, `crates/sandlang/src/check/derive.rs:251` rejects derived `Decode` on structs with `init` with the error "`{owner}` declares `init`, which decoding does not call". Field-by-field decoding would bypass `init` validation. Native decoders also could not call `init` before callbacks existed.

Under this design, the decode walker constructs such a struct through `init`. It reads `init`'s parameters from the source just as it reads struct fields. For example, the decode walker reads `{"n": 80}` to build `struct Port { n: int, fn init(n: int) -> Result<Port> }`:

1. **Fill missing arguments.** The walker sets each missing `Option` argument to `None`, just as `crates/sandlang/src/stdlib/json.rs:1173` does for missing `Option` fields. For a missing argument with a default, the checker in `crates/sandlang/src/check/mod.rs:2739-2775` lowers the default to a constant or a helper function of the earlier parameters. The walker calls helper functions through callbacks.
2. **Call `init`.** The walker calls `init` with the decoded arguments through a callback.
3. **Unwrap the result.** If `init` returns `Self` or `Ok(value)`, the walker uses `value`. If `init` returns `Err(e)` for an `Error` value `e`, the decode walker returns an error at the struct's path with `e`'s error kind. For any other error type, the decode walker returns an `"invalid"` error at the struct's path with `e`'s display text.

Structural `Encode` still writes the struct's fields. If `init`'s parameters match the fields, `json.decode` rebuilds the value from `json.encode`'s output. If they differ, the decode walker rejects the encoded output at runtime without a prior compile error. [Open question 2](#open-questions) asks how to handle this mismatch.

Other libraries also run validation during decoding:

- In `serde`, the "Container attributes" documentation on `serde.rs` states that `#[serde(try_from = "FromType")]` means "Deserialize this type by deserializing into FromType, then converting fallibly".
- In `kotlinx.serialization`, `serialization-customization-options.md` states: "After deserialization, the `kotlinx.serialization` plugin runs the class's initializer blocks, just like when you create an instance".

### `never`

Because `never` has zero inhabitants, the encode walker never visits a `never` value. For `never`'s shape, the decode walker returns a `"parse"` error just as it does for other type mismatches in `crates/sandlang/src/stdlib/json.sand:140-142`. For `let empty: [never] = []`, `json.encode(empty)` produces `"[]"`. On `"[1]"` for `[never]`, `json.decode` returns a `"parse"` error.

## Part 6: Checker and compiler changes

1. **`TypeConstructor`.** Rename `Ctor` to `TypeConstructor`. Add its doc comment and move it to `crates/sandlang/src/types.rs`. See [`TypeConstructor`](#typeconstructor).
2. **Native default methods.** Accept `native fn` declarations inside `trait` blocks as native default methods. On `main`, `crates/sandlang/src/check/traits.rs:455-461` treats every bodyless trait method as required.
3. **Registration.** Register each native default method in Rust setup code with its supported types. See [Registration](#registration).
4. **Structural implementations.** Compute structural implementations through ordinary trait resolution after declaring `impl` headers. See [How the checker builds structural implementations](#how-the-checker-builds-structural-implementations).
5. **Custom implementations.** Stop treating `Encode` and `Decode` as special derived traits. Accept user `impl` blocks for both traits in `crates/sandlang/src/check/traits.rs:492-497`.
6. **Independent traits.** Remove `Encode` from `Decode`'s supertraits. Keep `Ord + Encode` on `Num`.
7. **`never`.** Give `never` the structural dictionary of each registered trait in `crates/sandlang/src/check/traits.rs:1123` and `crates/sandlang/src/check/verify.rs:228`.
8. **Shapes and part templates.** Build part trait evidence in the checker. Lower that evidence to `PartTemplate` values in the compiler. Store shapes in `Program`'s shape table.
9. **Dictionary parameters for native functions.** Generalize `Num<N>` in `crates/sandlang/src/native/ty.rs:350-356` to `TraitDict<N>`. Generic native functions such as `json.encode<T: Encode>` use `TraitDict<N>` to read hidden dictionaries for any trait bound with methods. Keep `Num<N>` as a typed wrapper around `TraitDict<N>`.
10. **`reflect`.** Delete `reflect` from the language because `Decode` dictionaries supply target type structure. Host native functions declare `Decode` bounds and take `TraitDict<N>` instead of `Reflect<N>`.

This design also removes two existing checker limits on traits with methods across all traits. Local generic functions can require traits with methods. Struct field defaults can also use trait methods. A structural `Decode` implementation inherits any trait bounds required by the struct's field defaults.

## Part 7: JSON and `Value`

- **`json.encode`.** `pub native fn encode<T: Encode>(value: T, indent: int = 0) -> Result<str>` writes `value` to the JSON sink. On `main`, `json.encode` in `crates/sandlang/src/stdlib/json.sand:128` returns `str`. Each caller unwraps or propagates `Result<str>`. For example, `crates/sandlang/src/stdlib/http.sand:159` changes `json.encode(value).bytes()` to `json.encode(value)?.bytes()`. `json.encode` returns an `"invalid"` error for non-finite floats and for map keys without a text form.
- **`json.decode`.** `pub native fn decode<T: Decode>(text: str) -> Result<T>` reads `text` from the JSON source. We delete `decode_raw`, `Decoded`, and the post-decode default loop in `crates/sandlang/src/stdlib/json.sand:144-158`.
- **`json.parse`.** `json.parse` decodes `text` into a `Json` value through the same decode walker and JSON source.
- **`Json`.** `Json` is a native type with manual codecs: `#[sand(module = "json", manual_conversion, encode = "manual", decode = "manual")]`. Its Rust `encode` writes unit for `Null`, a list for `Arr`, a string-keyed map for `Obj`, and scalars directly. Its Rust `decode` queries the next input kind and runs the matching typed read. We remove the `Json` variant-ID special cases from `crates/sandlang/src/stdlib/json.rs:1100-1103` and `1556-1558`. `serde_json` 1.0.154 takes the same approach by implementing `Serialize` and `Deserialize` on `serde_json::Value` in `src/value/ser.rs:11` and `src/value/de.rs:22`.

  This manual wrapper copies the `Json` tree. Before calling `encode`, the `Encode` wrapper converts the heap value into a Rust `Json` tree with `FromVmValue`. After calling `decode`, the `Decode` wrapper converts the Rust `Json` tree back to the heap with `IntoVmValue`. On `main`, `json.encode` reads a `Json` value directly from the heap. We have not yet measured this copy cost.

  This change also updates `Value` conversion for `Json`. On `main`, `Vm::to_value` copies a `Json` value as a `Value::Variant` of `json.Json`. The `Value` sink converts a `Json` value into plain `Value` data such as `Value::Int(3)` for `Json.Int(3)`.

- **`json.to_json`.** `to_json` writes directly to the `Json` sink. Both the JSON text sink and the `Json` sink share a single set of JSON rules. `to_json` keeps `Json` as its return type. The `json!` macro keeps its current form without `Result` unwrapping. If conversion fails, `to_json` stops the run. Under the shared JSON rules, `to_json` also rejects non-finite floats. On `main`, `to_json` keeps a non-finite float as `Json.Float(NaN)`.
- **Conversion to `Value`.** Every conversion to `Value` requires `T: Encode` and takes an `Encode` dictionary. Because a custom `Encode` method can invoke a callback, the engine converts values through a single routine. That routine starts synchronously and suspends only on callbacks. `Later::new` futures call `NativeContext::to_value(value, dictionary)`. Synchronous native functions receive a `Value` that the engine converts before the call. We delete `Value::Opaque` and synchronous `Vm::to_value`.

## Changes to the plan in `SPLIT.md`

`SPLIT.md` divides a source branch into numbered changesets. This design updates changesets 14, 15, and 18:

- **Changeset 14: `Value` and `Vm::to_value`.** Pull request #41 merged changeset 14 as commit `a74cf0d` with three separate encode walks in `json.encode`, `json.to_json`, and `Vm::to_value`. We replace all three walks with a single encode walker and three sinks. See [Format-independent walkers](#format-independent-walkers).
- **Changeset 15: Shared decode path and `Value` input.** This design implements changeset 15 with two changes to its plan:
  - Changeset 15 planned for the `Value` source to accept JSON-encoded forms. This design drops that rule.
  - Changeset 15 planned `value::Decode: Encode` to mirror Sand's supertrait. Rust `value::Decode` drops `Encode` to match the independent Sand traits. Commit `6d23963` on `origin/encode-decode-traits` records the same decision: "`value::Decode` also required `Encode`, which the user decided against."
- **Changeset 18: Host function calls.** The engine decodes each host `Value` argument with the decode walker and `Value` source. For concrete parameter types, the compiler emits constant `Decode` dictionaries.

Changeset 16 needs a new scope for JSON-to-Sand decoding. Changeset 16 adds only `Serialize` and `Deserialize` on `Value`. JSON deserialization into `Value` produces JSON-encoded forms such as Base64 strings. The `Value` source rejects those forms.

## Costs and risks

- **Dictionary passing.** Generic calls with `T: Encode` or `T: Decode` bounds pass a hidden dictionary. Concrete types pass a constant tuple. Generic composite types build a tuple at runtime. In `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`, a generic call with a trait method call took 28.3 ns compared to 16.6 ns for a direct Sand call.
- **Per-part dispatch.** At each part, the walker evaluates a part template and checks slot 0 of the part dictionary. We have not yet measured this cost. See [Speed of the walker](#speed-of-the-walker).
- **Walker state across callbacks.** At the first callback, the walker allocates a `Future` and roots active heap values in proportion to nesting depth. Each nested walk from an `Encoder` or `Decoder` allocates another `Future`. The scheduler counts the root table and top-level `Future` against `max_memory`. As `TODO.md:54-57` notes, the engine does not yet count nested `Future` allocations.
- **Callback order.** A custom method or computed default can run before a later field fails to decode. See [Order of callbacks](#order-of-callbacks).
- **Two derivation mechanisms.** In this branch, `crates/sandlang/src/check/derive.rs` still derives `Eq`, `Ord`, and `Hash`. Registration handles `Encode` and `Decode`.
- **Code size.** We have not yet measured the net line change. We will report the measured delta in the pull request.

  This design deletes five groups of code:
  - The separate encode walks in `json.encode`, `json.to_json`, and `Vm::to_value`, plus `json.to_json`'s converter.
  - The recursive typed JSON decoder.
  - The post-order default fill path in `json.decode`: `decode_raw`, `Decoded`, and `FieldDefault::Computed` fill functions.
  - The `Json` special cases in the JSON encoder and decoder.
  - `crates/sandlang/src/compile/reflect.rs`, `TypeDesc`, `DescTemplate`, `Op::Describe`, and `reflect` support in the parser, checker, and VM.

  This design adds eight groups of code:
  - Explicit-stack encode and decode walkers that store indices and rooted handles instead of heap borrows.
  - `EncodeSink` and `DecodeSource` interfaces, each with JSON and `Value` implementations.
  - The shape table and `PartTemplate`.
  - Native default trait methods and their registration.
  - Structural implementation inference through trait resolution.
  - `TraitDict<N>`, `NativeContext::to_value`, and mid-call root release.
  - The `Encoder` and `Decoder` prelude types.
  - Rust `native::Encode` and `native::Decode` traits, Rust sink and source traits, derive-macro codec attributes, and binding codec entries.

## Rejected alternatives

- **Runtime lookup of custom implementations from value tags.** A table keyed by trait and type definition ID would duplicate dictionary dispatch. Because heap objects omit type arguments, value-tag lookup fails on generic implementations such as `impl<T: Encode> Encode for Box<T>`. The decode walker would also still require runtime type descriptors.
- **Generated Sand methods for all structural implementations.** Swift SE-0166 synthesizes `Encodable` and `Decodable` methods "in cases where all properties are also `Encodable` or `Decodable`". In `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`, Sand `ToJson` and `FromJson` methods took 2.3 to 3.7 times as long as the native walkers on `main`. An all-Sand prototype took up to 7 times as long.
- **Generated Sand codecs beside native walkers.** Two parallel codec paths must produce identical output, error paths, and retry behavior. That same report found that "at least a quarter" of the 8,210 new production lines on `origin/encode-decode-traits` existed only to keep the native and Sand paths aligned.
- **Placeholder walks without callbacks.** Without callbacks, the encode walker would build an intermediate `Value` tree with placeholders. It would then fill those placeholders in a second pass. The decode walker would read its input twice. Both walkers would allocate an intermediate tree on every `json.encode` and `json.decode` call. They would also need placeholder values, child-index paths, and a second decode pass. Finally, the walker could not distinguish Base64 `bytes` from plain strings in an untyped first pass without built-in JSON rules.
- **Part dictionaries stored inside parent dictionaries.** For recursive types, the compiler would have to emit cyclic constant tuples. On polymorphically recursive types, the compiler would expand nested dictionaries without bound.
- **Generated Sand helper functions for part dictionaries.** Each uncached lookup would require a callback. A callback round trip took about 90 ns in `experiments/native-async-sand-calls.md`. The walker would pay that callback cost even on derived types.
- **Shapes as constant tuples.** The compiler cannot store recursive shapes as constant tuples. In `crates/sandlang/src/vm/mod.rs:740-752`, the VM builds constant tuples in index order. Each constant tuple can reference only earlier constants. A recursive type forms a cycle through its shape. The compiler breaks that cycle by storing shapes in a side table indexed by integer IDs.
- **`Decode` with `Encode` as a supertrait.** Users implement `Decode` directly under this design. Both `serde` and Swift also keep the two traits separate. See [Independent traits](#independent-traits).

## Existing code that this design reuses

Each item below lists an existing component and how this design reuses it.

**Type classification and dispatch:**

- **`Ctor` and `TypeDefKind`.** We rename `Ctor` to `TypeConstructor`. Registration and shapes classify types with these two enums.
- **`propagate_to_fixed_point` in `crates/sandlang/src/check/derive.rs:166-190`.** Structural bound inference keeps this fixed-point driver and replaces its per-type index list with type-parameter and trait pairs.
- **Dictionary construction.** The compiler builds structural dictionaries with its existing dictionary builder. It deduplicates constant dictionaries in `dict_consts`.
- **Trait evidence.** The checker builds `PartTemplate` trees from the same trait evidence expressions used in generic function bodies.
- **`never` trait resolution in `crates/sandlang/src/check/traits.rs:1123` and `crates/sandlang/src/check/verify.rs:228`.** We extend this case to traits registered for `never`.

**Runtime data:**

- **`FieldDefault`.** Shapes reuse its constant variant and point its computed variant to the field's default helper function.
- **`ValueTypeInfo`.** Shapes read the native value parse function from `ValueTypeInfo`.
- **The VM type table.** Walkers read struct field names and enum variants from this table.
- **`RtIds`.** Walkers match `Some` and `None` variant IDs through `RtIds`.
- **Template fuel and depth checks.** Part template evaluation reuses the fuel and depth rules of `DescTemplate`.
- **`Program::desc_templates`.** `Program`'s shape table follows the side-table layout of `desc_templates`.

**Native calls and memory:**

- **`NativeContext::call`.** Walkers execute Sand callbacks through `NativeContext::call`.
- **`Later::ready` and `Later::new`.** Codec functions start synchronously with `Later::ready` and allocate a `Future` on demand with `Later::new`.
- **`NativeContext::step`.** Walkers handle post-callback memory limits through `NativeContext::step`.
- **`NativeContext` root table.** Walkers root heap values across callbacks in this table.
- **`Staging`.** The decode walker builds heap values in `Staging` closures.
- **Scheduler memory accounting.** The scheduler counts the top-level `Future` and root table against `max_memory`.
- **`NativeObject` and `NativeModule::bind`.** The prelude binds `Encoder` and `Decoder` with these APIs.
- **`Num<N>`.** We reimplement `Num<N>` as a typed wrapper over `TraitDict<N>`.

**Walkers, sinks, and sources:**

- **`Vm::to_value` heap copier in `crates/sandlang/src/value.rs:442-678`.** We model the encode walker on its explicit-stack walk. We replace borrowed `ContainerChildren` iterators with index-based stack entries.
- **`Value` builders and cached type names.** The `Value` sink reuses these builders from `Vm::to_value`.
- **JSON text encoder in `crates/sandlang/src/stdlib/json.rs:1430-1655`.** The JSON sink reuses its string escaping, number formatting, indentation, map key formatting, and memory checks. We move its value traversal into the encode walker.
- **Typed JSON decoder in `crates/sandlang/src/stdlib/json.rs:890-1386`.** The explicit-stack decode walker reuses its struct, enum, map key, field lookup, and constant default logic.
- **JSON parser.** The JSON source reuses the parser's position tracking, `skip`, and error offsets. It replaces input borrows with a rooted string handle and byte offset.
- **`path_str` in `crates/sandlang/src/stdlib/json.rs:923`.** Error reporting reuses `path_str` to format paths such as `$.items[2].price`.

**New components without an existing counterpart:**

- The `EncodeSink` trait. `main` omits an encode sink because changeset 14 rejected an earlier sink interface.
- The `Value` decode source. `main` does not decode `Value` into heap objects.
- The `Encoder` and `Decoder` prelude types.
- Rust `native::Encode` and `native::Decode` traits, Rust sink and source traits, and binding codec entries.
- Native default trait methods and their registration.
- Structural implementation inference through trait resolution.
- The shape table and `PartTemplate`.
- `NativeContext::to_value`.
- Mid-call root release.

## Test and measurement plan

- **Existing JSON behavior.** We preserve existing output in `crates/sandlang/tests/spec/std_json.test`, aside from in-place callback order and plain `Value` conversion for `Json`. We will document any planned error-message updates in this file.
- **New spec tests:**
  - Custom `Encode` and `Decode` implementations on structs and enums.
  - Native types with `#[sand(encode, decode)]`, with `"manual"` codecs, and without codec attributes. Unannotated native types must implement neither trait.
  - A manual `Encode` native type inside a list to verify direct Rust execution without callbacks.
  - Equivalence between a native type's derived Rust `native::Encode` output and VM walker output.
  - Generic custom implementations such as `impl<T: Encode> Encode for Box<T>`.
  - Custom implementations with non-codec bounds such as `impl<T: Describe> Encode for Box<T>` inside a structural struct.
  - Concrete custom implementations such as `impl Encode for W<int>`, plus the compile error on `W<str>: Encode`.
  - Tuples across multiple arities.
  - Direct recursive types and polymorphic recursion.
  - `[never]`: `json.encode` produces `"[]"` for `[]`. `json.decode` rejects `"[1]"`.
  - Composite values where both outer and inner types use custom implementations.
  - Async custom implementations that call `task.sleep`.
  - Invalid `Encoder` uses: omitted writes, duplicate writes, and post-callback writes.
  - Error paths reported from failed custom `Decode` implementations.
  - Struct decoding through `init` with missing `Option` parameters, parameter defaults, and `init` errors.
  - `Value` source rejection of JSON-encoded input such as Base64 `Value::Str` for `bytes`.
  - Duplicate struct fields: `json.decode` returns a recoverable `"parse"` error without stopping the run.
  - Non-finite floats: `json.encode` returns an `"invalid"` error. `json.to_json` stops the run.
  - Open-list mutation inside a custom `encode` callback during traversal.
- **Robustness checks:**
  - Run codec tests with `Config::retry_stress` enabled to exercise resource-limit retries.
  - Compare resident memory on large inputs against `max_memory` and equivalent Sand loops.
  - Encode and decode 512-level nested values on a 512 KiB host thread stack.
  - Trigger garbage collection inside every callback across values with many custom parts.
- **Benchmarks:**
  - Prototype and benchmark per-part dispatch on derived values before building the rest of the implementation. Use that measurement to decide whether dictionaries need the derived-subtree flag from [Speed of the walker](#speed-of-the-walker).
  - Benchmark `json` functions against `main` with `bench/run.py` and `crates/sandlang/benches/codec.rs`.
  - Benchmark the explicit-stack decode walker against the recursive decoder on `main`.
  - Measure call overhead on generic functions with `Encode` bounds.

## Design decisions

The repository owner settled these decisions during review:

- **Custom `encode` methods write through `Encoder`.** The method signature is `fn encode(self, encoder: Encoder) -> Result<()>`. The engine checks at runtime that each custom method writes a single value.
- **`encode` returns `Result<()>`.** Custom methods can report errors to callers. `json.encode` returns `Result<str>`. Both `serde::Serialize::serialize` in `serde_core` 1.0.229, `src/ser/mod.rs:149-150` and Swift's throwing `encode(to:)` in `Codable.swift:24-25` allow encoding to fail.
- **`json.encode` rejects non-finite floats and map keys without a text form.** It returns an `"invalid"` error to keep its output valid RFC 8259 JSON. RFC 8259 forbids `NaN` and infinity. Go's `encoding/json` likewise returns an error in `encoding/json/encode.go:80`. `serde_json` writes `null` instead. Its maintainer calls returning an error "better behavior" in `https://github.com/serde-rs/json/issues/985`. If nesting exceeds 512 levels, the walker still stops the run.
- **`json.to_json` uses a dedicated `Json` sink.** That sink builds the `Json` tree directly. Both the JSON text sink and the `Json` sink share a single set of JSON rules.
- **`json.to_json` returns `Json`.** If conversion fails, `to_json` stops the run. The `json!` macro keeps its current form without `Result` unwrapping.
- **`reflect` is deleted.** `Decode` dictionaries supply target type structure. Host functions use `TraitDict<N>` with a `Decode` bound instead of `Reflect<N>`.
- **Derivation remains automatic.** The checker derives `Encode` and `Decode` automatically just like `Eq`, `Ord`, and `Hash`. Type authors override derived codecs with custom `impl` blocks.
- **Field options are deferred.** This branch omits field attributes such as rename or skip. Authors who need custom field handling write custom `impl` blocks. A later change can add field attributes.
- **Custom `decode` methods read at most once without retries.** Input rewinding cannot undo side effects from callbacks that already ran.
- **The `Value` source accepts only exact `Value` variants.** JSON conversion rules stay in the JSON source. Hosts with JSON text call `json.decode` directly. Libraries disagree on JSON byte encodings. For example, `serde_json` 1.0.154 in `src/value/de.rs:414-420` reads bytes from a JSON string as UTF-8 text rather than Base64.
- **The decode walker returns a `"parse"` error on duplicate struct fields or `init` parameters.** Callers can handle the error without stopping the run. In `serde_derive` 1.0.229, `src/de/struct_.rs:269` likewise reports `duplicate_field`. For duplicate map keys, the decode walker overwrites the earlier value.
- **The decode walker ignores `init` visibility.** Field-based decoding on `main` likewise ignores field visibility.
- **Native types opt in to each trait.** `#[sand(encode, decode)]` selects derived codecs. `#[sand(encode = "manual")]` and `#[sand(decode = "manual")]` declare manual Rust codecs that run without Sand callbacks.
- **Generic native types cannot implement `Encode` or `Decode`.**
- **`native::Encode` replaces `value::Encode`.** A single Rust trait supports both VM walks and host `Value` conversion.

## Open questions

### 1. How should the Rust codec traits support both the VM walker and host code?

`native::Encode` and `native::Decode` have two callers:

- **The VM walker** operates on a heap value.
- **Host code** converts a Rust value into a `Value` in place of `value::Encode`.

With a `&self` receiver in `fn encode(&self, sink)`, the engine pays two costs:

- **Heap copies on manual codecs.** Manual `Encode` wrappers for `SandStruct`, `SandEnum`, and `SandValue` convert heap values to Rust values with `FromVmValue`. Manual `Decode` wrappers convert Rust values back to the heap with `IntoVmValue`. Every `json.parse` and `json.encode` call on `Json` copies a full tree because `FromVmValue` in `crates/sandlang/src/stdlib/json.rs:369` clones the entire tree.
- **Duplicate implementations on derived native types.** The VM walker encodes heap values from field shapes. The derive macro also generates a Rust `native::Encode` implementation from the same fields for host `Value` conversion. Only tests keep the two implementations aligned.

Candidate options:

- **Option A: a single trait over Rust values.** Keep the design in this document. The engine pays both costs above.
- **Option B: both heap and Rust methods on each trait.** The walker calls an in-place method that takes a heap handle and avoids heap copies. Native functions already read heap values in place. For example, `json.parse` reads `text.str(heap)` from `VmVal<String>` in `crates/sandlang/src/stdlib/json.rs:100-105`. Host code calls a second method that takes a Rust value. Manual codec authors must implement both methods. The derive macro generates both methods for derived types.
- **Option C: heap-only Rust traits plus `serde` for host conversion.** `native::Encode` and `native::Decode` expose only heap-handle methods. Derived native types then rely solely on the VM walker. Host code converts Rust values to and from `Value` through `serde` instead of `native::Encode`. This option reverses the decision that `native::Encode` replaces `value::Encode`. It also expands changeset 16 in `SPLIT.md`. Today changeset 16 adds `Serialize` and `Deserialize` only on `Value` itself. To convert arbitrary `T: Serialize` values into `Value`, `sandlang` would need a `Value` serializer. The equivalent serializer in `serde_json` 1.0.154, `src/value/ser.rs`, is 1,063 lines.

**Recommendation:** We leave this decision to the repository owner.

### 2. How should the checker handle `init` parameters that differ from struct fields?

Structural `Encode` writes struct fields. Structural `Decode` reads `init` parameters. For `struct Port { n: int, fn init(port: int) -> Result<Port> }`, the encode walker writes `{"n": 80}`. The decode walker expects `"port"` instead:

- **Option A: allow both structural implementations.** `json.decode` rejects `json.encode`'s output at runtime until the author writes a custom implementation.
- **Option B: report a compile error.** The checker reports an error if it derives both traits on a struct whose `init` parameters differ from its fields in name or type.
- **Option C: omit structural `Encode`.** If `init` parameters differ from struct fields, the checker derives only `Decode`. Users must write a custom `Encode` implementation to encode that struct.

**Recommendation:** We leave this decision to the repository owner.

### 3. Should `Decoder` expose the next input kind to Sand `decode` methods?

A custom Sand `decode` method reads input through `decoder.decode()` without retries. Because it cannot retry a failed read, it cannot accept multiple input kinds such as either `str` or `int`. Manual Rust codecs can branch on input kinds through `DecodeSource`'s next-kind method:

- **Option A: keep `Decoder` without kind inspection.** Each Sand `decode` method reads a single expected type.
- **Option B: add `decoder.kind()`.** `Decoder` exposes `DecodeSource`'s next-kind query. Sand `decode` methods can then branch before reading, just like manual Rust codecs.

**Recommendation:** We leave this decision to the repository owner.

### 4. Should a concrete custom implementation replace structural implementations for other instantiations?

As [Custom implementations](#custom-implementations) explains, `impl Encode for W<int>` replaces the structural `Encode` implementation for every `W<T>`. `W<str>` then does not implement `Encode`. The checker rejects a second implementation for a type constructor if it can unify the trait arguments. The checker therefore rejects keeping both implementations. See `crates/sandlang/src/check/traits.rs:537-550`:

- **Option A: keep standard coherence.** `Encode` and `Decode` follow the same overlap rules as every other trait.
- **Option B: reject concrete type arguments on custom codec implementations.** Authors must write generic targets such as `impl<T: Encode> Encode for W<T>`.

**Recommendation:** We leave this decision to the repository owner.
