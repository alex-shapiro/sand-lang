# VM and GC Experiment Results

This report gives the results of [vm_gc_experiment.md](../vm_gc_experiment.md) for all three arms.

**Recommendation: use Arm 2, and run host-code tests in a GC stress mode.**
- **Speed and memory:** Arm 2 has the runtime of Arm 1. That runtime has the fastest VM (B1), the shortest collection pause, and half the allocations of Arm 3 (B7).
- **Compile-time checks:** In a fault-injection run, the compiler rejected 4 of 6 planted rooting mistakes in Arm 2.
- **Silent failures:** In Arm 1, the same mistakes compile and fail silently in production: a stale handle reads the wrong object. Tests catch them only if collections run often, so the GC stress mode is necessary in both arms.

Arm 2 has three costs:
- **Code size:** about 440 more core lines, mainly the scope layer.
- **Verbosity:** 46 `.unbind()` calls and 60 `gc.reborrow()` calls in its codec and host code.
- **Speed:** a codec that is 0–8% slower than the Arm 1 codec.

Its guarantee is also narrow. The compiler checks only handles that code binds inside one function. It does not check function parameters or values that code passes through `.unbind()`.

**Arm 3 is the alternative.** It needs no rooting, and every planted mistake failed loudly. It also has the fastest structural encode. But its VM is 1.3–1.4 times slower, its cleanup is 3 times slower, its full collection pauses about 5 times longer, and it makes twice as many allocations.

**All three arms pass all eight tests.** Each arm has one unified codec walker and no structural fast path.

## Setup

The crate is in `experiments/vm_gc`. All arms share these parts:

- One register bytecode ISA ([isa.rs](src/shared/isa.rs)) and a text assembler ([asm.rs](src/shared/asm.rs)). Bytecode programs replace a compiler. They are in [programs/](programs).
- The dataset generator from `benches/codec.rs`, written in bytecode ([dataset.sasm](programs/dataset.sasm)).
- One open codec interface ([format.rs](src/shared/format.rs)). A `FormatWriter` receives events, and a `FormatReader` is a stateless scanner over `(src, pos)`. Neither trait sees VM values, so the JSON and S-expression formats work unchanged with every arm.
- One byte tracker, one GC trigger policy, fuel, a GC stress mode, and the eight tests ([suite.rs](src/shared/suite.rs)).

The arms differ in these modules:

| Arm | Model | Modules |
| --- | --- | --- |
| 1 | `Vec<Obj>` mark-sweep heap with `Copy` handles and an explicit root stack | [arm1/](src/arm1) |
| 2 | The Arm 1 heap, interpreter, and codec objects, with `GcScope` and `NoGcScope` lifetime tokens | [arm2/](src/arm2), and Arm 1's `heap.rs`, `vm.rs`, and `codec_objects.rs` |
| 3 | `Arc` objects, `Arc` and a `Mutex` for mutable objects, and a cycle collector over a `Weak` table | [arm3/](src/arm3) |

All measurements come from one Apple Silicon MacBook Pro with macOS 27.2 and Rust 1.100. Each timing is the minimum over 5 rounds that alternate the arms. Each round runs 5 fresh engines with 10 repetitions each. Code layout changes single rows by up to 10%, so treat differences under 10% as noise.

## B1: Pure VM Speed and Fuel Metering

The table gives nanoseconds per user for `build_walk` and per loop iteration for `micro`.

| Operation | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| `build_walk` operation | 349 | 339 | 446 |
| `build_walk` cleanup | 53 | 53 | 157 |
| `build_walk` total | 403 | 396 | 611 |
| `micro` operation | 29.4 | 29.4 | 42.1 |

Arm 3 is slower because register copies and overwrites change reference counts, each container needs two allocations, and each list access takes a mutex. Its cleanup is 3 times slower, because it frees two allocations per container and then prunes a `Weak` entry for each container.

Every arm stops these cases at the fuel limit, and fuel never refills:
- **Bytecode:** a runaway bytecode loop.
- **Native scans:** a decode that skips a 2 MB value, an object with 100,000 short unknown keys, and 2 MB of whitespace before a value.
- **Long text:** a 16 MB `repeat`.

Native work costs one unit of fuel per 16 input bytes, in addition to one unit per value.

## B2: Structural Encode and Decode

The table gives nanoseconds per user for `encode` and `decode`, and per call for the small operations. Cleanup is one full collection after the operation (in Arm 3, the drop of the output and a prune). The `origin/main` column comes from `benches/codec.rs` on the same machine. It includes collections, so compare it with the total rows.

| Operation | Arm 1 | Arm 2 | Arm 3 | `origin/main` |
| --- | --- | --- | --- | --- |
| `encode` | 291 | 303 | **259** | 264 |
| `decode` operation | 434 | 445 | 454 | |
| `decode` cleanup | 52 | 51 | 161 | |
| `decode` total | **492** | 499 | 616 | 618 |
| `small_encode` operation | 92 | 99 | 109 | |
| `small_encode` cleanup | 7.0 | 7.0 | 0.0 | |
| `small_encode` total | **100** | 107 | 109 | 119 |
| `small_decode` operation | 94 | 92 | 116 | |
| `small_decode` cleanup | 6.7 | 6.8 | 8.3 | |
| `small_decode` total | **101** | 100 | 124 | 219 |

Arm 3 encodes fastest because it borrows strings and fields directly. Arms 1 and 2 look up the heap slot again for each field. Arm 3 decodes small inputs slowest because each `Address` needs two allocations, against one in Arms 1 and 2. Against `origin/main`, a decode in Arms 1 and 2 takes 20% less total time, and a small decode is 2.2 times faster. Arm 3 decodes at the speed of `origin/main` in total.

## B3: Interleaved Custom Callbacks

`Address` and `Status` implement `Encode` and `Decode` in bytecode. Each arm counts its calls of custom methods, and B3 checks the counts. The units are the same as in B2.

| Operation | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| `encode` | **513** | 547 | 578 |
| `decode` operation | 747 | 774 | 772 |
| `decode` cleanup | 50 | 53 | 156 |
| `decode` total | **799** | 828 | 940 |
| `small_encode` operation | 221 | 232 | 297 |
| `small_encode` cleanup | 14.3 | 14.0 | 8.7 |
| `small_encode` total | **236** | 247 | 306 |
| `small_decode` operation | 297 | 313 | 358 |
| `small_decode` cleanup | 14.3 | 14.3 | 17.6 |
| `small_decode` total | **313** | 329 | 377 |

Two callbacks add 222 ns per user to the Arm 1 encode, 244 ns to Arm 2, and 319 ns to Arm 3. In Arm 3, each `Encoder` method call clones the `Encoder` handle and locks it twice.

B3 also runs a callback that removes the list that the walker is encoding from its parent, and then collects garbage. Every arm keeps that list alive.

## B4: GC Under Memory Pressure

All arms pass all four cases.

| Case | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| 70% dead acyclic objects in a 32 MB cap, then B2 and B3 | pass, 1 collection | pass, 1 collection | pass, 1 collection |
| 70% dead cycles in a 32 MB cap, then B2 and B3 | pass, 1 collection | pass, 1 collection | pass, 1 collection |
| Callback garbage in one decode (48 MB cap) | pass, 9 collections, 31 ms | pass, 9 collections, 31 ms | pass, 21 collections, 55 ms |
| One allocation larger than the cap | rejected before memory grows | rejected before memory grows | rejected before memory grows |
| Smallest cap for the B2 and B3 work, 4,000 users | 6.2 MB | 6.2 MB | 7.7 MB |
| Smallest cap for the B2 and B3 work, 5,000 users | 10.4 MB | 10.4 MB | 9.8 MB |

Every arm tracks more than 30% of the 32 MB cap while it runs the work without collecting, so each run must reclaim the garbage to finish.

Neither model needs less memory at every size: each arm's tables double at different sizes. Arm 3 collects more often during the callback-garbage decode. It counts a dead container's bytes until the next collection prunes its `Weak` entry, even after its inner buffer is freed.

## B5: Deep Recursion and Callback Stack Safety

All arms pass on a 512 KB thread in the test profile (opt-level 1, as in the SandLang workspace). The table gives the deepest `Node` list that each arm handles before its stack check stops it with a clean error.

| Check | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| Structural encode and decode (500 required) | 562 | 543 | 525 |
| Structural encode only | 1,087 | 1,164 | 1,253 |
| Drop of a 200,000-node list and a 200,000-object host chain | pass | pass | pass |
| Nested custom callbacks | 168 | 118 | 129 |

B5 asserts that callbacks overflow at 400 levels, a depth that the structural walker handles. **At opt-level 0, every arm fails B5: structural nesting stops at 16 to 18 levels, and callbacks stop at 6 to 8.** The walkers recurse on the Rust stack, and each level of an unoptimized build uses about 14 KB. A walker with an explicit work stack would remove this limit. The chosen arm needs one before WebAssembly or debug-build hosts use it.

## B6: Async Yield and `Send`

All arms pass. Fiber 1 holds a `User` and a long string across `.await`, and fiber 2 collects garbage. Fiber 1 then reads the user's name, email, and a tag from its mutable `tags` list. The compiler checks that `Engine` and its run future implement `Send`. After 50 more rounds of fibers, tracked bytes do not grow.

Arms 1 and 2 root the held values in a non-LIFO async root table. If Arm 1 skips that table during marking, B6 fails with `type error: expected struct`, because the slot now holds another object. Arm 3 moves the values into the future and reads them after `.await` without the VM.

## B7: Memory Footprint and GC Pause

The table gives the footprint of live users. A counting global allocator measures process bytes in a dedicated binary. Each pause is the fastest of 15 full collections.

| Users | Arm 1 tracked bytes | Arm 3 tracked bytes | Arm 3 / Arm 1 | Arm 1 pause | Arm 3 pause |
| --- | --- | --- | --- | --- | --- |
| 1,000 | 698,630 | 1,068,094 | 1.53 | 0.02 ms | 0.11 ms |
| 4,000 | 2,812,094 | 4,263,286 | 1.52 | 0.07 ms | 0.37 ms |
| 5,000 | 4,475,046 | 5,049,838 | 1.13 | 0.09 ms | 0.47 ms |
| 10,000 | 8,955,950 | 10,096,614 | 1.13 | 0.18 ms | 0.97 ms |

Arm 2 has the same footprint as Arm 1. For 5,000 users, Arm 1 makes 33,325 process allocations and Arm 3 makes 66,656, because Arm 3 needs two allocations per container and two per long string. The Arm 1 tracker counts 1.3% more than the process allocates, because it charges a whole byte for each slot's mark bit. The Arm 3 tracker matches the process exactly.

## B8: Host Module Ergonomics and Isolation

All arms pass all four integrations:

- `list.map` calls a Sand closure per element. One closure shrinks the input list. Another allocates and grows the input list. Neither call deadlocks.
- The S-expression format encodes and decodes the dataset, with and without custom callbacks.
- A host object stores a Sand closure that captures the host object. A collection keeps the closure alive while only the host object holds it, and frees the cycle after the last reference drops.
- Two engines run interleaved calls, then run on two threads at the same time. In Arm 3, a container of one engine inside a garbage cycle of another engine does not affect that engine's collector.

The table counts code lines: not blank, not comment-only, and not unit tests. Run `python3 loc.py` for the per-file counts.

| Lines | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| VM, GC, and codec core | 1,872 | 2,310 | 2,040 |
| Host integrations | 90 | 95 | 97 |

The Arm 2 core includes the 1,236 Arm 1 heap, interpreter, and codec-object lines, the 408-line scope layer, and a 624-line codec. All arms also use 761 shared lines for the codec interface and the two formats.

### What Catches a Host-Code Mistake

`python3 faults.py` plants the same mistakes in each arm's host code and codec, then builds and tests a copy. In Arm 2, each rooting mistake appears twice:
- **Natural form:** the code that an author writes first, with the handle bound to a scope.
- **Unbind form:** the code after the author adds `.unbind()` to silence the compiler.

| Mistake | Arm 1 | Arm 2 | Arm 2 with `.unbind()` |
| --- | --- | --- | --- |
| `list.map` output list not rooted | stress test fails | compile error | stress test fails |
| `list.map` mapped value not rooted | stress test fails | compile error | stress test fails |
| Async native holds a user without a root | test fails | compile error | test fails |
| Encoder does not root the list that it walks | test fails | test fails | |
| Decoder keeps items in an unrooted local vector | test fails | compile error | test fails |
| Host object's trace skips its callback | test fails | test fails | |

"Stress test fails" means that only the GC stress check, which collects at every allocation, finds the mistake. The Arm 2 encoder mistake compiles because the walked list arrives as a function parameter, which the compiler does not check.

For Arm 3, the faults are locks held across a callback in `list.map`, `host.fire`, and the list encoder, and a host hook that skips its callback. All four fail a test with or without the stress check. A held lock fails with "value is in use by a native call", and the missing hook leaks the cycle.

## Findings for Every Arm

These findings apply to whichever arm SandLang adopts.

- **Tests need a GC stress mode.** Two of the six rooting mistakes passed every test until collections ran at every allocation. Before the fix in this branch, stale registers also kept garbage alive in Arm 1, which hid a missing root from every test.
- **Fuel must charge per input byte.** At first, the walkers charged one unit per value, so a 1.2 MB object of short keys decoded with almost no fuel. The walkers now charge for the input bytes they move through, and formats do not meter themselves.
- **`std::sync::Mutex` allocates on first lock on macOS.** A probe of 1,000 mutexes made 1,000 allocations. The tracker cannot see this memory. Arm 3 uses `parking_lot::Mutex`, which does not allocate.
- **A `while let` over `lock()` holds the guard through the loop body.** The first Arm 3 `list.map` had this bug, so its callback found the list locked. Read the item in a `let ... else` statement, which drops the guard at the end of the statement.
- **Safe Rust revalidates UTF-8 each time it borrows an inline string.** `ShortStr::as_str` calls `str::from_utf8`, which takes 8% of the Arm 1 B3 encode. The only other options are `unsafe` code or a dependency that contains `unsafe` code.
- **Inlining changes unrelated timings.** An inline `Concat` fast path in the dispatch loop made unrelated operations 30% slower. Rare operations now live in functions outside the loop.
- **The event interface costs some encode speed.** The Arms 1 and 2 encode is 10–15% slower than `origin/main`, which writes JSON directly from VM values.

## Limits of the Prototype

- Programs are bytecode, because the prototype has no compiler.
- `Some(x)` is `x` itself, so the prototype does not support nested options. It has no `bytes` or tuple types, because the dataset does not need them.
- Maps are vectors in insertion order with linear lookup.
- The Arms 1 and 2 collector is a stop-the-world mark-sweep. `origin/main` has a generational, incremental collector.
- In Arms 1 and 2, a handle from another engine names a slot in the wrong heap. Host code must not move values between engines. Arm 3 checks the engine of each container during collection.
- These buffers are not tracked against the memory limit:
  - **Decoder scratch buffer:** one string token bounds it.
  - **Arm 3 drop work stack:** it lasts only for one drop, including the drop of contents that a collection clears.
- Arm 3 cannot store an immutable struct in one allocation in safe Rust. `Arc<[Value]>` is a fat pointer, and a `Value` must fit in 16 bytes.
- The JSON scanner rejects a missing or extra comma in the values that it reads. It skips unknown values without these separator checks.

## How to Reproduce

Run these commands in `experiments/vm_gc`:

```sh
cargo test                                                   # B1 to B6 and B8 for all arms
cargo run --release --bin bench                              # B1 to B3 timings
cargo run --release --bin footprint                          # B7
cargo test --release --test depth -- --ignored --nocapture   # B4 caps
cargo test --test depth nesting -- --ignored --nocapture     # B5 depth limits in the test profile
python3 loc.py                                               # B8 line counts
python3 faults.py                                            # fault injection
```
