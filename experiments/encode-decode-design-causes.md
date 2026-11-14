# Design causes of the `Encode` and `Decode` difficulty

This report answers one question. Which decisions in the design of the Sand
compiler and VM make it hard to turn `Encode` and `Decode` into regular traits?

The investigation ran on 2026-11-14. It examined `main` at `625d1b9` and the
branch `origin/encode-decode-traits`. It used the design records of that branch
and the research branch `origin/claude/sandlang-native-callbacks-research-i6bf14`.
It also used new probes and measurements on a release build of `main`.

## Summary

Two design decisions close every simple path to the goal.

The first decision: native Rust code cannot run Sand code. A native function is
a leaf. The VM can stop it and run it again from the start. This decision blocks
the usual interpreter design for a codec. In that design, a native walker does
most of the work and calls script code only for a type with a hand-written
implementation.

The second decision: the VM finds the behavior of a built-in trait from the
value, but the behavior of a user trait from the type. A VM value records its
struct or enum definition. It does not record its type arguments. The VM can
therefore compare, hash, and encode a value by a walk over the value. The
compiler does not need to supply type information for this walk. A trait that a user can implement must use the type.
In Sand, generic code receives type information only at run time, through
dictionaries. A dictionary call runs in the bytecode interpreter, so each
encoded value costs a few interpreted calls.

Together, these decisions leave two choices:

- Run every codec step as Sand code. This is simple, but 2 to 5 times slower
  than `main`.
- Keep a native walker for common types and Sand code for the other types. This
  is fast, but the two implementations must agree on every format, path, error,
  and retry.

The branch chose the second option. About a quarter of its new production code
exists only to keep a second implementation that agrees with the first.

Four smaller decisions add cost to each attempt. The checker treats traits with
methods differently from traits without methods. The restart rule makes each
native step a transaction. Natives receive trait evidence only as data. The
checker builds derived code in Rust, for every type, without a request.

The four culprits in the question are real. Each of them is partly a result of
the two main decisions. The last section of the findings explains this.

| Finding                                                     | Significance       |
| ----------------------------------------------------------- | ------------------ |
| 1. Native code cannot run Sand code                         | root cause         |
| 2. Built-in traits use the value, user traits use the type  | root cause         |
| 3. The dictionary regime of traits is incomplete            | partial root cause |
| 4. The restart rule makes each native a transaction         | partial root cause |
| 5. Natives receive trait evidence only as data              | partial root cause |
| 6. Derivation is implicit and the checker builds it in Rust | minor              |
| 7. Sandbox limits apply to all metadata                     | minor              |
| 8. Fuel counts expose the execution path                    | tangential         |

## How to read the tags

Each judgment has two tags.

The epistemic status tells how the claim is known:

- **Verified**: the claim is read in the code or docs at a given location, and
  a probe reproduced it where a probe is possible.
- **Measured**: a timed run produced the number. The report gives the method.
- **Likely**: the claim follows from verified facts by reasoning. No
  experiment tested it directly.
- **Speculative**: the claim is a plausible estimate with little direct
  evidence.

The significance tells how much the decision contributes to the difficulty:

- **(root cause)**: if this decision changed, a simple design would become
  possible.
- **(partial root cause)**: the decision adds a large, distinct cost to every
  design, but a change to it alone would not make a simple design possible.
- **(minor)**: the decision adds work, but does not choose between designs.
- **(tangential)**: the decision touches the work, but does not shape it.

## Background

This section defines the terms that the findings use.

**Trait.** A trait is a set of methods that types can implement, like a Rust
trait. A type parameter can require a trait, as in `fn f<T: Hash>(x: T)`.

**Built-in trait.** The prelude declares six built-in traits: `Eq`, `Ord`,
`Hash`, `Encode`, `Decode`, and `Num`. On `main`, `Encode` and `Decode` do not
declare methods (`crates/sandlang/src/stdlib/prelude.sand:313` and `:318`).

**Derivation.** The compiler decides by itself that a struct implements a
built-in trait if all of its fields implement that trait. The user does not ask
for this. The code that decides it is `crates/sandlang/src/check/derive.rs`. On
`main`, derivation does not produce an `impl` or code. It produces only a yes
or no answer.

**Type erasure.** The compiler emits one bytecode function for each generic
function. It does not emit one copy for each type argument. At run time, the
bytecode of a generic function does not know its type arguments.

**Dictionary.** A dictionary is a hidden argument that gives generic code the
methods of a trait for a specific type. In Sand, a dictionary is a tuple of
closures. The compiler passes one dictionary for each required trait that has
methods (`crates/sandlang/src/check/traits.rs:1-21`). The compiler does not
pass a dictionary for a trait without methods.

**`reflect` type parameter.** On `main`, a generic function that decodes must
mark its type parameter `reflect`. The compiler then passes a run-time
description of the type, which is a second kind of hidden argument
(`crates/sandlang/src/compile/reflect.rs`).

**Native function.** A native function is a Rust function that Sand code can
call. Its Rust type is `Fn(&mut Vm, &[VmValue]) -> Result<VmValue, Ctl>`
(`crates/sandlang/src/vm/mod.rs:29`).

**Fiber.** A fiber is the state of one Sand task. It keeps the Sand call frames
in a Rust `Vec`. A Sand call therefore does not use the Rust stack.

**Safe point.** A safe point is a location in the interpreter loop where the VM
can collect garbage or stop a task. Loop back edges are safe points.

**Root.** A root is a value that the garbage collector treats as live. In Sand,
the roots are VM registers and VM data structures. A `VmValue` that Rust code
keeps in a local variable is not a root.

**Restart rule.** If a native function reaches the memory limit, the VM collects
garbage and then runs the native function again from the start. A permission
prompt causes the same restart. SPEC §16.3 states the rule, and
`Config::retry_stress` tests it.

**Hand-written part.** On the branch, a type has a hand-written part if it, or
one of its components, has a hand-written `impl Encode` or `impl Decode`.

**Fast path.** On the branch, a fast path is a native walker that encodes or
decodes a whole type in Rust from a compile-time description called a
_schema_. A type without hand-written parts gets the fast path.

## Finding 1: Native code cannot run Sand code

**Significance: (root cause).** **Epistemic status: verified** for the
mechanism, **likely** for the conclusion.

### The decision

A native function is a leaf call. It can return a value, an error, or a request
to the scheduler. It cannot call a Sand function and wait for the result.

Three facts on `main` form this decision:

- The `Vm` does not have a method that calls a Sand function from Rust. The research
  branch confirms this (`research/native-callbacks/report.md`, fact F-S1).
- Any Sand function can suspend. SPEC §11 states: "There is no `async`/`await`
  coloring — any function may suspend." To suspend a task, the interpreter
  returns to the scheduler (`FiberStop::Async`, `crates/sandlang/src/vm/interp.rs:60-73`).
  The interpreter cannot return past a Rust stack frame of a native function.
- The garbage collector does not trace a `VmValue` that Rust code holds
  (`crates/sandlang/src/vm/value.rs:13-15`). Collection runs only at safe
  points (`interp.rs:1716-1730`). A Sand callback could allocate and trigger a
  collection. That collection could free an object that the calling native
  still uses.

The repository does not record a rationale for these choices.

### Why this decision blocks a simple design

Most interpreters build a codec as native code with script hooks. The native
walker encodes all ordinary values. It calls script code only for a type that
has its own method. For example, the C encoder of CPython's `json` module calls
the Python `default` function only for unknown objects. Lua has `lua_call` for
the same purpose. (This comparison comes from general knowledge of those
libraries. This investigation did not check their sources.)

Sand cannot use this design. A native walker cannot call a hand-written
`encode`. The branch tested the two ways around this wall:

- **Design W** used a VM continuation. The native walker returned a request to
  call Sand, and the VM resumed the walker afterward. The walker had to keep
  all its state on the heap, so that it could stop at any point. The branch
  measured about 435 ns for each hand-written field. About 270 ns of that went
  to saving and restoring the walker state. The heap state also made the
  ordinary decode 40 to 45 percent slower
  (`experiments/codec-traits/rejected-designs.md`, E13 to E16).
- **Design C** used Sand shells. Sand code called a native shell for each
  struct, and the shell stopped at each hand-written field. It cost about
  180 ns for each hand-written field (E8).

The callback itself is not the large cost. The research branch measured a
continuation callback at about 70 ns, and a nested call prototype at about
101 ns (`report.md` §5.2). The large cost comes from a walker that must be able
to stop. A native walker that could keep its state on the Rust stack would not
need to save that state.

**Likely conclusion.** If a native could call Sand code while its own state
stays on the Rust stack, then a single native walker with Sand hooks would be
possible. That design has one implementation for each format, as `main` has
today. The difficulty in Finding 2 would remain, but only for the type
information that the walker reads.

### What a change would require

A nested call needs three changes, which the research branch lists in §8.3:

- The VM must forbid suspension inside the nested call, or must handle it.
- The native must keep its heap handles as roots during the call.
- The VM must collect garbage in place during the call, instead of a restart.

Each change touches the core of the VM. This report does not recommend one.

## Finding 2: Built-in traits use the value, user traits use the type

**Significance: (root cause).** **Epistemic status: verified** for the
mechanism, **measured** for the cost.

### The decision

The VM implements `Eq`, `Ord`, `Hash`, and `Encode` by a walk over the value.
It does not use the type. The walk is possible because each heap object records
its own shape:

- A struct object records its definition number and its fields
  (`Obj::Struct(u32, Box<[VmValue]>)`, `crates/sandlang/src/vm/value.rs:1586`).
- A list object records only its items (`Obj::List(Vec<VmValue>)`, line 1582).

`crates/sandlang/src/vm/compare.rs` implements equality, order, and hashing in
this way. `json.encode` takes a value without a type description
(`pub native fn encode<T: Encode>(value: T, ...)`, `json.sand:128`).

User traits work in a different way. The checker resolves each trait
requirement to a dictionary, and generic code calls the method through it.

`Decode` is a third case. A decoder does not have a value to walk, so it needs the type.
On `main`, the compiler supplies the type as a `reflect` description.

### Why a regular `Encode` must use the type

A trait that a user can implement must find its implementation from the type,
for two reasons:

- During a decode, the value does not exist yet.
- An `impl` can target a concrete type argument, as in
  `impl Describe for [int]` (SPEC §4.9). The VM does not record type
  arguments. An empty `[int]` and an empty `[str]` are the same VM value. A
  walk over the value therefore cannot choose between two such `impl` blocks.

So a regular `Encode` must move from the walk over the value to dictionaries.

### Why dictionaries are slow for a codec

A dictionary call runs in the bytecode interpreter. A codec built from
dictionaries makes at least one interpreted call for each value that it
encodes.

**Measured on `main`.** A release build at `625d1b9` ran a script with 2,000,000
calls of each kind. The table gives the minimum time over 5 rounds, in two runs
that agreed within 0.3 ns.

| Loop body                              | ns per iteration |
| -------------------------------------- | ---------------: |
| empty                                  |              5.0 |
| direct call of a Sand function         |             16.6 |
| generic call, then a trait method call |             28.3 |

So a generic trait call costs about 23 ns above the loop. The native walk on
the branch spends about 26 ns on each value in total (`notes.md`, E3).

**Measured on `main`.** A second script defined `ToJson` and `FromJson` as user
traits, with `impl` blocks for scalars, lists, and a 5-field struct. It
compared them with the native codec on 20,000 structs. Both paths gave equal
results. The table gives the minimum over 9 rounds, in two runs.

| Operation                             | ns per struct |
| ------------------------------------- | ------------: |
| `json.to_json`, native walk           |     200 – 216 |
| `ToJson` in Sand, same `Json` tree    |     458 – 540 |
| `json.decode`, native, from text      |     305 – 317 |
| `json.parse`, then `FromJson` in Sand | 1,109 – 1,134 |

Codec logic in Sand costs 2.3 to 3.6 times the native walk. The branch found
the same range for its all-Sand design D1 (`notes.md` §5).

### Type erasure adds a second cost

The fast path needs a schema of the whole type. For a closed type such as
`[User]`, the compiler builds the schema. Generic code such as
`fn load<T: Decode>(text: str) -> Result<[T]>` knows `T` only at run time.

The branch therefore builds schemas at run time from templates
(`crates/sandlang/src/codec/composed.rs`). It stores them in the compiled
`Program`. The docs of `Program` on the branch say that it is "immutable once
built, except ... that runs add to its `RuntimeSchemas`"
(`vm/bytecode.rs:1276-1278`). Many engines can share a `Program` through `Arc`,
so this table needs a lock and a limit of 1,024 schemas.

A compiler that made one copy of each generic function for each type argument
would know every schema at compile time. This report does not claim that such a
compiler is a good choice for Sand. Code size and compile time would grow.

## How Findings 1 and 2 combine

The two root causes block two different escape routes:

- Finding 1 blocks a native walker with Sand hooks.
- Finding 2 makes an all-Sand codec slow.

A design that must be fast and must accept hand-written parts must therefore
keep two implementations. The fast implementation is a native walker for types
without hand-written parts. The slow implementation is synthesized Sand code
for all other types. The two must produce the same output, paths, and errors.
The branch states this contract in the docs of the fast path: the walkers "call
the same format operations, in the same order, as the synthesized Sand
implementations" (`codec/fast.rs:1-9`).

**Verified.** The branch has eight kinds of derived method
(`DerivedMethod`, `crates/sandlang/src/check/codec.rs:21-41`). Each pair exists
because of one constraint:

| Derived method kinds               | Constraint that requires them                         |
| ---------------------------------- | ----------------------------------------------------- |
| `Encode`, `Decode`                 | hand-written parts (Findings 1 and 2)                 |
| `FastEncode`, `FastDecode`         | speed of Sand code (Finding 2)                        |
| `ComposedEncode`, `ComposedDecode` | type erasure in generic code (Finding 2)              |
| `CheckedEncode`, `CheckedDecode`   | a stateful protocol that Sand code drives (Finding 1) |

**Verified.** The branch adds 8,210 net lines of production code. Four files
serve only the fast path, and they add 2,065 lines:

- `codec/fast.rs`
- `codec/composed.rs`
- `stdlib/json/text/walk.rs`
- `stdlib/json/text/read.rs`

A property test of 1,072 lines (`tests/props/codec.rs`) checks that the two
implementations agree. The test once missed a planted bug, because its random
inputs seldom reach a rare path (`rejected-designs.md`, last section).

**Likely.** These counts are a lower bound. Parts of other files also serve only
the fast path, such as the direct `Value` walk in `codec/value_format.rs`.

## Finding 3: The dictionary regime of traits is incomplete

**Significance: (partial root cause).** **Epistemic status: verified.**

### The decision

The checker has two regimes for traits:

- A trait without methods does not need run-time evidence. Every part of the language
  accepts it.
- A trait with methods needs a dictionary. Several parts of the language do not
  accept it.

The flag `TraitDef::has_dict` selects the regime. The checker tests it in 24
places. The probes below reproduced each restriction on `main`.

| Restriction of traits with methods   | Source                                        |
| ------------------------------------ | --------------------------------------------- |
| a local function cannot require one  | probe 1: "local functions cannot require ..." |
| `never` does not implement one       | probe 5: "never does not implement `Show`"    |
| a field default cannot use one       | SPEC §4.2                                     |
| a native function cannot require one | SPEC §16.3                                    |

The probe for `never` used `let e: [never] = []`. A call that requires
`Encode` accepted `e`. A call that requires the user trait `Show` gave the
error above.

### Why this matters for the goal

`Encode` and `Decode` are in the first regime on `main`. The goal moves them to
the second regime. Every restriction of the second regime then applies to
existing code that uses `Encode` and `Decode`. For example, a local function
`fn f<T: Encode>` is valid on `main`, and invalid if `Encode` has methods.

The branch had to remove these restrictions first. `SPLIT.md` lists four
prerequisite changesets, 15 to 18. The commits include:

- `204fb7e` "Let local functions require traits with methods"
- `e753d17` "Let field defaults use traits with methods"
- `ec986e6` "Default an unknown bound type to never when never satisfies the bound"
- `9fd0ec8` "Index impls by target head in the verifier and raise the proof depth to 128"

**Likely.** These restrictions are gaps in the dictionary regime, not
properties of codecs. Any built-in trait that gains methods would meet them
again.

## Finding 4: The restart rule makes each native a transaction

**Significance: (partial root cause).** **Epistemic status: verified.**

### The decision

If a native function reaches the memory limit, the VM rewinds the frame to the
instruction, collects garbage, and runs the instruction again
(`FiberStop::CollectAndRetry`, `interp.rs:72` and `:578-588`). A native must
therefore check all limits before any change of state
(`crates/sandlang/src/native/mod.rs:771-775`).

This rule follows from the same root model as Finding 1. The collector cannot
see handles in Rust variables, so it cannot run during a native call.

### Why this matters for the goal

On `main`, each codec operation is one native call. The restart rule is easy to
follow, because the native builds its result and returns it.

A trait-based codec drives an `Encoder` or a `Decoder` through many native
calls. Each call changes the state of the format. Each such call must check
memory before its change, or it must undo its change. The branch added `mark`
and `rollback` to its formats, and an `atomic` helper for operations that make
two changes. The term set `mark`, `rollback`, `atomic`, and `retry` occurs 129
times in its production code.

The branch records four bugs of this kind that `retry_stress` found:

- `end()` of two encoders removed a level before the memory check.
- The tree encoder opened the two containers of a variant in two checks.
- The value decoder used its input before the memory check of a long text.
- The tree encoder did not restore its state for `Some(0..=0)`.

The sources are `notes.md` §5 and §7, Q7.

## Finding 5: Natives receive trait evidence only as data

**Significance: (partial root cause).** **Epistemic status: verified** for the
mechanism, **likely** for the link to Finding 1.

### The decision

A native function can receive evidence for a type parameter only in a fixed set
of Rust types (SPEC §16.3):

- `Param<N, B>`: the value, with built-in traits `B`
- `Reflect<N>`: the `reflect` description of the type
- `Num<N>`: the `zero` and `one` methods of `Num`

`Num` shows the pattern. A native function reads `Num::zero` from the
dictionary and calls it. This works only because the checker requires each
`Num` impl to be native: "the methods of a `Num` impl must be native"
(`check/traits.rs:597-598`). It also requires the methods in a fixed order,
"because `native::Num` reads them by position" (`traits.rs:472`).

### Why this matters for the goal

A native cannot call a Sand method from a dictionary (Finding 1). So each new
kind of evidence that natives need becomes a new Rust type with its own rules.
The branch added two more: `CodecDict<N, B>`, a codec dictionary, and
`Encoded<N>`, a value already encoded to a host `Value`.

The branch also had to decide how a value leaves the VM. On `main`,
`Vm::to_value` converts a value without its type. A hand-written `Encode` makes
that conversion wrong, because the conversion cannot call the hand-written
method. The branch removed `Vm::to_value`, and passes every value to the host
through `Encode` (`notes.md` §4, Q3).

This is part of the "complexity of native interop" in the question. **Likely**:
a large part of that complexity follows from Finding 1, not from the interop
API itself.

## Finding 6: Derivation is implicit and the checker builds it in Rust

**Significance: (minor).** **Epistemic status: verified.**

Sand does not have a source-level macro or derive system. Derived code must therefore be
typed HIR, which Rust code in the checker builds. The branch module
`check/codec.rs` has 1,240 lines for this purpose.

Derivation is also implicit: every type that can implement `Encode` does so.
The branch therefore builds an `impl` for every type in the program, and builds
its methods only on first use. Before that change, a chain of 20,000 structs
took 111 s to compile in a debug build. After it, the chain took 2.6 s
(`notes.md` §5, D1 build log).

REPL sessions add more work. A later REPL input can replace an `impl`. The
branch had to renew derived implementations and roll back schemas for this case
(commits `35d2b3c` and `d719bda`).

Swift and Kotlin also synthesize codecs inside the compiler. This finding is
therefore not unusual. It adds size, but it does not choose between designs.

## Finding 7: Sandbox limits apply to all metadata

**Significance: (minor).** **Epistemic status: verified.**

The sandbox invariants in `docs/INTERNALS.md` apply to every structure that
script input can grow. Type descriptions and schemas are such structures.

An earlier attempt, `derive-payloads-generics-num`, found three hostile cases
in commit `f0a61bd`:

- A script used 2.65 GB of memory under a 64 MB limit, through copies of
  run-time type descriptions.
- A 1 KB script used 6.7 GB of memory in the checker.
- A description of 20,000 nested struct defaults overflowed the stack as Rust
  dropped it, and the process aborted.

Native walkers must also use explicit stacks. On `main`, `json.decode` of 512
nested structs needs 808 KiB of native stack (`notes.md` §5, D1 measurements).
`compare.rs` is iterative for the same reason.

Each new piece of metadata therefore needs a limit, a charge, and an iterative
walk. This cost applies equally to every design.

## Finding 8: Fuel counts expose the execution path

**Significance: (tangential).** **Epistemic status: verified.**

The VM charges fuel deterministically for each instruction and call. A
synthesized Sand implementation charges more fuel than a native walk of the
same value. On the all-Sand design D1, encoding cost 2 to 4 times the fuel of
`main` (`notes.md` §5, "Fuel of each kind"). Tests in `tests/metering.rs` check
these counts. Each change to the execution path of a codec therefore changes
visible behavior and its tests.

## The four culprits in the question

The question named four culprits. Each one connects to the findings above.

**Special built-in traits.** The special status has a concrete form: built-in
traits use the walk over the value, and they stay in the regime without
dictionaries. Findings 2 and 3 describe both parts.

**Complex native interop.** Part of this complexity is a direct result of
Finding 1. Finding 5 describes that part.

**`Decode` as a marker trait.** `Decode` is a marker because the walk over the
value cannot serve it. `main` therefore added `reflect` as a separate channel.
Finding 2 describes this.

**The current `json` module.** The module is three walkers over self-describing
values and one decoder over `reflect` descriptions. Its shape follows from
Finding 2. **Likely**: a rewrite of the module alone would not remove the
difficulty.

## The most important problem outside the question

**Epistemic status: likely.**

The branch started its build before anyone decided which root cause to accept.
Its brief required speed equal to `main` and support for hand-written parts.
Findings 1 and 2 make those two requirements produce two implementations. So
the branch could not become small.

A future attempt needs one of three decisions first:

- Change Finding 1: add a nested call from native code to Sand code, with the
  three changes that Finding 1 lists.
- Accept the cost of Finding 2: use synthesized Sand code for all types, and
  accept a slower codec.
- Accept two implementations, and design the second one so that the compiler
  generates it from the first one.

Each option is a product decision. This report does not choose one.

## Limits of this investigation

- The investigation did not read most of the 16,000 lines of the branch. It
  read the design records, the module docs, and selected definitions.
- The measurements on `main` ran on one macOS machine, without other control
  of noise. They use the minimum of several rounds.
- The numbers for designs C, W, and D1 come from the branch records. This
  investigation did not run them again.
- The comparison with CPython and Lua comes from general knowledge. This
  investigation did not check their sources.

## Appendix: probes and benchmark scripts

All probes ran with a release build of `main` at `625d1b9`, built in a separate
worktree.

The probes and their outputs:

```sand
// Probe 1. Output: "local functions cannot require traits with methods;
// declare the function at the top level"
trait Show { fn show(self) -> str }
fn main() {
    fn twice<T: Show>(x: T) -> str { x.show() + x.show() }
}

// Probe 2. Output: "`Encode` is a built-in trait; the compiler derives its
// impls, so it cannot be implemented"
struct Email { text: str }
impl Encode for Email {}

// Probe 3. Output: "this call needs a description of `[T]` at run time, so
// `T` must be a `reflect` type parameter of this function"
import json
fn load<T: Decode>(text: str) -> Result<[T]> { json.decode(text) }

// Probe 4. Output: "User does not implement `Decode` because `Email` declares
// `init`, which decoding does not call"
struct Email {
    text: str
    pub fn init(text: str) -> Result<Email> { Ok(Email(text: text)) }
}
struct User { name: str, email: Email }
let u: Result<User> = json.decode("{}")

// Probe 5. `enc(e)` is accepted. `show_all(e)` gives "never does not
// implement `Show`".
trait Show { fn show(self) -> str }
impl Show for int { fn show(self) -> str { "i" } }
fn show_all<T: Show>(x: [T]) -> int { x.len() }
fn enc<T: Encode>(x: [T]) -> int { x.len() }
let e: [never] = []
print(enc(e))
print(show_all(e))
```

The call benchmark times five loops of 2,000,000 iterations with
`time.Stopwatch`. The loop bodies are: nothing, a call of
`fn direct(x: int) -> int`, a call of a closure, a call of
`fn via_dict<T: Inc>(x: T) -> int { x.inc() }`, and `xs.len()`. The function
`direct` has an early `return`, so that the inliner does not inline it.

The codec benchmark builds 20,000 values of
`struct User { name: str, age: int, score: float, active: bool, tags: [str] }`.
It defines the user traits `ToJson { fn to_json(self) -> json.Json }` and
`FromJson { fn from_json(j: json.Json) -> Result<Self> }`, with `impl` blocks
for `int`, `float`, `str`, `bool`, `[T]`, and `User`. It checks that
`json.to_json(users) == users.to_json()`, and that both decoders give equal
lists.
