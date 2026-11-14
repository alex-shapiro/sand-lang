# Design causes of the `Encode` and `Decode` difficulty

Two design choices in the Sand compiler and VM make it hard to turn `Encode` and `Decode` into regular user-implementable traits:

1. Native Rust functions cannot call Sand functions.
2. Built-in traits inspect heap values in Rust, whereas user traits dispatch through interpreted type dictionaries.

We ran this investigation on 2026-11-14 across three sources:

- `main` at `625d1b9`, including new compiler probes and release-build benchmarks
- `origin/encode-decode-traits`, an 8,210-line branch that implements regular codec traits
- `origin/claude/sandlang-native-callbacks-research-i6bf14`, a study of Rust-to-Sand callbacks

## Summary

In most interpreters, a native codec walks standard values in host code. It calls script code only for types with custom implementations. Sand blocks this design because native Rust functions cannot call Sand functions.

User-implementable traits also require static type dispatch instead of value inspection. Decoding builds a value before any heap object exists. Custom implementations can also target erased type arguments such as `[int]`. Because the compiler erases type arguments, generic functions pass trait methods at run time through dictionaries of closures. The bytecode interpreter executes those closures for every value.

Together, these two choices force a trade-off between speed and implementation size:

- **Pure Sand codec.** Synthesizing Sand code for every type requires a single implementation. Encoding and decoding run 2 to 5 times slower than `main`.
- **Dual native and Sand codecs.** Keeping a native walker for compiler-derived types preserves `main` speed. Both implementations must then match on every format, error path, and memory-limit retry.

The `origin/encode-decode-traits` branch chose the dual-codec design. At least a quarter of its 8,210 new production lines exist only to keep the native fast path aligned with the Sand path.

Six additional design choices add engineering work or observable changes across codec designs:

| Finding | Impact | Evidence |
| --- | --- | --- |
| 1. Native functions cannot call Sand code | Root cause | Verified, likely conclusion |
| 2. Built-in traits read values; user traits use types | Root cause | Verified, measured |
| 3. Traits with methods have checker restrictions | Partial cause | Verified |
| 4. Memory-limit retries restart native calls | Partial cause | Verified |
| 5. Native functions receive trait bounds only as data | Partial cause | Verified, likely link |
| 6. The checker synthesizes derived codecs in Rust | Minor | Verified |
| 7. Run-time type metadata must obey sandbox limits | Minor | Verified |
| 8. Fuel metering exposes the codec execution path | Tangential | Verified |

### Impact and evidence ratings

Impact ratings show how much each choice shapes the problem:

- **Root cause**: Changing this choice enables a single-implementation codec at native speed.
- **Partial cause**: This choice adds substantial work to every design.
- **Minor**: This choice adds code volume without changing the codec architecture.
- **Tangential**: This choice affects observable metrics and tests without shaping the design.

Evidence ratings show how we established each claim:

- **Verified**: Confirmed in source code, specifications, or compiler probes.
- **Measured**: Timed on a release build of `main` or recorded in branch benchmarks.
- **Likely**: Inferred from verified facts.

## Background

**Built-in and user traits.** A trait defines bounds or methods that types can implement. The prelude declares built-in traits for comparison, hashing, numeric constants, encoding, and decoding. On `main`, `Encode` and `Decode` are empty marker traits that users cannot implement.

**Automatic derivation.** The type checker automatically derives built-in traits for any struct or enum whose fields satisfy that trait. On `main`, derivation records only trait membership in `check/derive.rs`.

**Type erasure and dictionaries.** The compiler erases type arguments and emits a single bytecode function per generic definition. For each required trait with methods, the caller passes a hidden dictionary tuple of method closures. Marker traits pass zero run-time arguments.

**`reflect` type parameters.** On `main`, generic functions that decode values mark their type parameter with `reflect`. The compiler then passes a hidden run-time type descriptor from `compile/reflect.rs`.

**Fibers, safe points, and roots.** A fiber represents a Sand task and stores its call frames in a heap `Vec`. The garbage collector runs only at interpreter safe points, such as loop back edges. It traces VM registers and VM structures. Local `VmValue` variables on the Rust call stack remain untraced.

**Native functions and retries.** A native function is a Rust callback that takes `&mut Vm` and a `VmValue` slice. If a native call hits the memory limit or triggers a permission prompt, the VM rewinds the instruction. After garbage collection or the prompt finishes, the VM runs the native function again from the start.

**Custom codecs and schemas.** On `origin/encode-decode-traits`, a type has a custom codec if it or a nested field defines a manual `Encode` or `Decode` implementation. Purely derived types use a fast path: a native Rust walker driven by a static type schema.

## Finding 1: Native functions cannot call Sand code

**Impact:** Root cause. **Evidence:** Verified mechanism, likely conclusion.

### Design choice

A native Rust function runs to completion on the Rust stack. It returns a value, an error, or a scheduler signal to the VM. Three VM invariants prevent a native function from calling back into Sand:

- `Vm` exposes zero methods for calling a Sand function from Rust.
- The VM suspends tasks for async I/O by returning `FiberStop::Async` from the interpreter loop to the scheduler. An active Rust native frame on the call stack blocks that return path.
- The garbage collector runs at interpreter safe points and traces only VM state. A nested Sand call could trigger garbage collection and free heap objects still held in Rust local variables.

### Why this blocks a single codec

Standard interpreter codecs walk built-in types in native code and invoke script hooks only for custom types. Python's `json` encoder and Lua's `lua_call` follow this pattern.

Because a Rust walker in Sand cannot call a custom Sand `encode` method, `origin/encode-decode-traits` tested two continuation designs:

- **Heap-state continuations.** Design W stored walker state on the heap so the walker could yield to the VM at each custom field. Each custom field cost about 435 ns, including 270 ns to save and restore walker state. Heap state management also slowed standard decoding by 40% to 45%.
- **Generated Sand wrappers.** Design C synthesized Sand functions that called native helpers for primitive fields and returned to Sand for custom fields. Each custom field cost about 180 ns.

Separate callback prototypes measured a raw continuation hop at about 70 ns and a nested interpreter call at about 101 ns. Saving and restoring resumable walker state caused most of Design W's overhead.

If native code could call Sand functions directly, a native walker could keep its traversal state on the Rust stack. A single native walker could then handle both derived and custom types.

### Requirements for nested calls

Supporting nested Rust-to-Sand calls requires three VM changes:

- Forbidding task suspension inside a nested call, or saving nested native state across suspensions.
- Registering `VmValue` handles in active Rust frames as garbage-collector roots.
- Running garbage collection in place during nested calls instead of restarting the outer native call.

## Finding 2: Built-in traits read values; user traits use types

**Impact:** Root cause. **Evidence:** Verified mechanism, measured cost.

### Design choice

On `main`, the VM implements comparison, hashing, and encoding by walking `VmValue` trees directly in Rust. Each struct object stores its definition ID and field slice. Each list object stores its elements. Because heap values store their own structure, `vm/compare.rs` and `json.encode` inspect values directly without type descriptors.

User traits dispatch on static types instead. The type checker resolves each trait bound to a dictionary. Generic bytecode then calls trait methods through that dictionary.

`Decode` forms a third case on `main`. Decoding builds a new value from raw text, which requires static type metadata. The compiler therefore passes a `reflect` type descriptor to `json.decode`.

### Why user-defined codecs require type dispatch

Turning `Encode` and `Decode` into user-implementable traits forces both traits onto type-based dispatch for two reasons:

- Decoding runs before any target `VmValue` exists.
- Users can implement a trait for a specific type argument, such as `impl Describe for [int]`. Type erasure removes type arguments from heap values. An empty `[int]` and an empty `[str]` therefore share the same `VmValue` representation, and a value walk cannot tell them apart.

### Measured cost of dictionary dispatch

The bytecode interpreter executes every dictionary call. A dictionary-driven codec therefore runs at least a full interpreted call per encoded or decoded value.

We timed 2,000,000 iterations of three loop bodies on a release build of `main` at `625d1b9`. Minimum times across 5 rounds agreed within 0.3 ns across two runs:

| Loop body | Time per iteration, ns |
| --- | ---: |
| Empty loop | 5.0 |
| Direct Sand function call | 16.6 |
| Generic call with trait method call | 28.3 |

A generic trait call adds about 23 ns over an empty loop. On `origin/encode-decode-traits`, the native fast-path walker spends about 26 ns per value in total.

A second benchmark on `main` implemented `ToJson` and `FromJson` as regular user traits for a five-field struct. Timing 20,000 structs across 9 rounds in two runs showed a 2.3x to 3.6x slowdown relative to native walkers:

| Operation | Time per struct, ns |
| --- | ---: |
| `json.to_json` native walk | 200 to 216 |
| `ToJson` user trait in Sand | 458 to 540 |
| `json.decode` native from text | 305 to 317 |
| `json.parse` plus `FromJson` in Sand | 1,109 to 1,134 |

The branch recorded a matching 2x to 5x slowdown for its all-Sand prototype, Design D1.

### Run-time schema assembly under type erasure

The native fast path needs a complete schema for the target type. Closed types such as `[User]` have static schemas built at compile time. Generic functions learn their type arguments only at run time.

To use the fast path inside generic functions, the branch builds composite schemas at run time from templates in `codec/composed.rs`. Multiple engines share a compiled `Program` through `Arc`, so `Program` caches up to 1,024 run-time schemas behind a lock. Monomorphizing generic functions at compile time would produce static schemas for every call site. That approach would increase bytecode size and compile time.

## How Findings 1 and 2 force dual implementations

Requiring both `main` speed and custom codec implementations under Findings 1 and 2 forces the engine to maintain two complete codecs:

- Finding 1 prevents a native walker from calling custom Sand methods.
- Finding 2 makes an all-Sand codec 2 to 5 times slower than `main`.

On `origin/encode-decode-traits`, compiler-derived types run through a native schema walker. Types with custom implementations run through synthesized Sand methods instead. Both paths must emit identical output, field paths, error messages, and retry behavior.

These constraints require four pairs of compiler-derived methods in `check/codec.rs`:

| Derived method pair | Constraint |
| --- | --- |
| `Encode`, `Decode` | Custom codec implementations in Sand |
| `FastEncode`, `FastDecode` | Native traversal speed for derived types |
| `ComposedEncode`, `ComposedDecode` | Run-time schema assembly in generic functions |
| `CheckedEncode`, `CheckedDecode` | Stateful encoder and decoder safety from Sand |

Four files totaling 2,065 lines exist solely for the native fast path:

- `codec/fast.rs`
- `codec/composed.rs`
- `stdlib/json/text/walk.rs`
- `stdlib/json/text/read.rs`

Additional fast-path logic lives in shared files such as `codec/value_format.rs`. Another 1,072 lines of property tests in `tests/props/codec.rs` check that both implementations match. Even so, random inputs initially missed a bug on a rare branch.

## Finding 3: Traits with methods have checker restrictions

**Impact:** Partial cause. **Evidence:** Verified.

Moving `Encode` and `Decode` from marker traits to traits with methods exposes existing Sand code to four restrictions in the type checker.

On `main`, the checker branches on `TraitDef::has_dict` in 24 places. Marker traits work everywhere because they pass zero run-time arguments. Traits with methods require dictionaries and hit four limits on `main`:

| Checker limit on traits with methods | Source |
| --- | --- |
| Local functions cannot require them | Probe 1 |
| The `never` type lacks implementations for them | Probe 5 |
| Struct field defaults cannot call their methods | SPEC §4.2 |
| Native functions cannot accept general dictionaries | SPEC §16.3 |

Existing Sand programs use `Encode` and `Decode` bounds in local functions, empty `[never]` lists, and struct defaults. The branch therefore needed four prerequisite changesets to lift these checker limits before adding codec methods. Any other built-in marker trait that gains methods will hit the same limits.

## Finding 4: Memory-limit retries restart native calls

**Impact:** Partial cause. **Evidence:** Verified.

Because the VM restarts native functions from the start after garbage collection, every stateful codec step must act as an atomic transaction.

If a native call hits `max_memory`, the interpreter returns `FiberStop::CollectAndRetry`, collects garbage, and runs the instruction again. Because the collector ignores `VmValue` variables on the Rust stack, native calls must check memory limits before mutating state. Otherwise, they must undo state mutations before returning an error.

On `main`, `json.encode` and `json.decode` each complete in a single native call that returns a fresh result. A trait-based codec instead drives a stateful `Encoder` or `Decoder` through many small native calls from Sand. Every call mutates format state and must roll back cleanly on retry.

The branch added transactional checkpoint and rollback helpers across its encoders and decoders. Even with those helpers, `Config::retry_stress` caught four bugs where format state changed before a memory check failed.

## Finding 5: Native functions receive trait bounds only as data

**Impact:** Partial cause. **Evidence:** Verified mechanism, likely link to Finding 1.

Because native Rust code cannot call Sand closures, native functions can access trait bounds only through fixed Rust wrapper types.

On `main`, SPEC §16.3 provides three wrapper types for generic native parameters:

- `Param<N, B>`: provides a `VmValue` constrained by built-in marker traits `B`.
- `Reflect<N>`: provides a run-time type descriptor.
- `Num<N>`: reads `zero` and `one` from a `Num` dictionary. This wrapper works only because `check/traits.rs` forces all `Num` implementations to use native functions in a fixed slot order.

Because native code cannot invoke arbitrary Sand closures, the branch needed two new Rust parameter wrappers:

- `CodecDict<N, B>`: passes a codec dictionary to native code.
- `Encoded<N>`: passes a value pre-encoded into a host `Value`.

Host value extraction also had to change. On `main`, `Vm::to_value` walks raw `VmValue` trees without static types, bypassing any custom `Encode` method. The branch removed `Vm::to_value` and routed host value conversion through `Encode`.

## Finding 6: The checker synthesizes derived codecs in Rust

**Impact:** Minor. **Evidence:** Verified.

Sand lacks source-level macros, so the type checker constructs typed HIR in Rust for every derived codec method. On the branch, HIR synthesis in `check/codec.rs` takes 1,240 lines.

Because trait derivation in Sand is automatic, every eligible type receives derived `Encode` and `Decode` implementations. Generating method bodies eagerly caused a 20,000-struct program to take 111 seconds to type-check in a debug build. Generating method bodies lazily on first use reduced that time to 2.6 seconds. REPL sessions add further invalidation work because later inputs can redefine a type or `impl` and invalidate cached schemas.

Swift and Kotlin synthesize codecs inside the compiler in the same way. Compiler synthesis adds code volume without forcing a dual-codec architecture.

## Finding 7: Run-time type metadata must obey sandbox limits

**Impact:** Minor. **Evidence:** Verified.

Every type descriptor and schema that scales with script input must enforce memory limits, fuel charges, and bounded stack depth.

An earlier prototype branch, `derive-payloads-generics-num`, hit three sandbox failures in commit `f0a61bd` before adding metadata accounting:

- Copied run-time type descriptors allocated 2.65 GB under a 64 MB memory limit.
- Nested generic types in a 1 KB script consumed 6.7 GB inside the type checker.
- Dropping 20,000 nested struct-default descriptors overflowed the Rust call stack and aborted the process.

Native walkers must also manage traversal state on an explicit heap stack rather than the Rust call stack. On `main`, `json.decode` on 512 nested structs uses 808 KiB of Rust stack. Every Sand codec design pays this sandboxing cost.

## Finding 8: Fuel metering exposes the codec execution path

**Impact:** Tangential. **Evidence:** Verified.

Moving codec traversal from native Rust walkers to synthesized Sand bytecode increases deterministic fuel consumption.

The VM charges deterministic fuel for each bytecode instruction and call. In the all-Sand Design D1 prototype, encoding consumed 2 to 4 times as much fuel as the native walk on `main`. Changing how codecs execute therefore shifts observable fuel budgets and requires updating metering tests in `tests/metering.rs`.

## Evaluation of four suspected causes

Four subsystems on `main` appear at first glance to cause the difficulty. Each subsystem reflects Findings 1 through 5 rather than an independent blocker:

- **Special built-in traits.** Built-in traits bypass dictionaries and inspect heap values directly. Findings 2 and 3 cover both parts.
- **Native interop rules.** Native functions require specialized parameter wrappers because Rust code cannot invoke Sand closures. Findings 1 and 5 cover this boundary.
- **`Decode` as a marker trait.** `Decode` relies on `reflect` descriptors because decoding starts before any value exists to walk. Finding 2 covers this split.
- **Structure of `stdlib/json`.** The `json` module contains three value walkers and a `reflect` decoder. That layout follows from Finding 2, so rewriting `stdlib/json` alone leaves both root causes intact.

## Architectural choices for future attempts

**Evidence:** Likely.

Work on `origin/encode-decode-traits` required both `main` throughput and custom codec implementations while keeping Findings 1 and 2 fixed. Those constraints forced two full implementations and produced an 8,210-line change.

Any future attempt must choose among three architectural options up front:

- **Allow nested Rust-to-Sand calls.** Add stack rooting, in-place garbage collection, and nested suspension rules to the VM. A single native walker can then call custom Sand methods directly.
- **Run all codecs in Sand.** Synthesize Sand methods for every type and accept 2x to 5x slower encoding and decoding.
- **Generate the fast path from the Sand path.** Keep both a native walker and synthesized Sand methods. Generate the fast path from the synthesized methods so both paths stay aligned automatically.

## Scope and limits

- For `origin/encode-decode-traits`, we read the design records, module docs, and core codec definitions.
- We ran the `main` benchmarks on a single macOS machine and reported minimum times across rounds.
- Figures for Designs C, W, and D1 come from the recorded branch benchmarks.
- Comparisons with Python, Lua, Swift, and Kotlin come from public language documentation.

## Appendix: probes and benchmark setup

We ran all compiler probes and benchmarks on a release build of `main` at `625d1b9`:

```sand
// Probe 1: "local functions cannot require traits with methods;
// declare the function at the top level"
trait Show { fn show(self) -> str }
fn main() {
    fn twice<T: Show>(value: T) -> str { value.show() + value.show() }
}

// Probe 2: "`Encode` is a built-in trait; the compiler derives its
// impls, so it cannot be implemented"
struct Email { text: str }
impl Encode for Email {}

// Probe 3: "this call needs a description of `[T]` at run time, so
// `T` must be a `reflect` type parameter of this function"
import json
fn load<T: Decode>(text: str) -> Result<[T]> { json.decode(text) }

// Probe 4: "User does not implement `Decode` because `Email` declares
// `init`, which decoding does not call"
struct Email {
    text: str
    pub fn init(text: str) -> Result<Email> { Ok(Email(text: text)) }
}
struct User { name: str, email: Email }
let decoded_user: Result<User> = json.decode("{}")

// Probe 5: `count_encodable(empty_list)` type-checks;
// `count_showable(empty_list)` fails with "never does not implement `Show`".
trait Show { fn show(self) -> str }
impl Show for int { fn show(self) -> str { "i" } }
fn count_showable<T: Show>(items: [T]) -> int { items.len() }
fn count_encodable<T: Encode>(items: [T]) -> int { items.len() }
let empty_list: [never] = []
print(count_encodable(empty_list))
print(count_showable(empty_list))
```

The call-dispatch benchmark timed 2,000,000-iteration loops with `time.Stopwatch`. The direct-call function included an early `return` statement to prevent inlining.

The full-codec benchmark encoded and decoded 20,000 five-field `User` structs with scalar fields and a string list. User-defined `ToJson` and `FromJson` traits converted between Sand values and `json.Json` trees. Assertions verified that both paths matched `json.to_json` and `json.decode`.
