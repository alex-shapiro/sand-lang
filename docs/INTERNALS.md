# Sand implementation internals

This document holds internal implementor information that is not available locally:
a map of the source tree and rules that span several files. Module-specific
functionality should be kept in its module file, as either a module doc or next
to code.

## Map

```
source    ─lexer────▶ tokens     src/syntax/lexer.rs
tokens    ─parser───▶ AST        src/syntax/parser.rs
AST       ─checker──▶ typed HIR  src/check/, src/hir.rs
typed HIR ─compiler─▶ bytecode   src/compile/
bytecode  ─VM───────▶ effects    src/vm/
```

- `src/engine.rs`: `Engine`, `Config`, the `Registry` of natives and module
  sources, module discovery; `src/modules.rs`: the standard library's
  `Snapshot` and a compilation's `ModuleGraph`.
- `src/types.rs`: types and interned names; `src/check/persist.rs`: the
  persistent tables that make copying the checker cheap.
- `src/check/help.rs`: `help` and `members` compile-time intrinsics.
  `scan_imports` preloads modules referenced in `help` and `members` calls.
  `src/check/docs.rs` builds structured module documentation for `Engine::docs`.
- `src/vm/`: values, heap, interpreter, scheduler, metering (`meter.rs`),
  memory held by pending I/O (`iomem.rs`).
- `src/stdlib/`: the standard library; `src/native/`: the `NativeModule`
  API for the standard library and host modules.
- `src/policy/`: capabilities and policies (`rules.rs` decides requests by
  rule indices: a trie of path patterns, hash tables of hosts); policy files
  (`file.rs`) and their formatter and linter; `src/files/`: the file systems
  scripts see (the host's through `unix.rs`/`windows.rs`, or a `MemFs`).
- `src/platform.rs`: time, timers, randomness, threads (`offload` for
  blocking calls, `on_deep_stack` for the compiler and formatter);
  `src/stdio.rs`: the writer for the process's standard output and error.
- `src/session.rs`: REPL sessions; `src/feed.rs`: standard input that the
  host passes to scripts; `src/fmt.rs`, `src/lint.rs`: tools (the linter
  runs inside a compilation).
- `cli/`: the `sand` command; `wasm/`: the WebAssembly bindings; `site/`: the static site generator and web playground.

## Natives

Each standard module `foo` pairs `src/stdlib/foo.sand` (declarations, docs,
Sand helpers, and `native fn` items) with `src/stdlib/foo.rs`, which registers
each native Rust function in a `NativeModule`:

| Declaration                                                    | Registration name                          |
| -------------------------------------------------------------- | ------------------------------------------ |
| `native fn bar` in module `foo`                                | `bar` (or `foo.bar` in `core.rs`)          |
| `native fn bar` in `struct`/`enum`/`extend`/`impl ... for Baz` | `Baz.bar` (or `foo.Baz.bar` in `core.rs`)  |
| in the prelude: `extend str`, `[T]`, `[K: V]`, ...             | `str.bar`, `List.bar`, ...                 |

- The checker verifies each `native fn` declaration against the `Signature`
  derived from the Rust function parameters and return type. Use `Val<T>` to
  reference a heap value without copying it, and `Param<N>` for the `N`-th type
  parameter.
- The VM passes arguments in declaration order with default values filled in,
  preceded by a `Reflect<N>` argument for each `reflect` type parameter. For
  trait method calls, the VM strips the leading trait dictionary before calling
  the native function.
- Never panic on script input. The engine catches Rust panics at scheduling
  slice boundaries and aborts the run with `Error::Internal`.
- Check permissions (`vm.permit`, or `vm.require` for a `permission_denied`
  error) before any side effect, because the engine re-runs the native function
  after a permission prompt.
- Reserve memory and acquire resource slots before mutating state, because the
  engine runs garbage collection and re-runs the native function on
  `CtlKind::Collect`. Return-value conversion skips the memory limit check; the
  VM runs garbage collection and checks the memory limit after the call
  completes.
- Return `Result<T, HostError>` from functions whose Sand return type is
  `Result<T>`. `Err(HostError::new(kind, message))` produces a Sand `Err(Error)`
  value, whereas `?` converts a `Ctl` into a `HostError` that halts or suspends
  the VM. Return `Result<T, Ctl>` from fallible functions whose Sand return
  type is `T`; `Err(Ctl::fail(kind, message))` aborts the run with a runtime
  error.
- Return `Later::new(future)` from async functions. The future must not hold
  raw `Value`s; it resolves to a closure that constructs the result on the VM
  thread (`stdlib::stream::suspend` wraps this pattern for I/O).
- Register opaque types (`ValueType`, `#[derive(SandObject)]`) and unit enums
  (`#[derive(SandEnum)]`) on `NativeModule` so the checker declares them from
  their Rust definitions and doc comments.
- Never store `Value`s inside a `NativeObject`: the garbage collector does not
  trace native objects, and `native_mut` skips the write barrier. Report
  dynamic heap growth inside a `NativeObject` with `vm.heap.account`.
- If `ValueType::PACKING` is `Some`, the VM stores the value inline in
  `Obj::Packed`; otherwise the VM boxes the value in `Obj::Value`.
- To construct a Sand struct from a native function, return a tuple of its
  fields to a Sand wrapper function. Never construct `Obj::Variant` directly,
  because single-payload variants use an inline representation required for
  equality and hashing.
- Use `vm.heap.staged` to construct heap values from existing heap values in
  place without intermediate copies.
- Use `value::FxHashMap` (seeded per process) for tables keyed by script data so
  scripts cannot trigger hash collisions.

## Metering

Every limit (instructions, CPU and wall time, interrupts) is enforced
through the meter. Work it does not see is unbounded (cost model:
`vm/meter.rs`, SPEC §15.2).

- A native whose work grows with its inputs must charge for it
  (`vm.charge`, `charge_bytes`, `charge_slow_bytes`): long loops, prior to
  work that cannot be split, and for every argument it scans.
  Unpredictable work must be charged by time (`stdlib/regex.rs`).
- Code with only `&Heap` charges a `meter::Budget`; `compare` and `display`
  have `*_metered` variants. Sort with `compare::try_sort_by`.
- Bytecode only moves forward between safe points. Every branch back must
  be a safe point that charges fuel (`Op::is_back_edge`, checked by
  `compile::assemble`). Back edges and calls also charge per 64
  instructions of the loop or function (`meter::code_fuel`), so a large
  body cannot run between checks for free.
- The checker, compiler, formatter and linter charge a `CompileBudget` for
  all work that grows with the source, and stop when it is spent.
  `TypeDesc`s are DAGs: never walk one as a tree without bounding the work.

## Memory

`max_memory` counts heap objects, fiber stacks and reserved buffers.

- Reserve Rust-side buffers and container growth before allocating
  (`vm.reserve`, `Heap::grown_capacity`); grow buffers of unknown size
  through `meter::Scratch`.
- Buffers that pending I/O fills off the heap are reserved in `vm/iomem.rs`
  and released in the finisher just before the value is allocated. Never
  reserve a size a peer merely announces (`Content-Length`).
- Values with shared structure can display, encode or compare far larger
  than their heap size; bound such work by what it produces.
- Quote script text in messages through `stdlib::truncate`.

## Resources

An object holding an OS resource uses an `iomem::ResourceSlot` while open.
It counts againt `max_open_resources` and returns `true` from `is_resource()`.
Allocating it paces collection, which closes dropped resources. Take the slot
before opening (`vm.resource_slot`, `resource_slot_with` for fixed buffers),
keep it in the pending operation, and move it into the object in the finisher
(`net::alloc_resource`). Closing or collecting the object drops it.

## Sandboxed I/O

- Files: resolve script paths with `vm.host.fs.target` and act only through
  the returned `Target` and `files::Fs`, inside `vm.offload`. It runs the
  work on a worker thread and stops waiting when the meter reports a limit
  or an interrupt. Work that grows with its input charges the job's
  `Allowance`. Never call path-based `std::fs` or `tokio::fs` on script
  paths. Operations on whole trees first scan them for protected entries
  (`fs::scan_tree`).
- Network: connect only through `net::connect_checked` (it checks every
  resolved address against the policy), TLS through `net::tls::connect`,
  and bound every wait on a peer with a timeout.
- Use `src/platform.rs` to access time, timers, randomness and thread APIs.
  Everything that reaches the OS is behind the `os` feature.

## Heap and VM

- Collection happens only at safe points, never inside natives.
  Natives may keep `Value`s in Rust locals.
- Store `Value`s into heap objects only through the barriered accessors,
  `Heap::get_mut` and those built on it. Never keep a `&mut Obj` across
  a safe point (`vm/heap.rs`).
- Only a fiber's live window (up to the innermost frame's `base + nregs`)
  is a root: the compiler must never read a register before writing it.
- Interpreter speed depends on register allocation across its loop;
  see the rules in `vm/interp.rs` before changing it.
- A REPL session only appends to the checker's tables and to the `Program`:
  nothing a live value refers to (protos, types, variants, globals,
  constants, natives) is ever renumbered.

## Tests

- `cargo test --test spec_runner <file>` runs `tests/spec/<file>.test`;
  `SPEC_FILTER=substring` limits it to matching cases. Every spec case is run
  on the host and memory file systems, with `Config::gc_stress`, and with
  `Config::optimize(false)`.
- `tests/props/` compares Sand execution against reference models on
  generated inputs. Set `PROPTEST_CASES=n` to increase the case count.
- `cli/tests/cli.rs` runs the `sand` binary; `cli/tests/prompt_tty.rs`
  drives prompts on a pseudo-terminal.
- `tests/fuzz.rs` holds randomised tests and the reproducers found by the
  `cargo fuzz` targets in `fuzz/`.
- `SAND_GC_STRESS=1` collects at every safe point;
  `SAND_DUMP_BYTECODE=1` prints the compiled code; `SAND_GC_STATS=1` (`sand
  run`) prints collector statistics; `cargo bench` prints the
  geometric mean of the benchmarks' best times.
