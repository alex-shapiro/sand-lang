# Sand implementation internals

This document holds only what no single file can say: a map of the source
tree and rules that span several files. Put all other information in module
docs or next to the code.

## Map

```
source    ─lexer────▶ tokens     src/syntax/lexer.rs
tokens    ─parser───▶ AST        src/syntax/parser.rs
AST       ─checker──▶ typed HIR  src/check/, src/hir.rs
typed HIR ─compiler─▶ bytecode   src/compile/
bytecode  ─VM───────▶ effects    src/vm/
```

- `src/engine.rs` and `src/modules.rs`: the embedding API and module loading.
- `src/native/`: the `NativeModule` API. `src/stdlib/`: the standard library.
- `src/policy/`, `src/files/`, `src/platform.rs`: the sandbox's access to the
  OS.
- `cli/`: the `sand` command. `wasm/`: the WebAssembly bindings. `site/`: the
  website and playground.

## Rules

- Script input must never cause a panic.
- All work that grows with script input charges the meter (`src/vm/meter.rs`)
  or, during compilation, the `CompileBudget` (`src/check/budget.rs`).
- All memory that script input causes counts against `max_memory`: heap
  objects (`src/vm/heap.rs`), and buffers outside the heap (`src/vm/iomem.rs`).
- All OS access goes through the policy and is behind the `os` feature.
- The engine can run a native more than once (`NativeModule`).

`SAND_GC_STRESS=1` collects garbage at every safe point,
`SAND_DUMP_BYTECODE=1` prints the compiled code, and `SAND_GC_STATS=1`
(`sand run`) prints collector statistics.
