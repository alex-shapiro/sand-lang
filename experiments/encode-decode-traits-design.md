# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** This document describes planned changes. Nothing in it is implemented. Code citations refer to `main` at commit `8a51c46`, with paths relative to `crates/sandlang/` unless a path says otherwise. [Design decisions](#design-decisions) records the choices that the owner made during review. [Open questions](#open-questions) lists the choices that remain.

## Summary

We propose to make `Encode` and `Decode` ordinary traits that users can implement. Each trait declares a native Rust walker as the default body of its method. The type checker generates implementations for built-in types, structs, and enums, and these implementations use the native default. A user can write a custom implementation for a type that the user's module declares.

The compiler passes dictionaries for `Encode` and `Decode` to generic functions, as it does for every trait with methods. The walker reads these dictionaries to find the implementation of each part of a value. It processes a part that uses the native default in Rust. For a part with a custom implementation, it calls the Sand method through a callback, waits for the result, and continues from the same position.

The design has four goals:

- **Custom implementations.** A user can implement `Encode` and `Decode` for any type that they define. The type can be concrete or generic. It can be defined in Sand, or defined in Rust as a native type. A Sand type implements the traits in Sand, and a native type implements them in Rust. The walkers apply these implementations at every level of a value.
- **JSON.** The encode walker writes JSON text directly. The decode walker reads JSON text directly. Neither builds an intermediate tree.
- **Host `Value` conversion.** The encode walker can also write a `Value`, and the decode walker can read one.
- **`json.Json` as an ordinary type.** `Json` implements `Encode` and `Decode` with ordinary implementations. The walkers treat `Json` as they treat any other type.

## Terms

- **Dictionary.** A dictionary is a tuple that supplies a trait implementation to generic code at run time. The checker selects it from static types, and the caller passes it as a hidden argument. A dictionary has three sections, in this order (`src/check/traits.rs:3-12`):
  1. The method functions of the implementation.
  2. The dictionaries of the supertraits that have dictionaries.
  3. The dictionaries that the bounds of the implementation require, such as the `Close` dictionary of `T` in `impl<T: Close> Close for [T]`.

  A trait has a dictionary only if the trait or a supertrait declares a method (`src/check/traits.rs:382`, `:408-411`). The checker verifies other traits, such as `Hash`, only at compile time.

- **Trait evidence.** Trait evidence is the checker's expression for a dictionary: a hidden parameter, a slot read from another dictionary, or a dictionary constructor.
- **Constant dictionary.** The compiler stores the dictionary of a concrete type in the constant pool. It stores each distinct dictionary once (`dict_consts`, `src/compile/mod.rs:2747-2762`).
- **Dynamic dictionary.** Generic code builds a dynamic dictionary at run time from the dictionaries of its type parameters. For example, `fn close_all<T: Close>(items: [T])` builds the `[T]: Close` dictionary at run time.
- **Derived trait.** On `main`, the checker implements five prelude traits automatically: `Eq`, `Ord`, `Hash`, `Encode`, and `Decode`. A struct or enum derives a trait if every field and payload type implements it, with two exceptions (`src/check/derive.rs:200-203`). A struct with a `var` field does not derive `Hash`, and a struct that declares `init` does not derive `Decode`. The checker rejects user implementations of derived traits (`src/check/traits.rs:492-497`).
- **Native function.** A native function is a Rust function that Sand code calls.
- **Native default method.** A native default method is a trait method whose default body is a native function. This design adds them.
- **Structural implementation.** A structural implementation is an implementation that uses the native default method. The checker generates most of them from the structure of a type.
- **Custom implementation.** A custom implementation is an `impl` block with a method body that a user writes.
- **Part.** A part is a direct child of a value: a struct field, a variant payload value, an element of a list, set, or tuple, or a key or value of a map.
- **Type constructor.** A type constructor is a type without its type arguments, such as `List` for `[int]`. The checker enum `TypeConstructor` lists them. On `main`, this enum has the name `Ctor`.
- **Walker.** A walker is a Rust routine that visits a value and its parts. The encode walker reads a Sand value. The decode walker builds a Sand value.
- **Encode sink.** An encode sink is a Rust object that receives the output of the encode walker. Each output format has its own encode sink.
- **Decode source.** A decode source is a Rust object that supplies the input of the decode walker. Each input format has its own decode source.
- **Implementation shape.** An implementation shape describes one level of a type to a walker. Each dictionary of a structural implementation refers to a shape.
- **Part template.** A part template tells the walker how to get the dictionary of a part.
- **`Value`.** `sandlang::value::Value` is a Rust type that represents a Sand value outside the VM. A `Value` stays valid after a run ends.
- **Callback.** A callback is a call from an async native function to a Sand function through `NativeContext::call` (`src/native/context.rs:169`). Commit `8a51c46` added callbacks.

## Why `Encode` and `Decode` need dictionaries

### The two traits on `main`

On `main`, the prelude declares `Encode` and `Decode` without methods (`src/stdlib/prelude.sand:313`, `:318`). Neither trait has a dictionary. The checker verifies `T: Encode` at compile time, and then the compiler erases the bound.

`json.encode` reads the run-time tags of the VM. A value slot tags each scalar and each inline variant. A heap object tags its kind. A struct object stores the ID of its type definition, and a variant stores its variant ID. The VM type table maps each ID to field and variant names.

`json.decode` must know the target type before it reads the input. Its type parameter is a `reflect` parameter (`src/stdlib/json.sand:144`). The caller therefore passes a run-time type descriptor, `TypeDesc`, as a hidden argument.

### Custom implementations need dictionaries

A custom implementation can target a generic type, such as `impl<T: Encode> Encode for Box<T>`. To encode a `Box<Email>`, the walker must find the implementation for `Email`. A heap object does not record its type arguments, so the walker cannot find `Email` from the value. The caller must supply that information. A trait dictionary is the mechanism that Sand already uses for this purpose.

### Why heap objects do not record type arguments

Two costs prevent the VM from recording type arguments on values:

- **Each generic call would pass type descriptors.** The compiler emits one copy of a generic function for all type arguments. To record `T` in a new list, `fn empty_list<T>() -> [T] { [] }` would need a descriptor of `T`. Each caller would then pass one. On `main`, a caller passes hidden arguments only for `reflect` parameters and for bounds on traits with methods (`src/check/traits.rs:3-6`).
- **Inline values would need heap objects.** A `VmValue` is 16 bytes (`src/vm/value.rs:60`). It stores scalars, payload-free variants such as `None`, single-payload variants such as `Some(3)`, and pairs of integers inline (`src/vm/value.rs:26-56`). An inline `None` of type `Email?` has the same bits as an inline `None` of type `int?`. To record type arguments, the VM would need a heap object for every `None`.

With dictionaries, only a call to a function that requires a trait with methods pays a cost.

## Part 1: Trait declarations

The prelude declares the two traits with native default methods:

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

A custom `encode` method writes its value through the `Encoder` argument, and returns `Result<()>`. See [Design decisions](#design-decisions).

A native default method works as other default methods do. An `impl` block that omits the method uses the default. A caller passes the dictionary of `Self` to each trait method as its first hidden argument (`src/check/traits.rs:9-11`). The native default reads the shape and the part dictionaries from that dictionary.

### Independent traits

`Decode` does not require `Encode`. On `main`, the supertrait `Decode: Encode` limited `json.decode` to `Encode` types that do not declare `init` (commit `49d4022`). After this change, users implement `Decode` directly, so that reason no longer applies. serde and Swift also keep the two traits independent:

- serde declares `pub trait Deserialize<'de>: Sized`, without a `Serialize` supertrait (`serde_core` 1.0.229, `src/de/mod.rs:554`).
- Swift declares `Encodable` and `Decodable` as two protocols. `Codable` is "a type alias for the `Encodable` and `Decodable` protocols" (`stdlib/public/core/Codable.swift:43-47` in the Swift repository).

### `Num` keeps `Encode`

`Num` keeps its supertraits: `pub trait Num: Ord + Encode` (`src/stdlib/prelude.sand:321`). The spec test "data implements Encode, and Num implies it" (`tests/spec/std_json.test:554`) passes a `T: Num` value to a function that requires `T: Encode`.

The cost is one more slot in each `Num` dictionary, for the `Encode` dictionary. Only the prelude can implement `Num` (`src/check/traits.rs:491-497`), and it implements `Num` only for `int` and `float` (`src/stdlib/prelude.sand:328-337`). The cost is therefore one slot in each of two constant dictionaries. `native::Num` reads `zero` and `one` by position (`src/check/traits.rs:466-474`). Methods stay first in a dictionary, so those positions do not change.

## Part 2: Structural implementations

### `TypeConstructor`

We rename the checker enum `Ctor` (`src/check/mod.rs:82-97`) to `TypeConstructor` and move it to `src/types.rs`, beside `TypeDefKind`. Identifiers that use the old name change with it, such as `ctor_of` (`src/check/mod.rs:3350`). `PatternKind::Ctor` in the parser is a different concept and keeps its name.

The enum gets this doc comment:

```rust
/// The constructor of a type without its type arguments, such as `List` for
/// `[int]`. The checker indexes trait implementations by type constructor.
pub enum TypeConstructor {
    Int, Float, Bool, Str, Bytes, Duration, Unit, Range,
    List, Map, Set, Tuple, Fn,
    /// A struct, enum, or opaque type, by its type definition ID.
    Def(TypeDefId),
}
```

The checker keys implementation lookup by trait, type constructor, and tuple arity (`impl_key`, `src/check/traits.rs:119-127`). `TypeDefKind` (`src/types.rs:700-709`) classifies a type definition as a struct, an enum, or an opaque type. An opaque type with `Opaque::Value` is a native value type, such as `time.PlainDate`.

### Registration

Rust setup code registers each native default method with the types that it supports. A registration lists three things:

- The built-in type constructors, such as `List` and `Map`.
- The categories of type definitions: structs, enums, and native value types.
- Whether `never` implements the trait.

Registration happens before the checker reads any source. It therefore lists categories, not type definition IDs.

Each walker owns its list of supported types. The walker module has one function that maps each type constructor and each category to a walker handler, or to "unsupported". The `match` in that function covers every variant, so the Rust compiler forces a decision for each new constructor or category. Registration calls the same function. The list therefore exists in one place.

Both walkers support the set that derives `Encode` on `main` (`src/check/derive.rs:34-37`, `:196-199`). This set has every built-in type constructor except `Fn`, plus structs, enums, and native value types. Tasks, channels, and native object types are not in the set. Both registrations declare support for `never`. The decode walker also supports structs that declare `init`.

A type that a native module binds from Rust implements `Encode` or `Decode` only if it opts in. See [Native types](#native-types).

### How the checker builds structural implementations

**Built-in type constructors.** For each registered type constructor, the checker creates an implementation that requires every type argument to implement the trait. For `List`, it creates `impl<T: Encode> Encode for [T] {}`. A tuple implementation needs one type parameter for each element, and the checker keys tuple implementations by arity. The checker therefore creates the tuple implementation for an arity at the first lookup of that arity.

**Structs and enums.** The checker computes the requirements of each structural implementation through ordinary trait resolution:

1. The checker declares the headers of all custom implementations.
2. For each struct and enum without a custom implementation, the checker resolves the trait for each part type. The type parameters of the struct or enum are the unknowns. The checker records each requirement on a type parameter, for any trait.
3. During resolution, the checker assumes that the structural implementations under computation hold. It repeats step 2 until the requirements stop changing.

For `struct Pair<A, B> { first: A, second: [B] }`, the result is `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.

The derivation pass on `main` cannot do this, for two reasons. It records only the indices of type parameters that must implement the same trait (`Derived::Needs`, `src/check/derive.rs:14-19`). Also, for type definitions it reads only its own table (`src/check/derive.rs:28-33`), and it first runs before the checker declares the `impl` blocks of a module (`src/check/mod.rs:1328-1330`, `:1365`, `:1366-1374`). Consider a field of type `Box<T>` with a custom `impl<T: Describe> Encode for Box<T>`. The outer struct then requires `T: Describe`, which the pass on `main` cannot record.

The fixed-point driver on `main`, `propagate_to_fixed_point` (`src/check/derive.rs:166-190`), already iterates in this way. Its documented rule is a greatest fixed point, "so that mutually recursive types implement a trait unless a component type refutes it" (`src/check/derive.rs:123-125`). The new pass keeps the driver and replaces its per-type record with a list of requirements, each a type parameter and a trait. The checker runs the pass after it declares the `impl` headers and before it checks the supertraits of each implementation (`check_impl_supertraits`, `src/check/mod.rs:1375-1377`).

**Structs that declare `init`.** A struct that declares `init` derives `Decode` through its `init` parameters, not its fields. Its `Decode` requirements come from the parameter types of `init`. See [Decoding a struct that declares `init`](#decoding-a-struct-that-declares-init). Decoding ignores the visibility of `init`. [Open question 3](#open-questions) asks about the trait bounds of `init`.

**`never`.** If a registration declares support for `never`, the checker gives `never` a structural implementation. For other traits with methods, the rule on `main` stays: `never` does not implement them (`src/check/traits.rs:1123`, `src/check/verify.rs:228`). serde and Swift also implement the codec traits for `never` one trait at a time:

- serde implements `Serialize` and `Deserialize` for `!` separately. `deserialize` returns `Err(Error::custom("cannot deserialize `!`"))` (`serde_core` 1.0.229, `src/de/impls.rs:42-49`), and `serialize` returns `*self` (`src/ser/impls.rs:351-357`).
- Swift SE-0396 conforms `Never` to `Encodable` and `Decodable`. For decoding, "the implementation throws a `DecodingError.typeMismatch` error if decoding is attempted."

### Custom implementations

The checker no longer rejects user implementations of `Encode` and `Decode`. It checks them as it checks implementations of any other trait.

A custom implementation replaces the structural implementation for its type constructor. The checker rejects a second implementation of a trait for the same type constructor and tuple arity if the trait arguments unify (`src/check/traits.rs:537-550`). `Encode` and `Decode` have no trait arguments. So `impl Encode for W<int>` replaces the structural implementation for every `W<…>`, and `W<str>` does not implement `Encode`.

An `impl` block that omits the method uses the native default. The checker treats the block as a structural implementation with the bounds that the block declares. It verifies that those bounds supply every part. For example, `impl<T: Encode> Encode for Box<T> {}` is valid if every part of `Box<T>` implements `Encode` under `T: Encode`.

A module can implement a trait only if the module declares the trait or the type. Built-in types belong to the prelude for this check (`src/check/traits.rs:527-535`). The prelude registrations therefore decide which built-in types implement `Encode` and `Decode`.

### Scope: `Eq`, `Ord`, and `Hash`

This branch changes only `Encode` and `Decode`. `Eq`, `Ord`, and `Hash` stay on their current derivation path in `src/check/derive.rs`. Until a later change migrates them, the checker has two derivation mechanisms.

## Part 3: Implementation shapes and part templates

### Why structural dictionaries need more data

A walker needs the dictionary of each part, including parts with concrete types. The standard dictionary layout stores bound dictionaries only for type parameters. For `struct User { email: Email }`, the standard `User` dictionary stores only the method of `User`. The walker also needs the `Encode` dictionary of `Email`. The implementation shape supplies it.

### Dictionary layout

A structural dictionary has this layout:

```text
[native default method, shape ID, bound dictionaries...]
```

Neither trait has supertraits, so the shape ID follows the method. A custom dictionary keeps the standard layout, `[method, bound dictionaries...]`. The walker reads slot 0 to tell the two kinds apart. If slot 0 holds the native default method, slot 1 holds a shape ID. Ordinary trait calls read only slot 0.

### The shape table

The compiler stores shapes in a table in `Program`. Slot 1 of a structural dictionary holds the index of its shape as an integer. `Program` already keeps the templates of type descriptors in such a table (`desc_templates`, `src/vm/bytecode.rs:1335`).

A side table keeps the constant pool acyclic. The VM builds constant tuples in index order, and each tuple can refer only to earlier constants (`src/vm/mod.rs:740-752`). A recursive type would otherwise need a cycle. For example, the dictionary of `Tree` refers to its shape, the shape refers to the `[Tree]` dictionary, and that dictionary refers to the `Tree` dictionary again. With an integer shape ID, the chain stops at the shape. The compiler allocates the ID of a shape before it lowers the part templates of that shape. A part template can therefore refer to a dictionary that holds the same shape ID.

A shape has these fields:

- **Constructor.** The `TypeConstructor` of the type, or a marker for `never`. For `TypeConstructor::Def`, the type definition ID is also the ID in the VM type table, because the compiler adds the VM type records in checker order (`src/compile/mod.rs:418-419`). The walker reads field names and variants from that table, as `json.encode` does on `main`.
- **Part templates.** The shape has a part template for each part.
- **Decode data.** A `Decode` shape has three more kinds of data, for facts that the VM type table does not store:
  - For a struct, the field defaults. A default is a constant or the default helper function of the field (`src/check/mod.rs:1610-1632`). On `main`, `FieldDefault::Computed` refers to a fill function that runs after the decode (`src/vm/value.rs:1135-1145`). The shape refers to the field's default helper instead.
  - For a struct that declares `init`, the parameters of `init`, their defaults, and the `init` function.
  - For a native value type, the parse function from its `ValueTypeInfo` (`src/types.rs:721-725`).

Only native default methods read shapes. In this branch, only `Encode` and `Decode` declare native defaults. A shape is therefore codec metadata, not a general part of the trait system.

### Part templates

Part templates replace the type descriptor templates of `main` (`DescTemplate`). A `DescTemplate` builds a whole `TypeDesc` at run time from the descriptors of the type arguments. A part template builds only the dictionary of one part.

```rust
enum PartTemplate {
    /// Loads a constant: a dictionary, a method function, or a shape ID.
    Constant(u32),
    /// Reads a bound dictionary from the enclosing dictionary.
    Bound(u32),
    /// Builds a dictionary tuple from element templates.
    Tuple(Vec<PartTemplate>),
}
```

The compiler emits a template for each form of part:

- **Concrete part.** For the field `email: Email` in `impl Encode for User`, the compiler emits `Constant(k)`, where `k` is the index of the constant `Encode` dictionary of `Email`.
- **Type parameter.** In `impl<T: Encode> Encode for [T]`, the compiler emits `Bound(0)` for each element. The walker reads the first bound dictionary of the enclosing dictionary.
- **Generic part.** For the field `items: [T]` in `impl<T: Encode> Encode for Page<T>`, the compiler emits `Tuple([Constant(native_encode), Constant(list_shape), Bound(0)])`. The walker evaluates the three elements and builds the `[T]` dictionary from them.

On `main`, `op_describe` charges fuel for each template instantiation (`src/vm/interp.rs:1952-1954`), and `DescTemplate::instantiate` stops at a depth limit (`src/vm/value.rs:1384-1389`). The evaluation of part templates applies both rules.

### How the compiler builds part templates

The checker already builds trait evidence for the bodies of generic functions. This evidence is an expression tree of dictionary constructors, slot reads, and constant loads, and the compiler lowers it to bytecode. For each part of a structural implementation, the checker resolves `PartType: Trait` into the same kind of tree. The compiler then converts that tree into a `PartTemplate` instead of bytecode.

### Worked example

This example shows how the walker reaches a custom method across an erased type argument. At run time, a `Page<Email>` object records only `Page`.

```sand
struct Email { address: str }

impl Encode for Email {
    fn encode(self, encoder: Encoder) -> Result<()> { encoder.encode(self.address) }
}

struct Page<T> { items: [T] }

fn format_page<T: Encode>(page: Page<T>) -> Result<str> { json.encode(page) }

format_page(Page(items: [Email(address: "a@b.c")]))
```

The user writes a custom `Encode` implementation for `Email`. The checker generates a structural implementation for `Page<T>` that requires `T: Encode`.

1. **The caller passes the `Email` dictionary.** `format_page` requires `T: Encode`, so its caller passes the `Encode` dictionary of `Email`. The implementation is custom and has no bounds, so this dictionary is a 1-item tuple that holds `Email.encode`.
2. **`format_page` builds the `Page<T>` dictionary.** The call `json.encode(page)` needs `Page<T>: Encode`. `T` is generic, so `format_page` builds a 3-item tuple at run time:
   - Slot 0 holds the native default `Encode.encode`.
   - Slot 1 holds the shape ID of `Page`.
   - Slot 2 holds the first bound dictionary, for `T`. In this call, it is the `Email` dictionary.
3. **The walker visits `page`.** `json.encode` starts the walker with `page` and the `Page<T>` dictionary. Slot 0 holds the native default, so the walker reads the shape ID from slot 1.
4. **The walker builds the `[T]` dictionary.** The `Page` shape has the template `Tuple([Constant(native_encode), Constant(list_shape), Bound(0)])` for `items`. `Bound(0)` reads slot 2 of the `Page<T>` dictionary. The result is a 3-item tuple: the native default, the shape ID of lists, and the `Email` dictionary.
5. **The walker visits the list.** Slot 0 of the `[T]` dictionary holds the native default, so the walker processes the list in Rust. The list shape has the template `Bound(0)` for each element. It reads slot 2 of the `[T]` dictionary, which is the `Email` dictionary.
6. **The walker calls `Email.encode`.** Slot 0 of the `Email` dictionary holds `Email.encode`, which is not the native default. The walker calls it through a callback.

The `Decode` dictionaries have the same layout. With independent traits, a `Decode` dictionary has no `Encode` slot.

## Part 4: Recursive types

### Dictionaries on demand

The walker builds the dictionary of a part only if it visits a value of that part. This rule stops the walker from building dictionaries forever for recursive types:

- **Direct recursion.** In `struct Tree { children: [Tree] }`, the `Tree` shape has `Constant(k)` for `children`, where `k` is the constant `[Tree]` dictionary. The `[Tree]` dictionary holds the `Tree` dictionary as its bound. See [The shape table](#the-shape-table) for why these constants stay acyclic.
- **Generic recursion.** For `Tree<T>`, the compiler emits a finite template for `children`: `Tuple([native, list_shape, Tuple([native, tree_shape, Bound(0)])])`. The walker builds the `[Tree<T>]` dictionary at `children`, and the `Tree<T>` dictionary from it.
- **Polymorphic recursion.** A recursive type can wrap its type parameter, as in `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. At each `Deep` value, the walker builds the `Nest<[T]>` dictionary from the dictionary of `T`. It builds one dictionary for each `Deep` layer in the value. Each layer is also a nesting level, so the 512-level limit (`src/vm/container_children.rs:15`) bounds the number of dictionaries.

Go's `encoding/json` also builds the handler for a type at run time, caches it, and breaks recursion with an indirect entry: "To deal with recursive types, populate the map with an indirect func before we build it" (`src/encoding/json/encode.go:386-400` in the Go repository).

### Per-walk dictionary cache

During a walk, the walker caches each dynamic part dictionary by shape, part index, and enclosing dictionary. Thus, for a list of 10,000 `Tree<T>` values in a generic function, the walker builds the `children` dictionary of `Tree<T>` once for each enclosing dictionary, not once for each value. The walker roots the cached dictionaries, as it roots its other heap values.

## Part 5: Native walkers

### Walkers that do not know the format

The walkers know the structure of Sand values. They know nothing about any format. The encode walker writes to an encode sink, and the decode walker reads from a decode source. The walkers are generic over the sink and the source, as serde code is generic over the format: `fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer` (`serde_core` 1.0.229, `src/ser/mod.rs:263-265`).

The walkers differ from serde in one way. serde's derive generates code for each type. The walkers read shapes at run time, as Go's `encoding/json` reads type metadata at run time.

This design reverses one decision of `SPLIT.md` changeset 14. That changeset rejected a shared heap walk with a sink, because the walk "required about 300 extra lines and `#[inline(always)]` across two trait layers." With callbacks inside a walk, three separate walkers would each need the callback, rooting, and resume logic. One walker with three sinks needs it once.

### Walker state across callbacks

A walker keeps an explicit stack of open containers and does not recurse on the Rust stack. During a callback, Sand code can run the garbage collector. The collector does not trace values that Rust variables hold. A future that waits for a callback also cannot hold borrows of the heap. The stack entries therefore hold indices and rooted handles, not borrows.

This rule requires new code in both walkers:

- **Encode walker.** The heap copier of `Vm::to_value` and the JSON text encoder already walk with explicit stacks. Their stack entries, `ContainerChildren`, hold borrowed slice iterators (`src/vm/container_children.rs:32-43`). The encode walker needs a new stack entry type.
- **Decode walker.** The typed JSON decoder on `main` recurses on the Rust stack (`src/stdlib/json.rs:1007-1133`). The decoder needs a rewrite to an explicit stack. `TODO.md:124-127` lists this conversion, with the condition "without regressing throughput". The JSON parser also borrows its input text (`Parser<'a>`, `src/stdlib/json.rs:567`), and the error path holds borrowed keys (`Seg::Key(Cow<'a, str>)`, `src/stdlib/json.rs:883-886`). The JSON source must keep a rooted handle to the input and a byte offset instead.

An async native call has a root table. `NativeContext::root` adds a heap value to it, and the collector traces it (`src/native/context.rs:143-145`). The scheduler counts the capacity of the table against `max_memory` (`src/vm/sched.rs:514`). Before each callback, the walker roots three groups of values:

- The containers that the walker is visiting.
- The values that the decode walker holds for containers that it has not finished.
- The dictionaries of the open containers.

The collector does not move objects (`src/vm/heap.rs:1-3`). A rooted handle therefore stays valid across a callback.

On `main`, the root table only grows during a call. A walk with many callbacks would add roots at each callback and keep them until the walk ends. This design therefore adds a way to release roots during a call. `TODO.md:46-53` lists this change: "Let an async native function release roots during a call."

The decode walker builds heap values in a `Staging` area, as the JSON decoder on `main` does. A staged value becomes valid only after the staging closure returns (`src/vm/heap.rs:447-450`). The decode walker therefore ends its staging closure before each callback, and starts a new one after the callback.

A callback can change a container that the walker visits. For example, a custom `encode` method can remove an element from the list that holds its value. The walker reads each child at the time that it reaches the child. Rooting keeps every container alive, so a changed container changes the output but cannot cause access to freed memory. `TODO.md:42-45` tracks detection of such changes.

### Synchronous start

A native function runs its walker synchronously first. If the walk needs no callback, the function returns `Later::ready`, which does not allocate a future (`src/native/func.rs:411-413`). If the walk needs a callback, the walker moves its state into a future with `Later::new` (`src/native/func.rs:403-408`). Between two callbacks, the walker runs inside one `NativeContext::with_vm` call, not one call for each part.

Before the first callback, a memory limit error makes the engine collect garbage and run the native function again from the start (`src/vm/mod.rs:927-929`). That restart is safe because no Sand code has run yet. After the first callback, a restart would repeat the side effects of the callbacks. The walker therefore allocates through `NativeContext::step`, which collects garbage and runs again only its own closure (`src/native/context.rs:179-199`).

### Speed of the walker

The speed of the proposed walker is unmeasured. The nearest measurements are in `experiments/native-async-sand-calls.md`, section "Measurements". They use the prototype walker from commit `1f7bb03` and encode 5,000 rows:

- The synchronous walker, as on `main`, took 98 to 142 ns per row.
- The async prototype took 98 to 119 ns per row. It reached this speed because a static check, `Schema::is_derived`, sent each subtree without custom parts to a synchronous walk inside one `with_vm` call.
- Without that check, with a `with_vm` call and a boxed future for each node, the prototype took 470 ns per row.

The proposed walker does not match either prototype. It does not box a future for each node, but it evaluates a part template and reads a dictionary at every part. The cost of that work is unknown. If the benchmark shows that this cost matters, the walker can skip it inside subtrees without custom parts. For that, it needs a precomputed flag that says that no custom implementation exists below a dictionary. The compiler can compute the flag for constant dictionaries. For a dynamic dictionary, the flag depends on its bound dictionaries, so the walker computes it when it builds the dictionary and keeps it in the per-walk cache. This design adds the flag only if the benchmark requires it.

### Encode walker

The encode walker reads a Sand value and writes it to an encode sink. For each part, it gets the part dictionary from the part template and reads slot 0:

- **Native default method.** The walker writes the part to the sink in Rust.
- **Custom method.** The walker creates an `Encoder` that refers to the same sink, and calls the custom method through a callback. The method writes a value with `encoder.encode(value)`. That call runs the encode walker on `value` with the same sink, so the value appears in the output at the position of the part.

`encoder.encode` returns `Result<()>`. If the nested walk fails, `encoder.encode` returns the error, and the custom method usually returns it with `?`. If a custom method returns `Err`, the encode stops, and `json.encode` returns that error.

The sink is in shared state that the outer walker and the `Encoder` both refer to. While a callback runs, the outer walker waits, so only the nested call writes to the sink.

An `Encoder` accepts exactly one value, and only during its callback. The encode returns an error of kind `"invalid"` in three cases:

- The custom method writes no value.
- The custom method writes a second value.
- Sand code keeps the `Encoder` and uses it after the callback returns.

Each nested `encoder.encode` call adds a nesting level. Thus, an `encode` method that writes `self` stops at the 512-level limit.

### Encode sink interface

The encode sink has a method for each kind of value:

- Scalars: unit, `bool`, `int`, `float`, `str`, `bytes`, and `duration`.
- Sequences: begin a list, set, or tuple with its length, then end it.
- Maps: begin a map with its length, then end it.
- Structs: begin a struct with its type name and field names, then end it.
- Enum values: write a variant with its type name, variant name, and payload form.
- Options: write `None`, or begin `Some`.
- Native value types: write a value with its type name and text.

The walker identifies `Some` and `None` by their variant IDs in `RtIds` (`src/vm/bytecode.rs:1300-1305`). The sink therefore gets option calls and does not compare type names.

This design has three encode sinks:

- **The JSON sink** writes JSON text and applies the JSON rules. It writes `Some(inner)` as `inner`, `bytes` as a Base64 string, and map keys and native value types as strings. It writes a unit variant as a string and another variant as an object with the variant name as its key, as on `main` (`src/stdlib/json.sand:1-14`). It returns an error of kind `"invalid"` for a non-finite float and for a map key without a text form. Nesting beyond 512 levels stays a limit that stops the run.
- **The `Value` sink** builds a `Value`. It writes `Option` as a `Value::Variant` of the prelude type `Option`, as `Vm::to_value` does on `main`.
- **The `Json` sink** builds a `Json` tree for `json.to_json`. It shares the JSON rules of the JSON sink.

### Decode walker

The decode walker builds a Sand value from the shape of each dictionary. It reads its input from a decode source. It waits for a callback in three cases:

- **Custom `Decode` implementation.** The walker creates a `Decoder` that refers to the same source at the current position, and calls the custom method through a callback. The method reads its input with `decoder.decode()`.
- **Computed field default.** A struct field can declare a computed default, such as `id: int = next_id()`. If the input lacks that field, the walker calls the default helper of the field after it reads the other fields of the struct.
- **Struct that declares `init`.** See [Decoding a struct that declares `init`](#decoding-a-struct-that-declares-init).

Each callback finishes before the walker uses its result. Sets and maps therefore insert finished values and hash them correctly.

As with the sink, the source is in shared state that the outer walker and the `Decoder` both refer to. A `Decoder` reads at most one value, and only during its callback. If the custom method reads no value, the source skips the part after the callback. If a read fails, the decode fails with the error of that read, whatever the custom method returns. A failed read can leave the source inside a partly read value, so the walker cannot continue. A custom method cannot retry a read as another type.

A struct field or `init` parameter that appears twice in the input is a decode error of kind `"parse"`. The decode returns that error, and the run continues. A map key that appears twice keeps its last value.

### Order of callbacks

The decode walker runs each callback at the point where it reaches the part. This order follows from walkers that wait for each callback in place. It changes a behavior of `main`. On `main`, `json.decode` runs computed field defaults "in post-order after parsing succeeds" (`src/stdlib/json.sand:137-138`). In this design, a default or a custom method can run before a later part of the input fails to decode. serde's derive also calls a `default = "path"` function at the point where it finds the field missing (`serde_derive` 1.0.229, `src/de.rs:766-779`).

The spec test "decode runs the defaults that are not constants, as a construction does" (`tests/spec/std_json.test:634-660`) checks the behavior of `main`. Its last line prints the number of calls after a failed decode. That number changes from 5 to 6. The default of `inner.a` runs before the walker reaches the invalid `items` field.

### Decode source interface

The decode source has a method for each request of the walker:

- Scalars: unit, `bool`, `int`, `float`, `str`, `bytes`, and `duration`.
- Sequences: begin a list, set, or tuple, read each element, and end it.
- Maps: begin a map, read each key as the requested key kind, read each value, and end it.
- Structs: begin a struct, read each field name and value, and end it.
- Enum values: read a variant name, then its payload.
- Options: read `None`, or begin `Some`.
- Native value types: read the text of a value.
- Next kind: report the kind of the next value, as the input describes it, such as `int`, list, or map. A reader of untyped data, such as the `Decode` implementation of `Json`, uses this request to choose its next read.
- Skip: skip the next value.
- Error paths: format the path of the current position, such as `$.items[2].price`.

This design has two decode sources:

- **The JSON source** reads JSON text as the walker makes requests, and applies the JSON rules. It decodes a Base64 string if the walker requests bytes. It parses a string key as an `int` if the walker requests an `int` key.
- **The `Value` source** reads a `Value`. It accepts only the exact form of each kind. For example, it accepts `Value::Bytes` for `bytes`, and rejects a Base64 `Value::Str`.

The JSON rules therefore stay in the JSON sink and the JSON source. The walkers know only the structure of Sand values.

### Native types

A native type is a Rust type that a native module binds with a derive macro and `NativeModule::bind::<T>()` (`src/native/mod.rs:904-905`). Four macros exist (`crates/sandlang-macros/src/lib.rs`):

- `SandStruct` binds a Rust struct to a Sand struct.
- `SandEnum` binds a Rust enum to a Sand enum.
- `SandValue` binds a Rust type to an opaque Sand value type. The type implements `ValueType`, and the runtime "encodes and decodes values in JSON as strings" (`src/vm/value.rs:879-880`).
- `SandObject` binds a Rust type to an opaque Sand reference type, such as `fs.File` or `net.Conn`.

`bind` reads the binding of the type from `T::binding()` and declares the type to the checker. The checker builds the type definition from that declaration (`src/check/mod.rs:1684-1720`).

#### Opt-in

A native type implements `Encode` or `Decode` only if its derive attribute declares the trait. The two traits are separate:

- `#[sand(encode, decode)]` derives both traits.
- `#[sand(encode = "manual")]` declares `Encode` and omits the derived implementation. The author implements the Rust trait `sandlang::native::Encode` by hand. `decode = "manual"` works the same way for `Decode`.

A `SandObject` type supports only the manual form, because the macro cannot see the structure of an opaque object.

On `main`, a bound struct, enum, or value type gets Sand `Encode` automatically from the checker (`src/check/derive.rs:196-199`). In this design, it gets the trait only from its attribute. The standard library adds the attribute to each data type that keeps its codec.

A manual declaration needs the attribute because `bind` is a generic function. Generic code in stable Rust cannot ask whether `T: Encode` holds. Specialization, the feature that would allow it, is unstable (https://github.com/rust-lang/rust/issues/31844). The codebase already uses this pattern for conversions: `#[sand(manual_conversion)]` omits `FromVmValue` and `IntoVmValue` so that the author writes them (`crates/sandlang-macros/src/lib.rs:9`), and `json::Json` uses it (`src/stdlib/json.rs:36`). For a manual declaration, the macro adds a compile-time assertion that the Rust implementation exists.

#### The Rust traits

The traits have this form. The names are illustrative:

```rust
pub trait Encode {
    fn encode(&self, sink: &mut dyn EncodeSink) -> Result<(), HostError<Fail>>;
}

pub trait Decode: Sized {
    fn decode(source: &mut dyn DecodeSource) -> Result<Self, HostError<Fail>>;
}
```

`EncodeSink` and `DecodeSource` give the [encode sink interface](#encode-sink-interface) and the [decode source interface](#decode-source-interface) as Rust traits. A serde `Serialize` implementation calls its `Serializer` in the same way (`serde_core` 1.0.229, `src/ser/mod.rs:247-259`). The traits take a trait object, so each native type needs one Rust function for each trait, not one for each sink. Only a manual implementation pays for the dynamic dispatch.

`native::Encode` replaces the Rust trait `value::Encode` (`src/value.rs:243-275`). A helper converts a Rust value to a `Value` by running its `Encode` implementation into the `Value` sink. serde_json's `to_value` is also a function over `Serialize` (`serde_json` 1.0.154, `src/value/mod.rs:995-998`).

For example, a host type encodes an amount of money as integer cents instead of its text form:

```rust
/// An amount of money in cents, such as `12.50`.
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

The binding records, for each declared trait, whether the implementation is derived or manual.

- **Derived.** The checker creates a structural implementation, as for a type that Sand source declares. A struct or an enum gets the structure of its fields, and a value type gets its text form. The walker walks the heap value, and no Rust implementation runs. The macro also generates a derived Rust implementation, for host code such as the conversion to `Value`. The macro generates both from the same field list, and a test compares their output. `main` has the same rule for `value::Encode`: a derived `encode` "returns the same `Value` that [`Vm::to_value`] copies from the VM" (`src/value.rs:250-251`).
- **Manual.** The macro generates a wrapper function for each manual trait and stores it in the binding. The method of the Sand implementation is that wrapper. If the walker reaches a part whose method is such a wrapper, the walker calls the wrapper directly in the same Rust walk, with the current sink or source. It makes no callback, and no Sand code runs.
  - The `Encode` wrapper of a `SandObject` type borrows `&T` from the heap (`VmVal::get`, `src/native/mod.rs:571`). The wrapper of another native type converts the heap value to `T` with `FromVmValue`. Each wrapper then calls `T::encode`.
  - The `Decode` wrapper calls `T::decode` and stores the result with `IntoVmValue`.

A manual implementation follows the rules of a Sand implementation. It writes or reads exactly one value, and its error becomes the decode or encode error. [Open question 4](#open-questions) asks whether a native type with type parameters can have a manual implementation.

### Decoding a struct that declares `init`

On `main`, a struct that declares `init` does not derive `Decode`. The error message is "`{owner}` declares `init`, which decoding does not call" (`src/check/derive.rs:251`). A field-by-field decode would skip the validation in `init`, and a native function could not call `init` before callbacks existed.

In this design, the decode walker constructs such a struct through `init`. It reads the parameters of `init` from the input, as it reads the fields of a struct. For example, `struct Port { n: int, fn init(n: int) -> Result<Port> }` decodes from the JSON object `{"n": 80}`. The walker then takes these steps:

1. It fills each missing argument. A missing argument of an `Option` type becomes `None`, as a missing `Option` field does on `main` (`src/stdlib/json.rs:1173`). A missing argument with a default uses that default. The checker lowers a parameter default to an inline expression or to a helper function of the earlier parameters (`src/check/mod.rs:2739-2775`). The walker calls a helper function through a callback.
2. It calls `init` with the arguments through a callback.
3. It converts the result of `init`. If `init` returns `Self` or `Ok`, the walker uses the value. If `init` returns `Err(e)`, the decode fails at the path of the struct. If `e` is an `Error`, the decode error keeps its kind. For any other error type, the decode error has kind `"invalid"` and the display text of `e`.

The structural `Encode` implementation of the struct still writes the fields. If the parameters of `init` match the fields, a value keeps its content through a JSON round trip. If they differ, the user writes a custom implementation.

Other libraries also decode through validating code:

- serde: `#[serde(try_from = "FromType")]` means "Deserialize this type by deserializing into FromType, then converting fallibly" (serde.rs, "Container attributes").
- Kotlin: "After deserialization, the `kotlinx.serialization` plugin runs the class's initializer blocks, just like when you create an instance" (kotlinx.serialization documentation, `serialization-customization-options.md`).

### `never`

The encode walker never visits a value of type `never`, because `never` has no values. The decode walker returns an error with kind `"parse"` for the shape of `never`, as it does for other type mismatches (`src/stdlib/json.sand:140-142`). Thus, `let empty: [never] = []` encodes as `[]`, and the decode of `[1]` into `[never]` fails.

## Part 6: Checker and compiler changes

1. **`TypeConstructor`.** Rename `Ctor` to `TypeConstructor`, add its doc comment, and move it to `src/types.rs`. See [`TypeConstructor`](#typeconstructor).
2. **Native default methods.** The checker accepts a `native fn` method in a `trait` declaration as a native default method. On `main`, a trait method without a body is a required method (`src/check/traits.rs:455-461`).
3. **Registration.** Rust setup code registers each native default with the types that its walker supports. See [Registration](#registration).
4. **Structural implementations.** The checker computes structural implementations after it declares the `impl` headers, through ordinary trait resolution. See [How the checker builds structural implementations](#how-the-checker-builds-structural-implementations).
5. **Custom implementations.** `Encode` and `Decode` stop being derived traits. The checker stops rejecting user implementations of them (`src/check/traits.rs:492-497`).
6. **Independent traits.** `Decode` loses its supertrait `Encode`. `Num` keeps `Ord + Encode`.
7. **`never`.** The checker and the debug verifier give `never` the dictionary of each trait whose registration declares `never` (`src/check/traits.rs:1123`, `src/check/verify.rs:228`).
8. **Shapes and part templates.** The checker builds trait evidence for each part. The compiler lowers it to part templates and stores shapes in the shape table of `Program`.
9. **Dictionary parameters for native functions.** A generic native function such as `json.encode<T: Encode>` receives the dictionary of `T` as a hidden argument. On `main`, the parameter type `Num<N>` reads the hidden `Num` dictionary of the `N`-th type parameter (`src/native/ty.rs:350-356`). This design generalizes it to `TraitDict<N>`, which reads the dictionary of any bound with methods. `Num<N>` becomes a typed view over `TraitDict<N>`.
10. **`reflect`.** `Decode` dictionaries supply the type structure that `json.decode` needs. This design deletes `reflect` from the language. A host native function that needs the structure of a type declares a `Decode` bound and takes `TraitDict<N>`, in place of `Reflect<N>`.

With methods, `Encode` and `Decode` become subject to two existing checker limits on traits with methods. [Open question 2](#open-questions) asks whether to keep them.

## Part 7: JSON and `Value`

- **`json.encode`.** `pub native fn encode<T: Encode>(value: T, indent: int = 0) -> Result<str>`. The encode walker writes the value to the JSON sink. On `main`, `json.encode` returns `str` (`src/stdlib/json.sand:128`). The new return type changes each caller. In the standard library, `http` calls `json.encode(value).bytes()` (`src/stdlib/http.sand:159`). That call becomes `json.encode(value)?.bytes()`. `json.encode` returns an error for a non-finite float and for a map key without a text form.
- **`json.decode`.** `pub native fn decode<T: Decode>(text: str) -> Result<T>`. The decode walker reads the text from the JSON source. The private `decode_raw` function, the `Decoded` enum, and the loop that runs computed defaults after the decode (`src/stdlib/json.sand:144-158`) go away.
- **`json.parse`.** `json.parse` decodes the text into a `Json` value with the same walker and source.
- **`Json`.** `Json` is a native type with manual implementations: `#[sand(module = "json", manual_conversion, encode = "manual", decode = "manual")]`. Its Rust `encode` writes the data model form: unit for `Null`, a list for `Arr`, a map with `str` keys for `Obj`, and each scalar as itself. Its Rust `decode` calls the "next kind" request, and then the matching typed read, for each value. On `main`, the JSON encoder and the typed decoder special-case `Json` by variant ID (`src/stdlib/json.rs:1100-1103`, `:1556-1558`). This design removes those special cases. serde_json takes the same approach: `serde_json::Value` implements `Serialize` and `Deserialize` (`serde_json` 1.0.154, `src/value/ser.rs:11`, `src/value/de.rs:22`).

  This form has a cost. The `Encode` wrapper converts the heap tree to a Rust `Json` tree with `FromVmValue` before it calls `encode`. The `Decode` wrapper builds a Rust `Json` tree and then converts it to the heap with `IntoVmValue`. On `main`, `json.encode` writes a `Json` value directly from the heap. The cost of the copy is unmeasured.

  This form also changes one behavior. On `main`, `Vm::to_value` copies a `Json` value as a `Value::Variant` of `json.Json`. With the `Value` sink, a `Json` value becomes its JSON data model form, such as `Value::Int(3)` for `Json.Int(3)`.

- **`json.to_json`.** `to_json` writes the value to a `Json` sink, which builds the `Json` tree directly. The JSON sink and the `Json` sink share a single layer of JSON rules. `to_json` keeps the return type `Json`. If a conversion fails, `to_json` stops the run, so the `json!` macro keeps its form. Under the shared rules, a non-finite float is a conversion failure. On `main`, `to_json` keeps it as `Json.Float(NaN)`.
- **Conversion to `Value`.** A conversion that applies custom `Encode` implementations needs an `Encode` dictionary. It can also reach a callback, so it must be able to wait. The engine therefore converts through one routine that starts synchronously and waits only if it reaches a custom implementation. Async native functions call it through a new method, `NativeContext::to_value(value, dictionary)`. [Open question 1](#open-questions) asks what happens to values whose types do not implement `Encode`, and to the synchronous `Vm::to_value`.

## Changes to the plan in `SPLIT.md`

`SPLIT.md` divides the work of a source branch into numbered changesets. This design changes the plan for changesets 14, 15, and 18:

- **Changeset 14: `Value` and `Vm::to_value`.** Pull request #41 merged this changeset as commit `a74cf0d`. This design replaces its three encode walks, in `json.encode`, `json.to_json`, and `Vm::to_value`, with one encode walker and three sinks. See [Walkers that do not know the format](#walkers-that-do-not-know-the-format) for why.
- **Changeset 15: Shared decode path and `Value` input.** This design becomes changeset 15. The plan for changeset 15 already describes a decoder that reads from a JSON source and a `Value` source. Two parts of the plan change:
  - The plan says that the `Value` source "also accepts the JSON form of each type". This design drops that rule.
  - The plan says that "`value::Decode: Encode` mirrors the Sand trait." The Rust trait `value::Decode` drops `Encode` to mirror the independent Sand traits. Commit `6d23963` on branch `origin/encode-decode-traits` records the same choice: "`value::Decode` also required `Encode`, which the user decided against."
- **Changeset 18: Host function calls.** The engine decodes each host `Value` argument with the decode walker and the `Value` source. For a parameter with a concrete type, the compiler emits a constant `Decode` dictionary.

Changeset 16 adds only `Serialize` and `Deserialize` for `Value`. A `Value` that `Deserialize` builds from JSON has JSON forms, which the `Value` source rejects. That part of changeset 16 therefore needs a new scope.

## Costs and risks

- **Dictionary passing.** A call to a generic function with a `T: Encode` bound passes a dictionary. For a concrete type, the dictionary is a constant. For a constructed generic type, the caller builds a tuple at run time. The report `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes` measured a generic call with a trait method call at 28.3 ns, against 16.6 ns for a direct Sand function call.
- **Per-part dispatch.** At each part, the walker evaluates a part template and reads slot 0 of the part dictionary. This cost is unmeasured. See [Speed of the walker](#speed-of-the-walker).
- **Walker state across callbacks.** At the first callback, the walker moves its state into a future and roots its heap values. The rooted values grow with the nesting depth of the value. The scheduler counts the root table against `max_memory`. `TODO.md:54-57` tracks the memory of recursive futures, which the engine does not count. The proposed walker uses one future for each walk. Each nested walk from an `Encoder` or a `Decoder` adds one more future.
- **Callback order.** A custom method or a computed default can run before a later part of the input fails to decode. See [Order of callbacks](#order-of-callbacks).
- **Two derivation mechanisms.** In this branch, `Eq`, `Ord`, and `Hash` keep the derivation table in `src/check/derive.rs`, and `Encode` and `Decode` use registration.
- **Code size.** The net line change is unmeasured. We will report the measured change in the pull request.

  The design deletes these parts:
  - The separate encode walks of `json.encode`, `json.to_json`, and `Vm::to_value`. The `Json` sink replaces the converter of `json.to_json`.
  - The typed JSON decoder, which the decode walker replaces.
  - The post-order fill path of `json.decode`: `decode_raw`, `Decoded`, and the fill functions of `FieldDefault::Computed`.
  - The special cases for `Json` in the encoder and the decoder.
  - `src/compile/reflect.rs`, `TypeDesc`, `DescTemplate`, `Op::Describe`, and `reflect` support in the parser, checker, and VM.

  The design adds these parts:
  - The encode and decode walkers, with stacks that hold no borrows.
  - The encode sink and decode source interfaces, each with a JSON implementation and a `Value` implementation.
  - The shape table and part templates.
  - Native default trait methods and their registration.
  - The computation of structural implementations through trait resolution.
  - `TraitDict<N>`, `NativeContext::to_value`, and the release of roots during a native call.
  - The `Encoder` and `Decoder` prelude types.
  - The Rust traits `native::Encode` and `native::Decode`, the Rust sink and source traits, the codec attributes of the derive macros, and the codec entries in bindings. The macros and bindings exist on `main`.

## Rejected alternatives

- **Runtime lookup of custom implementations from value types.** A table indexed by trait and type definition would duplicate dictionary dispatch. A heap object does not record its type arguments, so a lookup from a value works only for concrete types. `impl<T: Encode> Encode for Box<T>` would not work, and the decoder would still need run-time type descriptors.
- **Generated Sand methods for all structural implementations.** Swift uses this model: the compiler can synthesize the methods of a type "in cases where all properties are also `Encodable` or `Decodable`" (SE-0166). In `experiments/encode-decode-design-causes.md` on branch `encode-decode-design-causes`, `ToJson` and `FromJson` traits in Sand took 2.3 to 3.7 times as long as the native walkers on `main`. An all-Sand prototype took up to about 7 times as long.
- **Generated Sand codecs beside native walkers.** With two codec paths, both must produce the same output, error paths, and retry behavior. The same report found that "at least a quarter" of the 8,210 new production lines on `origin/encode-decode-traits` kept the native path aligned with the Sand path.
- **Walks without callbacks, with placeholders.** The encode walker would build a `Value` tree with placeholders for custom parts and fill them after the walk. The decode walker would read its input twice. This approach builds an intermediate tree for every `json.encode` and `json.decode` call. It also needs placeholders, child-index paths, and a second decode pass. Finally, it moves JSON rules into the walker: a JSON parser that runs without the target type cannot tell bytes from text, because both arrive as a JSON string.
- **Part dictionaries stored inside each dictionary.** For a recursive type, the compiler would have to build cyclic tuples. For polymorphic recursion, it would expand nested dictionaries without end.
- **A generated Sand function that returns part dictionaries.** Each uncached lookup would cost a callback. A callback round trip took about 90 ns in `experiments/native-async-sand-calls.md`. Types that use only native defaults would also pay that cost.
- **Shapes as constant tuples.** The VM builds constant tuples in index order, and each tuple can refer only to earlier constants (`src/vm/mod.rs:740-752`). A recursive type needs a cycle through its shape, which a constant tuple cannot express. The shape table avoids the cycle.
- **`Decode` with `Encode` as a supertrait.** The reason for the supertrait on `main` no longer applies, and serde and Swift keep the traits independent. See [Independent traits](#independent-traits).

## Existing code that this design reuses

Each item below names an existing component and what this design takes from it.

**Type classification and dispatch:**

- **`Ctor`, renamed `TypeConstructor`, and `TypeDefKind`.** Registration and shapes use these enums.
- **`propagate_to_fixed_point`** (`src/check/derive.rs:166-190`). The computation of structural implementations keeps this driver. It replaces the record for each type and the requirement computation.
- **Dictionary construction.** The compiler builds structural dictionaries with its existing dictionary code, and stores constant dictionaries once through `dict_consts`.
- **Trait evidence.** The checker builds part templates from the same evidence trees that it builds for function bodies.
- **The `never` case in trait resolution** (`src/check/traits.rs:1123`, `src/check/verify.rs:228`). This design extends it to registered traits.

**Run-time data:**

- **`FieldDefault`.** Shapes reuse its constant variant. Its computed variant changes from a fill function to the default helper of the field.
- **`ValueTypeInfo`** supplies the parse function of a native value type.
- **The VM type table** supplies the field names and variants of each type definition.
- **`RtIds`** identifies the `Some` and `None` variants.
- **The fuel and depth rules of type descriptor templates** apply to part templates.
- **The `desc_templates` table in `Program`** is the model for the shape table.

**Native calls and memory:**

- **`NativeContext::call`** runs each callback.
- **`Later::ready` and `Later::new`** give the synchronous start and the move into a future.
- **`NativeContext::step`** handles memory limits after the first callback.
- **The root table of `NativeContext`** keeps heap values alive across callbacks.
- **`Staging`** builds heap values in the decode walker.
- **The memory accounting of the scheduler** counts the future and the root table against `max_memory`.
- **`NativeObject` and `NativeModule::bind`** define the `Encoder` and `Decoder` types.
- **`Num<N>`** becomes a typed view over `TraitDict<N>`.

**Walkers, sinks, and sources:**

- **The heap copier of `Vm::to_value`** (`src/value.rs:442-678`). Its explicit-stack walk is the model for the encode walker. The walker needs a new stack entry type, because `ContainerChildren` holds borrows.
- **The copier's value builders** become the `Value` sink, with their cached type names.
- **The JSON text encoder** (`src/stdlib/json.rs:1430-1655`). Its string escaping, number formatting, indentation, map key text, and memory checks become the JSON sink. Its traversal moves into the encode walker.
- **The typed JSON decoder** (`src/stdlib/json.rs:890-1386`). Its struct, enum, map key, field index, and constant default logic moves into the decode walker. The decoder needs a rewrite from recursion to an explicit stack.
- **The JSON parser** becomes the JSON source. The source keeps its position tracking, `skip`, and error positions. It must stop borrowing its input across callbacks.
- **The path formatting of the JSON decoder** (`path_str`, `src/stdlib/json.rs:923`) formats error paths such as `$.items[2].price`.

**Parts without an existing equivalent:**

- The encode sink interface. Changeset 14 rejected an earlier sink interface, so `main` has none.
- The `Value` source. On `main`, no code converts a `Value` into a heap value.
- The `Encoder` and `Decoder` prelude types.
- The Rust traits `native::Encode` and `native::Decode`, the Rust sink and source traits, and the codec entries in bindings.
- Native default methods in traits, and their registration.
- The computation of structural implementations through trait resolution.
- The shape table and part templates.
- `NativeContext::to_value`.
- The release of roots during a native call.

## Test and measurement plan

- **Existing JSON behavior.** We keep the existing output in `tests/spec/std_json.test`, except for the changes that this document lists: the callback order and the `Value` form of `Json`. We will add each planned error-message change to this document.
- **New spec tests:**
  - Custom `Encode` and `Decode` implementations on structs and enums.
  - Native types with `#[sand(encode, decode)]`, with manual implementations, and without the attribute. A type without the attribute must not implement either trait.
  - A manual `Encode` implementation that the walker calls inside a list, to show that it runs without a callback.
  - A test that compares the derived Rust implementation of a native type with the walker output for the same value.
  - Generic custom implementations, such as `impl<T: Encode> Encode for Box<T>`.
  - A custom implementation with a bound on another trait, such as `impl<T: Describe> Encode for Box<T>`, inside a struct with a structural implementation.
  - A concrete custom implementation, such as `impl Encode for W<int>`, and the error for `W<str>: Encode`.
  - Tuples of several arities.
  - Direct recursive types and polymorphic recursion.
  - `json.encode(empty)` with `let empty: [never] = []`, and a failed decode into `[never]`.
  - Composite values whose outer and inner types both have custom implementations.
  - Async custom implementations that wait on `task.sleep`.
  - Faulty `Encode` implementations that skip `encoder.encode`, call it twice, or keep the `Encoder` after the callback.
  - Error paths from failed custom `Decode` implementations.
  - Decoding through `init`, including missing arguments with defaults and `init` errors.
  - The `Value` source with JSON-form input, such as `Value::Str` for `bytes`. The decode must fail.
  - A struct field that appears twice in the input. The decode must return an error and the run must continue.
  - `json.encode` of a non-finite float, which must return an error, and `json.to_json` of the same value, which must stop the run.
  - A custom `encode` method that changes a list that the walker visits.
- **Robustness checks:**
  - Enable `Config::retry_stress` in tests to force retries at resource limits.
  - On large inputs, compare resident memory against `max_memory` and against equivalent Sand loops.
  - Encode and decode deeply nested values on a 512 KiB thread stack.
  - Encode and decode values with many custom parts, and force a garbage collection in each callback.
- **Benchmarks:**
  - Benchmark the `json` functions against `main` with `bench/run.py` and `benches/codec.rs`.
  - Measure the cost of per-part dispatch on values without custom parts. This result decides whether the walker needs the flag from [Speed of the walker](#speed-of-the-walker).
  - Measure the decode walker with its explicit stack against the recursive decoder on `main`.
  - Measure call overhead for generic functions with `Encode` bounds.

## Design decisions

The owner made these decisions during review.

- **Custom `encode` methods write through an `Encoder`.** The signature is `fn encode(self, encoder: Encoder) -> Result<()>`. The engine checks at run time that a method writes exactly one value.
- **`encode` returns `Result<()>`.** A custom method can report an error to the caller of `json.encode`. serde returns `Result<S::Ok, S::Error>` (`serde_core` 1.0.229, `src/ser/mod.rs:149-150`), and Swift's `encode(to:)` throws (`Codable.swift:24-25`). `json.encode` returns `Result<str>`.
- **`json.encode` returns an error for a non-finite float**, and for a map key without a text form. The error has kind `"invalid"`. The output therefore conforms to RFC 8259, which does not allow `NaN` or infinity. Go's `encoding/json` also returns an error (`encoding/json/encode.go:80`). serde_json writes `null`, and its maintainer calls an error "better behavior" (https://github.com/serde-rs/json/issues/985). Nesting beyond 512 levels stays a limit that stops the run.
- **`json.to_json` uses a dedicated `Json` sink.** The sink builds the `Json` tree directly. A single layer of JSON rules serves both the JSON sink and the `Json` sink.
- **`json.to_json` keeps the return type `Json`.** If a conversion fails, `to_json` stops the run. The `json!` macro keeps its form.
- **`reflect` is deleted.** `Decode` dictionaries supply type structure. Hosts use `TraitDict<N>` with a `Decode` bound in place of `Reflect<N>`.
- **Derivation stays automatic,** as for `Eq`, `Ord`, and `Hash`. A type author changes the encoding of a type with a custom implementation.
- **This branch adds no field options,** such as a rename or a skip. A type that needs one writes a custom implementation. Field options can come later.
- **A custom `decode` method reads at most one value, without a retry.** A rewind could not undo the callbacks that a failed attempt already ran.
- **The `Value` source accepts only exact forms.** The JSON rules stay in the JSON source, and a host with JSON data passes JSON text. serde_json reads bytes from a JSON string as UTF-8 text, not Base64 (`serde_json` 1.0.154, `src/value/de.rs:414-420`), so a JSON form of `bytes` is not universal.
- **A duplicate struct field or `init` parameter is a decode error.** The decode returns an error of kind `"parse"`, and the run continues. serde's derive also reports `duplicate_field` (`serde_derive` 1.0.229, `src/de/struct_.rs:269`). A duplicate map key keeps its last value.
- **Decoding ignores the visibility of `init`,** as decoding on `main` ignores the visibility of fields.
- **Native types opt in to each trait.** `#[sand(encode, decode)]` derives both. `#[sand(encode = "manual")]` or `decode = "manual"` declares a trait that the author implements in Rust, with no Sand code in the path.
- **`native::Encode` replaces `value::Encode`.** A single Rust trait serves both the walkers and the conversion to `Value`.

## Open questions

Each question below changes what host developers or script authors can do. Each lists the options, the evidence, and a recommendation.

### 1. What happens to values whose types do not implement `Encode`?

On `main`, `Vm::to_value` accepts any value and is synchronous (`src/vm/mod.rs:914-947`). Its doc example registers `pub native fn record<T>(event: T)`, without a bound. The copier turns closures, tasks, channels, and native objects into `Value::Opaque` (`src/value.rs:59-61`, `:549-553`).

A conversion cannot both accept any value and apply custom implementations. An unbounded `T` brings no `Encode` dictionary. Without that dictionary, the walker cannot find a custom implementation, and its output would differ from `json.encode` for the same value. Also, a synchronous native function cannot run Sand code, because `NativeFn` is a plain `Fn(&mut Vm, &[VmValue])` (`src/vm/mod.rs:28`).

- **Option A: keep `Value::Opaque` and an untyped copy.** `Vm::to_value` stays synchronous and keeps the copier.
  - For: a host can inspect a value of any type.
  - Against: the result ignores custom implementations. The engine keeps a second walker. A host can use an opaque part only to read its type name, and the engine cannot rebuild an opaque value in the VM. `Vm::display` and `Vm::debug` already format any value (`src/vm/mod.rs:974-981`).
- **Option B: every conversion requires `T: Encode`.** Delete `Value::Opaque` and the public synchronous `Vm::to_value`. A synchronous native function that needs a `Value` declares `T: Encode` and takes a new parameter type, for example `ValueArg<N>`. The engine converts the argument before the Rust body runs. Branch `origin/encode-decode-traits` built this mechanism as `Encoded<N>` (commit `964029c`). An async native function calls `NativeContext::to_value`.
  - For: the checker reports the problem at compile time, and custom implementations always apply. serde_json also requires the bound: `pub fn to_value<T>(value: T) -> Result<Value, Error> where T: Serialize` (`serde_json` 1.0.154, `src/value/mod.rs:995-998`).
  - Against: a host cannot convert a value whose type does not implement `Encode`. `SPLIT.md` changeset 17 must then state what `Outcome::Returned(Value)` holds for such a result. One rule: the engine rejects a function whose return type does not implement `Encode` before the call, and a script result of such a type becomes `Value::Unit`.

A synchronous `Vm::to_value` that takes a dictionary fits neither option. It would fail at run time as soon as a script author adds a custom `Encode` implementation.

**Recommendation:** Option B.

### 2. Should the limits on local functions and field defaults stay?

With methods, `Encode` and `Decode` become subject to two checker limits:

- The checker rejects a bound on a trait with methods on a local function: "local functions cannot require traits with methods; declare the function at the top level" (`src/check/expr.rs:618-626`). A local function is a closure value with a plain function type, so a call has no slot for hidden dictionaries.
- The checker rejects the use of such a bound in a struct field default, "because defaults receive no trait dictionaries" (`src/check/traits.rs:1133-1138`). The default helper of a field takes no dictionaries (`src/check/mod.rs:1632`).

On `main`, both accept `Encode` and `Decode`, because the traits have no methods. For example, a local `fn helper<T: Encode>(x: T) -> str { json.encode(x) }` compiles on `main`. Under this design, a local `fn helper<T: Encode>(x: T) -> Result<str> { json.encode(x) }` fails to compile.

For local functions:

- **Option A: keep the limit.** Programs with local generic codec helpers stop compiling.
- **Option B: lift the limit for all traits, in its own changeset before this one.** Commit `204fb7e` on `origin/encode-decode-traits` did this in the checker only: each local function gets hidden dictionary parameters, and a use as a value builds a closure that passes them. Its diff in `src/check/` was about 83 added and 15 deleted lines. Commit `5c6e21c` added verifier support in about 21 added and 7 deleted lines. A local function that uses the feature allocates one closure for each use as a value.

For field defaults:

- **Option A: keep the limit in this branch.** Struct type parameters cannot be `reflect` (`src/syntax/parser.rs:461-464`), so no field default can decode a `T` today. A default has no value of `T` to encode.
- **Option B: lift the limit.** The default helper takes the struct's bound dictionaries. Commit `e753d17` on `origin/encode-decode-traits` did this for construction only. Most of its added checker code is a new compile error at decode calls, because `json.decode` could not pass the dictionaries. In this design, the decode walker must also pass them, so a structural `Decode` implementation must require them and its shape must hold templates for them. Lift the limit for both construction and decoding, or for neither.

**Recommendation:** Option B for local functions. Option A for field defaults.

### 3. Which trait bounds can the `init` of a decoded struct use?

A struct that declares `init` derives `Decode` through `init`. `init` inherits the bounds of the struct. A bound on a trait with methods, such as `T: Num`, needs a dictionary in the `init` call. `TODO.md:268-277` lists this issue as open work.

- **Option A: the structural `Decode` implementation also requires the bounds of `init`.** Its shape holds part templates for their dictionaries, and the walker passes them to `init`. Parameter default helpers already take the dictionaries of their function (`src/check/mod.rs:2768`).
- **Option B: a struct whose `init` needs dictionaries does not derive `Decode`.**

**Recommendation:** Option A.

### 4. Can a native type with type parameters have a manual implementation?

`SandStruct` and `SandEnum` support type parameters (`crates/sandlang-macros/src/lib.rs`, "Rust type parameters define Sand type parameters"). A Rust implementation of `encode` for such a type cannot encode a part of type `T` by itself. The Sand type of `T` is erased, so the part needs its dictionary, and Rust code has no access to it.

- **Option A: only native types without type parameters can have a manual implementation.** A generic native type can still derive its implementation.
- **Option B: give manual implementations a way to encode and decode a part with its dictionary.** The sink and source interfaces gain a request that runs the walker on a part. That part can then reach a custom Sand implementation, so the request must be able to wait for a callback.

**Recommendation:** Option A.
