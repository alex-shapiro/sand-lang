# TODO

Follow-up work, roughly in priority order.

## Open

### Sandbox

- [ ] **Windows rename.** No safe API renames by handle (`fs_at`, `winx`
      and `cap-std` all rename by path). `rename` keeps every directory on
      both paths open, so that none of them can be renamed or replaced. A
      parent that is emptied and turned into a junction during the call is
      still possible; it is detected after the rename, not prevented.

- [ ] **A wait for the user pauses every wait.** While any task waits for
      the user (`Ctl::suspend_for_user`, a read of standard input), idle time
      does not count toward the wall-time limit, including time that other
      tasks spend on timers and HTTP. A script that animates a spinner while
      it waits for input needs this, but a script that leaves a read pending
      can sleep past its limit; `max_cpu_time` still bounds its computation.
      Consider a separate limit for idle time.

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
- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`).
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Non-constant trait method defaults.
