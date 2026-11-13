# Design memo: `Encode` and `Decode` as regular traits

## 1. Summary

I recommend design **D2**: regular traits that synthesized Sand code drives
through a format-neutral `Encoder` and `Decoder`, plus a native fast path for
closed types that are derived all the way down.

- `Encode` and `Decode` become ordinary prelude traits with one method each.
  A user implements either one by hand with an `impl` block, for any type.
- The compiler synthesizes the `impl` of each derived type as ordinary Sand
  code. Generic code passes dictionaries, so `fn load<T: Decode>` needs no
  `reflect`.
- A format is a Rust implementation behind the opaque `Encoder` and
  `Decoder` types. JSON text, `json.Json` values, and host `Value`s are three
  formats. A future format needs no change to user types.
- Native code never calls Sand code. Sand code calls the format, and the
  engine runs host conversions in Sand.
- For a closed type with no hand-written part and no computed default, the
  synthesized `impl` is one native call to the existing walkers. This keeps
  today's speed for the common case.

The design removes `reflect` type parameters, `Reflect<N>`, `DescTemplate`,
`Decoded.Pending`, the computed-default thunks, and the set and map fill
helpers. It keeps closed `TypeDesc` constants, but only as the input of the
fast path, which users cannot see.

The evidence for the fast path is measured: without it, Sand-driven decode
is 2.9 times slower than native, and encode is 4.0 times slower (E2 in
`DESIGN_NOTES.md`). The cost comes from about 10 ns for each VM call (E3),
not from the shape of the API.

If the lead prefers the smallest machinery over speed, **D1** (the same
design without the fast path) is a complete subset. Phase 2 builds D1 first
and adds the fast path as a separate commit, so the two can be measured
against each other.

## 2. The current system

Generics are type-erased. A trait bound with methods adds a hidden
dictionary parameter, which is a tuple of closures (F1 to F3 in the notes).

`Encode` and `Decode` are built-in traits without methods. The compiler
derives them from components by a fixed point over the type graph, and user
code cannot implement them (F7, F8). They act as compiler markers:

- The three encoders (`json.encode`, `json.to_json`, `Vm::to_value`) walk VM
  values without type information. A VM value carries its struct or variant
  id, so the walk needs no type (F15).
- The decoders cannot know the target type from the input, so the compiler
  builds a `TypeDesc` for each `reflect` type parameter (F11, F12). The native
  decoder cannot run Sand code. So it returns closures for computed
  defaults in `Decoded.Pending`, and the Sand wrapper runs them (F13).
- A struct with `init` cannot implement `Decode`, because the decoder cannot
  call `init` (F9).

## 3. Limits that the current documents put on the design

Each item is a current limit, not a requirement. D2 removes or changes each
one.

| Limit                                                                                  | Source          | In D2                                                     |
| -------------------------------------------------------------------------------------- | --------------- | --------------------------------------------------------- |
| User code cannot implement built-in traits                                             | SPEC §4.9       | Users implement `Encode` and `Decode`                     |
| `Decode` requires `Encode`                                                             | SPEC §5.6       | Removed (user decision)                                   |
| A struct with `init`, and any type that contains it, never implements `Decode`        | SPEC §4.2, §5.6 | No derived `Decode`, but a hand-written `impl` is allowed |
| `reflect` type parameters, the 256-level descriptor limit                              | SPEC §4.1       | Removed                                                   |
| The checker verifies bounds on `reflect` type arguments                                | SPEC §4.9       | Removed                                                   |
| Local functions and field defaults cannot require traits with methods                  | SPEC §4.1, §4.2 | Must be removed, or `fn f<T: Encode>` breaks in a block (F4) |
| `never` implements only traits without methods                                        | SPEC §5.1       | `never` gets compiler-provided `Encode` and `Decode` (F5) |
| Native functions can only require built-in traits; `Param<N, traits::Encode>`, `Reflect<N>` | SPEC §16.3 | `Reflect<N>` removed; see section 6 for natives       |
| Computed defaults run after the whole decode succeeds                                  | `json.sand` doc | They run when each struct finishes (Q2)                   |
| `json.decode` fails with `"invalid"` if a map key type "does not implement `Encode`"  | `json.sand` doc | Fails with `"invalid"` if the key type has no JSON key form |
| `Vm::to_value` gives `Opaque` for any part that does not implement `Encode`           | SPEC §16.7      | Unchanged for the type-free copy; see section 6           |
| Every derived trait is a marker with no `impl`                                         | `derive.rs`     | `Encode` and `Decode` get synthesized `impl` blocks       |

## 4. How other languages solve the problem

The full table is in section 3 of `DESIGN_NOTES.md`. The facts that decide
the design:

- **Swift `Codable`, Kotlin serialization, and Haskell aeson pass
  dictionaries at run time, as Sand does.** They are the closest models.
- **serde depends on monomorphization.** Its generic `Serializer` is free
  only because Rust compiles each format into each type. Sand has no
  monomorphization and no JIT, so serde's speed does not transfer.
- **Go and today's Sand depend on run-time type descriptions.** Go's hooks
  (`MarshalJSON`) are also format-specific, which scenario 9 forbids.
- **Swift and Kotlin drive a format-neutral decoder from synthesized code,
  and the decoder tracks the path.** E2 shows that this gives correct paths
  at no cost on the success path.
- **aeson decodes through an intermediate `Value`.** It later added
  `toEncoding`, because encoding through `Value` was slow. E1 confirms the
  cost for Sand (4.5 times slower encode).
- **Every other system makes derivation opt-in.** Sand derives implicitly,
  so the compiler must synthesize code only for types that a program uses.

## 5. The designs

Each design lists its traits, the synthesized code, formats, native types,
limits and errors, compiler and VM changes, the strongest case for it, and my
judgment.

### D1: Sand-driven traits over a format-neutral `Encoder` and `Decoder`

This is the Swift and Kotlin design, with dictionaries in place of witness
tables.

**Traits** (prelude):

```sand
/// A type that can write itself to any data format.
pub trait Encode {
    fn encode(self, e: Encoder)
}

/// A type that can read itself from any data format.
pub trait Decode {
    fn decode(d: Decoder) -> Result<Self>
}
```

`Encoder` and `Decoder` are opaque prelude types. Each holds a Rust format
behind a trait object. Their methods follow the data model of
`value::Value`: unit, `bool`, `int`, `float`, `duration`, `str`, `bytes`,
list, tuple, set, map, range, struct, variant, and native value text. Each
format maps the data model to its own form, so JSON writes `bytes` as Base64
and `duration` as nanoseconds, and `Value` keeps them as `Value::Bytes` and
`Value::Duration`.

**Hand-written implementations** (scenarios 2 and 3):

```sand
impl Decode for Email {
    fn decode(d: Decoder) -> Result<Email> {
        Email(d.decode()?)        // reads a str, then `init` validates it
    }
}

impl Encode for Money {
    fn encode(self, e: Encoder) {
        e.encode("\(self.amount) \(self.currency)")
    }
}
```

`d.decode<T: Decode>()` is `T.decode(d)`, and `e.encode(x)` is
`x.encode(e)`. To validate a struct-shaped value, a user decodes a private
derived struct and converts it. This is the serde `try_from` idiom, and it
needs no further API.

**Synthesized code.** The compiler writes HIR for each derived type, as E2
writes by hand. For `struct Address { street: str, city: str, zip: str? }`
the decoder is, in effect:

```sand
fn decode(d: Decoder) -> Result<Address> {
    d.begin_struct(ADDRESS_SHAPE)?
    while true {
        let i = d.field(ADDRESS_SHAPE)?        // -1 after the last field
        if i < 0 { break }
        if i == 0 { street = str.decode(d)? }  // uninitialized slots, not Option
        else if i == 1 { city = str.decode(d)? }
        else { zip = Option.decode(d)? }
    }
    // An omitted field takes its default expression, or `None` for an
    // `Option`, or fails with `d.missing(ADDRESS_SHAPE, i)`.
    Ok(Address(street: street, city: city, zip: zip))
}
```

`ADDRESS_SHAPE` is a constant with the type name and the field names, as in
Kotlin's `SerialDescriptor`. The `Value` format reuses it as the shared
`StructDesc`. The prelude implements `Decode` and `Encode` for primitives,
lists, maps, sets, `Option`, and `Range` in Sand. The compiler synthesizes
tuple impls for each arity that a program uses.

**Formats.** `json.decode` and `json.encode` become Sand functions:

```sand
pub fn decode<T: Decode>(text: str) -> Result<T> {
    let d = Decoder.json(text)
    d.finish(T.decode(d))     // checks trailing input, and adds the path to an error
}
```

A map key goes through the same trait in key position. JSON text gives the
key text, so `int.decode` parses `"1"`. The `Value` format gives the key
`Value`, so tuple and struct keys work, as the user decided.

**Native types.** A native value type, such as `time.PlainDate`, gets
compiler-provided `impl` blocks that write and parse its text. A `SandStruct`
or `SandEnum` is a Sand struct or enum, so the compiler derives its `impl`
blocks as for a script type, and the result agrees with the Rust derive by
construction. A native module implements either trait by hand with
`native fn encode` or `native fn decode` in its Sand source (F17). The Rust
function receives the `Encoder` or `Decoder`. It can feed or read a whole
`Value` through its Rust `value::Encode` or `value::Decode`, so one Rust
implementation serves the host and Sand.

**Limits and errors.** The `Decoder` counts nesting on each `begin_*` call
and fails with `"limit"` past 512 levels. Sand code charges fuel for each
instruction. A hand-written implementation is ordinary Sand code, so it
cannot escape fuel, memory, or `max_call_depth` (F19). Sand frames do not
use native stack, and the format natives do not recurse, so deep input
needs less native stack than `main`. The `Decoder` keeps the path. When a
nested decode fails, `?` returns at once, so the path at the top is the
path of the failure (E2). `d.finish` prefixes the path to the message of any
error, including an error from a hand-written `init`.

**Compiler and VM changes.** Synthesize derived `impl` blocks as HIR, only
for types that need a dictionary. Remove the built-in trait restriction for
`Encode` and `Decode`. Suppress derivation when a hand-written `impl` exists.
Pass dictionaries to local functions and field defaults. Give `never`
implementations. Remove `reflect`, `TypeDesc`, and all decode helpers.

**Strongest case.** D1 is the most regular design. Every behavior is Sand
code that a user can read, write, or replace. It has one code path, so no
two paths can disagree. It removes the most machinery. Future VM call
speedups benefit it, and they benefit all generic code too. For the small
payloads of typical scripts, the absolute cost is small: a 10 KB document
takes about 0.1 ms instead of 0.03 ms.

**Weakness.** It is 2.9 times slower to decode and 4.0 times slower to encode
than `value-decode` (E2, **measured**). E3 shows why: a native call and a
dictionary call each cost about 10 ns, and D1 makes at least two calls for
each value, where the native walker spends 26 ns for each value in total.
Plain return values in place of `Result` save only 5 ns for each call
(**measured**). Fuel for each decode rises about 2 to 4 times, because
each call and each loop iteration costs one unit (SPEC §15.2; **reasoned**).

**Judgment.** Correct and regular, but it gives up most of today's speed.
AGENTS.md requires maximum performance without a JIT.

### D2: D1 plus a native fast path for closed, fully derived types (recommended)

**Traits, hand-written code, formats, and native types:** as in D1.

**The fast path.** A type is *fast* if it is closed (it has no type
parameters) and if every part of it is derived or a primitive, with no
hand-written `impl`, no struct with a computed default, and no native type
with a hand-written Rust `impl`. For a fast type, the synthesized `impl`
calls the existing native walker once:

```sand
fn decode(d: Decoder) -> Result<Doc> { d.read_closed(DOC_DESC) }   // closed TypeDesc
fn encode(self, e: Encoder) { e.write_closed(self) }                 // type-free walk
```

The encoders need no descriptor, because a fast value has no hand-written
part (F15). The decoders read a closed `TypeDesc` constant, without
templates, thunks, or fill helpers. When the checker resolves evidence for a
closed instantiation of a generic type, such as `Page<User>` or `[int]` at a
call site, it uses the fast `impl` for that instantiation, if the
instantiation is fast. Inside a generic function, `T` is open, so the
dictionary layers run in Sand, and each fast `T` takes the native path.

**Strongest case.** It keeps D1's semantics and gives today's speed for the
common case: a script decodes an API response or a config into plain
structs. Users cannot see the fast path; it is an optimization of a
synthesized `impl`, as Swift specializes generic code. A hand-written type
slows only its ancestors, and its fast siblings stay native. The estimate
for a `Doc` whose `User` has a hand-written `Email` field is 1.8 times
native, against 2.9 times for D1 (**reasoned** from the E2 call counts).

**Weakness.** Two paths must give the same result for every fast type. A
differential property test can enforce this: for random fast types and
values, compare the Sand path with the native path for text, `Json`, and
`Value`. It keeps about half of the descriptor code and both decode walkers.
Error messages must agree between the paths, so container messages name the
kind (`expected list`), not the full type (Q1).

**Judgment.** The best balance of regular semantics and speed. I recommend
it.

### D3: an intermediate dynamic value (the aeson design)

**Traits:**

```sand
pub trait Encode { fn encode(self) -> Value }
pub trait Decode { fn decode(v: Value) -> Result<Self> }
```

`Value` is a Sand enum that mirrors the host `value::Value`. A format
parses its input into a `Value` natively, and writes a `Value` natively.
The compiler synthesizes the conversions as Sand code, and can use a fast
path as in D2.

**Strongest case.** It is the simplest design for users and for LLMs. A
hand-written `decode` pattern-matches a `Value`, with random access to
fields, so an untagged union or a field rename is trivial. The traits have
exactly the shape of the Rust `value::Encode` and `value::Decode`, so the
host and Sand agree by construction. A format is two pure functions, with
no cursor state, so `retry_stress` is easy to satisfy. Tuple map keys work.

**Weakness.** Every value that is not on a fast path goes through a full
tree. E1 measures 3.1 times slower decode and 4.5 times slower encode than
native (**measured**). Generic code is the worst case: inside
`load<T: Decode>`, the whole document becomes a `Value` tree first, so the
cost of a tree applies to the whole input, and memory doubles. A
hand-written `decode` must add path segments itself, or use helpers such
as `v.field("x")` that add them.

**Judgment.** Rejected as the core, because generic code always pays for the
tree. D2 can still give users random access: `Value` (or `json.Json`)
implements `Decode`, so `let v: Value = d.decode()?` buffers one subtree
when a hand-written `impl` needs it.

### D4: a native-driven walk that calls Sand through continuations

**Traits:** as in D1. Every derived `impl` is a `TypeDesc`, also inside
generic types, where the dictionary carries the descriptor. The native
walker drives the whole decode. When it reaches a hand-written `impl` or a
computed default, it returns a P2 continuation request
(`Ctl::call(f, args, state, then)`), the VM runs the Sand function on the
same `Decoder`, and the walker resumes.

**Strongest case.** It is the fastest design in every case. Only the
hand-written parts run in Sand, so the mixed `Email` case costs about 1.1
times native (**reasoned**, from the 70 ns callback in the research report).
It also replaces `Decoded.Pending`: a computed default becomes one more
continuation. The research report already calls P2 continuations
**[necessary]** for UI callbacks, so the VM may get them anyway.

**Weakness.** It needs the most machinery. Each walker must become a state
machine whose partial results the GC roots, because Sand code runs, and
can collect garbage, in the middle of the walk. The walkers use staged heap
allocation today, which cannot cross a Sand call. Dictionaries must carry
descriptors, and generic types need `DescTemplate`s again. This keeps the
"hidden run-time type description" that the brief asks to remove, and adds
continuations.

**Judgment.** Rejected for now. Its gain over D2 is about 0.7 times native,
and only for bulk data with hand-written parts inside it (**reasoned**). If
P2 continuations land for other reasons, D4's walker can replace D2's fast
path later without a change to the traits or to user code.

### D5: representation proxies

**Traits:** a hand-written type names a derived representation and a
conversion, as with serde's `#[serde(try_from = "str")]`:
`impl Decode for Email via str { fn from_repr(s: str) -> Result<Email> }`.
The native walker decodes the representation, and the conversions run in
Sand afterwards, in post-order, as computed defaults do today.

**Strongest case.** It covers the common cases, such as `Email` and `Money`,
with one small function and no API to learn, and it keeps the native walk
for everything.

**Weakness.** It is not a regular trait: a user cannot implement `Decode`
with arbitrary logic. It generalizes `Decoded.Pending`, which the brief asks
to remove. Sets and maps must wait for each conversion before they hash.
`Encode` cannot defer its conversions, because the output order is fixed,
so it needs a pre-pass or a continuation. It needs new syntax.

**Judgment.** Rejected. D1 and D2 express the same idiom as a three-line
`impl` that decodes the representation.

## 6. Decisions inside D2

- **Derivation stays implicit.** A hand-written `impl` replaces the derived
  one. A struct with `init` gets no derived `Decode`, because a derived
  decoder would bypass validation. The diagnostic says to write
  `impl Decode`. An optional extension, from triage 063, derives `Decode`
  through `init` when the parameters of `init` implement `Decode`; D2 makes
  it cheap, but it is not in scope.
- **The compiler synthesizes code only for types that need a dictionary.**
  Most program and standard library types are never encoded. Phase 2
  measures compile time and code size on a program with many types.
- **Native functions.** A native function that needs the `Encode` form of an
  argument declares `T: Encode` in Sand source, and Sand code encodes the
  argument to a `Value` before the call. `Vm::to_value` of a `VmValue` stays
  the type-free structural copy. It cannot apply a hand-written `Encode`,
  because it has no dictionary and native code cannot call Sand code. I
  propose to document it as a copy of the VM representation, and to add a
  typed path that gives the `Encode` form.
- **Host calls (changesets 17 and 18).** The engine runs a short Sand entry
  that decodes each `Value` argument with `T.decode` on a `Value` decoder,
  and encodes the result with `T.encode` on a `Value` encoder. Hand-written
  implementations apply in both directions, with no native-to-Sand call. A
  test hook that returns a `Value` decoder to Sand code covers scenario 6
  until changeset 18.
- **Encode cannot fail in Sand.** `encode` returns `()`. A format error, such
  as a tuple key in JSON text, aborts with `"invalid"`, as `json.encode` does
  today.
- **Error kinds and paths stay the same** (scenario 1). An error from a
  hand-written `decode` keeps its kind and gets the path.
- **Format natives check before they change state.** Each `Decoder` native
  checks memory before it moves its cursor, and each `Encoder` native checks
  before it writes, so that `retry_stress` reruns are safe (Q5).

## 7. Evidence

| ID | Claim                                                          | Kind     | Result                                   |
| -- | -------------------------------------------------------------- | -------- | ---------------------------------------- |
| E1 | Tree design (aeson) against the native walks                   | measured | decode 3.1x, encode 4.5x slower          |
| E2 | Sand-driven streaming design against the native walks          | measured | decode 2.9x, encode 4.0x slower          |
| E2 | Paths from the decoder's stack are correct                     | measured | `$.users[3].score: expected float`       |
| E3 | Cost of one native call, one dictionary call                   | measured | about 10 ns each; inlined direct call 1 ns |
| E3 | `Result` returns from natives                                  | measured | about 5 ns for each call                 |
| -- | D2 mixed case (one hand-written field in `User`)               | reasoned | about 1.8x native                        |
| -- | D4 mixed case                                                  | reasoned | about 1.1x native                        |
| F4 | Local functions cannot require traits with methods             | measured | compile error                            |
| F11 | Generic `json.decode` needs `reflect`                         | measured | compile error                            |

## 8. Lines and machinery

**reasoned** estimates, to measure in phase 2.

- Removed: `reflect` in the parser, checker, and compiler; `HKind::TypeDesc`;
  `DescTemplate`; `Reflect<N>`; `Decoded`; computed-default thunks; set and
  map fill helpers; most of `DecodeTarget`. About 1,200 lines.
- Added: HIR synthesis of derived `impl` blocks; `Encoder` and `Decoder` with
  three formats; prelude `impl` blocks for built-in types; dictionaries for
  local functions; `never` implementations. About 1,500 lines.
- Kept for the fast path: closed `TypeDesc` constants, both decode walkers,
  and the three encode walkers. Without the fast path (D1), about 2,000 more
  lines go.

## 9. Open risks

1. **Compile cost of synthesis.** Implicit derivation could synthesize code
   for every type. Mitigation: synthesize on demand. Measure in phase 2.
2. **Dictionary allocation in recursive generic types.** In
   `enum Tree<T> { Node([Tree<T>]) }`, each `Node` builds the `[Tree<T>]`
   dictionary at run time (F2). Measure; if it matters, cache dictionaries
   per parameter dictionary.
3. **Local function dictionaries.** Removing F4 is a general change to
   closures. It is necessary for D1 to D4, because `Encode` gets methods. If
   it is larger than this task, the fallback is to keep the limit and
   report each break.
4. **Two paths in D2.** The fast and Sand paths can disagree. The
   differential property test must cover every format.
5. **Message changes.** Container type names leave mismatch messages (Q1).
6. **Default timing.** Computed defaults run before the decode completes
   (Q2).
7. **The text copy.** A JSON `Decoder` cannot hold a VM string handle (F18),
   so it may copy the text. Measure in phase 2.

## 10. The experiment that decides between D1 and D2

Build D1 completely, with every acceptance test. Then add the fast path in
one commit. Measure the `codec` benchmark on an idle machine against `main`
and `value-decode`, and count the lines that the fast path keeps. If the
fast path gives less than a 1.5 times speedup on `decode` and `encode`, drop
it and ship D1.
