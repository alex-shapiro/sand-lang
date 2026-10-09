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

- [ ] **Unconfirmed from the metering audit.** Path normalisation
      (`canonicalize`, `read_dir`) runs on the VM thread before the
      permission check, so a hung network file system could block it beyond
      the interrupt and wall-time limits; buffered standard output is
      flushed synchronously (a stalled pipe could block the run); list and
      map growth doubles capacity before the next memory check (up to ~2x
      slack); `[a, ..rest]` patterns copy without a memory check;
      `check::traits::resolve_holes` re-walks a body per progress pass
      without charging the compile budget.

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

- [ ] Dispatch loop: the loop now keeps its state minimal and its cold
      paths out of line (docs/INTERNALS.md "Dispatch loop"), and `cargo bench`
      prints a geometric mean to compare builds, but the cost of individual
      instructions still shifts by up to ±15% between builds as LLVM
      reallocates registers (a full split of cold instructions was measured
      and is slower). A threaded design (one function per instruction with
      guaranteed tail calls, Rust's `become`) would remove the shared
      register allocation once it is stable.

### Language

- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`). The dictionary layout was chosen so
      trait objects can be a (value, dictionary) pair.
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Natives implementing trait methods; non-constant trait method defaults.
