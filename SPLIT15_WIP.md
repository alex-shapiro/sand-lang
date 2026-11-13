# State of the `Encode` and `Decode` redesign

This file records the state of the work on 2026-11-13.

## Branch

86 files, 16,642 lines added, and 4,102 lines deleted. By area, the net change is:

| Area                        | Net lines |
| --------------------------- | --------- |
| `crates/sandlang/src`       | +8,122    |
| `experiments/codec-traits/` | +2,010    |
| tests                       | +1,737    |
| docs                        | +371      |
| benchmarks, macros, other   | +300      |

## Design

This design has been determined by a LLM and is not set in stone; it may be sub-optimal and may be modified.

- `Encode` and `Decode` are prelude traits with one method each:
  `fn encode(self, e: Encoder)` and `fn decode(d: Decoder) -> Result<Self>`.
  `Decode` also has `decode_missing`, with a default.
- The compiler synthesizes an implementation as Sand code for each derived
  type. A user can write either trait by hand.
- A hand-written implementation passes a value to another type, with
  `e.encode(x)` or `d.decode()`. `Encoder` and `Decoder` internals are private.
- A type with no hand-written part and no computed default also has a native
  fast path, over a compile-time schema. Generic code gets the fast path
  through dictionaries that the engine builds at run time.
- `Encoder` and `Decoder` serve three formats: JSON text, `json.Json` values, and
  the host `Value`.
- A value leaves the VM only through `Encode`. Native functions receive
  encoded values through the parameter type `Encoded<N>`.
- A decoder checks that each hand-written call reads one value. It infers a
  failed read from a count of the errors that it made. After a failed read,
  the input returns to the start of the value, so a hand-written `decode` can
  try another type.
- Native objects can keep VM values that the collector traces.
- The inliner accepts small generic functions.

Removed: the `reflect` keyword, `Reflect<N>`, the runtime type
descriptions, `Decoded.Pending`, the computed-default closures,
`Vm::to_value`, `Value::Opaque`, `compile/reflect.rs`, and
`vm/container_children.rs`.

## Decisions

User-driven product decisions that are set in stone:

- Every Sand type can implement `Decode` by hand.
- `Decode` and `Encode` are independent of each other.
- A `Value` can decode map keys that are not representable in JSON.
- A hand-written implementation returns a full value. There are no keyed containers.

LLM-driven technical decisions that are NOT set in stone:

- A value leaves the VM only through `Encode`.
- `never` implements a trait if every method of the trait and its supertraits takes `self`.
- A failed `d.decode()` returns the input to the start of the value.
- A derived `Decode` for a type with private fields is valid only in the
  type's module, as on `main`.

## How the design was chosen

Two agents designed independently from one brief. Four designs were built or
prototyped. `experiments/codec-traits/README.md` records the contest:

- D1: synthesized Sand code only.
- D2: D1 plus the native fast path. D2 is on the branch.
- C: native walks with Sand shells. It failed its cost limit.
- W: a native walk that pauses through VM continuations. It failed its cost
  limit.

`experiments/codec-traits/rejected-designs.md` records C, W, the VM
continuation feature, and a prototype of one general encode walker.

## Measurements

The last full benchmark table is in `experiments/codec-traits/notes.md` §7,
"Summary of the second pass". Agents measured it at `63179f3`, before the two
review rounds. The ratios there, against `value-decode`, include these:

| Case                                   | Ratio |
| -------------------------------------- | ----- |
| `decode`                               | 0.87  |
| `encode`                               | 1.15  |
| `to_json`                              | 1.35  |
| `small_to_value`                       | 1.79  |
| `decode_custom` (hand-written `Email`) | 2.9   |
| `encode_custom` (hand-written `Email`) | 3.8   |

After that table, the fix commits give their own measurements in their
messages. No one measured the full table again at `8cfdcdb`.

## Review history

- Two review playbook audit rounds out of the maximum of five.
  Each round had three reviewers: product, system design, and code.
- The fixes of both rounds are on the branch.
- The user judges the work of these reviewers to be poor.
- The Gemini copy edit has not run.

## Docs on the branch

- `SPLIT.md` plans this work as changesets 15 to 19.
- `experiments/codec-traits/` holds `README.md`, `memo.md`, `notes.md`, and `rejected-designs.md`. Together they have 2,010 lines.
