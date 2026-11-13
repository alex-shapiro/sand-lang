# Designs C and W, and the walker prototype

This file records the prototypes that did not merge, so that later work can
use their results without their code. `README.md` gives the contest and the
reasons for each decision. The experiment numbers E8 to E16 belong to this
file.

## Design C: native walks with Sand shells

Design C kept the native walk for each type with no hand-written part. For
any other derived type, synthesized Sand code called a native *shell*. The
shell decoded the native fields of one struct and stopped at each field with a
hand-written implementation. The rule set before the build: stop if one such
field costs much more than 68 ns, for example over 150 ns.

### What the prototype is

The prototype was a native module beside the JSON decoder. It still used
`reflect` to get the type description, and a bitmask to mark seam fields. A
*seam* is a field whose type has a hand-written implementation.

- `json.Decoder`, a native object that holds a copy of the text and the
  cursor state (position, depth, path, open frames, recorded error).
- `json.shell<reflect T>(d, seams) -> T`: allocates the struct with
  `Uninit` fields, decodes table fields with the existing JSON walker, and
  stops at the first seam field or at `}`.
- `d.seam()`: the pending seam index, -1 at the end, -2 after an error.
- `json.shell_resume(d, s, v, seams)`: stores the seam value, continues.
- `d.list_begin()`, `d.list_next()`: the prelude list `impl` primitives.
- Errors are recorded in the decoder and read with `d.take_error()`, so
  the success path allocates no `Result` per native call. A first version
  returned `Result` from every native and cost about 20 ns more per
  element.
- Each call commits the cursor state only at its end, for retries.

The experiments used Sand code written by hand in the form that the compiler
would synthesize for `Contact` and for the prelude list `impl`.

### E8. `[Contact]` with one seam (`email: Email`, 4 fields)

`sand run`, 5,000 elements, ns per
element, 3 rounds:

| case                                    | round 1 | round 2 | round 3 |
| --------------------------------------- | ------: | ------: | ------: |
| table `json.decode<[Plain]>` (email: str) |   186 |     183 |     179 |
| shell, no seam (`[Plain]` through shells) |   340 |     339 |     372 |
| shell, one seam (`[Contact]`)             |   537 |     518 |     559 |

- Fixed cost of a non-table element (list step, closure call, shell call,
  seam check): about **155 ns**.
- One seam: about **180 ns**. This is over the 150 ns stop threshold.
- Errors: `$[17].email: invalid email` (kind `invalid`) and
  `$[0]: missing field `tags``. Both are correct.

### E9. Per-call costs

200,000 elements:

| case                                      | ns per element |
| ----------------------------------------- | -------------: |
| `d.seam()` (borrowed object native)        | 9 to 10 |
| `list_next` + `shell` of `{}` + `seam()`   | 85 to 87 |
| table `json.decode<[E]>` of `{}`           | 15 to 18 |

So one `shell` call costs about 40 to 50 ns before it decodes anything.

### E10. The same `[Contact]` as all-bytecode code (contestant D1 shape)

A probe host with a native cursor: 598 to 628 ns
per element, against 188 to 196 ns for the table decode in the same binary.
**The shell (518 to 559 ns) is only about 10% faster than all-bytecode
code for this 4-field struct.**

### E11. A wide struct: `[UserE]` (the bench `User`, 10 fields, with
`email: Email`)

Shell 1,143 to 1,258 ns per element; table
`[User]` 651 to 657 ns. An earlier probe measured `User` as all-bytecode code at 2,121 to 2,246
ns. So for a wide struct with one seam, the shell is about 1.8 times the
table and about 0.55 times all-bytecode code.

### Where the time goes (profile with `sample`, 4 s)

- `run_shell` self time 12%, `malloc`/`free` about 19%, `Obj::size`,
  `Heap::alloc`, and staging about 10%, native call wrappers about 6%.
- Each `run_shell` call makes a new `DecodeTarget` and `Builder`. So it
  allocates new path, stack, and result vectors, and it loses the shared
  map-key cache of the table walk (the `meta` keys `"k"` and `"j"` are new
  strings in each element).

### Reading (reasoned)

- The prototype has removable costs: per-call vectors and the key cache.
  A cache cannot hold VM values across calls in a native object, so the key
  sharing stays lost. My estimate of the floor after optimization: 100 to
  130 ns per seam and 100 ns fixed per non-table element.
- The shell pays off in proportion to the table fields that it decodes
  natively. For narrow structs it is close to all-bytecode code. For wide
  structs it is about twice as fast.
- So the distinctive part of design C, the shells, gives a small gain on
  narrow types and a 2x gain on wide types, over a D2-like design (table
  types native, everything else bytecode). Its cost is the shell protocol
  per format.

## Design W: a native walk that pauses through continuations

Design W had one native walker for each format and no synthesized code for
derived types. At each hand-written part, the walker paused, the VM ran the
Sand implementation, and the walker resumed. The pause used a general VM
feature, described first, that lets a native function call a Sand function.
The stop rule was the same: 150 ns at a hand-written part.

### Continuations in the VM

- A native function returns `Err(Ctl::call(callee, args, roots, then))`.
  The VM pushes a `Continuation` on the fiber and a frame for `callee` above
  the live registers, with `Frame::ret == CONTINUATION_RET`. When that frame
  returns, `do_return` runs `then(vm, result, &mut roots)`. `then` returns
  the native's result, or `Ctl::call_again(callee, args)` to call again with
  the same continuation, or a new `Ctl::call`.
- Rooting: the GC traces `roots` of every continuation (`gc_roots`). On a
  collection retry, the VM also roots the callback result, and runs only the
  step again (the rule of the native-callbacks research, which a test there found).
- `then` is `FnMut + Send + 'static`, so it cannot capture `VmVal`, `Param`,
  or other VM handles: they are `!Send`. A raw `VmValue` is `Send`, as for
  `Later::new`; the rule for it is documented, not enforced.
- A callback costs 1 fuel unit, as a call does. Without it, the scheduler
  read two memory retries at the same instruction in two callbacks as one
  failed retry, and ended a `retry_stress` run with "memory limit exceeded".
- A native `callee` (a `NativeFn`) runs at once, in a loop, so a chain of
  native callbacks uses no native stack.
- `unwind_to_pad` drops continuations whose callback frames it removes.
- Tests: 8 tests, each in the 4
  combinations of `gc_stress` and `retry_stress` where it makes sense:
  basic call; 300 callbacks that allocate; a retry runs only the step; a
  retried step keeps the callback's heap result (fails with "index out of
  bounds" if the VM does not root the result: checked by hand); 3,000-level
  Sand -> native -> Sand recursion, and 100,000 levels end with
  "stack overflow"; a callback that yields and sleeps while another task
  runs; an error in a callback ends the run; cancelling a task in a callback.

### E12. Cost of one callback

A probe host, 1,000,000 calls, 3 rounds:

| case                                            | ns per call |
| ----------------------------------------------- | ----------: |
| `cur.apply(f, i)`: native -> continuation -> Sand -> resume | 50 to 51 |
| `f(i) + 1` in Sand                              | 10 |
| native no-op call                               | 9 |

So one round trip through a continuation costs about 40 ns more than a Sand
call of the same closure. This is under the 150 ns limit. The cost at a
hand-written part also includes the decoder object and the user's code; it
is measured with the real walker below.

### The W walkers

- Traits: `Encode { fn encode(self, encoder: Encoder) }`, `Decode { fn
  decode(decoder: Decoder) -> Result<Self> }`; `Encoder.encode<T>` and
  `Decoder.decode<T>` are prelude natives that take the dictionary
  (`Dict<0, traits::Encode>`, a generalization of `Num<N>`).
- A derived dictionary is the constant tuple `(derived method proto,
  description object)`. A description has `Leaf(key)` nodes for type
  parameters (key = `ParamId`) and for types with written impls (key =
  `IMPL_LEAF | id`). `Op::Describe` binds leaf dictionaries at run time
  without copying the description (no templates, no 256-level limit).
- The checker resolves a derived codec bound to `HKind::CodecDict { trait_,
  ty, leaves, args }`, where `args` are the leaf dictionaries. `reflect`,
  `Reflect<N>`, `DescTemplate` instantiation, `Decoded.Pending`, thunks,
  and the set and map fill helpers are gone.
- A module can write `impl Encode`/`impl Decode` for its types; derivation
  skips such a type. A later REPL input cannot add one for a type of an
  earlier input (code of earlier inputs uses the derived one).
- Decode: one iterative walker over an `Input` trait with two inputs: JSON
  text and `Value`.
  Each continuation step is a transaction: the walk and input state are
  snapshotted, and `Vals` keeps an undo log of the rooted partial values, so
  a memory retry reruns the step from a consistent state.
- Encode: pure dictionaries (no written impl reachable) use the existing tag
  walkers. Other values go through a walker that follows the description,
  copies pure parts with the `Value` copier, calls written `encode` methods
  through continuations, and builds a `Value`. A second step writes that
  `Value` as JSON text or converts it to `Json`.

### E13 to E15. The cost at a hand-written part (stop rule)

`sand run`, release,
5,000 elements, ns per element, 3 rounds (same binary):

| case | ns |
| ---- | -- |
| decode `[Plain]` (email: str, no written impl) | 238 to 244 |
| decode `[ContactFixed]`: written `decode` that reads nothing | 509 to 530 |
| decode `[ContactRaw]`: written `decode` that reads one `str` | 653 to 661 |
| decode `[Contact]`: reads one `str`, then `Email.init` | 671 to 699 |
| encode `[Plain]` | 67 to 72 |
| encode `[Contact]` | 467 to 486 |

- One hand-written field costs about **435 ns** on decode: about 270 ns for
  the walk to stop, call, and resume (of which the VM callback is about 50
  ns, E12), about 145 ns for the nested `d.decode<str>()` walk, and about 20
  ns for `init`. This is over the 150 ns stop rule.
- The pure path (`[Plain]`) is slower than the recursive decoder: an
  explicit-stack walk with an `Arc` description per frame. A scalar fast
  path brought it from 271 to 240 ns.
- Encoding `[Contact]` goes through a `Value` and then JSON text, so the
  whole document pays about 400 ns per element, not only the hand-written
  part.

### E16. Pure derived types against the baselines

Release binaries of `main` (`625d1b9`), `value-decode` (`fe58101`), and W,
without the align-loops flag. Each binary ran twice in turn. The table gives
the minimum ns per element:

| case | main | value-decode | W |
| ---- | ---: | -----------: | -: |
| decode `[Plain]` | 180 | 181 | 254 |
| decode `Doc` (bench document) | 678 | 687 | 988 |
| encode `Doc` | 301 | 300 | 303 |

The iterative decode walk is about 40 to 45% slower than the recursive
decoder on pure types. Encode of a pure type uses the old tag walker, so it
does not change.

## One general encode walker with stateful formats

The merged design has a general native walker in `codec/fast.rs` and a
separate JSON text walk in `stdlib/json/text/walk.rs`. The text walk exists
because the general walker was slower: 399 ns against 263 ns on `encode`
(`notes.md` §6, E1). The cause was that each format operation had to find its
place in the output again.

A prototype tested the fix that serde uses with `SerializeSeq` and
`SerializeStruct`:

- A trait `EncodeStream` has an associated type `Open`, which is the state of
  one open container.
- Each `begin_*` operation returns an `Open`. The walker keeps it in its own
  frame, and passes `&mut Open` to `item`, `field(name)`, and `key` before each
  part. `end_*` gets the `Open` back.
- The walker gives `field` the field name, and `begin_variant` the variant
  name, so that the format does not look them up.
- `begin_some` returns `Option<Open>`. `None` means that the format writes
  only the payload, so the walker pushes no frame for `Some`. This made the
  walker 4 to 5% faster.
- For JSON text, `Open` is `{is_empty, close}`.

Measurements: release builds with `-C llvm-args=-align-loops=64`,
`BENCH_RUNS=5 BENCH_REPS=10`, minimum of 4 alternating rounds, ns per item.
The two values in each cell come from two sets of rounds, in forward and in
reverse order.

| case | JSON text walk | prototype | ratio | old general walker | ratio |
| --- | --- | --- | --- | --- | --- |
| `encode` | 340 / 344 | 331 / 329 | 0.97 / 0.96 | 407 | 1.20 |
| `encode_indent` | 420 / 425 | 408 / 415 | 0.97 / 0.98 | 502 | 1.20 |
| `small_encode` | 124 / 128 | 123 / 125 | 0.99 / 0.98 | 181 | 1.46 |

The output was the same byte for byte, and the tests passed with equal fuel.

Line counts, without comments and blank lines: the JSON text walk had 272
lines and served one format. The general stream walker had 238 lines, and the
JSON implementation of `EncodeStream` had 178 lines.

Conclusions:

- One walker in this shape can replace both the general encode walker and the
  JSON text walk, at no measured cost. `TODO.md` records this change.
- The `Value` build in `ValueEncoder` and the `Json` value writers can
  probably use the same trait. This was not measured.
- The decode side needs its own prototype, because its restart and fuel rules
  differ.
- A full migration also needs an adapter that is compatible with `dyn`,
  because `Encoder` holds a `Box<dyn EncodeFormat>`, and a key mode for map
  keys that are not `str`, as serde's `MapKeySerializer` does.
- The differential property test did not catch a bug that the prototype
  planted in the separator of `str` map keys. Its random inputs seldom reach
  that path.
