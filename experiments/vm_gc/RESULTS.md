# VM and GC Experiment Results

This report gives the results of [vm_gc_experiment.md](../vm_gc_experiment.md) for all three arms.

**Recommendation: use Arm 2, and run host-code tests in a GC stress mode.**
- **Speed and memory:** Arm 2 has the runtime of Arm 1, which has the fastest VM (B1), the shortest collection pause, and half the allocations of Arm 3 (B7).
- **Compile-time checks:** in a fault-injection run, the compiler rejected 4 of 6 planted rooting mistakes in Arm 2's host and codec code. In Arm 1, the same mistakes compile, and in production a stale handle silently reads the wrong object.
- **Stress mode:** tests catch the remaining mistakes in both arms only if collections run often, so the stress mode is necessary.

Arm 2's checks are narrow:
- **Coverage:** they cover only the host and codec code that Arm 2 adds. Arm 2 runs the Arm 1 interpreter (1,011 lines) unchanged, without scope checks.
- **Escapes:** inside the checked code, the compiler does not check function parameters or values that pass through `.unbind()`.

Arm 2 has four costs:
- **Code size:** about 440 more core lines, mainly the scope layer.
- **Verbosity:** 46 `.unbind()` calls and 63 `gc.reborrow()` calls in its codec and host code.
- **Speed:** a codec that is 0–9% slower than the Arm 1 codec.
- **Stack depth:** 118 nested callback levels on a 512 KB stack, against 168 in Arm 1.

**Arm 3 is the alternative.** It needs no rooting, and every planted mistake failed a test without the stress mode. It also has the fastest structural encode. But it has three costs:
- **VM speed:** its VM is 1.4 to 1.7 times slower.
- **Collection:** its cleanup is almost 3 times slower, and its full collection pauses about 5 times longer.
- **Allocations:** it makes twice as many.

Arm 3 also has failure modes that the tests do not exercise: a race with other threads and a silent leak from a missing host hook (see the B8 section).

**All three arms pass all eight tests in the test profile (opt-level 1).** In an unoptimized build, every arm fails B5. Each arm has one unified codec walker and no structural fast path.

## Setup

The crate is in `experiments/vm_gc`. All arms share these parts:

- One register bytecode ISA ([isa.rs](src/shared/isa.rs)) and a text assembler ([asm.rs](src/shared/asm.rs)). Bytecode programs replace a compiler. They are in [programs/](programs).
- The dataset generator from `benches/codec.rs`, written in bytecode ([dataset.sasm](programs/dataset.sasm)).
- One open codec interface ([format.rs](src/shared/format.rs)). A `FormatWriter` receives events, and a `FormatReader` is a stateless scanner over `(src, pos)`. Neither trait sees VM values, so the JSON and S-expression formats work unchanged with every arm.
- One byte tracker, one GC trigger policy, fuel, a GC stress mode, and the eight tests ([suite.rs](src/shared/suite.rs)). A conformance test asserts that all arms give the same output, fuel use, and custom-method calls for the B2 and B3 work.

The arms differ in these modules:

| Arm | Model | Modules |
| --- | --- | --- |
| 1 | `Vec<Obj>` mark-sweep heap with `Copy` handles and an explicit root stack | [arm1/](src/arm1) |
| 2 | The Arm 1 heap, interpreter, and codec objects, with `GcScope` and `NoGcScope` lifetime tokens on the host and codec API | [arm2/](src/arm2), and Arm 1's `heap.rs`, `vm.rs`, and `codec_objects.rs` |
| 3 | `Arc` objects, `Arc` and a `Mutex` for mutable objects, and a cycle collector over a `Weak` table | [arm3/](src/arm3) |

All measurements come from one Apple Silicon MacBook Pro with macOS 27.2 and Rust 1.100. Each timing is the minimum over 5 rounds that alternate the arms. Each round runs 5 fresh engines with 10 repetitions each. Code layout changes single rows by up to 10%, and the small Arm 3 rows vary more, so treat differences under 10% as noise. The prototype does not measure x86-64 or WebAssembly, where atomic operations and stack use differ.

## B1: Pure VM Speed and Fuel Metering

The table gives nanoseconds per user for `build_walk` and per loop iteration for `micro`. Cleanup is one full collection after the operation (in Arm 3, a prune of dead `Weak` entries).

| Operation | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| `build_walk` operation | 338 | 334 | 464 |
| `build_walk` cleanup | 52 | 53 | 142 |
| `build_walk` total | 394 | **388** | 609 |
| `micro` operation | **28.0** | 28.1 | 48.4 |
| `micro` cleanup | 0.0 | 0.0 | 1.1 |
| `micro` total | **28.0** | 28.1 | 49.6 |

Arm 3 is slower because register copies and overwrites change reference counts, each container needs two allocations, and each list access takes a mutex. Its cleanup is slower because it frees two allocations per container and then prunes a `Weak` entry for each container.

Every arm stops these cases at the fuel limit, and fuel never refills:
- **Bytecode:** a runaway bytecode loop.
- **Native scans:** a decode that skips a 2 MB value, an object with 100,000 short unknown keys, and 2 MB of whitespace before a value.
- **Long text:** a 16 MB `repeat`, and 1,000 lookups of a 100 KB map key.

Native work costs one unit of fuel per 16 bytes of input, text, or compared keys, in addition to one unit per value. A decode charges for a skipped value after the skip ends, so one skip can scan past the limit by the length of its input.

## B2: Structural Encode and Decode

The table gives nanoseconds per user for `encode` and `decode`, and per call for the small operations. The `origin/main` column comes from `benches/codec.rs` on the same machine. It includes collections, so compare it with the total rows. `origin/main` also checks types and wraps results in `Result`, which the prototype does not.

| Operation | Arm 1 | Arm 2 | Arm 3 | `origin/main` |
| --- | --- | --- | --- | --- |
| `encode` | 300 | 317 | **270** | 264 |
| `decode` operation | 430 | 433 | 428 | |
| `decode` cleanup | 52 | 52 | 144 | |
| `decode` total | **484** | 488 | 576 | 618 |
| `small_encode` operation | 96 | 97 | 112 | |
| `small_encode` cleanup | 6.8 | 6.9 | 0.0 | |
| `small_encode` total | **104** | 105 | 112 | 119 |
| `small_decode` operation | 90 | 89 | 116 | |
| `small_decode` cleanup | 6.7 | 6.7 | 6.1 | |
| `small_decode` total | 97 | **95** | 122 | 219 |

Arm 3 encodes fastest because it borrows strings and fields directly. Arms 1 and 2 look up the heap slot again for each field. Arm 3 decodes small inputs slowest because each `Address` needs two allocations, against one in Arms 1 and 2.

## B3: Interleaved Custom Callbacks

`Address` and `Status` implement `Encode` and `Decode` in bytecode. Each arm counts its calls of custom methods, and B3 checks the counts for encode and decode. The units are the same as in B2.

| Operation | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| `encode` | **548** | 594 | 581 |
| `decode` operation | 732 | 747 | 731 |
| `decode` cleanup | 50 | 53 | 142 |
| `decode` total | **788** | 801 | 875 |
| `small_encode` operation | 240 | 256 | 304 |
| `small_encode` cleanup | 13.5 | 14.0 | 6.4 |
| `small_encode` total | **254** | 271 | 311 |
| `small_decode` operation | 288 | 307 | 343 |
| `small_decode` cleanup | 14.1 | 13.6 | 14.1 |
| `small_decode` total | **302** | 321 | 358 |

Two callbacks add 248 ns per user to the Arm 1 encode, 277 ns to Arm 2, and 311 ns to Arm 3. In Arm 3, each `Encoder` method call clones the `Encoder` handle and locks it twice.

B3 also runs a callback that removes the list that the walker is encoding from its parent, and then collects garbage. Every arm keeps that list alive. An encoder fails if a callback changes the length of a list or map that it is encoding.

## B4: GC Under Memory Pressure

All arms pass all four cases.

| Case | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| 70% dead acyclic objects in a 32 MB cap, then B2 and B3 | pass, 1 collection | pass, 1 collection | pass, 1 collection |
| 70% dead cycles in a 32 MB cap, then B2 and B3 | pass, 1 collection | pass, 1 collection | pass, 1 collection |
| Callback garbage in one decode (48 MB cap) | pass, 9 collections, 30 ms | pass, 9 collections, 30 ms | pass, 21 collections, 55 ms |
| One allocation larger than the cap | rejected before memory grows | rejected before memory grows | rejected before memory grows |
| A 1 MiB string encode under a 7 MiB cap | pass | pass | pass |
| Smallest cap for the B2 and B3 work, 4,000 users | 6.2 MB | 6.2 MB | 7.7 MB |
| Smallest cap for the B2 and B3 work, 5,000 users | 10.4 MB | 10.4 MB | 9.8 MB |

Every arm tracks more than 30% of the 32 MB cap while it runs the work without collecting, so each run must reclaim the garbage to finish.

Neither model needs less memory at every size, because each arm's tables double at different sizes. Arm 3 collects more often during the callback-garbage decode, because of an accounting choice: it counts a dead container's bytes until the next collection prunes its `Weak` entry, even after it frees the container's inner buffer. Exact counting would need a tracker handle in each container (8 bytes) and an atomic subtraction on each drop.

No arm limits the work of collections. A program that keeps its heap a few bytes under the cap runs one full collection per allocation. In one reviewer probe, 20,000 such allocations took 7.5 s in Arm 1 on about 100,000 fuel. A fix needs a product decision: fail after repeated collections that free too little, or charge fuel for collection work.

## B5: Deep Recursion and Callback Stack Safety

All arms pass on a 512 KB thread in the test profile (opt-level 1, as in the SandLang workspace). The table gives the deepest `Node` list that each arm handles before its stack check stops it with a clean error.

| Check | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| Structural encode and decode (500 required) | 563 | 544 | 526 |
| Structural encode only | 907 | 960 | 1,019 |
| Drop of a 200,000-node list and a 200,000-object host chain | pass | pass | pass |
| Nested custom callbacks | 168 | 118 | 128 |

B5 asserts that callbacks overflow at 400 levels, a depth that the structural walker handles. The margin over 500 structural levels is only 5–13%, so small code changes can break it.

**At opt-level 0, every arm fails B5: structural nesting stops at 16 to 18 levels, and callbacks stop at 6 to 8.** The walkers recurse on the Rust stack, and each level of an unoptimized build uses about 14 KB. An explicit work stack in the structural walker would remove the structural limit. It would not remove the callback limit: a nested callback runs the interpreter on the Rust stack, and the interpreter calls the walker again. That limit needs callbacks that run as resumable VM frames. The cost of that design differs by arm:
- **Arms 1 and 2:** the suspended walker state holds handles that the collector must trace.
- **Arm 2:** its scope lifetimes cannot follow values on a heap stack, so it loses its compile-time check there.
- **Arm 3:** the suspended state holds owned values, which need nothing extra.

## B6: Async Yield and `Send`

All arms pass. Fiber 1 holds a `User` and a long string across `.await`, and fiber 2 collects garbage. Fiber 1 then reads the user's name, email, and a tag from its mutable `tags` list. The compiler checks that `Engine` and its run future implement `Send`. After 50 more rounds of fibers, tracked bytes do not grow.

Arms 1 and 2 root the held values in a non-LIFO async root table. If Arm 1 skips that table during marking, B6 fails with `type error: expected struct`, because the slot now holds another object. Arm 3 moves the values into the future and reads them after `.await` without the VM.

## B7: Memory Footprint and GC Pause

The table gives the footprint of live users. A counting global allocator measures process bytes in a dedicated binary. Each pause is the fastest of 15 full collections over 3 runs.

| Users | Arm 1 tracked bytes | Arm 3 tracked bytes | Arm 3 / Arm 1 | Arm 1 pause | Arm 3 pause |
| --- | --- | --- | --- | --- | --- |
| 1,000 | 698,630 | 1,068,094 | 1.53 | 0.02 ms | 0.09 ms |
| 4,000 | 2,812,094 | 4,263,286 | 1.52 | 0.07 ms | 0.38 ms |
| 5,000 | 4,475,046 | 5,049,838 | 1.13 | 0.09 ms | 0.48 ms |
| 10,000 | 8,955,950 | 10,096,614 | 1.13 | 0.19 ms | 0.95 ms |

Arm 2 has the same footprint as Arm 1. For 5,000 users, Arm 1 makes 33,325 process allocations and Arm 3 makes 66,656, because Arm 3 needs two allocations per container and two per long string. Safe Rust cannot store an immutable struct in one allocation: `Arc<[Value]>` is a fat pointer, and a `Value` must fit in 16 bytes. The Arm 1 tracker counts 1.3% more than the process allocates, because it charges a whole byte for each slot's mark bit. The Arm 3 tracker matches the process exactly.

The Arms 1 and 2 collector is a stop-the-world mark-sweep, and its pause grows with the live heap (about 0.19 ms per 10,000 users). A production version of Arm 2 at large heap sizes would need incremental or generational marking, and with it a write barrier on every heap store. B1 does not measure that barrier.

## B8: Host Module Ergonomics and Isolation

All arms pass all four integrations:

- `list.map` calls a Sand closure per element. One closure shrinks the input list. Another allocates and grows the input list. Neither call deadlocks.
- The S-expression format encodes and decodes the dataset, with and without custom callbacks.
- A host object stores a Sand closure that captures the host object. A collection keeps the closure alive while only the host object holds it. A collection frees the cycle after the last reference drops, and so does a drop of the engine.
- Two engines run interleaved calls, then run on two threads at the same time.

The table counts code lines: not blank, not comment-only, and not unit tests. Run `python3 loc.py` for the per-file counts.

| Lines | Arm 1 | Arm 2 | Arm 3 |
| --- | --- | --- | --- |
| VM, GC, and codec core | 1,914 | 2,353 | 2,107 |
| Host integrations | 95 | 100 | 98 |

The Arm 2 core includes the 1,264 Arm 1 heap, interpreter, and codec-object lines, the 408-line scope layer, and a 638-line codec. All arms also use 794 shared lines for the codec interface and the two formats.

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

"Stress test fails" means that only the GC stress check, which collects at every allocation, finds the mistake. The Arm 2 encoder mistake compiles because the walked list arrives as a function parameter, which the compiler does not check. The faults are all in host and codec code. The shared interpreter, which makes its own rooting decisions, has no compile-time checks in either Arm 1 or Arm 2.

For Arm 3, the faults are locks held across a callback in `list.map`, `host.fire`, and the list encoder, and a host hook that skips its callback. All four fail a test with or without the stress check. A held lock fails with "value is in use by a native call". A missing hook is silent in production: it leaks the cycle, and only the test's drop counter sees it.

Arm 3 has one failure mode that no test covers. Its values are `Send` and `Sync`, so a native can move one to another thread. If that thread changes a container while the engine collects, the collector sees an inconsistent graph and can clear a live container. A reviewer reproduced this after 2,231 collections. A fix would tie mutable containers to their engine, so that only the engine thread can change them, and Arm 3 natives could then no longer read mutable containers inside a future.

## Findings for Every Arm

These findings apply to whichever arm SandLang adopts.

- **Tests need a GC stress mode.** Two of the six rooting mistakes passed every test until collections ran at every allocation. The stress mode also missed zero-byte allocations until the second review. Before an earlier fix, stale registers kept garbage alive in Arm 1, which hid a missing root from every test.
- **Fuel must charge per input byte and per compared byte.** At first, the walkers charged one unit per value, and map lookups ignored key length. A 1.2 MB object of short keys decoded with almost no fuel, and 20,000 lookups of a 1 MiB key ran on about 100,000 fuel.
- **Output reservations must follow the escaped length.** The first JSON writer reserved six bytes per text byte, so a 1 MiB string needed about 8 MiB. Every walker now also charges any growth that a format makes beyond its declared bound.
- **`std::sync::Mutex` allocates on first lock on macOS.** A probe of 1,000 mutexes made 1,000 allocations, which the tracker cannot see. Arm 3 uses `parking_lot::Mutex`, which does not allocate.
- **A `while let` over `lock()` holds the guard through the loop body.** The first Arm 3 `list.map` had this bug, so its callback found the list locked. Read the item in a `let ... else` statement, which drops the guard at the end of the statement.
- **Borrowing an inline string revalidates its UTF-8.** `ShortStr::as_str` calls `str::from_utf8`, which takes 8% of the Arm 1 B3 encode. A writer API that accepts the inline bytes directly would avoid it in safe Rust.
- **Inlining changes unrelated timings.** An inline `Concat` fast path in the dispatch loop made unrelated operations 30% slower. Rare operations now live in functions outside the loop.
- **The event interface costs some encode speed.** The Arms 1 and 2 encode is 14–20% slower than `origin/main`, which writes JSON directly from VM values.

## Limits of the Prototype

- Programs are bytecode, because the prototype has no compiler.
- `Some(x)` is `x` itself, so the prototype does not support nested options. It has no `bytes` or tuple types, because the dataset does not need them.
- Maps are vectors in insertion order with linear lookup.
- In Arms 1 and 2, handles are bare slot indexes. A handle from another engine, or a stale handle, names a valid slot that holds another object. A variant with a generation in each handle, which would turn these mistakes into errors, is not yet measured.
- These buffers are not tracked against the memory limit:
  - **Decoder scratch buffer:** one string token bounds it.
  - **Arm 3 drop work stack:** it lasts only for one drop, including the drop of contents that a collection clears.
- The JSON scanner requires commas between the items that it reads, but it skips an unknown value without checking its separators.

## How to Reproduce

Run these commands in `experiments/vm_gc`:

```sh
cargo test                                                   # B1 to B6 and B8 for all arms, and conformance
cargo run --release --bin bench                              # B1 to B3 timings
cargo run --release --bin footprint                          # B7
cargo test --release --test depth -- --ignored --nocapture   # B4 caps
cargo test --test depth nesting -- --ignored --nocapture     # B5 depth limits in the test profile
python3 loc.py                                               # B8 line counts
python3 faults.py                                            # fault injection
```
