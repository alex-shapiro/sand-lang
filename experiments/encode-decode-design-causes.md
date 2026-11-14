# Why `Encode` and `Decode` are hard to make user traits

Two rules in the Sand compiler and VM make it hard to let users implement `Encode` and `Decode`:

1. **Native functions run only as leaf calls.** Most interpreters walk standard values in host code. The walker calls script hooks only for custom types. Sand prevents this pattern because native Rust functions run to completion on the Rust stack and return directly to the VM.
2. **The VM runs user traits through interpreted dictionaries.** For built-in traits, the VM walks heap values directly in Rust. User traits require static types because a decoder starts from raw input. An `impl` can also target an erased type argument such as `[int]`. Callers therefore pass user trait closures in run-time dictionaries. The bytecode interpreter runs those closures for every value.

These two rules leave two codec designs:

- **Pure Sand codec.** The compiler generates Sand codec methods for every type. Encoding and decoding then take 2 to 5 times as long as on `main`.
- **Dual native and Sand codecs.** A native walker handles compiler-derived types at `main` speed. Generated Sand methods handle custom types. Both codecs must produce the same output, errors, and retries across every format.

Authors on `origin/encode-decode-traits` built the dual-codec design. At least a quarter of its 8,210 new production lines maintain the native fast path alongside the Sand path.

Six other compiler and VM rules add work to any codec design. The table below summarizes all eight findings:

| Finding                                            | Impact        | Evidence                    |
| -------------------------------------------------- | ------------- | --------------------------- |
| 1. Native functions run only as leaf calls         | Root cause    | Verified, likely conclusion |
| 2. The VM runs user traits through dictionaries    | Root cause    | Verified, measured          |
| 3. The type checker restricts traits with methods  | Partial cause | Verified                    |
| 4. The VM restarts native calls on memory limits   | Partial cause | Verified                    |
| 5. Native functions use fixed trait wrappers       | Partial cause | Verified, likely link       |
| 6. The type checker builds derived codecs in Rust  | Minor         | Verified                    |
| 7. The engine enforces sandbox limits on type data | Minor         | Verified                    |
| 8. Sand codecs use more VM fuel than native codecs | Tangential    | Verified                    |

### Impact and evidence ratings

We rate each finding by its effect on codec design:

- **Root cause**: Changing this rule allows a single codec at native speed.
- **Partial cause**: This rule adds large effort to every codec design.
- **Minor**: This rule increases code size only.
- **Tangential**: This rule changes fuel counts and test outputs only.

We label the evidence for each claim:

- **Verified**: We checked the behavior in source code, docs, or compiler probes.
- **Measured**: We timed the behavior on a release build of `main` or read it from branch benchmarks.
- **Likely**: We reasoned from verified facts.

## Background

**Built-in and user traits.** A trait lists methods that a type can implement. The prelude defines six built-in traits: `Eq`, `Ord`, `Hash`, `Encode`, `Decode`, and `Num`. On `main`, `Encode` and `Decode` are marker traits. Only the compiler implements them; see Probe 2. If a struct declares an `init` constructor, the compiler skips `Decode` for that struct and any type that contains it; see Probe 4.

**Automatic derivation.** The type checker derives a built-in trait for any struct or enum whose fields implement that trait. On `main`, `check/derive.rs` records only whether each type implements the trait.

**Type erasure and dictionaries.** The compiler erases type arguments at compile time. It builds a single bytecode function for each generic definition. For each trait bound with methods, callers pass a hidden tuple of closures called a dictionary.

**`reflect` type parameters.** On `main`, a generic function that decodes values requires a `reflect` type parameter; see Probe 3. Callers then pass a hidden run-time type description from `compile/reflect.rs`.

**Fibers, safe points, and roots.** Each fiber holds the state of a Sand task. It stores Sand call frames in a heap `Vec`. The garbage collector runs only at interpreter safe points such as loop back edges. It traces VM registers and VM structures. It ignores `VmValue` variables on the Rust stack.

**Native functions and retries.** A native function is a Rust callback that takes `&mut Vm` and a `VmValue` slice. If a native call reaches the memory limit or requests host permission, the VM rewinds the call instruction. After garbage collection or the permission prompt finishes, the VM runs the native function again from the start.

**Custom codecs and schemas.** On `origin/encode-decode-traits`, users can implement `Encode` or `Decode` for a type or any of its fields. For types with only derived implementations, a native Rust walker reads a static type schema.

## Finding 1: Native functions run only as leaf calls

**Impact:** Root cause. **Evidence:** Verified mechanism, likely conclusion.

### Design choice

The VM runs each native Rust function to completion on the Rust stack. The function returns a value, an error, or a scheduler signal. Three VM rules prevent calls from Rust back into Sand:

- The `Vm` API starts only top-level fibers from Rust.
- To suspend a task for async I/O, the interpreter loop returns `FiberStop::Async` to the scheduler. The interpreter can return to the scheduler only after every native Rust frame returns.
- A nested Sand call could trigger garbage collection at a safe point. Collection would then free unrooted heap objects held in outer Rust variables.

### Why this prevents a single codec

In CPython, the C `json` encoder handles standard types directly. It calls a Python hook only for custom types. Lua provides `lua_call` so C libraries can do the same.

On `origin/encode-decode-traits`, authors tested two ways to switch between native walks and custom Sand methods:

- **Heap-state continuations.** In Design W, the native walker kept its state on the heap. It returned control to the VM at each custom field. Handling a custom field took about 435 ns. The walker spent 270 ns of that time saving and restoring heap state. Heap state also made standard decoding 40% to 45% slower.
- **Native struct shells.** In Design C, generated Sand code called a native shell for each struct. The shell handled standard fields in Rust and returned to Sand at each custom field. Each custom field added about 180 ns.

In the callback study, we measured a raw continuation hop at about 70 ns. A nested interpreter call took about 101 ns. Design W spent most of its extra time saving and restoring walker state.

Direct calls from Rust to Sand would let a native walker keep its state on the Rust stack. A single native walker could then handle both derived and custom types.

### Requirements for nested calls

To support nested Rust-to-Sand calls, we must make three VM changes:

- Block task suspension inside nested calls, or save native state across suspensions.
- Track `VmValue` handles in active Rust frames as garbage-collector roots.
- Collect garbage in place during nested calls instead of restarting the outer native call.

## Finding 2: The VM runs user traits through dictionaries

**Impact:** Root cause. **Evidence:** Verified mechanism, measured cost.

### Design choice

On `main`, the VM compares, hashes, and encodes values by walking `VmValue` trees in Rust. Each heap struct holds its definition ID and fields. Each heap list holds its items. The VM therefore reads heap values directly in `vm/compare.rs` and `json.encode`.

For user traits, the type checker resolves each trait bound from static types. Callers pass the resulting dictionary to generic functions. The bytecode interpreter then calls trait methods through that dictionary.

`Decode` already requires static types on `main` because a decoder builds a new value from raw text. The compiler passes a `reflect` type description to `json.decode`.

### Why user codecs require static types

If users can implement `Encode` and `Decode`, the compiler must resolve both traits from static types for two reasons:

- A decoder starts from raw input and builds the target `VmValue`.
- Users can implement a trait for a concrete type argument such as `impl Describe for [int]`. The compiler erases type arguments from heap values. An empty `[int]` and an empty `[str]` produce identical heap values. Only static types distinguish them.

### Measured cost of dictionary calls

The bytecode interpreter runs every dictionary call. In a dictionary-based codec, the interpreter runs at least a full call per value.

We timed 2,000,000 passes per loop on a release build of `main` at `625d1b9`. Across 5 rounds in each of two runs, the minimum times differed by at most 0.3 ns:

| Loop body                           | Time per pass, ns |
| ----------------------------------- | ----------------: |
| Empty loop                          |               5.0 |
| Direct Sand function call           |              16.6 |
| Generic call with trait method call |              28.3 |

The generic trait call takes about 23 ns more than the empty loop. On `origin/encode-decode-traits`, the native walker spends about 26 ns per value in total.

In a second benchmark on `main`, we implemented `ToJson` and `FromJson` as user traits for scalars, lists, and a five-field struct. Across 9 rounds in two runs on 20,000 structs, the Sand traits took 2.3 to 3.6 times as long as the native walkers:

| Operation                            | Time per struct, ns |
| ------------------------------------ | ------------------: |
| `json.to_json` native walk           |          200 to 216 |
| `ToJson` user trait in Sand          |          458 to 540 |
| `json.decode` native from text       |          305 to 317 |
| `json.parse` plus `FromJson` in Sand |      1,109 to 1,134 |

In the branch benchmarks, the all-Sand Design D1 prototype took 2 to 5 times as long as `main`.

### Building schemas at run time

The native walker requires a full schema of the target type. The compiler builds static schemas for closed types such as `[User]`. Generic functions receive type information only at run time.

In `codec/composed.rs`, the VM builds schemas at run time from compile-time templates. Multiple engines can share a compiled `Program` through `Arc`. The VM therefore caches up to 1,024 run-time schemas behind a lock on `Program`. Copying each generic function per type argument would let the compiler build every schema at compile time. That change would increase bytecode size and compile time.

## How Findings 1 and 2 force dual codecs

To keep `main` speed while supporting custom codecs, `origin/encode-decode-traits` maintains both a native schema walker and generated Sand methods. Both codecs must produce the same output, field paths, errors, and retries. In `check/codec.rs`, the type checker generates four pairs of derived methods to support both paths:

| Derived method pair                | Purpose                                            |
| ---------------------------------- | -------------------------------------------------- |
| `Encode`, `Decode`                 | Custom codec implementations in Sand               |
| `FastEncode`, `FastDecode`         | Native walk speed for derived types                |
| `ComposedEncode`, `ComposedDecode` | Run-time schema building in generic functions      |
| `CheckedEncode`, `CheckedDecode`   | State checks for Sand-driven encoders and decoders |

The branch adds 2,065 lines across four files for the native fast path alone:

- `codec/fast.rs`
- `codec/composed.rs`
- `stdlib/json/text/walk.rs`
- `stdlib/json/text/read.rs`

Shared files such as `codec/value_format.rs` also contain fast-path code. The branch includes 1,072 lines of property tests in `tests/props/codec.rs` to compare both codecs. Even so, the property test missed a planted bug. Random inputs rarely reached the faulty branch.

## Finding 3: The type checker restricts traits with methods

**Impact:** Partial cause. **Evidence:** Verified.

Adding methods to `Encode` and `Decode` requires trait dictionaries. On `main`, the type checker rejects trait dictionaries in four language positions.

The type checker checks `TraitDef::has_dict` in 24 places on `main`. It accepts marker traits in every position because it checks them only at compile time. For traits with methods, the checker enforces four limits:

| Checker limit on traits with methods                  | Source     |
| ----------------------------------------------------- | ---------- |
| Only top-level functions can require them             | Probe 1    |
| The checker derives only marker traits for `never`    | Probe 5    |
| Struct field defaults accept only marker trait bounds | SPEC §4.2  |
| Native functions accept only fixed trait wrappers     | SPEC §16.3 |

Existing Sand programs use `Encode` and `Decode` on local functions, empty `[never]` lists, and struct defaults. Authors on `origin/encode-decode-traits` landed four commits to lift these checker limits before adding codec methods. Adding methods to any other built-in marker trait requires the same checker changes.

## Finding 4: The VM restarts native calls on memory limits

**Impact:** Partial cause. **Evidence:** Verified.

After garbage collection, the VM restarts the interrupted native function from the start. Every native call that changes codec state must therefore roll back cleanly on retry.

If a native function reaches `max_memory`, the interpreter returns `FiberStop::CollectAndRetry`. The VM collects garbage and runs the instruction again. Native functions must check memory limits before changing state, or undo their changes before returning.

On `main`, `json.encode` and `json.decode` each build a fresh result in a single native call. In a trait-based codec, Sand methods make many small native calls on a stateful `Encoder` or `Decoder`.

Encoders and decoders on `origin/encode-decode-traits` use `mark`, `rollback`, and `atomic` helpers to protect format state. Even with those helpers, `Config::retry_stress` tests found four retry bugs. In each bug, an encoder or decoder changed its state before a memory check failed.

## Finding 5: Native functions use fixed trait wrappers

**Impact:** Partial cause. **Evidence:** Verified mechanism, likely link to Finding 1.

Native Rust functions run outside the bytecode interpreter. They read trait information through fixed Rust parameter wrappers rather than calling Sand closures.

SPEC §16.3 defines three wrapper types for generic native parameters on `main`:

- `Param<N, B>` holds a `VmValue` whose type implements built-in marker traits `B`.
- `Reflect<N>` holds a run-time type description.
- `Num<N>` reads `zero` and `one` from a `Num` dictionary. In `check/traits.rs`, the type checker requires every `Num` implementation to define native functions in a fixed order.

To pass user codecs to Rust, `origin/encode-decode-traits` added two wrapper types:

- `CodecDict<N, B>` holds a codec dictionary.
- `Encoded<N>` holds a value already encoded as a host `Value`.

The branch also changed how the Rust host reads VM values. On `main`, `Vm::to_value` walks raw `VmValue` trees directly. That walk skips custom `Encode` methods. Authors on `origin/encode-decode-traits` replaced `Vm::to_value` with conversion through `Encode`.

## Finding 6: The type checker builds derived codecs in Rust

**Impact:** Minor. **Evidence:** Verified.

The type checker builds derived codec methods directly as typed HIR in Rust. On `origin/encode-decode-traits`, `check/codec.rs` uses 1,240 lines to build this HIR.

The checker derives `Encode` and `Decode` automatically for every compatible type. At first, it built method bodies for every type immediately. Type-checking 20,000 structs then took 111 seconds in a debug build. On the branch, the checker builds a method body only on first use. That change cut the 20,000-struct check time to 2.6 seconds. In REPL sessions, a later input can replace an `impl`. The checker must then rebuild derived codecs and roll back cached schemas.

The Swift and Kotlin compilers generate codecs in the same way. Building codecs in the compiler increases code size only.

## Finding 7: The engine enforces sandbox limits on type data

**Impact:** Minor. **Evidence:** Verified.

Script input controls the size of type descriptions and schemas. The checker and VM must enforce memory, fuel, and stack limits on them.

In commit `f0a61bd`, authors on the earlier `derive-payloads-generics-num` branch fixed three sandbox bugs in type data:

- A script allocated 2.65 GB of copied run-time type descriptions under a 64 MB memory limit.
- A 1 KB script made the type checker use 6.7 GB of memory.
- Recursive Rust destructors for 20,000 nested struct-default descriptions overflowed the call stack and aborted the process.

Native walkers must also keep walk state on a heap stack rather than the Rust call stack. On `main`, `json.decode` uses 808 KiB of Rust stack to decode 512 nested structs. Every Sand codec design must enforce these sandbox limits.

## Finding 8: Sand codecs use more VM fuel than native codecs

**Impact:** Tangential. **Evidence:** Verified.

Generated Sand codec methods use more VM fuel than native Rust walkers.

The VM charges fixed fuel for each bytecode instruction and call. In the all-Sand Design D1 prototype, encoding values used 2 to 4 times as much fuel as on `main`. Moving a codec from Rust walkers to Sand bytecode changes script fuel budgets. We must then update the fuel tests in `tests/metering.rs`.

## Four suspected causes on `main`

Four parts of `main` look like separate causes of the codec problem. Each part actually results from Findings 1 through 5:

- **Special built-in traits.** The VM walks heap values for built-in traits. The checker also skips dictionaries for them. See Findings 2 and 3.
- **Native interop rules.** Native functions use fixed parameter wrappers because Rust code runs outside the Sand call stack. See Findings 1 and 5.
- **`Decode` as a marker trait.** Generic decoders use `reflect` type descriptions because they build new values from raw input. See Finding 2.
- **Structure of `stdlib/json`.** Finding 2 shapes the three value walkers and the `reflect` decoder in `json`. Rewriting `stdlib/json` alone leaves both root causes in place.

## Design choices for future work

**Evidence:** Likely.

Before starting a new codec branch, we must choose among three designs:

- **Allow nested Rust-to-Sand calls.** Make the three VM changes listed in Finding 1. A single native walker can then call custom Sand methods.
- **Run all codecs in Sand.** Generate Sand codec methods for every type. Encoding and decoding will take 2 to 5 times as long as on `main`.
- **Generate the fast path from the Sand path.** Keep both a native walker and generated Sand methods. Have the compiler build the native schema from the generated Sand methods so both codecs always match.

## Scope and limits

We ran this study on 2026-11-14 across three sources:

- **`main` at `625d1b9`.** We ran compiler probes and release-build benchmarks on a single macOS machine. Each table reports the minimum time across rounds.
- **`origin/encode-decode-traits`.** We read the design docs, module docs, and selected definitions rather than all 16,000 changed lines. We took the numbers for Designs C, W, and D1 from the branch benchmark records.
- **`origin/claude/sandlang-native-callbacks-research-i6bf14`.** We read the callback prototype timings from the study report.

Our notes on Python, Lua, Swift, and Kotlin rely on general knowledge rather than source or doc audits.

## Appendix: probes and benchmark setup

We ran five compiler probes on a release build of `main` at `625d1b9`.

**Probe 1.** The checker rejects a local function that requires a trait with methods:

```sand
trait Show { fn show(self) -> str }
fn main() {
    fn show_twice<T: Show>(value: T) -> str { value.show() + value.show() }
}
```

**Probe 2.** The checker rejects a user `impl` of `Encode`:

```sand
struct Email { address: str }
impl Encode for Email {}
```

**Probe 3.** The checker requires `reflect` on a generic `Decode` parameter:

```sand
import json
fn decode_json_list<T: Decode>(json_text: str) -> Result<[T]> { json.decode(json_text) }
```

**Probe 4.** The checker skips `Decode` if a field type defines `init`:

```sand
import json
struct Email {
    address: str
    pub fn init(address: str) -> Result<Email> { Ok(Email(address: address)) }
}
struct User { name: str, email: Email }
let decoded_user: Result<User> = json.decode("{}")
```

**Probe 5.** The checker accepts `[never]` for `Encode` and rejects it for `Show`:

```sand
trait Show { fn show(self) -> str }
impl Show for int { fn show(self) -> str { "int" } }
fn count_showable<T: Show>(items: [T]) -> int { items.len() }
fn count_encodable<T: Encode>(items: [T]) -> int { items.len() }
let empty_items: [never] = []
print(count_encodable(empty_items))
print(count_showable(empty_items))
```

In the call benchmark, we timed five loops of 2,000,000 passes with `time.Stopwatch`. Finding 2 reports the empty loop, the direct call, and the generic trait call. The script also timed closure calls and list-length calls. We put an early `return` in the direct-call function. That early return kept the compiler from inlining the call.

In the codec benchmark, we encoded and decoded 20,000 five-field `User` structs. Each `User` held four scalar fields and a string list. We implemented user traits `ToJson` and `FromJson` to convert between Sand values and `json.Json` trees. We asserted that both trait outputs matched `json.to_json` and `json.decode`.
