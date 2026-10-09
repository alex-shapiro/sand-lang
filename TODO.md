# TODO

Follow-up work, roughly in priority order.

## Open

### Sandbox

- [ ] **Windows file system races.** Windows still uses path-based file
      operations, so the symlink-swap race closed on Unix (`fs_safe.rs`)
      remains there. Use handle-relative opens (`NtCreateFile` with a root
      directory handle, via a safe wrapper crate) or verify handles after
      opening.
- [ ] **Symlink names in deny rules.** A symlink's own final name is matched
      by the folded (case/Unicode-insensitive) comparison, but not converted
      to its stored spelling like other entries.
- [ ] **HTTP server fairness.** `accept` reads up to 64 pending connections
      at once; 64+ idle connections can still delay a new request by up to
      30s, and a waiting `accept` holds up to half the free resource slots.
- [ ] **Small accounting gaps.** Fixed 8 KiB per-connection buffers aren't
      counted against `max_memory`; checks made inside a native may overrun
      `max_instructions` by up to one slice (20,000 units); the bytecode
      compiler, linter and formatter are not covered by the compile budget.

### Test coverage

- [ ] Run the CI workflow (`.github/workflows/ci.yml`) on Windows and Linux:
      nothing has run there yet (Windows is only cross-checked with
      `--no-default-features`; `ring` needs a Windows C toolchain).
- [ ] An automated test of the real `/dev/tty` / `CONIN$` prompter (the
      dialogue is unit tested and `--prompt-stdin` is tested end to end).

### Tooling

- [ ] REPL line editing: output of background tasks re-prints the prompt but
      not the text typed after it (it stays in the terminal's line buffer);
      a line editor (history, redraw) would fix this. A script read of
      standard input started during an input and still pending between
      inputs competes with the REPL for lines.

### Performance

- [ ] Lazy iterator chains cost about 2x eager ones per element (closure
      call overhead); consider fusing adapters or cheaper closure calls.
- [ ] The interpreter's dispatch loop is sensitive to code layout (±10-25%
      on call-heavy benchmarks from unrelated edits). Consider splitting cold
      opcodes into separate functions systematically, or a threaded-dispatch
      design.
- [ ] Task spawn/wait got slower during the sandbox fixes (5k tasks: 2.5ms
      -> 4.5ms, about 0.4µs per task), probably from per-task stack and
      wait-list accounting; profile and trim.
- [ ] Incremental (not just generational) GC: major collections still pause
      in proportion to the heap; heaps under 8 MB only get major collections.

### Language

- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`). The dictionary layout was chosen so
      trait objects can be a (value, dictionary) pair.
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Natives implementing trait methods; non-constant trait method defaults.
