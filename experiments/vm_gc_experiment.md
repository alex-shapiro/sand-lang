# VM and GC Architecture Experiment

This experiment compares six VM and garbage collection designs on one shared workload. Each arm tests a single concrete design. The results will give us measured data to pick SandLang's runtime model.

## Notes

`AGENTS.md` requires 100% safe Rust, and `crates/sandlang/tests/api.rs` requires `Engine: Send`. This experiment treats all six arms as equal candidates, including arms that relax one of those two rules. Implement every arm to the same standard of quality and performance.

- Do not skip or warn against Arms 2 and 3 because `gc-arena` uses `unsafe` internally. Keep all `unsafe` code inside the GC crate, and keep the VM, stdlib, and host API safe Rust.
- Do not skip or shortchange Arm 6 because `Rc` is `!Send`. Implement both Arm 5 and Arm 6 as real candidates.

## Background

On `origin/main`, `Encode` and `Decode` are marker traits driven by reflection inside `Heap::staged`. `Heap::staged` freezes `&Heap` for reading and buffers new allocations in a write-only `Staging` buffer until the call finishes. User Sand code cannot write custom `Encode` or `Decode` methods on `origin/main`.

Branch `encode-decode-traits-v4` is in git for reference. It turned `Encode` and `Decode` into regular traits with synchronous callbacks, but became an exercise in working around VM and GC constraints rather than building a small, coherent trait on a solid foundation:

- The general callback walkers ran roughly twice as slow as `origin/main`.
- Recovering speed required a second fast-path codec beside the general callback codec, pushing the branch to a large line count.
- Avoiding large input-buffer copies across native calls was difficult under `&Heap` vs `&mut Heap` borrow rules.
- Natives could not hold VM objects across calls cleanly, forcing manual root stacks and session state in `vm.native_state`.

The purpose of this experiment is to avoid repeating those failures. We want to find out which VM and GC foundation lets regular traits with callbacks stay small, coherent, and fast without workarounds.

SandLang is a single-threaded cooperative fiber VM with two distinct native call styles:

- Codec callbacks are synchronous. They run on the active fiber and forbid task switches, waits, and permission prompts.
- I/O natives such as `Later::new` are asynchronous. They yield across `.await` so other fibers can run and collect garbage while the native holds Sand values.

## Goals

We want a runtime model that meets these requirements:

- Fast pure VM execution without a JIT.
- Clean interleaving of VM and native calls, including regular `Encode` and `Decode` traits.
- Borrowing heap values across interleaved calls without copying input buffers.
- Safe garbage collection while VM and native calls are interleaved.
- Hard `max_memory` limits even if a large native call starts with a mostly full heap.
- Monotonic fuel metering across VM bytecode and native loops.
- `Send` engines and run futures across async yields, matching [api.rs](../crates/sandlang/tests/api.rs#L56-L62).
- 500 levels of interleaved recursion inside a 512 KB stack on every target, including WebAssembly, without `mmap`, signal handlers, or stack scanning.
- Simple host module APIs that let external crates add new encoding formats.
- Low memory overhead and short GC pauses.
- Small total implementation size across both the VM/GC core and host modules, with any `unsafe` code isolated in a small GC crate.

## SandLang Value and Mutability Rules

All six arms follow the same language semantics:

- Primitive values (`int`, `float`, `bool`, unit, and payload-less enum tags) stay unboxed inside `VmValue`.
- Each arm uses its natural string layout (`CompactString` inside the heap string object for Arms 1 to 4, `EcoString` for Arm 5, and a matching non-atomic inline string or `Rc<str>` for Arm 6) instead of double-boxing strings. Note inline-string effects separately in B7.
- Immutable heap types: `str`, `bytes`, tuples, enums, and structs without `var` fields.
- Mutable heap types: `list`, `map`, structs with `var` fields, and closure capture cells.

## The Six Arms

### Arm 1: Current `Vec<Obj>` Model

Flat `Vec<Obj>` heap indexed by `Copy` handles, with an explicit root stack for callbacks. 100% safe Rust and `Send`.

This arm tests one unified codec walker that handles both structural types and custom Sand callbacks, with no separate `Staging` fast path for structural-only types. Clean up two flaws from `encode-decode-traits-v4`: read JSON input by heap handle instead of copying the string, and store codec state on `Encoder` and `Decoder` instead of `vm.native_state`.

### Arm 2: `gc-arena` with Synchronous Callbacks

`Copy` branded pointers allocated through a shared `&Mutation<'gc>` token. Safe VM, stdlib, and host code; `unsafe` stays inside `gc-arena`. `Arena` is `Send` outside a turn.

Code reads `&'gc str` and allocates at the same time, without `Staging`. Callbacks run synchronously on the Rust stack inside one turn. Collection stays paused until that outer turn returns, so temporary callback garbage accumulates until then.

### Arm 3: `gc-arena` with Yielding Turns

Same `gc-arena` heap as Arm 2, but exits the turn on custom callbacks, fuel or GC checkpoints, and async yields. Safe VM, stdlib, and host code; `Arena` is `Send` between turns.

Values kept across a turn boundary are stashed as roots across `.await` or stored in a `Collect` state machine. Collection runs between turns without rolling back finished work.

### Arm 4: Nova-Style `Vec<Obj>` with `GcScope`

Flat `Vec<Obj>` heap like Arm 1, with compile-time root checking via `GcScope` and `NoGcScope`. 100% safe Rust and `Send`.

Fast unrooted code borrows `NoGcScope`. Functions that may collect consume `GcScope` by value so Rust rejects unrooted handles used across a GC point. Values kept across callbacks use `Scoped` roots, and strings are read by handle inside `NoGcScope`.

### Arm 5: `Arc` with Cycle Collector

Each immutable heap object lives behind `Arc<T>`, strings use `EcoString` (inline up to 15 bytes, single atomic refcounted buffer above that), and each mutable heap object lives behind `Arc<RwLock<T>>` (or `Arc<Mutex<T>>`) so the engine stays `Send` in 100% safe Rust.

Cloning a string handle takes constant time without copying bytes. Natives hold an owned `EcoString` across allocations, callbacks, and `.await`, and borrow `&str` from it with zero manual rooting and without `Staging`.

For cycles, every mutable container registers a `Weak` pointer in a per-engine list. On memory pressure, the VM traces from VM roots through both immutable objects and mutable containers, clears the inner elements of any live unmarked mutable container so reference counting frees the cycle, and prunes dead `Weak` entries. Implement iterative `Drop` on linked values so deep drops fit on a 512 KB stack.

### Arm 6: `Rc` with Cycle Collector

Same design as Arm 5, using non-atomic `Rc<T>` for immutable objects, a matching non-atomic inline string (or `Rc<str>`) for strings, and `Rc<RefCell<T>>` for mutable objects. 100% safe Rust and `!Send`.

Arm 6 runs the async fiber yield test in B6 without the compile-time `Send` assertion and records `!Send` as a relaxed requirement. If selected, SandLang would either relax `Engine: Send` or add an isolated `unsafe` single-engine `Send` wrapper.

## Experimental Harness

Build one disposable prototype crate in `experiments/vm_gc`. Every arm implements the same minimal register VM with closures, trait dispatch, native calls, a minimal two-fiber cooperative scheduler for `.await`, fuel metering, and the types needed for the dataset below.

Define one open streaming codec interface for `Encoder` and `Decoder` across all arms: scalar reads and writes, open and close list, open and close map or struct, field keys, and enum variant tag and payload. Custom Sand `Encode` and `Decode` methods call these methods synchronously.

Keep measurements fair across all six arms:

- Share one bytecode ISA, test suite, dataset generator, and stateless JSON scanner (`(src: &str, pos: &mut usize)`) across all arms.
- Let Arm 3 suspend and resume bytecode execution across turn boundaries without forcing state-machine overhead onto other arms.
- Share one strict byte tracker checked before every allocation. Count object headers, inner container buffers, root stacks, and cycle-collector tables. Wrap `Gc::new` in Arms 2 and 3 so allocations fail cleanly at `max_memory`.
- Do not build a second structural-only fast-path codec beside the general callback codec in any arm. Give every arm one equal review and tuning pass before recording final numbers.
- Use the same GC trigger policy across Arms 1 to 4 (such as collecting if allocated bytes reach 2x the last post-GC live set, plus emergency collection at `max_memory`) so B1 to B3 compare equal collection cadence.
- In B1, B2, and B3, report both operation time (including any GC triggered during the run) and post-run cleanup time (one full collection for Arms 1 to 4; dropping the output for Arms 5 and 6) separately.

### Dataset and Baseline

Reuse the schema and generator from [benches/codec.rs](../crates/sandlang/benches/codec.rs#L24-L37):

```sand
enum Status { Active, Suspended(reason: str), Banned(int, str) }
struct Address { street: str, city: str, zip: str? }
struct User {
    id: int, name: str, email: str, score: float, active: bool,
    tags: [str], address: Address, status: Status,
    logins: [int], meta: [str: int],
}
struct Doc { users: [User] }
```

The large dataset is a `Doc` of 5,000 generated `User` records. It creates about 100,000 heap objects and encodes to 1.28 MB of JSON. The small dataset is one `Address` record run 20,000 times.

For B2 reference only, `origin/main` in `benches/codec.rs` runs at 279 ns per `User` for `encode`, 624 ns per `User` for `decode`, and 218 ns per call for `small_decode`.

## Tests and Benchmarks

Run all eight tests on every arm and report every result.

### B1: Pure VM Speed and Fuel Metering

Run two bytecode workloads without native codecs:

- Build 5,000 `User` records in bytecode, walk their fields, sum scores, and filter a list.
- Run a tight microbenchmark of register copies, field reads, list pushes, and closure calls so heap costs stand out above dispatch overhead.
- Verify that a runaway bytecode loop and a native loop skipping large JSON both stop cleanly at a fuel limit.

### B2: Structural Encode and Decode

Encode and decode the large and small datasets without custom Sand callbacks, using each arm's single unified codec walker.

### B3: Interleaved Custom Callbacks

Repeat B2 with `Address` and `Status` implemented as Sand bytecode methods calling `Encoder` and `Decoder`.

### B4: GC Under Memory Pressure

Check four cases under a hard `max_memory` cap:

- Fill the heap to 70% with dead acyclic objects, then run B2 and B3. Treat this case as a correctness and memory-limit check rather than a speed race, since refcounting arms free acyclic garbage immediately on drop.
- Fill the heap to 70% with dead reference cycles, then run B2 and B3.
- Run B3 with a custom callback that allocates temporary garbage in a loop inside one decode call.
- Attempt a single string or list allocation larger than `max_memory` and verify rejection before memory grows.

### B5: Deep Interleaved Recursion

Define `struct Node { value: int, next: Node? }`. Encode, decode, and drop a 500-level linked `Node` with custom Sand callbacks on a 512 KB native thread stack. Note that WebAssembly stack limits are a follow-up check for any winning arm.

### B6: Async Yield and `Send`

Run two cooperative fibers on one VM. Fiber 1 enters a native, holds a live `User` and `str` across `.await`, and yields. Fiber 2 allocates objects and triggers GC. Fiber 1 resumes and reads its `User` and `str`. Check at compile time that `Engine` and its run future implement `Send` (Arm 6 skips the `Send` assertion and records `!Send`).

### B7: Memory Footprint and GC Pause

Measure live bytes for 5,000 `User` records and the pause time of one full collection (mark-sweep for Arms 1 to 4; `Weak`-list cycle sweep for Arms 5 and 6). Measure actual process allocations via a counting global allocator in a dedicated B7 measurement mode, never enabled during B1 to B3 timing runs.

### B8: Host Module Ergonomics and Isolation

Build four host integrations using only the public host API:

- `List.map` calling a Sand closure per element.
- A second format (`SExpr`) implementing both encoding and decoding.
- A host object storing a Sand callback across multiple calls.
- Two independent engines running interleaved in the same process.

Report both host-integration lines of code and total VM, GC, and codec core lines of code so complexity cannot hide inside the VM crate.

## Evaluation

Report all eight tests for all six arms side by side. Note which requirements each arm satisfies or relaxes, then weigh pure VM speed (B1), single-walker codec speed (B2 and B3), memory and GC pauses (B7), and total simplicity across VM core and host APIs (B8).
