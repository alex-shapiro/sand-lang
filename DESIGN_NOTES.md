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

### D2: native fast path (commit after tag `codec-d1`)

- Shape: a closed list, set, map, tuple, struct, or enum (not `Option`)
  whose parts all have synthesized or prelude impls, and, for `Decode`, no
  field default, gets one more impl per type (`ImplDef.fast`). Its method
  calls `Encoder.write_fast` or `Decoder.read_fast` with a compile-time
  `Schema` (`codec/fast.rs`, table `Program.schemas`). Trait resolution
  prefers it for closed types, so closed instances of generic types
  (`Page<User>`, `[int]`) and generic callers (`load<T: Decode>` with
  `T = Doc`) get it. Types deeper than 64 levels, such as polymorphic
  recursion (`P<[T]>` in `P<T>`), keep the synthesized impls.
- The walkers call the same format operations in the same order as the
  synthesized code, with an explicit stack. Formats implement `mark` and
  `rollback`, so a walk that fails a memory check runs again after garbage
  collection. The value decoder's mark copies the value that the walk reads.
- Verifier: fast impls are not in the impl index; tuple impls that a fast
  type contains are registered so that the verifier proves tuple bounds.
- Differential property test `native_and_synthesized_codecs_agree`: random
  types (now with payload enums and `time.PlainDate`) and JSON inputs; JSON
  text, `Json`, host `Value`, decoded values, errors, also under
  `retry_stress`. 3000 cases pass. Failure traces differ: the fast path runs
  fewer Sand functions, and its method shows as `encode (<prelude>:1)`.

Measurements (same method as D1, a new set of 3 alternating rounds of all
four builds):

| case (ns per item)   | main  | value-decode | D1    | D2    | D1 / D2 |
| -------------------- | ----- | ------------ | ----- | ----- | ------- |
| encode               | 290   | 285          | 1522  | 618   | 2.5     |
| encode_indent        | 389   | 395          | 1599  | 713   | 2.2     |
| decode               | 651   | 650          | 2613  | 992   | 2.6     |
| to_json              | 495   | 491          | 3381  | 2307  | 1.5     |
| decode_json          | 861   | 846          | 900   | 967   | 0.9     |
| encode_json          | 387   | 389          | 859   | 881   | 1.0     |
| to_value             | 372   | 367          | 1530  | 668   | 2.3     |
| small_encode         | 127   | 126          | 414   | 301   | 1.4     |
| small_to_json        | 262   | 260          | 1006  | 867   | 1.2     |
| small_decode         | 235   | 244          | 532   | 374   | 1.4     |
| small_to_value       | 99    | 95           | 436   | 328   | 1.3     |
| deep_encode          | 4044  | 4187         | 9291  | 9374  | 1.0     |
| deep_decode          | 15082 | 14289        | 14565 | 14841 | 1.0     |
| encode_custom        | 80    | 82           | 406   | 377   | 1.1     |
| decode_custom        | 209   | 214          | 724   | 650   | 1.1     |
| decode_generic       | 647   | 650          | 2619  | 999   | 2.6     |
| decode_page          | 653   | 638          | 2586  | 1003  | 2.6     |
| decode_ints          | 114   | 114          | 639   | 155   | 4.1     |
| decode_ints_generic  | 114   | 114          | 656   | 161   | 4.1     |
| small_decode_default | 252   | 256          | 287   | 300   | 1.0     |

Decision rule of the memo (keep the fast path only with at least 1.5x on
decode and encode): decode 2.6x, encode 2.5x. The fast path stays.

Fuel per item, D2 (D1): encode 48.8 (55.8), to_json 72.2 (79.2), decode
108.6 (135.9), decode_ints 30.0 (40.0), small_decode 26.0 (29.0). Native
stack at 512 levels, D2: parse 120 KiB, decode 16 KiB, encode 16 KiB.

Compile time of 20,000 chained structs (`struct S{i} { v: int, next:
S{i+1}? }`, decode and encode `S0`, release, wall time of `sand run`):
value-decode rejects it ("type too large to describe at runtime"), D1
0.38 s, D2 0.08 s.

`to_json` stays slow in both: the tree encoder builds a Rust `Json` (an
`IndexMap` with SipHash and a `String` for each key), copies it, and then
converts it to a Sand value. Taking it instead of copying it saved 13% but
broke `retry_stress` (the conversion can fail a memory check and run
again), so it is not in. A faster form records the operations and builds
the Sand value in one staged pass, as `json.parse` does.

## 6. D2 performance pass

Goal: on every benchmark case without a hand-written part, D2 runs within
10% of `value-decode` (`fe58101`) and uses the same fuel, with a flat native
stack. Tools: release builds with `-C llvm-args=-align-loops=64` in separate
target directories; `sample` on a `profiling` build (release plus debug
lines); the aggregator `scratchpad/ptools/prof.py` (inclusive and self
samples for each function).

### P1: profiles of D2 before the pass (commit `664a7d3`)

Decode of `Doc` (5000 users), share of the engine thread:

| part                                   | D2  | value-decode |
| -------------------------------------- | --- | ------------ |
| walk, inclusive                        | 85% | 74%          |
| walker or `Decoder::decode`, self      | 17% | 22%          |
| `Parser::string`, self                 | 10% | 13%          |
| `TextDecoder::next` + `next_key`, self | 14% | (inlined)    |
| `codec::next_field`, self              | 5%  | (inlined)    |
| malloc and free                        | 12% | 10%          |
| garbage collection                     | 12% | 18%          |

In ns per user (share times total time), the walker self time is 168 against
143, and `Parser::string` 100 against 83. The explicit stack costs little.
The extra time is in the format operations: 360 ns per user in `next`,
`next_key`, `next_field`, and the walker, against 143. Each operation builds
a `Parser`, skips white space, charges fuel, updates the path frames, and
goes through a `dyn DecodeFormat` call. Map keys allocate two `String`s
(`next_entry` returns an owned key, and `TextDecoder` keeps a copy).

Encode of `Doc`: `TextEncoder::field` takes 40% of the walk. It clones the
field name (a `malloc` and a `free` for each field). `begin_member`,
`scalar`, and `room` add 33%.

Small cases: `json.encode(1)` takes 102 ns on D2 and 35 ns on
value-decode. The difference is the Sand plumbing of the trait design: the
`json.encode` wrapper, a dictionary call, three native calls
(`text_encoder`, the write, `encoded_text`), an `Encoder` object with two
boxes, and its sweep (garbage collection is 16% of this loop). The
`Budget` of each new encoder counts 4096 bytes on the heap, so small encodes
make the collector run often.

Fuel: D2 charges a unit in the walker for each value, and the format
charges one more unit for each operation (`TextDecoder::parse`,
`TextEncoder::scalar`, `TreeEncoder::prepare`). value-decode charges one
unit for each JSON value plus the input bytes, and no unit for map keys.

Conclusion: the cause is the cost of each format operation, not the explicit
stack or `mark` and `rollback` (those run once for each walk). The plan:
monomorphize the walker over the format, charge fuel once for each value,
keep the path in the walker's own stack and build the path frames only on an
error, and remove the allocations in the format operations.

### C1: walker compiled for each format, fuel once for each value

Changes:

- `EncodeFormat::write_fast` and `DecodeFormat::read_fast` are default
  methods that call the walkers in `codec/fast.rs`. Rust compiles a default
  method for each format that implements the trait, so the walker calls the
  format's operations statically, and can inline them. No format writes code
  for this, and a host format gets the fast path as well.
- `mark` and `rollback` (a boxed state) became `save` and `restore`, which
  keep the state in the format. No allocation for each walk.
- The decode walker keeps the path in its own stack, and writes the path
  frames of the `Decoder` only when it fails. The map key texts that a path
  needs go to one reused buffer.
- `next_entry` returns a borrowed key. `TextDecoder` keeps a pending key as
  a span of the input, not a `String`.
- The encode walker copies the fields of a struct, tuple, or variant to its
  own stack once, and copies a `str` into one reused buffer.
- `TextEncoder::field` no longer clones the field name.
- `encoded_text` stops counting the encoder's buffer when it takes the text.
  Before, the heap counted the text twice until the encoder was swept, and
  the collector ran more often.
- Fuel: the walkers charge a unit for each value and no unit for a map key,
  as value-decode does. Formats charge only for bytes (`TextDecoder` reports
  its input through `take_input_fuel`, a unit for each 8 bytes). A Sand call
  of a format operation that writes or reads a value charges a unit;
  `field`, `end`, `next_item`, and the other structural operations charge
  only for the input that they read.
- `Json.encode` and `Encoder::write_value` restore the format if a memory
  check fails. Before, a run again after a garbage collection wrote the
  value twice.

Timing build (`ptools/inst_build.sh`): a timer around the `write_fast`
native shows that the walk is 99% of `json.encode(doc)` (509 of 512 ns per
user before the last two changes). A format whose operations do nothing
gives the cost of the walker alone: 120 ns per user. The JSON text
operations cost the rest. `sample` shows 18% of samples in
`__thread_selfusage`, but a timer around the 200 `cpu_time` calls of 100
encodes measured 82 µs in total. The sampler distorts the weight of the
time functions; I ignore them.

Measurements (3 alternating rounds, minimum, ns per item):

| case                 | value-decode | D2 before | C1   |
| -------------------- | ------------ | --------- | ---- |
| encode               | 294          | 623       | 424  |
| encode_indent        | 390          | 731       | 515  |
| decode               | 653          | 1009      | 785  |
| to_json              | 493          | 2006      | 2350 |
| decode_json          | 856          | 966       | 907  |
| encode_json          | 387          | 888       | 411  |
| to_value             | 365          | 663       | 619  |
| small_encode         | 123          | 303       | 291  |
| small_to_json        | 261          | 766       | 936  |
| small_decode         | 239          | 374       | 352  |
| small_to_value       | 99           | 332       | 358  |
| deep_encode          | 4166         | 9650      | 5109 |
| deep_decode          | 15176        | 14840     | 15074 |
| encode_custom        | 83           | 376       | 341  |
| decode_custom        | 211          | 657       | 656  |
| decode_generic       | 653          | 1010      | 795  |
| decode_page          | 654          | 1007      | 790  |
| decode_ints          | 114          | 162       | 160  |
| small_decode_default | 265          | 301       | 294  |

Fuel per item (value-decode, D2 before, C1): encode 22.8, 48.8, 22.8;
decode 47.1, 108.5, 45.7; to_value 22.8, 49.5, 22.8; decode_ints 10, 30, 10;
encode_json 45.1, 38.7, 22.7; small_decode 16, 26, 16; small_encode 6, 12,
8; encode_custom 8, 18, 15; decode_custom 15, 40.8, 26.8;
small_decode_default 8, 14, 10. `to_json` (45.4) still converts a Rust
`Json`, which charges again.

The differential test passed 5000 cases (with `retry_stress`).

### C2: `to_json` records tokens and builds the `Json` value in one staged pass

Profile of `to_json` after C1: the old `TreeEncoder` built a Rust `Json`
(an `IndexMap` and a `String` for each key), copied it, and converted it
with `json_into_vm`, which charged a unit for each node again.

Change: `TreeEncoder` records the value as compact tokens (`Token`: scalars,
`Str` with its text in one buffer, `Field` and `Variant` names by id,
`Arr`, `Obj`, `Map`, `End`). `encoded_json` builds the Sand value from the
tokens in one `Heap::staged` pass with the `Builder` of `json.parse`, which
shares key strings. The build caches the key `str` and hash of each field
and variant name, and checks memory once for each 32 tokens. The tokens
stay until the build succeeds, so the call can run again after a garbage
collection (`retry_stress`). The build charges no fuel for each node, so
`to_json` charges what `encode` charges.

Measured (3 alternating rounds, ns per item; value-decode, C1, C2):
`to_json` 491, 2350, 785; `small_to_json` 263, 936, 671. Fuel per item:
`to_json` 22.8, 45.4, 22.8; `small_to_json` 6, 12, 8.

Profile of C2 (`to_json`, share of the engine thread): the build 50%, the
recording walk 26%, garbage collection 20%. Normalized to the work of
`Builder::map` (the same maps in both), the build alone costs as much as
the whole conversion of value-decode (410 against 390 ns per user). The
rest is the recording walk (about 210 ns per user) and more garbage
collection: the build allocates a new `str` for each text value, and
value-decode shares the `str` objects of the source value.

Tradeoff that I could not resolve: one pass, as value-decode has, needs the
`Encoder` to hold Sand values between the native calls of a `to_json` (the
walk, then `encoded_json`). The collector does not trace values in native
objects, and a hand-written `Encode` can run Sand code, and so a
collection, between those calls. So the encoder holds tokens, not Sand
values, and the copy of each text stays. A native object that the
collector traces (with a write barrier for the generational collector)
would remove both costs; that is a VM feature beyond this pass.

### C3: `Encoded<N>` shares the encoded value

A native parameter of type `Encoded<N>` cloned the whole `Value` out of the
`Encoder`, so `to_value` built the value twice. `ValueEncoder` now keeps
the finished value in an `Arc`, and `Encoded<N>` shares it.
`Encoded::value` costs nothing; `Encoded::into_value` copies the value only
while the engine still holds the `Encoder`.

Measured (3 alternating rounds, ns per item; value-decode, C2, C3):
`to_value` 368, 616, 357; `small_to_value` 95, 361, 298.

### C4: list items in their own buffer; scalar parts read in the inner loop

Profile of `decode_ints` (C3): number parsing 54% (value-decode does the
same work), the walker 25%, `next_item` 11%, and the copy of the list from
the value stack (`drain(..).collect()`) 6%. A list frame now owns its items
(with a memory check when the buffer grows), so finishing a list moves the
buffer into the heap object. A part with a scalar schema is read in the
inner loop of the walk, without the round trip through the code that opens
containers.

Measured, 6 alternating runs of `BENCH_RUNS=5 BENCH_REPS=20` (min, mean):
`decode_ints` C3 147, 152; C4 140, 149. `decode_ints_generic` C3 148, 156;
C4 145, 148. value-decode: 109 to 114. The benchmark varies about ±5%
between runs, so 3 rounds of the default settings cannot show changes of
this size.

`sample` keeps one address for each function, so it cannot show which
lines of the inlined walker cost the time; `atos -i` on those addresses
gives only the call site.

### C5: synthesized code calls the operation of a scalar or native field directly

The synthesized `Encode` and `Decode` of a type with a hand-written part
called the trait method of each field: a Sand call to the prelude impl of
`int`, which calls `Encoder.write_int`, or to a native impl, which calls
`write_fast`. The synthesized code now calls `write_int`, `read_str`,
`write_fast`, `read_fast`, and the other operations directly for a scalar
field and for a field whose type has a native impl. The prelude impls of
the scalars cannot be replaced, so the meaning is the same.

Measured (3 alternating rounds of `BENCH_RUNS=5 BENCH_REPS=10`, ns per
item; value-decode, C4, C5): `encode_custom` 81, 335, 310; `decode_custom`
205, 644, 612; `small_decode_default` 255, 288, 277. Fuel per item (C4,
C5): `encode_custom` 15, 12; `decode_custom` 26.8, 23.8;
`small_decode_default` 10, 9.

Native stack at 512 nesting levels (KiB; main, value-decode, D2 before,
C5): `json.parse` 152, 152, 120, 120; `json.decode` of structs 808, 824,
16, 16; `json.encode` 16, 16, 16, 16. 16 KiB is the floor of an empty run.

### C6: the text of `json.decode` is copied once

`text_decoder` copied the input twice: into a `String`, and then into the
decoder's `Box<str>`. It now copies once. Timing build (a timer around
`text_decoder`, minimum of 3 runs): before, the copies took 22 ns of a
390 ns small decode (`Address`) and 32 µs of a 3972 µs decode of `Doc`
(1.28 MB); after, the copy takes 18 ns (about 5%) and 17 µs (0.4%). The
benchmark shows 1 to 2% on `small_decode` and `decode_json`, inside its
noise (5 alternating runs: `decode_json` mean 887 to 880,
`small_decode` 351 to 346).

The one copy stays, for a reason that I could not remove: the `Decoder`
is a native object that lives across native calls, and a hand-written
`Decode` runs Sand code, and so maybe a garbage collection, between its
operations. The collector does not trace values in native objects, so
the decoder cannot hold the Sand `str`. The copy is counted against the
memory limit, so a decode of a text of N bytes needs N more bytes of
memory than on value-decode while it runs.

### E1: general walker against a walker specialized to JSON text

Experiment (not committed): `TextEncoder::write_fast` overridden with a
walk that knows JSON text, for the subset of schemas of `Doc` without
indentation (about 110 lines). It writes separators, keys, and brackets
from its own frames, so it needs no bracket stack, no field name lookup,
and one memory check for each value.

Timing build, `json.encode(doc)`, ns per user (minimum of 3 runs): the
general walker compiled for `TextEncoder` 399, the specialized walk 263,
value-decode about 290. A format whose operations do nothing gives 120 for
the general walker alone. Removing the memory checks and the byte fuel of
`TextEncoder` saves only 5% (399 to 377).

Conclusion: the cost is the interface between the walker and a stateful
format. Each operation finds its context again (the innermost bracket, a
map's key state, the names of the open struct), which the walker already
knows. Only a walk that knows the format reaches the goal for JSON text.

### C7: a JSON text walk for `TextEncoder`

After E1, `TextEncoder` overrides `write_fast` with a walk that knows JSON
text (`stdlib/json/text/walk.rs`). It writes separators, keys, brackets,
and indentation from its own frames, and makes one memory check for each
part that also covers a short scalar. Rare values (`Range`, `bytes`,
native values, `Json`) and map keys other than `str` go through the
format operations, so their text and their errors stay the same. A map key
of a fast type, in Sand-driven code, takes the general walker.
`start_line` is now an inlined check with the indentation out of line,
which saved about 4% on its own (timing build 331 to 318 ns per user).
`charge_written` skips the meter below 64 bytes; it charges the same units.

Line counts: the general encode walker in `codec/fast.rs` is 246 lines and
serves every format; the JSON text walk adds 367 lines for one format.

Measured (4 alternating rounds of `BENCH_RUNS=5 BENCH_REPS=10`, ns per
item; value-decode, C6 (general walker), C7):

| case          | value-decode | C6   | C7   |
| ------------- | ------------ | ---- | ---- |
| encode        | 290          | 412  | 340  |
| encode_indent | 382          | 508  | 412  |
| small_encode  | 122          | 280  | 232  |
| deep_encode   | 3987         | 5189 | 4811 |
| encode_json   | 386          | 410  | 415  |

`encode` is still 1.17 times value-decode. With the share of float
formatting (the same work in both) as the yardstick, the walk's own time is
1.26 times that of value-decode's walker. The schema walk reads a schema
node for each value, which value-decode does not need. `deep_encode` and
`encode_json` write `Json` values through the format operations.

The differential test passed 10000 cases (JSON text with and without
indentation, `retry_stress`).

### C8: buffers with room at the start

Profile of `small_encode` (C7): growing buffers took 27% of the samples
(`RawVecInner::finish_grow` and `realloc`). The output `String` of a
`TextEncoder` started empty and grew several times for 60 bytes, and each
walk grew a new frame stack and value stack. The output now starts with
room for 128 bytes, and the stacks of a walk start with room for 16 frames
and 32 parts.

Tried and rejected: `SmallVec` stacks (no allocation for small values)
gave `small_encode` 170 ns, but made `encode` 5% and `decode_ints` 6%
slower, because each access checks where the items are. Smaller
`Budget` steps for small buffers (powers of two from 256 bytes) made no
difference (`small_encode` 237 to 236).

Measured (4 alternating rounds of `BENCH_RUNS=5 BENCH_REPS=10`, ns per
item; value-decode, C7, C8): `small_encode` 119, 234, 204;
`small_decode` 233, 344, 345; `small_to_value` 93, 301, 302; `encode` 282,
338, 338; `decode` 652, 786, 788; `decode_ints` 108, 146, 146.
