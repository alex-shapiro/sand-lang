# Sand implementation internals

Sand executes scripts through a five-stage pipeline under strict CPU, memory,
and capability limits. Every stage and native module must uphold four
cross-cutting sandbox invariants:

- Do not panic on script input.
- Charge `Meter` (`src/vm/meter.rs`) at runtime and `CompileBudget`
  (`src/check/budget.rs`) during compilation for all work proportional to
  script input.
- Reserve all memory allocated for script input against `max_memory`,
  including heap objects (`src/vm/heap.rs`) and off-heap I/O buffers
  (`src/vm/iomem.rs`).
- Gate all OS access behind the `os` feature and check `Policy` before every
  OS operation.

## Pipeline and source layout

The compiler and runtime process source code in five stages:

```
source    ─lexer────▶ tokens     src/syntax/lexer.rs
tokens    ─parser───▶ AST        src/syntax/parser.rs
AST       ─checker──▶ typed HIR  src/check/, src/hir.rs
typed HIR ─compiler─▶ bytecode   src/compile/
bytecode  ─VM───────▶ effects    src/vm/
```

In debug builds, `src/check/verify.rs` verifies all trait bounds and
dictionaries in the typed HIR before code generation so that type-checker bugs
are caught at compile time.

The remaining modules provide the embedding API, native bindings, sandboxed OS
access, and host targets:

- `src/engine.rs`, `src/modules.rs`: `Engine`, `Config`, and module loading.
- `src/native/`, `src/stdlib/`: the `NativeModule` API and the standard
  library.
- `src/policy/`, `src/files/`, `src/platform.rs`: capability policies,
  sandboxed file systems, and OS platform abstractions.
- `cli/`, `wasm/`, `site/`: the `sand` CLI, WebAssembly bindings, and website.
