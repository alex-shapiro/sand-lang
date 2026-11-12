# Sand implementation internals

Sand executes scripts through a five-stage pipeline under strict CPU, memory,
and capability limits. Every stage and native module must uphold four
cross-cutting sandbox invariants:

- Do not panic on script input.
- Charge `Meter` (`crates/sandlang/src/vm/meter.rs`) at runtime and `CompileBudget`
  (`crates/sandlang/src/check/budget.rs`) during compilation for all work proportional to
  script input.
- Reserve all memory allocated for script input against `max_memory`,
  including heap objects (`crates/sandlang/src/vm/heap.rs`) and off-heap I/O buffers
  (`crates/sandlang/src/vm/iomem.rs`).
- Gate all OS access behind the `os` feature and check `Policy` before every
  OS operation.

## Pipeline and source layout

The compiler and runtime process source code in five stages:

```
source    ─lexer────▶ tokens     crates/sandlang/src/syntax/lexer.rs
tokens    ─parser───▶ AST        crates/sandlang/src/syntax/parser.rs
AST       ─checker──▶ typed HIR  crates/sandlang/src/check/, crates/sandlang/src/hir.rs
typed HIR ─compiler─▶ bytecode   crates/sandlang/src/compile/
bytecode  ─VM───────▶ effects    crates/sandlang/src/vm/
```

In debug builds, `crates/sandlang/src/check/verify.rs` verifies all trait bounds and
dictionaries in the typed HIR before code generation so that type-checker bugs
are caught at compile time.

The remaining modules provide the embedding API, native bindings, sandboxed OS
access, and host targets:

- `crates/sandlang/src/engine.rs`, `crates/sandlang/src/modules.rs`: `Engine`, `Config`, and module loading.
- `crates/sandlang/src/native/`, `crates/sandlang/src/stdlib/`: the `NativeModule` API and the standard
  library. Each standard module `name` has two files. `name.sand` defines the
  public declarations, documentation, and Sand helpers. `name.rs` registers the
  native functions by name.
- `crates/sandlang/src/policy/`, `crates/sandlang/src/files/`, `crates/sandlang/src/platform.rs`: capability policies,
  sandboxed file systems, and OS platform abstractions. Run all script file and
  module I/O through `Fs` on a worker thread with `Vm::offload` instead of
  `std::fs` or `tokio::fs`.
- `crates/sand-cli/`, `crates/sandlang-wasm/`, `crates/sandlang-site/`: the `sand` CLI, WebAssembly bindings, and website.
