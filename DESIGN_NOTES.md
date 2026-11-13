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

## 5. Phase 2 log

### Shared prerequisites (on `codec-traits`, commits `204fb7e`, `e753d17`, `ec986e6`)

- Local functions take hidden dictionaries. A dictionary built from them is
  not hoisted to the outer function entry.
- Field default helpers take the struct's dictionaries. On the base,
  `json.decode` rejects a type whose computed default reads a dictionary
  (checked at module end, after default bodies are checked).
- `never` implements a trait if all methods of it and its supertraits take
  `self`; an unknown type in a bound defaults to `never` if `never`
  satisfies the bound.

### Contest decisions in force (CONTEST.md)

- Contestants: D1 (all Sand), D2 (D1 + fast path), W (agent A: native
  walker with continuations).
- A value leaves the VM only through `Encode`. Native functions take
  `Encoded<N>`. Remove public `Vm::to_value`.
- Lead asks: measure fuel for every case; keep full-type container mismatch
  messages unless costly (show before/after if changed); measure or avoid
  the text copy; closed instances of generic types in D2; keep these notes.

### Facts found while planning D1

- JSON text mismatch messages already name kinds, not full types:
  `expected array`, `expected object`, `expected array of length 2`,
  `expected int`, `expected Status` (enum name). Only the `Value` decoder
  (a test hook on `value-decode`) names full container types
  (`expected [int], found str`).
- Named variant payloads: an omitted field is `None` for `Option`, else
  `missing field`; payloads have no defaults.
- JSON forms: `Some(x)` writes `x`; `Json` variants write their payload;
  `Range` writes `{"start","end"}` and `"inclusive": true` only when
  inclusive; a payload variant writes `{"Name": [..]}` or `{"Name": {..}}`.
- The checker's `TypeDefId` is the runtime struct def id, and variant ids
  are the checker's `variants` index, so synthesized HIR can pass them as
  int constants to natives, which read names from `vm.prog`.

### D1 build log

- Shape: prelude traits `Encode { encode(self, e: Encoder) }` and
  `Decode { decode(d: Decoder) -> Result<Self>; decode_missing() -> Self? }`.
  `Encoder`/`Decoder` are prelude native objects over a Rust `EncodeFormat` /
  `DecodeFormat` (`src/codec/`). Formats: JSON text and `json.Json`
  (`stdlib/json/text.rs`), host `Value` (`codec/value_format.rs`).
- Synthesized impls: `check/codec.rs` builds HIR (`FnDeclRef::Derived`).
  Impls are registered eagerly (cheap) and get their methods on first use
  (`ensure_derived_methods`), so a program pays only for the types it codes.
  Measured: a 20,000-struct chain compiled in 111 s in a debug build before
  two fixes (lazy methods, and a verifier index of impls by head); 2.6 s after.
  Release: 0.27 s (`value-decode`: 0.14 s).
- Path tracking lives in the `Decoder` (frames); errors of formats get the
  path at the failing call; `d.finish(r)` adds the path to errors that
  hand-written code returns (`Decoder.locate`).
- Retry rule (`retry_stress`): every format op checks memory before it changes
  state. Bugs found by the spec suite under retry stress and fixed: `end()` of
  the tree and value encoders popped before the memory check; the tree
  encoder opened a variant's two containers in two checks; the value decoder
  consumed its value before the memory check of a long text.
- General changes needed by D1 (own commits): trait proof depth 32 -> 128 in
  the checker and the verifier (a nested `[[...]]` type of 64 levels needs 64
  nested `List` impls; measured: 250 levels fit a 512 KiB stack); the
  verifier indexes impls by trait and head (was a scan of all impls).
- Container mismatch messages: JSON text messages already named kinds
  (`expected array`, `expected object`) and are unchanged. Only the `Value`
  decoder (a test hook on `value-decode`) named full container types; it now
  names the kind: before `expected [int], found str`, after
  `expected list, found str`. Map keys without a JSON key form: before
  `cannot decode map keys of type (int, int)`, after
  `cannot decode map keys of type tuple`. Enum names lose their type
  arguments: before `unknown variant `Other` of Shape<[P]>`, after `of Shape`.
- Text copy: `TextDecoder::new` copies the text once (counted by `Budget`).
  To measure in the benches (small_decode).

### Fuel of each kind (tests/metering.rs, `a_value_of_each_kind...`)

value-decode -> D1, units for encode / to_json / Encoded (was to_value):

| value                    | encode    | to_json   | to_value/Encoded |
| ------------------------ | --------- | --------- | ---------------- |
| int                      | 1 -> 3    | 1 -> 4    | 1 -> 3           |
| list of 3 ints           | 4 -> 11   | 4 -> 15   | 4 -> 11          |
| map, 2 str keys          | 4 -> 15   | 4 -> 19   | 4 -> 15          |
| struct (3 fields)        | 6 -> 12   | 6 -> 18   | 6 -> 13          |
| list of 2 structs        | 12 -> 26  | 12 -> 38  | 12 -> 27         |
| Json array and object    | 13 -> 12  | 1 -> 19   | 13 -> 16         |
| 256 nested variants      | 257 -> 551| 257 -> 1028 | 513 -> 1027 (512 variants) |

Encode costs 2 to 4 times the fuel of the native walk: one unit for each Sand
call plus one for each format op. `to_json` no longer reuses `Json` values.

### D1 measurements (tag `codec-d1`)

Method: release bench binaries of main `625d1b9`, value-decode `fe58101`, and
D1, each built with `-C llvm-args=-align-loops=64` in its own target
directory and copied aside; three alternating rounds of
`cargo bench --bench codec` (5000 users, 3 runs of 5 repetitions); the
minimum of each case. Baselines use proxies for the new cases: `Email` is a
plain derived struct, and `load` is `fn load<reflect T: Decode>`.

| case (ns per item)   | main  | value-decode | D1    | D1 / vd |
| -------------------- | ----- | ------------ | ----- | ------- |
| encode               | 284   | 288          | 1530  | 5.3     |
| encode_indent        | 379   | 373          | 1595  | 4.3     |
| decode               | 662   | 646          | 2611  | 4.0     |
| to_json              | 498   | 480          | 3330  | 6.9     |
| decode_json          | 859   | 845          | 888   | 1.05    |
| encode_json          | 381   | 386          | 883   | 2.3     |
| to_value             | 358   | 369          | 1523  | 4.1     |
| small_encode         | 116   | 127          | 417   | 3.3     |
| small_to_json        | 260   | 263          | 1026  | 3.9     |
| small_decode         | 234   | 231          | 538   | 2.3     |
| small_to_value       | 97    | 94           | 444   | 4.7     |
| deep_encode          | 3823  | 4198         | 9398  | 2.2     |
| deep_decode          | 13820 | 14892        | 14737 | 0.99    |
| encode_custom        | 80    | 80           | 394   | 4.9     |
| decode_custom        | 198   | 212          | 725   | 3.4     |
| decode_generic       | 655   | 645          | 2598  | 4.0     |
| decode_page          | 636   | 645          | 2588  | 4.0     |
| decode_ints          | 106   | 113          | 657   | 5.8     |
| decode_ints_generic  | 104   | 113          | 658   | 5.8     |
| small_decode_default | 248   | 253          | 286   | 1.1     |

Fuel per item (`BENCH_FUEL=1`; main and value-decode are equal):

| case                 | main, vd | D1    |
| -------------------- | -------- | ----- |
| encode, to_value     | 22.8     | 55.8, 56.5 |
| to_json              | 22.8     | 79.2  |
| decode               | 46.0     | 135.9 |
| decode_json          | 43.6     | 43.6  |
| encode_json          | 45.1     | 38.7  |
| small_encode         | 6.0      | 11.0  |
| small_to_json        | 6.0      | 16.0  |
| small_decode         | 16.0     | 29.0  |
| small_to_value       | 6.0      | 12.0  |
| deep_encode          | 805      | 412   |
| deep_decode          | 457      | 459   |
| encode_custom        | 8.0      | 18.0  |
| decode_custom        | 14.5     | 41.0  |
| decode_generic, page | 46.0     | 135.9 |
| decode_ints          | 10.0     | 40.0  |
| small_decode_default | 8.0      | 14.0  |

Native stack at 512 nesting levels (smallest thread stack that runs the
case, binary search in 8 KiB steps; 16 KiB is the floor of an empty run):

| case                     | main | value-decode | D1  |
| ------------------------ | ---- | ------------ | --- |
| `json.parse` of arrays   | 152  | 152          | 120 |
| `json.decode` of structs | 808  | 824          | 16  |
| `json.encode` of structs | 16   | 16           | 16  |

Profile of `decode_ints` on D1 (`sample`): the interpreter loop 21%, native
call dispatch 12%, the two `Decoder` natives 16%, number parsing 15%,
`Ok` wrapping 5%. The cost is the Sand loop and two native calls for each
element, not the format.
