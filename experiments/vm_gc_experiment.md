# VM and GC Architecture Experiment

This experiment compares six VM and garbage collection designs on one shared workload. Each arm tests a single concrete design. The results will give us measured data to pick SandLang's runtime model.

## Notes

`AGENTS.md` requires 100% safe Rust, and `crates/sandlang/tests/api.rs` requires `Engine: Send`. This experiment treats all six arms as equal candidates, including arms that relax one of those two rules. Implement every arm to the same standard of quality and performance.

- Do not skip or warn against Arms 2 and 3 because `gc-arena` uses `unsafe` internally. Keep all `unsafe` code inside the GC crate, and keep the VM, stdlib, and host API safe Rust.
- Do not skip or shortchange Arm 6 because `Rc` is `!Send`. Implement both Arm 5 and Arm 6 as real candidates.

## Background

SandLang is a single-threaded fiber VM with two distinct native call styles:

- Codec callbacks are synchronous. They run on the active fiber and forbid task switches, waits, and permission prompts.
- I/O natives such as `Later::new` are asynchronous. They yield across `.await` so other fibers can run and collect garbage while the native holds Sand values.

On `origin/main`, `Encode` and `Decode` are marker traits driven by reflection inside `Heap::staged`. User Sand code cannot write custom `Encode` or `Decode` methods on `origin/main`.

Branch `encode-decode-traits-v4` is in git for reference. It turned `Encode` and `Decode` into regular traits with synchronous callbacks, but became an exercise in working around VM and GC constraints rather than building a small, coherent trait on a solid foundation:

- The general callback walkers ran roughly twice as slow as `origin/main`.
- Recovering speed required duplicating them with a second set of fast-path walkers, pushing the branch to a large line count.
- Avoiding large object copies across native calls was difficult under `&Heap` vs `&mut Heap` borrow rules.
- Natives could not hold VM objects across calls cleanly, forcing manual root stacks and session state in `vm.native_state`.

**The purpose of this experiment is to avoid repeating past failures. We want to find out which VM and GC foundation allows for native <=> VM interop such that regular traits with callbacks stay small, coherent, and fast without workarounds.**

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
- Small implementation size, with any `unsafe` code isolated in a small GC crate.

## The Six Arms

### Arm 1: Current `Vec<Obj>` Model

Flat `Vec<Obj>` heap indexed by `Copy` handles, using `Staging` for pure bulk work and an explicit root stack for callbacks. 100% safe Rust and `Send`.

Clean up two known flaws from `encode-decode-traits-v4` so this arm is a fair baseline: read JSON input by heap handle instead of copying the string, and store codec state on `Encoder` and `Decoder` instead of `vm.native_state`.

### Arm 2: `gc-arena` with Synchronous Callbacks

`Copy` branded pointers allocated through a shared `&Mutation<'gc>` token. Safe VM, stdlib, and host code; `unsafe` stays inside `gc-arena`. `Arena` is `Send` outside a turn.

Code reads `&'gc str` and allocates at the same time, removing `Staging`. Callbacks run synchronously on the Rust stack inside one turn. Collection stays paused until that outer turn returns, so temporary callback garbage accumulates until then.

### Arm 3: `gc-arena` with Yielding Turns

Same `gc-arena` heap as Arm 2, but exits the turn on custom callbacks and GC thresholds. Safe VM, stdlib, and host code; `Arena` is `Send` between turns.

Values kept across a turn boundary are stashed as roots across `.await` or stored in a `Collect` state machine. Collection runs between turns without rolling back finished work.

### Arm 4: Nova-Style `Vec<Obj>` with `GcScope`

Flat `Vec<Obj>` heap like Arm 1, with compile-time root checking via `GcScope` and `NoGcScope`. 100% safe Rust and `Send`.

Fast unrooted code borrows `NoGcScope`. Functions that may collect consume `GcScope` by value so Rust rejects unrooted handles used across a GC point. Values kept across callbacks use `Scoped` roots, and strings are read by handle inside `NoGcScope`.

### Arm 5: `Arc` with Cycle Collector

Each heap object lives behind `Arc`, paired with a Bacon-Rajan cycle collector. 100% safe Rust and `Send`.

Cloning a string pointer takes constant time without copying bytes. Natives hold `&str` across allocations, callbacks, and `.await` with zero manual rooting and without `Staging`. Only mutable containers and closures enter the cycle collector; immutable strings, primitives, and leaf structs skip it.

### Arm 6: `Rc` with Cycle Collector

Same design as Arm 5, using non-atomic `Rc` instead of `Arc`. 100% safe Rust and `!Send`. If selected, SandLang would either relax `Engine: Send` or add an isolated `unsafe` single-engine `Send` wrapper.

## Experimental Harness

Build one disposable prototype crate in `experiments/vm_gc`. Every arm implements the same minimal register VM with closures, trait dispatch, native calls, an async yield point, and the types needed for the dataset below.

Keep measurements fair across all six arms:

- Share one bytecode format, JSON scanner, dataset, and test suite across all arms.
- Use the same register-VM dispatch structure in every arm. Calibrate Arm 1 against `origin/main` in [benches/codec.rs](../crates/sandlang/benches/codec.rs) (`encode` 279 ns, `decode` 624 ns, `small_decode` 218 ns) so dispatch overhead does not hide heap costs.
- Keep primitives unboxed in `VmValue`, and store short strings up to 24 bytes inline with `CompactString` where the heap design allows.
- Share one strict byte tracker checked before every allocation. Count object headers, inner container buffers, root stacks, and cycle-collector tables. Wrap `Gc::new` in Arms 2 and 3 so allocations fail cleanly at `max_memory`.
- Do not add arm-specific two-tier walker shortcuts. Give every arm one equal review and tuning pass before recording final numbers.

### Dataset

Reuse the schema from [benches/codec.rs](../crates/sandlang/benches/codec.rs#L24-L37):

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

## Tests and Benchmarks

Run all eight tests on every arm and report every result.

### B1: Pure VM Speed

Build 5,000 `User` records in bytecode, walk their fields, sum scores, and filter a list. Also run a tight microbenchmark of register copies, field reads, list pushes, and closure calls so heap costs stand out above dispatch.

### B2: Structural Encode and Decode

Encode and decode the large and small datasets without custom Sand callbacks.

### B3: Interleaved Custom Callbacks

Repeat B2 with `Address` and `Status` implemented as Sand bytecode methods calling `Encoder` and `Decoder`.

### B4: GC Under Memory Pressure

Check four cases under a hard `max_memory` cap:

- Fill the heap to 70% with dead acyclic objects, then run B2 and B3.
- Fill the heap to 70% with dead reference cycles, then run B2 and B3. Report acyclic and cyclic runs separately.
- Run B3 with a custom callback that creates temporary garbage in a loop during one decode call.
- Attempt a single string or list allocation larger than `max_memory` and verify rejection before memory grows.

### B5: Deep Interleaved Recursion

Encode and decode a 500-level linked `Node` struct with custom Sand callbacks on a 512 KB thread stack.

### B6: Async Yield and `Send`

Hold a live `User` and `str` in a native across `.await` while another fiber triggers GC. Check at compile time that `Engine` and its run future implement `Send`.

### B7: Memory Footprint and GC Pause

Measure live bytes for 5,000 `User` records and the pause time of one full collection. Compare tracked bytes against actual process allocations to surface hidden allocator overhead.

### B8: Host Module Ergonomics and Isolation

Build four host integrations using only the public host API, and measure lines of code and boilerplate:

- `List.map` calling a Sand closure per element.
- A second format (`SExpr`) implementing both encoding and decoding.
- A host object storing a Sand callback across multiple calls.
- Two independent engines running interleaved in the same process.

## Evaluation

Report all eight tests for all six arms side by side. Note which requirements each arm satisfies or relaxes, then weigh pure VM speed (B1), single-walker codec speed (B2 and B3), memory and GC pauses (B7), and host API simplicity (B8).
