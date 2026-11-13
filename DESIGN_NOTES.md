# Design notes: `Encode` and `Decode` as regular traits

This file is the running log for the redesign. It records facts, experiments,
and open questions. `DESIGN.md` is the memo that uses them.

Each claim has a tag: **measured** (with the command and the output) or
**reasoned**. The probe programs `p1.sand` to `p5.sand` are in
`experiments/codec-design/probes/`.

Machine: macOS, 16 cores, another agent builds at the same time. The
baseline binary is `value-decode` `fe58101`, copied aside as
`scratchpad/bin/sand-vd`. All Sand-level experiments run on this one
binary, so the ratios compare code paths in the same binary.

## 1. Facts about the current system

### Traits and dictionaries

- F1. Generics are type-erased. A bound on a trait with methods adds a hidden
  dictionary parameter. A dictionary is a tuple of method closures, then the
  supertrait dictionaries, then the `impl` bound dictionaries
  (`check/traits.rs` module doc). **reasoned** (read the code)
- F2. The compiler builds a dictionary that does not depend on the function's
  own dictionaries once, as a constant (`compile/mod.rs` `const_dict`).
  Otherwise it builds the tuple at run time with `NewTuple`, which allocates
  at each evaluation. **reasoned** (read the code)
- F3. A trait method call through a dictionary is a tuple projection and a
  `CallR` of a closure. **reasoned** (read the code)
- F4. Local functions cannot require traits with methods. **measured**:
  `sand-vd run p4.sand` prints `error: local functions cannot require traits
  with methods; declare the function at the top level`. If `Encode` gets
  methods, each local `fn f<T: Encode>` breaks. The same limit applies to
  field defaults (SPEC §4.2).
- F5. `never` implements only traits without methods (SPEC §5.1). Today
  `json.encode([])` and `json.encode(e)` with `e: [never]` both print `[]`.
  **measured** (`p5.sand`). If `Encode` gets methods, `[never]` stops being
  `Encode` unless the design gives `never` an implementation.
- F6. `Num: Ord + Encode` (`prelude.sand:321`). If `Encode` gets methods, the
  `Num` dictionary contains an `Encode` dictionary, and the native `Num<N>`
  evidence layout changes. **reasoned**

### Derivation

- F7. `Eq`, `Ord`, `Hash`, `Encode`, and `Decode` are built-in traits without
  methods. `derive.rs` computes, for each type definition, the type
  parameters that must implement each trait, by a greatest fixed point over
  the type graph. No `ImplDef` exists for a derived trait. **reasoned**
- F8. User code cannot implement a built-in trait. **measured**: `impl Decode
  for Email {}` gives ``error: `Decode` is a built-in trait; the compiler
  derives its impls, so it cannot be implemented`` (`p2.sand`).
- F9. A struct with `init` never implements `Decode`, and neither does any type
  that contains it. **measured**: ``error: User does not implement `Decode`
  because `Email` declares `init`, which decoding does not call`` (`p3.sand`).
- F10. `Decode: Encode` today. The user decided that `Decode` does not
  require `Encode`.

### `reflect` and the decoders

- F11. A generic caller of `json.decode` must declare `reflect T`.
  **measured**: `fn load<T: Decode>(text: str) -> Result<[T]> {
  json.decode(text) }` gives `error: this call needs a description of [T] at
  run time, so T must be a reflect type parameter of this function`
  (`p1.sand`).
- F12. `compile/reflect.rs` builds `TypeDesc` DAGs as constants, or as
  `DescTemplate`s that the VM instantiates with the descriptors of `reflect`
  type parameters. It also emits bytecode helpers: one per struct with
  computed defaults (closures that the decoder returns as thunks), one set
  fill, and one map fill. **reasoned**
- F13. `json.decode` is a Sand wrapper. The native `decode_raw` returns
  `Decoded.Done`, `Decoded.Pending(value, fills)`, or `Decoded.Failed`. The
  wrapper calls each fill closure to compute omitted computed defaults
  (`json.sand:143`). **reasoned**
- F14. Two native decoders exist: JSON text (`json.rs` `Decoder`) and `Value`
  (`json/decode.rs` `ValueDecoder`). They share `DecodeTarget` (paths,
  defaults, thunks, set and map fills). `decode_value` is a test hook only
  (`#[cfg_attr(not(test), expect(dead_code))]`). **reasoned**
- F15. Three native encoders exist: JSON text (`JsonTextEncoder`), `Json`
  values (`ToJsonConverter`), and `Value` (`ValueCopier` in `value.rs`). All
  three walk VM values without type information. A VM value carries its
  struct definition or variant id, so the walk needs no `TypeDesc`.
  **reasoned**
- F16. `Vm::to_value` takes a `VmValue` with no static type. Its only callers
  in the crate are tests and benches. Native functions get a value through
  `Param<0>` without an `Encode` bound. **measured** (`grep -rn 'to_value('`).

### Native types

- F17. A native module can implement a Sand trait with native methods, as in
  `impl Close for Conn { native fn close(self) }` (`tests/api.rs`,
  `host_modules_can_define_resources`). **reasoned**
- F18. `NativeObject` cannot hold a VM handle, because the GC does not trace
  native objects (SPEC §16.3). So a native `Encoder` that builds a `Json`
  VM value must keep its partial tree in Rust, not in VM handles.
  **reasoned**

### VM

- F19. The interpreter keeps Sand frames in a `Vec` (`fiber.frames`), so a
  Sand call does not use native stack. `max_call_depth` defaults to 10,000
  (`engine.rs:63`). **reasoned**
- F20. Single-payload variants with an inline payload, such as `Ok(1)` or
  `Ok(obj)`, do not allocate (`VmValue::VInt`, `VmValue::VObj`). `Ok(Some(obj))`
  allocates. **reasoned**
- F21. Native code cannot call Sand code. The callbacks research
  (`origin/claude/sandlang-native-callbacks-research-i6bf14`) measured a P2
  continuation callback at 70 ns and a Sand closure call at 27 ns, on a
  different Linux machine. It recommends P2 continuations and rejects nested
  calls on the Rust stack. **reasoned** (read the report)

## 2. Experiments

### E1: a Sand codec over an intermediate `Json` tree (the aeson design)

File: `experiments/codec-design/e1_tree.sand`. It defines user traits
`FromJson` and `ToJson`, with `impl` blocks for `int`, `float`, `bool`,
`str`, `[T]`, `[str: V]`, `T?`, and the four benchmark types of
`benches/codec.rs`. Decoding first calls `json.parse`. The program checks
that both decoders give the same `Doc`, and that both encoders give the
same text.

Command: `sand-vd run e1_tree.sand -- 5000`, two runs alternated with E2.
**measured**, nanoseconds per user:

| operation                       | run 1 | run 2 |
| ------------------------------- | ----: | ----: |
| native `json.decode`            |   669 |   672 |
| `json.parse` only               |   833 |   841 |
| `json.parse` + Sand `FromJson`  |  2075 |  2116 |
| native `json.encode`            |   304 |   301 |
| native `json.to_json`           |   506 |   512 |
| Sand `ToJson` (tree only)       |  1058 |  1073 |
| Sand `ToJson` + `json.encode`   |  1361 |  1400 |

Results:

- The native decoder is faster than `json.parse` alone, because it builds
  no tree. The tree design decodes 3.1 times slower than native, and
  encodes 4.5 times slower.

### E2: a Sand-driven codec over a native streaming reader and writer

Files: `crates/sandlang/benches/codec_design.rs` (a prototype `codec` host
module with `Reader`, `Writer`, and `Names` objects) and
`experiments/codec-design/e2_stream.sand` (the Sand code that a compiler would
synthesize, written by hand). The reader tracks the error path. The
reader skips JSON escapes, which the benchmark text does not have. The
program checks that both decoders give the same `Doc`, and that both
encoders give the same text.

Command: `BENCH_FILE=e2_stream.sand codec_design --bench`, two runs
alternated with E1. **measured**, nanoseconds per user:

| operation                    | run 1 | run 2 | first run |
| ---------------------------- | ----: | ----: | --------: |
| native `json.decode`         |   643 |   648 |       657 |
| Sand-driven decode           |  1881 |  1897 |      1887 |
| native `json.encode`         |   291 |   288 |       295 |
| Sand-driven encode           |  1152 |  1184 |      1172 |

Results:

- Sand-driven decode is 2.9 times slower than native. Sand-driven encode is
  4.0 times slower.
- The error path from the reader's path stack is correct without cost on
  the success path. **measured**: a `"score"` of `"x"` in user 3 gives
  `Err(Error(message: "$.users[3].score: expected float", kind: "parse"))`.
  When a nested decode fails, `?` returns at once, so no code pops the path.
  The path at the top is the path of the failure.

### E3: the cost of one call

File: `experiments/codec-design/e3_calls.sand`, 1,000,000 calls of each kind.
Command: `BENCH_FILE=e3_calls.sand codec_design`, two runs. **measured**,
nanoseconds per iteration (both runs agree within 1 ns):

| loop body                                     | ns |
| --------------------------------------------- | -: |
| empty loop                                    |  2 |
| direct call of a small function               |  3 |
| generic call, then trait method through dict  | 20 |
| `result(i).unwrap()`                          | 10 |
| native method on an object (`w.null()`)       | 10 |
| `next_item()` + `int().unwrap()` (Result)     | 34 |
| `next_item_raw()` + `int_raw()` (no Result)   | 23 |

The last two rows subtract the 9 ns that building the input costs.

Results:

- A native call costs about 10 ns. A dictionary call costs about 10 ns. The
  compiler inlines a small direct call.
- The native walker spends about 26 ns on each value (657 ns for about 25
  values in a user). A Sand-driven decoder makes at least two calls for each
  value. So the 2.9 times ratio of E2 comes from call cost, not from the
  shape of the API. Returning plain values instead of `Result` saves about
  5 ns for each call, which is not enough to close the gap.

### Estimate: a hybrid with a native fast path

**reasoned** from the E2 call counts. A user in E2 makes about 52 native
calls and 20 Sand calls. About 27 of the 72 calls are at the top level of
`User`; the rest are in `address`, `status`, `tags`, `logins`, and `meta`. If
a hand-written field type stops the fast path for `User` only, its five
nested fields still take the fast path. The estimate for that mixed case is
657 + 37% of 1230 + 5 calls of 10 ns, which is about 1170 ns, or 1.8 times
native. A native-driven walker that calls Sand through P2 continuations
would take about 657 + 70 ns, or 1.1 times native.

## 3. Other languages

**reasoned** (from knowledge of each library; not re-checked against sources
in this phase).

| System                  | User trait shape                                   | Synthesis                       | Dispatch at run time                     |
| ----------------------- | -------------------------------------------------- | ------------------------------- | ---------------------------------------- |
| Rust serde              | `serialize<S: Serializer>`, `deserialize<D>` + visitor | proc macro (compiler plugin)  | monomorphization; zero-cost             |
| Swift `Codable`         | `encode(to: Encoder)`, `init(from: Decoder)`       | compiler, on declared conformance | witness tables and existential `Decoder` |
| Kotlin serialization    | `KSerializer<T>` with `SerialDescriptor`; `decodeElementIndex` loop | compiler plugin, on `@Serializable` | serializer objects; generic serializers take type-argument serializers |
| Go `encoding/json`      | `MarshalJSON() []byte`, `UnmarshalJSON([]byte)`    | none                            | reflection walk that calls interface methods |
| Haskell aeson           | `toJSON :: a -> Value`, `parseJSON :: Value -> Parser a`, `toEncoding` | GHC.Generics or Template Haskell | type-class dictionaries |

Facts that matter for Sand:

- Swift, Kotlin, and aeson pass the equivalent of dictionaries at run time, as
  Sand does. serde depends on monomorphization for speed. Go and today's Sand
  depend on run-time type descriptions.
- Swift and Kotlin drive a format-neutral `Decoder` from synthesized code.
  The decoder tracks the coding path (`codingPath` in Swift, `$.a.b` paths in
  kotlinx JSON errors).
- aeson decodes through an intermediate `Value`. It added `toEncoding`
  because encoding through `Value` was slow, which E1 also shows.
- Go's hooks are format-specific (`MarshalJSON`). Each format needs new user
  code, which acceptance scenario 9 forbids.
- serde's `#[serde(try_from = "Repr")]` and Kotlin's surrogate serializers
  are the common way to validate: decode a derived representation, then
  convert. Sand can use the same idiom with a private derived struct.
- Every other system makes derivation opt-in. Sand derives implicitly.

## 4. Open questions

- Q1. Error messages for containers: today the message names the full type,
  such as `expected [int], found str`. A Sand-driven `List` decoder does not
  know its element type. Acceptance scenario 1 requires the same paths and
  kinds, not the same messages. Proposal: name the kind (`expected list`),
  and use the same text on the fast path, so that both paths agree.
- Q2. Field defaults: today computed defaults run after the whole decode
  succeeds. A Sand-driven decoder runs them when each struct finishes. A
  default with a side effect can then run for a decode that fails later.
  Swift and Kotlin behave the same way. Proposal: accept, and document.
- Q3. Should `Vm::to_value` (type-free) apply hand-written `Encode`? It cannot
  without a dictionary or a native-to-Sand call. Proposal: native functions
  that need the `Encode` form declare `T: Encode` in Sand source and receive
  an encoded `Value` (section 6 of `DESIGN.md`).
- Q4. A JSON-text `Decoder` must not hold a VM string handle (F18). It
  copies the text, or the VM shares an immutable buffer. Cost of the copy:
  to measure in phase 2.
- Q5. `retry_stress`: each `Decoder` and `Encoder` native must check memory
  before it moves its cursor or writes output. To test in phase 2.
