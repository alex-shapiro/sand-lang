# Design: `Encode` and `Decode` as ordinary traits

**Status: proposal for review.** Nothing in this document is implemented yet. The branch `encode-decode-traits-v2` holds an earlier code draft. That draft does not compile. [What the earlier draft keeps](#what-the-earlier-draft-keeps) lists the parts of it that this design reuses.

## Terms

This document uses these terms. Each term has a single meaning in this document.

**Trait evidence.** A generic function such as `fn save<T: Encode>(x: T)` must know how `T` implements `Encode`. The compiler erases type arguments, so the function cannot find the impl from `T` at run time. The caller therefore passes information about the impl as a hidden argument. This hidden argument is the trait evidence.

**Dictionary.** On `main`, the trait evidence for a trait with methods is a tuple. This tuple is the dictionary. It holds three groups of items, in this order:

1. The method closures of the impl.
2. The dictionaries of the supertraits. For `Decode: Encode`, this group holds the `Encode` dictionary of the same type.
3. The dictionaries for the bounds of the impl. For `impl<T: Show> Show for [T]`, this group holds the `Show` dictionary of `T`.

A trait without methods, such as `Hash` on `main`, has no dictionary. Its evidence is a compile-time check only.

**Constant dictionary.** If the compiler knows every type in a dictionary, it emits the dictionary once as a program constant. Example: the dictionary of `[int]: Show`.

**Runtime dictionary.** Inside generic code, a dictionary can depend on the dictionary of a type parameter. The generated code then builds the tuple at run time. Example: the dictionary of `[T]: Show` inside `fn f<T: Show>`.

**Derived trait.** The compiler implements some traits automatically. On `main`, these traits are `Eq`, `Ord`, `Hash`, `Encode`, and `Decode`. A struct or enum implements a derived trait if all its fields and variant payloads implement it. A user does not write an impl for a derived trait.

**Native function.** A native function is a Rust function that Sand code calls.

**Native default method.** This design adds native default methods. A native default method is a trait method whose default body is a native function. An impl that does not define the method uses the native default.

**Part.** A part of a value is a value that it contains directly. The parts of a struct are its fields. The parts of an enum value are its payloads. The parts of a list or set are its elements. The parts of a map are its keys and values.

**Walker.** A walker is a native function that visits a value and its parts in turn. This design has two walkers. The encode walker reads a value in the VM heap. The decode walker builds a value in the VM heap.

**`Value`.** `sandlang::value::Value` is a Rust type that holds a copy of a Sand value. A `Value` has no reference to the VM. A host can keep a `Value` after a run ends.

**Callback.** Commit `8a51c46` lets an async native function call a Sand function. This document calls that action a callback. A round trip from Rust to Sand and back costs about 90 ns. `experiments/native-async-sand-calls.md` reports this measurement.

**Triage.** The triage is the document `experiments/encode-decode-design-causes.md` on the branch `encode-decode-design-causes`. It explains why earlier codec designs were slow or complex.

## Summary

`Encode` and `Decode` become traits with methods. Each trait therefore has a dictionary, as other traits with methods do.

Each method has a native default. The native default walks the value in Rust.

The compiler still derives `Encode` and `Decode` for structs and enums. A derived impl becomes an ordinary impl. That impl uses the native defaults.

A user can write an impl of `Encode` or `Decode` for a type. The user impl replaces the derived impl for that type.

The native default must find the impl of each part. It finds that impl through dictionaries. It does not use the type of the part at run time.

If the impl of a part also uses the native default, the walk stays in Rust. If the impl of a part is a user impl, the walker makes a callback to the user method.

The VM has no code for `Encode` or for `Decode` in particular. The mechanism works for any trait that has native default methods. `Eq`, `Ord`, and `Hash` can use the same mechanism later.

The design meets the four goals of the task:

- **`Value` can be encoded and decoded.** The encode walker converts a Sand value to a `Value`. The decode walker converts a `Value` to a Sand value.
- **JSON can be encoded and decoded.** Plain Rust code converts JSON text to a `Value`. Other plain Rust code converts a `Value` to JSON text.
- **`Encode` and `Decode` are ordinary traits.** User impls, generic impls, and bounds follow the trait rules in SPEC §4.9.
- **`json.Json` uses the ordinary JSON encoder.** `Json` has ordinary impls of `Encode` and `Decode`. The JSON writer has no special case for `Json`.

## The problem with the earlier draft

The earlier draft found user impls through the type of a value at run time. That approach needed three special cases:

- **The type checker.** `Encode` and `Decode` declared methods but had no dictionary. The checker named the two traits in an exception.
- **The VM type table.** The VM keeps a runtime record for each struct and enum type. The draft added two fields to that record. One field held the user `encode` method of the type. The other field held the user `decode` method.
- **The impl rules.** An impl of `Encode` or `Decode` had to be for a struct or enum without type parameters. The two fields could hold a method, but not the type arguments of a generic impl.

This design removes all three special cases. The walkers find impls through dictionaries, as all other trait code does.

## Part 1: Native default methods

The prelude declares the two traits with native default methods:

```sand
pub trait Encode {
    /// Writes `self` to `e` as a single value with `e.encode(x)`.
    native fn encode(self, e: Encoder)
}

pub trait Decode: Encode {
    /// Reads a value of this type from `d`, usually with `d.decode()`.
    native fn decode(d: Decoder) -> Result<Self>
}
```

On `main`, every trait method already receives the dictionary of `Self` as its first hidden argument. A native default method therefore receives the dictionary of the impl that it runs for. The native default reads that dictionary to find the impls of the parts.

## Part 2: Structural impls

A structural impl is an impl that uses the native default methods of its trait. Structural impls have two sources.

**Explicit structural impls.** The prelude declares a structural impl for each built-in type. Each impl has an empty body:

```sand
impl Encode for int {}
impl<T: Encode> Encode for [T] {}
impl<K: Encode, V: Encode> Encode for [K: V] {}
```

**Derived structural impls.** The type checker creates a structural impl for each struct or enum that can derive the trait. The checker already computes which type parameters must implement the trait. Those type parameters become the bounds of the derived impl. Example: `struct Pair<A, B> { a: A, b: [B] }` gets `impl<A: Encode, B: Encode> Encode for Pair<A, B> {}`.

A user impl of a trait for a type stops the checker from deriving that trait for the type.

A derived impl is an ordinary impl in the checker. The checker therefore finds the evidence for `Encode` and `Decode` through ordinary impl lookup. On `main`, a separate code path handles derived traits.

## Part 3: Impl shapes and part templates

### Why the dictionary needs more

A native default must reach the impl of each part. The dictionary on `main` does not always give it that impl.

Consider `struct User { email: Email }`. `User` has no type parameters. Its dictionary therefore holds no bound dictionaries. The native default for `User` cannot find the `Encode` impl of `Email`.

### The impl shape

Each structural impl gets an impl shape. The impl shape is a program constant. The compiler builds it.

The impl shape has two fields:

- **The kind of the type.** Examples of kinds are `Int`, `Str`, `List`, `Map`, `Tuple(3)`, `Struct`, `Enum`, a native value type such as `time.PlainDate`, and `Never`. A struct kind also holds the field names and the field defaults. A native value type kind also holds the function that parses its text.
- **A part template for each part.** A part template tells the walker how to build the dictionary of the part.

The dictionary of a structural impl holds its impl shape after the supertrait dictionaries:

```text
[methods..., supertrait dictionaries..., impl shape, bound dictionaries...]
```

A call to a trait method reads only the methods and the supertrait dictionaries. Only the native defaults read the impl shape. The dictionaries of other impls therefore keep their layout.

### Part templates

A part template has three forms:

```rust
enum PartTemplate {
    /// The program constant with index `k`. An example is a constant
    /// dictionary.
    Const(u32),
    /// The bound dictionary with index `j` in the dictionary of this impl.
    Bound(u32),
    /// A new dictionary tuple. The items are built from their own templates.
    Tuple(Vec<PartTemplate>),
}
```

Examples:

| Impl                                                   | Part    | Part template                                            |
| ------------------------------------------------------ | ------- | -------------------------------------------------------- |
| `impl Encode for User` with `email: Email`             | `Email` | `Const(k)`, where constant `k` is the dictionary of `Email` |
| `impl<T: Encode> Encode for [T]`                       | `T`     | `Bound(0)`                                               |
| `impl<T: Encode> Encode for Pair<T>` with `items: [T]` | `[T]`   | `Tuple([list method, list shape, Bound(0)])`             |

### How the compiler builds part templates

The type checker already builds trait evidence for the body of an impl method. That code produces an expression tree. Each node of the tree builds a dictionary, reads a bound dictionary, or loads a constant.

For each part of a structural impl, the checker builds the evidence for `PartType: Trait` in the same way. The compiler then converts the expression tree into a part template. On `main`, the compiler converts the same kind of tree into bytecode.

## Part 4: Recursive types

### The problem

Consider `struct Tree { kids: [Tree] }`. The impl shape of `Tree` needs the dictionary of `[Tree]`. The dictionary of `[Tree]` holds the dictionary of `Tree` as a bound dictionary. The dictionary of `Tree` holds the impl shape of `Tree`. The references form a cycle.

### The solution: build part dictionaries during the walk

The walker builds the dictionary of a part only when it visits that part. This rule removes the cycles in two cases.

**Constant dictionaries.** A part template refers to another constant by its index. The VM reads the constant at that index during the walk. The VM does not read it when it creates the constants. The compiler reserves the index of an impl shape before it builds the part templates of that shape. A part template that reaches the same shape again uses the reserved index. No constant then contains another constant that contains it.

**Runtime dictionaries.** Consider a generic `Tree<T>` inside a generic function. The part template of `kids` is `Tuple([list method, list shape, Tuple([tree method, tree shape, Bound(0)])])`. This template is finite. The walker builds the dictionary of `[Tree<T>]` when it visits `kids`. It builds the dictionary of each `Tree<T>` element when it visits that element.

The same rule handles polymorphic recursion. In polymorphic recursion, a generic type contains itself with a different type argument. Example: `enum Nest<T> { Leaf(T), Deep(Nest<[T]>) }`. The part template of `Deep` builds the dictionary of `Nest<[T]>` from the dictionary of `T`. Each level of the value needs one more level of dictionary. The depth of the value limits the work.

### Cache

The walker keeps a cache for the duration of a walk. The cache maps a part template and the dictionary of the containing value to the dictionary of the part. A list of 10,000 `Tree<int>` values therefore builds the dictionary of `Tree<int>` one time.

### Two schemes that were not chosen

| Scheme                                                      | Problem                                                                                                                                      |
| ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Store the part dictionaries in each dictionary              | Recursive types make cyclic dictionaries. Polymorphic recursion makes an infinite dictionary at compile time.                                |
| Generate a Sand function that returns the part dictionaries | The walker must make a callback for each new dictionary. Each callback costs about 90 ns, also for types without user impls.                 |

Other languages solve the same problem in a similar way. Swift builds the witness tables of generic conformances at run time when it needs them, and caches them. A witness table in Swift has the same role as a dictionary in Sand. GHC builds the dictionaries of recursive instances as lazy values. These two statements come from general knowledge. I did not audit the Swift or GHC sources.

## Part 5: The walkers

### Encode

The encode walker copies a heap value into a `Value`. It does not use recursion on the Rust stack. It keeps a stack of open containers in a vector.

Each open container holds its dictionary. For each part, the walker builds the dictionary of the part from its part template.

The walker then reads the method in the dictionary of the part:

- If the method is the native default, the walker copies the part in Rust.
- If the method is a user method, the walker records a hole. A hole holds the position of the part in the `Value`, the part itself, and the dictionary of the part. The walker puts a placeholder in the `Value` at that position.

After the walk, the native function makes a callback for each hole. The callback runs the user `encode` method. The method writes a value to an `Encoder`. The walker encodes that value and puts the result at the position of the hole.

### Decode

The decode walker builds a heap value from a `Value`. It reads the kind in each impl shape to know what to build.

These parts become holes:

- A part whose type has a user `Decode` impl.
- A missing struct field whose default is not a constant. An example is `id: int = next_id()`.

The decode walker runs in two passes if it records holes:

1. The first pass decodes the input and records the holes.
2. The native function makes a callback for each hole and keeps the results.
3. The second pass decodes the input again. It uses the results at the holes.

This order has two effects. The first effect matches `main`: computed defaults run in post-order, and only after the whole input decodes. The second effect is new: sets and maps receive finished values, so they hash the correct values.

### No holes

If a walk records no holes, the native function returns its result at once. It creates no future and makes no callbacks. An async native call whose future is ready at the first poll costs about 112 ns. This path avoids that cost.

## Part 6: Type checker changes

1. **Native default methods.** A trait can declare a `native fn` method. On `main`, only an impl method can be native.
2. **Derived impls become ordinary impls.** The checker creates the derived structural impls described in Part 2. Two derivation conditions stay as they are. A struct with a `var` field does not derive `Hash`. A struct that declares `init` does not derive `Decode`.
3. **Part templates.** The checker builds the evidence for each part of each structural impl.
4. **`never`.** On `main`, `never` implements only traits without methods (SPEC §5.1). The expression `json.encode([])` needs `[never]: Encode`. In this design, `never` gets a structural impl of each derived trait. The kind of its impl shape is `Never`. The native default `decode` for `never` always fails. The native default `encode` for `never` never runs, because no value has type `never`.
5. **Native functions read dictionaries.** `json.encode<T: Encode>` receives the dictionary of `T` as a hidden argument. A new native parameter type, `Dict<N>`, reads the dictionary of the `N`-th bound that has methods. The existing parameter type `Num<N>` already reads the `Num` dictionary in the same way.
6. **`reflect` goes away.** On `main`, a generic decode function needs a `reflect` type parameter. The `reflect` parameter passes a runtime description of the type. In this design, the `Decode` dictionary holds the impl shape, so the description is no longer necessary. No stdlib function other than `json.decode` uses `reflect`. These parts are deleted: `reflect` in the parser and the checker, the runtime type descriptions, their templates, the bytecode instruction that builds them, and `compile/reflect.rs`.

Two limits stay in place. They apply on `main` to every trait with methods. After this change, they also apply to `Encode` and `Decode`:

- **A local function cannot require `Encode` or `Decode`.** The checker reports `local functions cannot require traits with methods`. On `main`, a local function can require them.
- **A struct field default cannot require `Encode` or `Decode`** (SPEC §4.2).

[Open question 2](#open-questions) asks whether to lift these limits in this branch.

## Part 7: JSON and `Value`

**Parsing.** The JSON parser converts JSON text to a `Value`. A JSON object becomes a `Value` map with string keys. JSON `null` becomes `Value::Unit`.

**Writing.** The JSON writer converts a `Value` to JSON text. The writer holds the JSON rules:

- It writes `Some(x)` as `x`.
- It writes `bytes` as a Base64 string.
- It writes map keys and native value types as text.

**Decoding JSON forms.** The decode walker accepts the JSON form of each type. Examples:

- It accepts a Base64 string for `bytes`.
- It accepts text keys for a map with `int` keys.

A `Value` from JSON text is therefore valid input for every decodable type.

**`json.Json`.** `Json` has ordinary impls whose methods are native functions:

```sand
impl Encode for Json { native fn encode(self, e: Encoder) }
impl Decode for Json { native fn decode(d: Decoder) -> Result<Json> }
```

Each method converts a whole `Json` tree in one call. `encode` converts a `Json` tree to a `Value`. `decode` converts a `Value` to a `Json` tree. `json.parse(text)` becomes `json.decode(text)` with `T = Json`.

**`Vm::to_value`.** `Vm::to_value` copies a heap value into a `Value`. In this design, it also takes the `Encode` dictionary of the value. `Vm::to_value` is a synchronous function. A synchronous function cannot make a callback. `Vm::to_value` therefore fails with kind `"invalid"` if it reaches a user impl. The new async function `NativeContext::to_value` runs user impls.

## Reinterpretation of SPLIT tasks 14 and 15

`SPLIT.md` divides the journal work into numbered tasks. Tasks 14 and 15 cover `Value`, encoding, and decoding.

**Task 14** added `Value` and `Vm::to_value`. It merged in pull request #41. This design changes two of its decisions:

- `Vm::to_value` takes the `Encode` dictionary of the value, so that it can find user impls.
- On `main`, three separate walkers read heap values: `json.encode`, `json.to_json`, and `Vm::to_value`. In this design, the encode walker replaces all three. `json.encode` and `json.to_json` convert its `Value` result.

**Task 15** becomes "`Encode` and `Decode` as ordinary traits". It has this content:

- Parts 1 to 7 of this design.
- The decode walker reads a `Value`. `json.decode` parses JSON text to a `Value` first.
- No Rust trait `value::Decode`. Task 16 adds `serde` support for `Value`. A host can then convert a `Value` to its own Rust types with `serde`.

**Task 18** lets a host call a script function with `Value` arguments. The decode walker converts each argument. Each parameter type of a script function contains no type parameters. The compiler can therefore emit a constant `Decode` dictionary for each parameter type.

## Costs and risks

**Dictionary passing.** Each call to a generic function with a `T: Encode` bound passes a dictionary. On `main`, `Encode` has no dictionary, so these calls pass nothing. For a type without type parameters, the dictionary is a constant. In generic code, the call builds the tuple at run time. The triage measured a generic trait method call at about 12 ns more than a direct call.

**Walk overhead.** For each part, the walker builds or reads a dictionary and compares a method. For a type without type parameters, every part template is a constant. The walker then reads constants and builds no tuples.

**The `Value` tree.** `json.encode` now builds a `Value` before it writes text. On `main`, `json.encode` writes text directly from the heap. I expect `json.encode` to be slower. I will measure the change.

**Memory accounting.** The VM limits the memory of a run with `max_memory`. The walkers count the `Value` tree against `max_memory` while they build it. During callbacks, the VM does not count the tree. `TODO.md` already records this class of problem for async native calls: "Charge boxed recursive futures against `max_memory`."

**Expected size.** This design deletes these parts:

- `compile/reflect.rs`
- the runtime type descriptions and their templates
- the typed JSON decoder
- the JSON text encoder
- the `json.to_json` converter
- the helper functions that fill computed defaults after `json.decode`
- `reflect` in the parser and the checker

This design adds these parts:

- the two walkers
- impl shapes and part templates
- native default methods
- structural impls for `never` and for the built-in types
- the native parameter type `Dict<N>`

I expect the total line count to go down. I will report the measured count.

## Rejected alternatives

| Alternative                                                                                    | Reason                                                                                                                                                                                                                      |
| ---------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Find user impls through the runtime type of a value, in a table keyed by trait and type        | This table is a second way to find impls, beside dictionaries. The compiler erases type arguments, so the table cannot support generic impls. `Decode` has no value to inspect, so it still needs a runtime type description. |
| Generate Sand code for each derived impl, as Swift does                                        | There is one code path. But each part costs a call from Sand code. The triage measured these codecs at 2.3 to 7 times slower.                                                                                               |
| Generate Sand code, and keep a native fast path for derived types                              | The two code paths must give the same output, paths, and errors. The branch `origin/encode-decode-traits` used this design. The triage found that at least a quarter of its 8,210 new production lines kept the two paths in agreement.                                                                 |

## What the earlier draft keeps

The earlier draft on `encode-decode-traits-v2` has three parts that this design keeps:

- **The walkers** in `codec/encode.rs` and `codec/decode.rs`. This includes holes, the two-pass decode, and the paths in error messages, such as `$.items[2].price`. Only the way the walkers find user impls changes.
- **The JSON parser and writer**, which operate on `Value`, and the native `Json` impl methods.
- **Computed defaults as holes.** This change deletes the helper functions that fill computed defaults on `main`.

This design drops four parts of the earlier draft:

- The two fields that the draft added to the VM type record.
- The record of user impl methods that the draft added to each type definition in the checker.
- The checker exception that gave `Encode` and `Decode` no dictionary.
- The rule that limited impls to struct and enum types without type parameters.

## Test and measurement plan

**Existing behavior.** The JSON spec tests in `tests/spec/std_json.test` keep their output. Some error messages will change on purpose. This document will list each changed message.

**New spec tests:**

- a user impl of `Encode` and of `Decode` on a struct, and on an enum
- a generic user impl, such as `impl<T: Encode> Encode for Box<T>`
- a recursive type, and a type with polymorphic recursion
- `json.encode([])`, which uses `[never]`
- a user impl of a type that contains another type with a user impl
- a user impl that waits on `task.sleep`
- an impl that writes no value, and an impl that writes two values
- the error path of a failed user `Decode` impl

**Robustness.**

- Run the tests with `Config::retry_stress`. This option forces the VM to retry calls that check memory or resources.
- Compare the resident memory of the process against `max_memory` on large inputs. Also compare it against an equivalent loop in Sand.
- Decode and encode deeply nested values.
- Encode a value with many holes.
- Run the deep tests on a thread with a 512 KiB stack.

**Speed.**

- Compare `json.encode`, `json.decode`, `json.parse`, and `json.to_json` on `main` and on this branch. Use `bench/run.py` and `benches/codec.rs`.
- Measure a call to a generic function that passes an `Encode` dictionary.

## Open questions

1. **How does a trait declare that the checker derives it?** On `main`, the checker recognizes derived traits by name. I propose a modifier: `pub derive trait Encode { ... }`. The modifier tells the checker to create a structural impl for each struct and enum that qualifies. The two derivation conditions stay checker rules for now: `var` fields for `Hash`, and `init` for `Decode`. The alternative is to keep the recognition by name in this branch.
2. **Should this branch lift the limits on local functions and struct field defaults?** These limits apply to every trait with methods. I recommend that this branch keep them. A local function that requires `Encode` then becomes an error.
3. **Should this branch remove `reflect`?** I recommend yes. Without `reflect`, decoding has one source of type information. The deletion is also large.
