# VM and GC Architecture Experiment

This experiment compares three VM and garbage collection designs on one shared workload. Each arm tests a single concrete design. The results will give us measured data to pick SandLang's runtime model.

All three arms use 100% safe Rust and keep `Engine` and its run future `Send`, matching `AGENTS.md` and [api.rs](../crates/sandlang/tests/api.rs#L56-L62). Implement every arm to the same standard of quality and performance.

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
- Borrowing or reading heap values across interleaved calls without copying input buffers.
- Safe garbage collection while VM and native calls are interleaved.
- Hard `max_memory` limits even if a large native call starts with a mostly full heap.
- Monotonic fuel metering across VM bytecode and native loops.
- `Send` engines and run futures across async yields, matching [api.rs](../crates/sandlang/tests/api.rs#L56-L62).
- 500 levels of interleaved recursion inside a 512 KB stack on every target, including WebAssembly, without `mmap`, signal handlers, or stack scanning.
- Simple host module APIs that let external crates add new encoding formats.
- Low memory overhead and short GC pauses.
- Small total implementation size across both the VM/GC core and host modules, in 100% safe Rust.

## SandLang Value and Mutability Rules

All three arms follow the same language semantics:

- Primitive values (`int`, `float`, `bool`, unit, and payload-less enum tags) and short strings (`<= 14` bytes) stay unboxed inside a 16-byte `VmValue` across all three arms.
- Immutable heap types: long `str` (`> 14` bytes), `bytes`, tuples, enums, and structs without `var` fields.
- Mutable heap types: `list`, `map`, structs with `var` fields, and closure capture cells.

## The Three Arms

### Arm 1: Current `Vec<Obj>` Model

Flat `Vec<Obj>` heap indexed by `Copy` handles, with an explicit root stack for callbacks. 100% safe Rust and `Send`.

This arm tests one unified codec walker that handles both structural types and custom Sand callbacks, with no separate `Staging` fast path for structural-only types. Clean up two flaws from `encode-decode-traits-v4`: read JSON input by rooted heap handle plus byte offset instead of copying the string, and store codec state on `Encoder` and `Decoder` instead of `vm.native_state`.

Trade-offs: code cannot hold `&str` across allocations or callbacks (it re-indexes the heap on each read), and the compiler does not catch forgotten roots.

### Arm 2: Nova-Style `Vec<Obj>` with `GcScope`

Same flat `Vec<Obj>` heap and root stack as Arm 1, with `GcScope` and `NoGcScope` lifetime tokens on top. 100% safe Rust and `Send`.

Fast unrooted code borrows `NoGcScope`. Functions that may collect take `GcScope`, so Rust rejects bound local handles used across `gc.reborrow()`. Values kept across callbacks use `Scoped` roots, and strings are read by handle plus byte offset inside `NoGcScope`.

Trade-offs: runtime layout and speed match Arm 1, and code still cannot hold `&str` across allocations or callbacks. Passing handles into functions that may collect requires `.unbind()` at the call site and `.bind(gc.nogc())` on entry, and forgetting `.bind()` still compiles.

### Arm 3: `Arc` with Cycle Collector

Each immutable heap object lives behind `Arc<T>`, each mutable heap object lives behind `Arc<RwLock<T>>` (or `Arc<Mutex<T>>`), and long strings (`> 14` bytes) live behind an engine-tracked `Arc` that subtracts freed bytes on `Drop`. 100% safe Rust and `Send`.

Cloning a string handle takes constant time without copying bytes. Natives hold an owned string handle across allocations, callbacks, and `.await`, and borrow `&str` directly with zero manual rooting and without `Staging`.

For cycles, use a Python-style reference-count cycle detector over all container types (both immutable and mutable, since an immutable struct can reference a mutable container that points back to it). Leave externally held native values untouched and allocate no new memory at `max_memory`. Implement iterative `Drop` on linked values so deep drops fit on a 512 KB stack.

Trade-offs: atomic refcount traffic on clones and drops, lock overhead on mutable containers, `Weak` cycle-table entries that pin `Arc` headers until pruned, and two heap allocations per tracked long string in 100% safe Rust.

## Experimental Harness

Build one disposable prototype crate in `experiments/vm_gc`. Every arm implements the same minimal register VM with closures, trait dispatch, native calls, a minimal two-fiber cooperative scheduler for `.await`, fuel metering, and the types needed for the dataset below.

Define one open streaming codec interface for `Encoder` and `Decoder` across all arms: scalar reads and writes, open and close list, open and close map or struct, field keys, and enum variant tag and payload. Custom Sand `Encode` and `Decode` methods call these methods synchronously.

Keep measurements fair across all three arms:

- Share one bytecode ISA, test suite, dataset generator, and stateless JSON scanner (`(src: &str, pos: &mut usize)`) across all arms.
- Share one strict byte tracker checked before every allocation. Count object headers, inner container buffers, root stacks, and cycle-collector tables.
- Do not build a second structural-only fast-path codec beside the general callback codec in any arm. Give every arm one equal review and tuning pass before recording final numbers.
- Use the same memory-threshold collection policy across all three arms (running mark-sweep for Arms 1 and 2, and `Weak`-table pruning plus cycle collection for Arm 3, if tracked bytes reach the larger of 2x the last post-sweep live set or a shared minimum threshold, plus emergency sweeps at `max_memory`). In B2 and B3, set that minimum threshold above the working set of one full benchmark run so B2 and B3 isolate steady-state codec speed while B4 tests GC under pressure.
- In B1, B2/B3 `decode`, and B2/B3 `small_encode`, report operation time, post-run cleanup time (one full collection for Arms 1 and 2; dropping the output and pruning dead `Weak` entries for Arm 3), and total combined time. Compare total combined time on `small_decode` and `small_encode`, since Arm 3 drops per-iteration outputs during operation time while Arms 1 and 2 defer freeing them to cleanup. In B2/B3 `large_encode`, report operation time without forcing a full-heap GC over the live input objects.

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

The large dataset is a `Doc` of 5,000 generated `User` records. It creates about 37,000 heap objects (including 4,990 `email` strings over the 14-byte inline limit) and encodes to 1.28 MB of JSON. The small dataset is one `Address` record run 20,000 times.

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

- Fill the heap to 70% with dead acyclic objects, then run B2 and B3. Treat this case as a correctness and memory-limit check rather than a speed race, since Arm 3 frees acyclic inner buffers on drop while `Weak` entries pin `Arc` headers until pruned.
- Fill the heap to 70% with dead reference cycles, then run B2 and B3.
- Run B3 with a custom callback that allocates temporary garbage in a loop inside one decode call.
- Attempt a single string or list allocation larger than `max_memory` and verify rejection before memory grows.

### B5: Deep Interleaved Recursion

Define `struct Node { value: int, next: Node? }`. Encode, decode, and drop a 500-level linked `Node` with custom Sand callbacks that recurse through the native `Encoder` and `Decoder` walker at every level on a 512 KB native thread stack in debug builds (matching [api.rs](../crates/sandlang/tests/api.rs#L154-L167)). Note that WebAssembly stack limits are a follow-up check for any winning arm.

### B6: Async Yield and `Send`

Run two cooperative fibers on one VM. Fiber 1 enters a native, holds a live `User` and `str` across `.await`, and yields. Fiber 2 allocates objects and triggers GC. Fiber 1 resumes and reads its `User` and `str`. Check at compile time that `Engine` and its run future implement `Send`.

### B7: Memory Footprint and GC Pause

Measure live bytes for 5,000 `User` records and the pause time of one full collection (mark-sweep for Arms 1 and 2; `Weak`-list cycle sweep for Arm 3). Measure actual process allocations via a counting global allocator in a dedicated B7 measurement mode, never enabled during B1 to B3 timing runs.

### B8: Host Module Ergonomics and Isolation

Build four host integrations using only the public host API:

- `List.map` calling a Sand closure per element.
- A second format (`SExpr`) implementing both encoding and decoding.
- A host object storing a Sand callback across multiple calls, verifying that a cycle between the host object and its stored callback is collected.
- Two independent engines running interleaved in the same process.

Report both host-integration lines of code and total VM, GC, and codec core lines of code so complexity cannot hide inside the VM crate.

## Evaluation

Report all eight tests for all three arms side by side. Weigh pure VM speed (B1), single-walker codec speed (B2 and B3), memory and GC pauses (B7), and total simplicity across VM core and host APIs (B8).
