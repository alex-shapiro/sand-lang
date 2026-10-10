# TODO

Follow-up work, roughly in priority order.

## Open

### Sandbox

- [ ] **Windows paths still take two walks.** `files/windows.rs`
      canonicalizes, then walks again by handle, refusing reparse points;
      macOS and Linux resolve in one walk (`files/unix.rs`). One walk needs
      reparse-point targets and `GetFinalPathNameByHandleW`, which have no
      safe API among the dependencies (a small vetted crate, or a safe
      wrapper upstream in `fs_at`, would do).
- [ ] **Windows rename.** No safe API renames by handle, so on Windows
      `rename` is path-based: it holds the directories on both paths open
      (they cannot be renamed or replaced), but an entry's parent could
      still be emptied and turned into a junction during the call (usually
      detected afterwards, not prevented). The Windows code has only been
      cross-compiled, not run (see "Test coverage").
- [ ] **Linux canonical paths can go stale.** The policy checks a path
      built from the names walked; a directory renamed by another process
      during the walk makes it stale (as with `realpath`). macOS asks the
      held directory for its path (`F_GETPATH`); Linux could read
      `/proc/self/fd/N` for the held directory the same way, falling back to
      the walked names when `/proc` is not mounted.
- [ ] **Recursive `fs.remove_dir` re-walks from the root for every
      entry.** It is written in Sand (`stdlib/fs.sand`): `is_dir_entry`,
      `list_dir` and `remove_file` each resolve the whole path again. A
      native removal that descends through held directory handles
      (`unlinkat` relative to each) would take one walk per directory and
      check each entry against the policy as it goes.
- [ ] **HTTP server fairness.** `accept` reads up to 64 pending connections
      at once; 64+ idle connections can still delay a new request by up to
      30s, and a waiting `accept` holds up to half the free resource slots.

- [ ] **Channel queues are undercounted against the memory limit.**
      `let ch = task.channel()` then `while true { ch.send([1, 2, 3]) }`
      now stops with "memory limit exceeded" (in about 1s), but peaks at
      2.5 GB RSS under the default 512M limit (276 MB under 128M; the gap
      grows with the limit). For comparison, a growing list peaks at
      1.2-1.3x the limit, a map at 1.7x. Find what the queue holds outside
      the heap's accounting (its buffer's capacity, per-message nodes),
      charge it, and add a test that compares peak RSS with the limit.
- [ ] **Unconfirmed from the metering audit.** Path resolution (the
      one-walk resolver in `files/`) runs on the VM thread before the
      permission check, so a hung network file system could block it beyond
      the interrupt and wall-time limits; buffered standard output is
      flushed synchronously (a stalled pipe could block the run); list and
      map growth doubles capacity before the next memory check (measured: a
      growing map peaks at 1.7x the limit in RSS); `[a, ..rest]` patterns
      copy without a memory check; `check::traits::resolve_holes` re-walks a
      body per progress pass without charging the compile budget.

### Standard library

- [ ] **HTTP headers are a `[str: str]`.** Repeated headers are joined with
      `", "` (`stdlib/http.rs`, `read_head`), which corrupts `Set-Cookie`
      (its `Expires` dates contain commas) and loses the order of repeated
      headers. Represent headers as `[(str, str)]` (or a small `Headers`
      type with `get`, `get_all`), for requests and responses alike, and
      update SPEC §13.5.

### Test coverage

- [ ] Run the CI workflow (`.github/workflows/ci.yml`) on Windows and Linux:
      nothing has run there yet (Windows is only cross-checked with
      `--no-default-features --features os`; `ring` needs a Windows C
      toolchain). This includes the ConPTY variant of `cli/tests/prompt_tty.rs`,
      which has only been compiled, and the REPL's line editor
      (`cli/tests/repl_tty.rs` is Unix only).
- [ ] The one-walk resolver's Linux path (`openat2` with
      `RESOLVE_NO_SYMLINKS`, and its fallback when `openat2` is missing or
      filtered by seccomp) has only been compiled. The non-UTF-8 file name
      tests in `tests/sandbox_io.rs` skip on APFS, which refuses such names;
      Linux CI would run them.
- [ ] **Wall-clock bounds make tests flaky under load.** These failed
      under heavy load and passed on rerun:
      `hostile_runtime_programs_are_contained` (`tests/fuzz.rs`, 10s bound),
      `code_generation_is_covered_by_the_compile_budget`
      (`tests/compile_limits.rs`), `keys_chosen_to_collide_do_not_slow_down_maps`
      (`tests/metering.rs`), `many_locals_resolve_in_linear_time`
      (`tests/compile_limits.rs`). Bound the work instead of the time where
      possible (fuel used, instructions executed, the ratio between two
      input sizes measured in the same run).
- [ ] The playground is tested in headless Chrome by hand (see
      `playground/README.md`), not in CI, and not in Firefox or Safari.

### Web

- [ ] Stopping a busy engine in the browser terminates its worker: an
      engine computing without waiting never returns to the event loop, so
      `Engine::interrupt` only reaches it while it waits. With
      cross-origin isolation (COOP/COEP headers), the interrupt flag could
      live in a `SharedArrayBuffer` the page sets directly.
- [ ] The playground has no REPL pane yet; the JavaScript `Session` API
      (`engine.session()`) is there, but tasks spawned by an input only run
      while a later input runs (no `run_tasks_until` between inputs).
- [ ] WebAssembly size: the `wasm-release` profile gives 3.3 MB before
      wasm-opt; `opt-level = "s"` gives 2.8 MB and measured as fast on a
      busy machine (within noise). Measure on a quiet one and choose.

### Tooling

- [ ] **The linter parses every file a second time.** `lint::run_lints`
      calls `syntax::parse_module` on the source the engine already parsed,
      and matches the checker's facts to it by (line, column). The engine
      could hand the linter its syntax tree and span-keyed facts, which
      removes the second parse and the position matching.
- [ ] `wasm/build.sh` reads the build from `target/`, so it fails when
      `CARGO_TARGET_DIR` is set; use `cargo metadata`'s target directory.
- [ ] REPL standard input: a script read of standard input started during
      an input and still pending between inputs competes with the REPL for
      lines (with the line editor, for the terminal's keys).

### Performance

- [ ] Dispatch loop: the loop now keeps its state minimal and its cold
      paths out of line (docs/INTERNALS.md "Dispatch loop"), and `cargo bench`
      prints a geometric mean to compare builds, but the cost of individual
      instructions still shifts by up to ±15% between builds as LLVM
      reallocates registers (a full split of cold instructions was measured
      and is slower). A threaded design (one function per instruction with
      guaranteed tail calls, Rust's `become`) would remove the shared
      register allocation once it is stable.
- [ ] **A thread per compile.** Every compile, format and lint runs on a
      new 64 MB-stack thread (`platform::with_stack`, from `engine.rs`,
      `fmt.rs`, `lint.rs`). Spawning it is now about 70% of what
      `check_source("print(1)")` costs (about 45 us in all). One long-lived
      compile thread per engine (or per process) would remove it; in safe
      Rust that needs compile inputs that own their data instead of
      borrowing the engine (they are sent to the thread).
- [ ] **REPL sessions check the standard modules they import.** A session
      starts from the core snapshot and type checks each std module the
      first time an input imports it. Since the checker's tables are now
      shared (copying a snapshot costs a reference count per table),
      sessions can start from the full standard-library snapshot, which
      removes that path from `session.rs`.
- [ ] A loop that reads immutable globals is compiled twice
      (`compile::hoisted_loop`): the hoisted copy, and an unhoisted one that
      `HoistGlobal` jumps to if a global is not initialized yet (so the read
      panics where it happens). Loops with lambdas or over
      `MAX_VERSIONED_NODES` get no global hoisting at all. Proving
      initialization (globals with constant initializers set before any
      code runs, or a call graph from the checker) would drop the second
      copy and lift both restrictions.

### Code health

- [ ] `Checker::detach` exists because the checker borrows the source map
      and the native lookup while its tables are owned; a session keeps a
      detached checker and re-attaches it per input. Splitting the checker
      into owned tables and a per-compile borrowing context would remove
      `detach`/`attach`, at the cost of touching most field accesses.

### Language

- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`). The dictionary layout was chosen so
      trait objects can be a (value, dictionary) pair.
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Natives implementing trait methods; non-constant trait method defaults.
