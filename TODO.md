# TODO

Follow-up work, roughly in priority order.

## Open

### Miscellaneous

- [ ] Move all crates into a `crates/` directory and standardize all crate metadata (versions, licenses, etc.) to use workspace values.

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
- [ ] Run the `cargo fuzz` targets on a schedule (or on OSS-Fuzz). Update the
      `run` target to compare optimized and unoptimized executions.
- [ ] `help.test` fails with `--no-default-features` because the overview
      includes modules that require the `os` feature.

### Spec

- [ ] §14.4 does not say that a denied `TimeZone` makes `time.local_zone()`
      return `UTC`. §13 says it.

### Standard Library

- [ ] `http.serve` should separate IP address and port arguments, and so should `http.Server.addr`, and so should `net.listen`, `net.connect`, `net.connect_tls`, etc.
- [ ] `log` should be configurable to use a non-stderr, host-defined sink. The host should be allowed to configure an arbitrary sink function and logging level at engine start time. The default sink is still stderr. E.g. a larger Rust app that uses `tracing` should be able to configure (not encessarily ergonomically, but it should be possible) a Sand engine to forward logs to an off-device subscriber. All logging must still be gated by the sandbox.
- [ ] `random.normal` should be removed. If the function is required elsewhere then we should add a `statistics.Normal` type and a `statistics.sample<D>(dist: D) -> float` that allows a user to sample from a parameterized distribution (and which would be extensible to other distribution kinds).

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

- [ ] `sand fmt` breaks every chain of three or more calls that has a
      lambda argument, also when the chain fits on one line
      (`[1, 2, 3].map(x => x * 2).filter(x => x > 2).sum()`). §17.3 breaks
      only lines longer than 100 characters, and the first example of §12.13
      is not formatted. Make the formatter or §17.3 agree.

### Diagnostics

- [ ] After a bad pattern (`1..5 =>`, `Rect(..) =>`), the parser also
      reports `unexpected }` at the end of the `match`. Recover at the next
      arm.
- [ ] `[1, "a"]` gives "use a struct or a tuple for mixed values". An enum
      is the usual fix for a list of mixed values.
- [ ] When a failing `assert` is the last top-level statement, the stack
      trace has no `<top level>` frame. When a statement follows it, the
      frame is there.

### Performance

- [ ] Dispatch loop: the cost of individual instructions shifts by up to
      ±15% between builds as LLVM reallocates registers. A threaded design
      (one function per instruction with guaranteed tail calls, Rust's
      `become`) would remove the shared register allocation once `become`
      is stable.

### Language

- [ ] **Construct all types with `T(...)`.** Replace struct literals with
      named-argument calls (`Point(x: 1.0, y: 2.0)`, with defaults and
      `..base`) and struct patterns with `Point(x, ..)`. Allow a type to
      declare an `init` method (or a native `init` for a native type) that
      replaces field-by-field construction and can return a `Result`. Inside
      `init`, `Self(...)` performs field-by-field construction. Remove
      module-level constructor functions (`time.date`, `regex.new`,
      `random.rng`, `task.channel`, `set`, `error`) and the parser restriction
      on struct literals in `if`, `while`, `match`, and `for` headers. Keep
      static factory methods (`T.parse`, `T.from_x`).
- [ ] Variant patterns take only positional payloads: `Rect(..)` and
      `Rect(w: w, h: h)` do not parse, but `Shape.Rect(w: 2.0, h: 3.0)`
      constructs a variant. Part of the `T(...)` item above.
- [ ] The compiler accepts an `import` after other statements. §3.1 puts
      imports before all items and statements.
- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`).
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Non-constant trait method defaults.
- [ ] Remove the `|>` operator because basically nothing uses it.
