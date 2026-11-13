# `Encode` and `Decode` as regular traits

Run on 2026-11-12 and 2026-11-13. The baselines are `main` `625d1b9` and
`value-decode` `fe58101`.

## Question

`Encode` and `Decode` were compiler markers without methods. A script could
not implement them, a struct with `init` could never be decoded, and a generic
caller of `json.decode` had to declare `reflect T`. How can both traits become
regular traits that the compiler synthesizes for ordinary types and that a user
can implement by hand, for script types and native types, without speed loss?

## Conclusion

Each trait has one method, over an `Encoder` or `Decoder` object that the
engine supplies, as in Swift `Codable`:

```sand
impl Decode for Email {
    fn decode(d: Decoder) -> Result<Email> {
        Email(d.decode()?)
    }
}
```

The compiler synthesizes Sand implementations for derived types. A closed
type with no hand-written part and no computed default also gets a native fast
path, which keeps the speed of `value-decode`. A hand-written part is a plain
Sand call. A property test checks that the native path and the synthesized
path agree for every format.

## Method

Two agents worked from the same brief, without contact. Each one wrote a memo
with at least three designs and the strongest case for each, and ran
experiments before it chose. Both chose the same trait shape, and different
runtime designs. Each design then had to pass the same acceptance scenarios
and the same benchmark cases. Each contestant had a cost limit that was set
before its build. Times are the minimum of at least three alternating rounds
of release builds with `-C llvm-args=-align-loops=64`.

## Contestants

| Design | How derived code runs | Result |
| ------ | --------------------- | ------ |
| D1 | All implementations are synthesized Sand code. | Built (tag `codec-d1`). Decode is 4 times slower and encode is 5 times slower than `value-decode`. |
| D2 | D1, plus a native fast path for closed types. | Chosen. Built on this branch, and then made faster in two performance passes. |
| C | Native walks for table types. Synthesized shells that return to Sand at each hand-written part. | Dropped. A hand-written part cost 180 ns, against a limit of 150 ns. On narrow structs it was only about 10% faster than D1. |
| W | One native walker for each format. It pauses through a VM continuation at each hand-written part. | Dropped. A hand-written field cost about 435 ns, against a limit of 150 ns. Ordinary decode was 40–45% slower, because the resumable walker keeps its state on the heap. |

C and W failed for the same reason. A switch from a native walk into Sand
code must save and restore the walk state. In W, this cost about 270 ns, and
the VM callback alone cost about 50 ns. In D1 and D2, a hand-written part is
an ordinary Sand call.

The evidence for C and W is on the branch `codec-traits-a`: the memo, the
log in `DESIGN_NOTES.md` §7 and §8, and the general VM continuation in commit
`e1f8588`.

## Files

- `memo.md`: the Phase 1 memo for D1, D2, an intermediate value tree (aeson),
  a native walk with continuations, and representation proxies.
- `notes.md`: facts about the earlier system, the experiments, the build log,
  and both performance passes, with each profile and measurement.
- `e1_tree.sand`, `e2_stream.sand`, `e3_calls.sand`, `probes/`: the Phase 1
  experiments. The host module of experiment E2 was the bench target
  `codec_design` in commit `49b3c7f`.
