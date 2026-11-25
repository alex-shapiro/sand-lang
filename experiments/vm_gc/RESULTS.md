# VM and GC Experiment Results

This report gives the results of [vm_gc_experiment.md](../vm_gc_experiment.md) for all three arms.

**Recommendation: use Arm 2.** Arm 2 has the runtime of Arm 1: the fastest VM (B1), the smallest footprint, and the shortest collection pause (B7). The Rust compiler also rejects a heap handle that code uses after a call that can collect garbage. In Arm 1, this mistake gives a silently wrong object. Arm 2 costs about 400 lines for its scope layer and adds `.unbind()` and `gc.reborrow()` calls to host code. Its codec runs 0–13% slower than the Arm 1 codec.

Arm 3 needs no rooting in host code, and it has the fastest structural encode. But its VM is 1.4–1.7 times slower, it makes twice as many allocations, it uses 13% more memory, and its full collection pauses 5 times longer. Its lock discipline also adds a new class of runtime errors.

All three arms pass all eight tests. Each arm has one unified codec walker and no structural fast path. All arms beat `origin/main` on decode, and Arms 1 and 2 also beat it on small encode.

## Setup

The crate is in `experiments/vm_gc`. All arms share these parts:

- One register bytecode ISA ([isa.rs](src/shared/isa.rs)) and a text assembler ([asm.rs](src/shared/asm.rs)). Bytecode programs replace a compiler. They are in [programs/](programs).
- The dataset generator from `benches/codec.rs`, written in bytecode ([dataset.sasm](programs/dataset.sasm)).
- One open codec interface ([format.rs](src/shared/format.rs)). A `FormatWriter` receives events, and a `FormatReader` is a stateless scanner over `(src, pos)`. Neither trait sees VM values, so the JSON and S-expression formats work unchanged with every arm.
- One byte tracker, one GC trigger policy, fuel, and the eight tests ([suite.rs](src/shared/suite.rs)).

The arms differ in these modules:

| Arm | Model | Modules |
| --- | --- | --- |
| 1 | `Vec<Obj>` mark-sweep heap with `Copy` handles and an explicit root stack | [arm1/](src/arm1) |
| 2 | The Arm 1 heap and interpreter, with `GcScope` and `NoGcScope` lifetime tokens | [arm2/](src/arm2) and the Arm 1 `heap.rs` and `vm.rs` |
| 3 | `Arc` objects, `Arc` and a `Mutex` for mutable objects, and a cycle collector over a `Weak` table | [arm3/](src/arm3) |

All measurements come from one Apple Silicon MacBook Pro with macOS 27.2 and Rust 1.100. Each timing is the minimum over 5 rounds that alternate the arms. Each round runs 5 fresh engines with 10 repetitions each. Code layout changes single rows by up to 10%, so treat differences under 10% as noise.

## B1: Pure VM Speed and Fuel Metering

The table gives nanoseconds per user for `build_walk` and per loop iteration for `micro`.

| Operation | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| `build_walk` operation | 325 | 328 | 466 |
| `build_walk` cleanup | 56 | 57 | 154 |
| `build_walk` total | 381 | 388 | 624 |
| `micro` operation | 27.2 | 27.3 | 45.1 |

Arm 3 is slower because every register copy and overwrite changes a reference count, every container needs two allocations, and every list access locks a mutex. Its cleanup is 2.8 times slower, because it frees two allocations per container and then prunes a `Weak` entry for each container.

All arms stop a runaway bytecode loop and a native JSON skip of 2 MB at the fuel limit. Fuel never refills.

## B2: Structural Encode and Decode

The table gives nanoseconds per user for `encode` and `decode`, and per call for the small operations. The `origin/main` column comes from `benches/codec.rs` on the same machine.

| Operation | Arm 1 | Arm 2 | Arm 3 | `origin/main` |
| --- | --- | --- | --- | --- |
| `encode` | 292 | 303 | **251** | 264 |
| `decode` operation | 419 | 407 | 440 | 618 |
| `decode` cleanup | 55 | 54 | 153 | |
| `decode` total | 476 | 460 | 594 | |
| `small_encode` operation | 108 | 109 | 126 | 119 |
| `small_encode` cleanup | 7.2 | 6.9 | 0.0 | |
| `small_encode` total | 116 | 117 | 126 | |
| `small_decode` operation | 92 | 88 | 132 | 219 |
| `small_decode` cleanup | 6.8 | 6.9 | 7.8 | |
| `small_decode` total | **100** | **95** | 141 | |

Arm 3 encodes fastest because it borrows strings and fields directly. Arms 1 and 2 look up the heap slot again for each field. Arm 3 decodes small inputs slowest because each `Address` needs two allocations, against one in Arms 1 and 2.

## B3: Interleaved Custom Callbacks

`Address` and `Status` implement `Encode` and `Decode` in bytecode. The units are the same as in B2.

| Operation | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| `encode` | **502** | 550 | 562 |
| `decode` operation | 694 | 698 | 716 |
| `decode` cleanup | 54 | 54 | 155 |
| `decode` total | 753 | 752 | 882 |
| `small_encode` operation | 208 | 235 | 319 |
| `small_encode` cleanup | 13.4 | 14.3 | 8.3 |
| `small_encode` total | **224** | 250 | 328 |
| `small_decode` operation | 289 | 300 | 351 |
| `small_decode` cleanup | 13.5 | 14.4 | 16.3 |
| `small_decode` total | **305** | 317 | 370 |

Two callbacks add 210 ns per user to the Arm 1 encode, 247 ns to Arm 2, and 311 ns to Arm 3. In Arm 3, each `Encoder` method call clones the `Encoder` handle and locks it twice. I did not find the cause of the extra 37 ns in Arm 2.

## B4: GC Under Memory Pressure

All arms pass all four cases. For the dead-garbage cases, the cap is 1.5 times the bytes that the workload tracks on an empty heap, so each run must reclaim the garbage to finish.

| Case | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| Cap for the dead-garbage cases | 20.5 MB | 20.5 MB | 26.6 MB |
| 70% dead acyclic objects, then B2 and B3 | pass, 1 collection | pass, 1 collection | pass, 1 collection |
| 70% dead cycles, then B2 and B3 | pass, 1 collection | pass, 1 collection | pass, 1 collection |
| Callback garbage in one decode (48 MB cap) | pass, 6 collections, 29 ms | pass, 6 collections, 29 ms | pass, 21 collections, 55 ms |
| One allocation larger than the cap | rejected before memory grows | rejected before memory grows | rejected before memory grows |

Arm 3 needs a 30% larger cap for the same workload. It also tracks a dead acyclic container until the next collection prunes its `Weak` entry, because the entry keeps the `Arc` allocation alive.

## B5: Deep Recursion and Callback Stack Safety

All arms pass on a 512 KB thread in the test profile (opt-level 1, as in the SandLang workspace). The table gives the deepest `Node` list that each arm handles before its check stops it.

| Check | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| Structural encode and decode (500 required) | 544 | 526 | 509 |
| Structural encode only | 1,165 | 1,165 | 1,254 |
| Drop of a 200,000-node list | pass | pass | pass |
| Nested custom callbacks before a clean stack-overflow error | 172 | 121 | 139 |

The 500-level margin is thin in every arm. The structural walkers recurse on the Rust stack and check the 256 KB budget at each container. Arm 3 drops linked values with an explicit work stack.

## B6: Async Yield and `Send`

All arms pass. Fiber 1 holds a `User` and a long string across `.await`, and fiber 2 collects garbage. Fiber 1 then reads the user's name, email, and a tag from its mutable `tags` list. The compiler checks that `Engine` and its run future implement `Send`.

Arms 1 and 2 root the held values in a non-LIFO async root table. If Arm 1 skips that table during marking, B6 fails with `type error: expected struct`, because the slot now holds another object. Arm 3 moves the values into the future and reads them after `.await` without the VM.

## B7: Memory Footprint and GC Pause

The table gives the footprint of 5,000 live users. A counting global allocator measures process bytes in a dedicated binary.

| Measure | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| Tracked bytes | 4,484,518 | 4,484,518 | 5,049,838 |
| Process bytes | 4,427,174 | 4,427,174 | 5,049,838 |
| VM objects | 33,325 | 33,325 | 28,335 containers |
| Process allocations | 33,328 | 33,328 | 66,656 |
| Full collection pause | 0.11 ms | 0.10 ms | 0.53 ms |

The Arm 1 tracker counts 1.3% more than the process allocates, because it charges a whole byte for each slot's mark bit. The Arm 3 tracker matches the process exactly. Arm 3 needs two allocations per container and two per long string.

## B8: Host Module Ergonomics and Isolation

All arms pass all four integrations:

- `list.map` calls a Sand closure per element. The closure shrinks the input list, and the call does not deadlock.
- The S-expression format encodes and decodes the dataset, with and without custom callbacks.
- A host object stores a Sand closure that captures the host object. A collection frees the cycle.
- Two engines run interleaved calls, then run on two threads at the same time.

The table counts code lines: not blank, not comment-only, and not unit tests.

| Lines | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| VM, GC, and codec core | 1,805 | 2,206 | 2,073 |
| Host integrations | 90 | 95 | 100 |

The Arm 2 core includes the 1,136 Arm 1 heap and interpreter lines, the 402-line scope layer, and a 618-line codec. All arms also use 694 shared lines for the codec interface and the two formats. Run `python3 loc.py` for the per-file counts.

Host code in each arm has a different main risk:

| Arm | What host code must do | What happens if it does not |
| --- | --- | --- |
| 1 | Root every handle that it holds across an allocation or a callback. | The handle can point to a freed or reused slot. The error is silent. |
| 2 | Call `.unbind()` before it passes a handle into a function that can collect, and call `.bind()` on entry. | The compiler rejects a bound handle that code uses after `gc.reborrow()`. A doc test in [scope.rs](src/arm2/scope.rs) proves this. The compiler does not catch a missing `.bind()`, a `Scoped` root that `truncate_roots` invalidates, an async root, or a handle stored in a host object. |
| 3 | Release each mutex guard before it calls Sand code. | The callback fails with "value is in use by a native call". It does not deadlock, because every lock uses `try_lock`. |

The Arm 2 codec and host code have 46 `.unbind()` calls, 60 `gc.reborrow()` calls, and 11 `.scope()` calls. While the code was written, the scopes rejected every use of a stale handle. They found no real bug, because the Arm 2 codec follows the Arm 1 codec, which was already correct.

## Findings for Every Arm

These findings apply to whichever arm SandLang adopts.

- **`std::sync::Mutex` allocates on first lock on macOS.** A probe of 1,000 mutexes made 1,000 allocations. The tracker cannot see this memory. Arm 3 uses `parking_lot::Mutex`, which does not allocate.
- **A `while let` over `lock()` holds the guard through the loop body.** The first Arm 3 `list.map` had this bug, so its callback found the list locked. Read the item in a `let ... else` statement, which drops the guard at the end of the statement.
- **Safe Rust revalidates UTF-8 each time it borrows an inline string.** `ShortStr::as_str` calls `str::from_utf8`, which takes 8% of the Arm 1 B3 encode. The only other options are `unsafe` code or a dependency that contains `unsafe` code.
- **Inlining changes unrelated timings.** An inline `Concat` fast path in the dispatch loop made unrelated operations 30% slower. Rare operations now live in functions outside the loop.
- **The event interface costs some encode speed.** The Arms 1 and 2 encode is 11–15% slower than `origin/main`, which writes JSON directly from VM values. Decode is 1.4 to 2.5 times faster than `origin/main` in every arm.

## Limits of the Prototype

- Programs are bytecode, because the prototype has no compiler.
- `Some(x)` is `x` itself, so the prototype does not support nested options. It has no `bytes` or tuple types, because the dataset does not need them.
- Maps are vectors in insertion order with linear lookup.
- The Arms 1 and 2 collector is a stop-the-world mark-sweep. `origin/main` has a generational, incremental collector.
- The decoder scratch buffer and the Arm 3 drop work stack are not tracked. One string token bounds the scratch buffer. The work stack lasts only for one drop.
- The prototype does not check WebAssembly stack limits. The experiment plan makes this a follow-up for the chosen arm.

## How to Reproduce

Run these commands in `experiments/vm_gc`:

```sh
cargo test                                                   # B1 to B6 and B8 for all arms
cargo run --release --bin bench                              # B1 to B3 timings
cargo run --release --bin footprint                          # B7
cargo test --release --test depth -- --ignored --nocapture   # B4 caps and B5 depth limits
python3 loc.py                                               # B8 line counts
```

Run the B5 depth probe without `--release` to measure the test profile.
