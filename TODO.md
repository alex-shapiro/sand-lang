# TODO

Follow-up work, roughly in priority order.

## Open

### Sandbox

- [ ] **Deny patterns protect whole trees.** With `deny ./**/.env`, every
      directory under `.` may hold a denied file, so none can be renamed
      or removed recursively (`Policy::denies_under`). Scanning the tree
      for entries the rules deny would refuse only trees that hold one.
- [ ] **Windows paths take two walks.** `files/windows.rs` canonicalizes,
      then walks again by handle; macOS and Linux resolve in one walk. One
      walk needs reparse-point targets and `GetFinalPathNameByHandleW`, which
      no dependency exposes safely (a vetted crate or `fs_at` could).
- [ ] **Windows rename.** No safe API renames by handle, so on Windows
      `rename` is path-based: it holds the directories on both paths open,
      but an entry's parent could still be emptied and turned into a
      junction during the call (usually detected afterwards, not prevented).
- [ ] **Linux canonical paths can go stale.** The policy checks a path
      built from the names walked, which a concurrent rename makes stale.
      macOS asks the held directory for its path (`F_GETPATH`); Linux could
      read `/proc/self/fd/N`, falling back to the walked names.
- [ ] **Channel queues are undercounted against the memory limit.**
      `while true { ch.send([1, 2, 3]) }` on an unbounded channel peaks at
      2.5 GB RSS under the default 512M limit before it stops (a list
      peaks at 1.2-1.3x the limit, a map at 1.7x). Charge what the queue
      holds outside the heap's accounting (buffer capacity, per-message
      nodes) and test peak RSS against the limit.
- [ ] **Suspected metering gaps.** Path resolution and `fs.File` reads
      and writes run on the VM thread, so a hung network file system could
      block them beyond the interrupt and wall-time limits; buffered
      standard output is flushed synchronously; list and map growth doubles
      capacity before the next memory check; `[a, ..rest]` patterns copy
      without a memory check; `check::traits::resolve_holes` re-walks a body
      per progress pass without charging the compile budget.

### Standard library

- [ ] **HTTP keep-alive.** Every client request and server connection
      carries one exchange (`Connection: close`).

### Test coverage

- [ ] Run the CI workflow on Windows and Linux, where nothing has run
      yet (Windows is only cross-checked with `--no-default-features
  --features os`), including the ConPTY variant of
      `cli/tests/prompt_tty.rs` and the REPL's line editor.
- [ ] Opening files for each `fs.Mode` on Windows (`open_mode` in
      `files/windows.rs`, through `fs_at` with `create_new`) has only been
      cross-compiled.
- [ ] The one-walk resolver's Linux path (`openat2` with
      `RESOLVE_NO_SYMLINKS`, and its fallback when `openat2` is missing or
      filtered by seccomp) has only been compiled, and the non-UTF-8 file
      name tests in `tests/sandbox_io.rs` skip on APFS.
- [ ] **Wall-clock bounds make tests flaky under load:**
      `hostile_runtime_programs_are_contained` (`tests/fuzz.rs`, 10s bound),
      `code_generation_is_covered_by_the_compile_budget`
      (`tests/compile_limits.rs`), `keys_chosen_to_collide_do_not_slow_down_maps`
      (`tests/metering.rs`), `many_locals_resolve_in_linear_time`
      (`tests/compile_limits.rs`). Bound the work instead (fuel, instructions,
      the ratio between two input sizes in the same run).
- [ ] The playground page is tested by hand in headless Chrome, not in CI,
      Firefox or Safari.

### Web

- [ ] Stopping a busy engine in the browser terminates its worker, since
      `Engine::interrupt` only reaches an engine while it waits. With
      cross-origin isolation (COOP/COEP), the interrupt flag could live in a
      `SharedArrayBuffer` the page sets directly.
- [ ] WebAssembly size: the `wasm-release` profile gives 3.3 MB before
      wasm-opt, `opt-level = "s"` 2.8 MB at similar speed. Measure on a
      quiet machine and choose.

### Tooling

- [ ] Policy lints (`redundant-rule`, `overridden-rule`) only judge rules
      that name one path. Rules with wildcards could be judged by whether
      one pattern covers another.
- [ ] **The linter parses every file a second time.** `lint::run_lints`
      calls `syntax::parse_module` on the source the engine already parsed,
      and matches the checker's facts to it by (line, column). The engine
      could hand the linter its syntax tree and span-keyed facts.
- [ ] `wasm/build.sh` reads the build from `target/`, so it fails when
      `CARGO_TARGET_DIR` is set; use `cargo metadata`'s target directory.
- [ ] REPL standard input: a script's read of standard input still pending
      between inputs competes with the REPL for lines.

### Performance

- [ ] Dispatch loop: the cost of individual instructions shifts by up to
      ±15% between builds as LLVM reallocates registers. A threaded design
      (one function per instruction with guaranteed tail calls, Rust's
      `become`) would remove the shared register allocation once `become`
      is stable.
- [ ] **A thread per compile.** Every compile, format and lint runs on a
      new 64 MB-stack thread (`platform::with_stack`, from `engine.rs`,
      `fmt.rs`, `lint.rs`), about 70% of the ~45 us that
      `check_source("print(1)")` costs. One long-lived compile thread would
      remove it; in safe Rust that needs compile inputs that own their data
      instead of borrowing the engine.
- [ ] **REPL sessions check the standard modules they import.** A session
      starts from the core snapshot and type checks each std module the
      first time an input imports it. Snapshots are cheap to copy (shared
      tables), so sessions could start from the full standard-library
      snapshot instead.
- [ ] A loop that reads immutable globals is compiled twice
      (`compile::hoisted_loop`): the hoisted copy, and an unhoisted one that
      `HoistGlobal` jumps to if a global is not initialized yet (so the read
      panics where it happens). Loops with lambdas or over
      `MAX_VERSIONED_NODES` get no global hoisting. Proving initialization
      (constant initializers, or a call graph from the checker) would drop
      the second copy and lift both restrictions.

### Code health

- [ ] `Checker::detach` exists because the checker borrows the source map
      and the native lookup while its tables are owned; a session keeps a
      detached checker and re-attaches it per input. Splitting the checker
      into owned tables and a per-compile borrowing context would remove
      `detach`/`attach`.
- [ ] `Engine::check_source` and `check_file` return a list of warnings
      that is always empty: return just the result.

### Language

- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`).
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Non-constant trait method defaults.
- [ ] **A pattern may bind a name twice.** `let (a, a) = (1, 2)` checks and
      binds the last value; it should be an error, as in an or-pattern
      that binds different names.
- [ ] **NaN and infinity have no literals.** Floats display as `NaN` and
      `inf`, but neither is valid source (only `math.nan`, `math.inf`,
      `1.0 / 0.0` or `"inf".parse_float()`), so float display and debug
      forms do not read back the way a string's debug form does.
