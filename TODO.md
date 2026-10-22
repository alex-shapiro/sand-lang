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
      yet (Windows is only cross-checked with
      `--no-default-features --features os`), including the ConPTY variant of
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
      `code_generation_is_covered_by_the_compile_budget`,
      `many_loop_constants_compile_in_linear_time`
      (`tests/compile_limits.rs`), `keys_chosen_to_collide_do_not_slow_down_maps`
      (`tests/metering.rs`), `many_locals_resolve_in_linear_time`
      (`tests/compile_limits.rs`). Bound the work instead (fuel, instructions,
      the ratio between two input sizes in the same run).
- [ ] The playground page is tested by hand in headless Chrome, not in CI,
      Firefox or Safari.
- [ ] `Config::retry_stress` fails only the first check in the body of a
      `vm_function`, so a side effect between two checks of one call is not
      found. Run the stressed tests once for each check index that a call
      reaches, and fail that check.
- [ ] Run the `cargo fuzz` targets on a schedule (or on OSS-Fuzz). Update the
      `run` target to compare optimized and unoptimized executions.

### Spec

- [ ] Document in §14.4 that `time.local_zone()` returns `UTC` if the
      `TimeZone` capability is denied (currently documented only in §13).

### Web

- [ ] Stopping a busy engine in the browser terminates its worker, since
      `Engine::interrupt` only reaches an engine while it waits. With
      cross-origin isolation (COOP/COEP), the interrupt flag could live in a
      `SharedArrayBuffer` the page sets directly.
- [ ] WebAssembly size: the `wasm-release` profile gives 3.3 MB before
      wasm-opt, `opt-level = "s"` 2.8 MB at similar speed. Measure on a
      quiet machine and choose.

### Embedding

- [ ] Support enum variant payloads in `#[derive(SandEnum)]` so native modules
      such as `json` do not construct variant values by runtime ID.
- [ ] Register `json.Json` through `NativeModule` instead of matching its
      module and type name in `compile/reflect.rs`.
- [ ] Return `SandStruct`s from the private `http` natives (`send`,
      `read_response`, `start_upload`, `next_request`) instead of tuples that
      Sand wrappers turn into `Response`, `Stream`, `Upload`, and `Request`.
- [ ] Bind the prelude's `Error` with `SandStruct` instead of building it by
      runtime ID (`RtIds::error_def`, `Vm::err_value`).
- [ ] Reject `json.encode<T>` and `json.to_json<T>` at compile time if `T`
      contains a function, task, or channel (they abort at run time). This
      needs a check like `ReflectCheck` for type parameters that are not
      `reflect`.
- [ ] Let natives require `Num` (`Param<N, bound::Num>`). A `Num` bound
      carries a trait dictionary, and natives cannot take one, so
      `List.sum_from` depends on the bound of its Sand caller.
- [ ] Support generic structs (such as the prelude's `Iter<T>`) and field
      defaults in `#[derive(SandStruct)]`.
- [ ] Reject invalid and duplicate Sand field names (`#[sand(name)]`, Sand
      keywords) when the derive expands, not when `NativeModule::bind` panics
      at engine setup. The derive can emit a `const` assertion that calls a
      `const fn` over the lexer's keyword table.
- [ ] `http.send` closes an idle pooled connection, instead of putting it
      back in the pool, if the request then fails to buffer or runs out of
      fuel.

### Tooling

- [ ] Fix broken intra-doc links to private items and run `cargo doc` with
      `-D warnings` in CI.
- [ ] Policy lints judge `http` rules only by their exact text (and
      `bypassable-http-deny`); rules with wildcards in hosts or paths could
      be compared as file rules are. A rule made unnecessary only by several
      other rules together is not reported.

### Diagnostics

- [ ] Suppress follow-on parser errors after an initial syntax error
      (`unexpected }` after a missing `,` or `:`, `expected )` after an
      unterminated string) and remove the follow-on expectations from the spec
      tests.
- [ ] Recover at the next `match` arm after an invalid pattern
      (`1..5 =>`, `Rect(..) =>`) instead of reporting a spurious
      `unexpected }` at the end of the `match`.
- [ ] Suggest an enum in addition to a struct or tuple for mixed-type list
      literals (`[1, "a"]`).
- [ ] Include the `<top level>` stack frame if a failing `assert` is the
      final top-level statement.
- [ ] `help` shows a type of another module without its module name
      (`help(http.Request)` shows `conn: Conn`, not `conn: net.Conn`, and
      `help(fs.Metadata)` shows `modified: Instant?`).

### Performance

- [ ] A dead string stays rooted after the function that made it returns
      (`fn junk() -> int { "x".repeat(12_000_000).len() }`, then
      `junk() + host.call()`): a full collection inside the native call does
      not free it. Find the root. If the optimizer inlines `junk`, a register
      of the current frame may still hold the string.
- [ ] Benchmark `HostFn::Static` against `HostFn::Shared` to determine whether
      avoiding the `Arc` dereference justifies `Box::leak` and the enum
      dispatch; if not, keep only `Arc`.
- [ ] Profile `HostFn` dispatch and argument slice construction (`fannkuch` and
      `results` in `bench/programs`) to reduce the ~0.5 ns typed native call
      overhead.
- [ ] Dispatch loop: the cost of individual instructions shifts by up to
      ±15% between builds as LLVM reallocates registers. A threaded design
      (one function per instruction with guaranteed tail calls, Rust's
      `become`) would remove the shared register allocation once `become`
      is stable.

### Language

- [ ] Infer the enum type of a variant expression from the expected type, as
      pattern matching does (for example, `fs.open(path, Write)` or
      `fs.open(path, .Write)`).
- [ ] Add an `Ordering` enum to the prelude and return it from
      `time.Span.compare` instead of `-1`, `0`, or `1`.
- [ ] Evaluate returning a `time.Weekday` enum instead of an `int` (`1` to `7`)
      from `day_of_week()` in the `time` module.
- [ ] Evaluate replacing `is_file` and `is_dir` on `fs.Metadata` with an
      `fs.Kind` enum.
- [ ] Evaluate replacing string error kinds (`"not_found"`, `"invalid"`) with
      an enum.

- [ ] **Construct all types with `T(...)`.** Replace struct literals with
      named-argument calls (`Point(x: 1.0, y: 2.0)`, with defaults and
      `..base`) and struct patterns with `Point(x, ..)`. Allow a type to
      declare an `init` method (or a native `init` for an opaque type) that
      replaces field-by-field construction and can return a `Result`. Inside
      `init`, `Self(...)` performs field-by-field construction. Remove
      module-level constructor functions (`time.date`, `regex.new`,
      `random.rng`, `task.channel`, `set`, `error`) and the parser restriction
      on struct literals in `if`, `while`, `match`, and `for` headers. Keep
      static factory methods (`T.parse`, `T.from_x`).
- [ ] Support `Rect(..)` and `Rect(w: w, h: h)` in enum variant patterns to
      match variant construction (`Shape.Rect(w: 2.0, h: 3.0)`), as part of
      the `T(...)` item above.
- [ ] Reject `import` statements after non-import items or statements, as
      specified in §3.1.
- [ ] Trait objects (`dyn Trait`), associated types and constants, and
      blanket impls (`impl<T> X for T`).
- [ ] Trait bounds on struct/enum type parameters.
- [ ] Non-constant trait method defaults.
