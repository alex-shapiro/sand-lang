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
- `src/stdlib/`: the standard library; `src/native.rs`: the `NativeModule`
  API for hosts.
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

A module `foo` is `src/stdlib/foo.sand` (declarations, docs, Sand helpers,
`native fn`s) and `src/stdlib/foo.rs`, whose `register` adds the source and
each native under its key:

| Declaration                                                    | Key                                |
| -------------------------------------------------------------- | ---------------------------------- |
| `native fn bar` in module `foo`                                | `foo.bar`                          |
| `native fn bar` in `struct`/`enum`/`extend`/`impl ... for Baz` | `foo.Baz.bar`                      |
| in the prelude, top level / `extend str`, `[T]`, `[K: V]`, ... | `bar` / `str.bar`, `List.bar`, ... |

- A native in an `impl` is registered with `Registry::add_method`
  (`NativeModule::raw_method` for hosts): trait methods are passed their
  impl's dictionary first, which these drop.
- Arguments arrive in declaration order, defaults filled in, after one
  `TypeDesc` per `reflect` type parameter.
- Never panic on script input. A Rust panic is caught per slice and ends the
  run with an internal error, leaving the VM possibly inconsistent.
- Check permissions (`vm.permit`) before any effect: a prompt re-executes
  the call.
- Reserve memory and take resource slots before mutating anything: on a
  memory or resource limit (`CtlKind::Collect`) the native is called again
  after a collection.
- Return `Err(Ctl::native_err(kind, message))` to return a recoverable
  `Err(Error)` from a native function that returns `Result`. Return
  `Err(Ctl::fail(kind, message))` to abort execution with a runtime error.
- An async native returns `Ctl::suspend(future)`. The future must not hold
  `Value`s; it yields a `Finisher` that builds the result on the VM thread
  (`stdlib::net::suspend` wraps the pattern).
- Native objects (`NativeObject`) never hold `Value`s: the collector does
  not trace them and `native_mut` skips the write barrier. An object whose
  owned memory grows must report it with `vm.heap.account`.
- Register a native value type (`ValueType`) with `Registry::add_value_type`.
  If `ValueType::PACKING` is set, values are stored inline in `Obj::Packed`;
  otherwise they are boxed in `Obj::Value`.
- Represent enum parameters and return values as Rust enums: derive `SandEnum`,
  register the type with `Registry::add_enum_type`, and convert values with
  `Vm::enum_value` and `Vm::new_enum`. Do not redeclare a registered enum in
  the `.sand` module source.
- Natives return a struct's fields as a tuple and let Sand build the struct.
  Never build `Obj::Variant` by hand: variants with one payload are stored
  inline, and equality and hashing rely on the canonical form.
- Build values derived from other heap values with `vm.heap.staged`, which
  reads them in place, instead of copying them out.
- Tables keyed by script data use `value::FxHashMap` (seeded per process),
  so scripts cannot choose colliding keys.

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
