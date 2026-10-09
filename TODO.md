# TODO

Follow-up work, roughly in priority order.

## Open

### Sandbox

- [ ] **Windows rename.** No safe API renames by handle, so on Windows
      `fs_safe::rename` is path-based: it holds the directories on both paths
      open (they cannot be renamed or replaced), but an entry's parent could
      still be emptied and turned into a junction during the call (usually
      detected afterwards, not prevented). The Windows `fs_safe` code has
      only been cross-compiled, not run (see "Test coverage").
- [ ] **HTTP server fairness.** `accept` reads up to 64 pending connections
      at once; 64+ idle connections can still delay a new request by up to
      30s, and a waiting `accept` holds up to half the free resource slots.

### Test coverage

- [ ] Run the CI workflow (`.github/workflows/ci.yml`) on Windows and Linux:
      nothing has run there yet (Windows is only cross-checked with
      `--no-default-features`; `ring` needs a Windows C toolchain). This
      includes the ConPTY variant of `tests/prompt_tty.rs`, which has only
      been compiled.

### Tooling

- [ ] REPL line editing: output of background tasks re-prints the prompt but
      not the text typed after it (it stays in the terminal's line buffer);
      a line editor (history, redraw) would fix this. A script read of
      standard input started during an input and still pending between
      inputs competes with the REPL for lines.

### Performance

- [ ] The interpreter's dispatch loop is sensitive to code layout (±10-25%
      on call-heavy benchmarks from unrelated edits). Consider splitting cold
      opcodes into separate functions systematically, or a threaded-dispatch
      design.

### Language

- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`). The dictionary layout was chosen so
      trait objects can be a (value, dictionary) pair.
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Natives implementing trait methods; non-constant trait method defaults.
