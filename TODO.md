# TODO

Follow-up work, roughly in priority order.

## Open

### Sandbox

- [ ] **Windows rename.** No safe API renames by handle (`fs_at`, `winx`
      and `cap-std` all rename by path). `rename` keeps every directory on
      both paths open, so that none of them can be renamed or replaced. A
      parent that is emptied and turned into a junction during the call is
      still possible; it is detected after the rename, not prevented.

### Test coverage

- [ ] Run the CI workflow on Windows and Linux, where nothing has run
      yet (Windows is only cross-checked with `--no-default-features
  --features os`), including the ConPTY variant of
      `cli/tests/prompt_tty.rs` and the REPL's line editor.
- [ ] The Windows resolver and file operations (`files/windows.rs`,
      through `fs_at` and `winx`) have only been cross-compiled.
- [ ] The one-walk resolver's Linux path (`openat2` with
      `RESOLVE_NO_SYMLINKS`, its fallback when `openat2` is missing or
      filtered by seccomp, and `/proc/self/fd` paths) has only been
      compiled, and the non-UTF-8 file name tests in `tests/sandbox_io.rs`
      skip on APFS.
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

- [ ] Policy lints judge `http` rules only by their exact text (and
      `bypassable-http-deny`); rules with wildcards in hosts or paths could
      be compared as file rules are. A rule made unnecessary only by several
      other rules together is not reported.

### Performance

- [ ] Dispatch loop: the cost of individual instructions shifts by up to
      ±15% between builds as LLVM reallocates registers. A threaded design
      (one function per instruction with guaranteed tail calls, Rust's
      `become`) would remove the shared register allocation once `become`
      is stable.
- [ ] **A thread per compile.** Every compile (with its lints) and format
      runs on a new stack thread (`platform::with_stack`, from `engine.rs`
      and `fmt.rs`), about 70% of the ~45 us that `check_source("print(1)")`
      costs. One long-lived compile thread would remove it; in safe Rust
      that needs compile inputs that own their data instead of borrowing the
      engine.
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
