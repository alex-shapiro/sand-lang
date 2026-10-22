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
- [ ] Derive Sand structs from Rust structs (`#[derive(SandStruct)]`) so that
      natives can take and return structs such as `regex.Match` and `uri.Uri`
      without private tuple-returning natives and Sand wrappers.
- [ ] Rust signatures cannot state the bounds of type parameters (`Eq`, `Ord`,
      `Hash`): the checker does not detect a `native fn` declaration that omits
      a bound its Rust function relies on.
- [ ] Natives build heap values with untyped constructors and wrap them in
      `Val::from_raw`. Give `Vm` constructors that return
      typed handles (`new_str` → `Val<String>`, `new_bytes`, lists, tuples)
      and accessors for map, set, task and channel handles, so that the
      Rust types of results are correct by construction. Where a native
      reserves and charges as `IntoValue` does (`uri.parse_query`,
      `http_common::header_list`, fs `stat`, `uuid`, `crypto`), return the
      Rust value instead. Typed map and set accessors also remove the dead
      `_ => 0` fallbacks in `core.rs`.
- [ ] The checker accepts `json.decode<T>` for a `T` that contains a type
      it cannot decode (a function, a task); the call then returns an `Err`
      of kind `"invalid"`. Reject such a `T` at the call, as private fields
      are, without restricting other `reflect` natives.
- [ ] `compile/reflect.rs` recognizes `json.Json` by its module and type
      name. Bind it as the other types of native modules are.
- [ ] `http.request`, `http.stream` and the web `http.request` repeat the
      request setup (URL, method, headers, capability, `filter_request`),
      and `Duration::try_from(timeout).unwrap_or_default()` appears six
      times in `net` and `http`. Share the setup, and convert timeouts with
      a `FromValue` for `std::time::Duration`.
- [ ] `ValueType` sets `MODULE`, `NAME` and `DOC` by hand. Derive them as
      `SandEnum` and `SandObject` do.
- [ ] Types bound to Rust types have no source position, so `help` lists
      them before the module's other types (`Task`, `Channel` before
      `Option`).

### Tooling

- [ ] Fix broken intra-doc links to private items and run `cargo doc` with
      `-D warnings` in CI.

- [ ] Policy lints judge `http` rules only by their exact text (and
      `bypassable-http-deny`); rules with wildcards in hosts or paths could
      be compared as file rules are. A rule made unnecessary only by several
      other rules together is not reported.

### Diagnostics

- [ ] Recover at the next `match` arm after an invalid pattern
      (`1..5 =>`, `Rect(..) =>`) instead of reporting a spurious
      `unexpected }` at the end of the `match`.
- [ ] Suggest an enum in addition to a struct or tuple for mixed-type list
      literals (`[1, "a"]`).
- [ ] Include the `<top level>` stack frame if a failing `assert` is the
      final top-level statement.

### Performance

- [ ] `HostFn::Static` avoids an `Arc` dereference for standard natives at
      the cost of a `Box::leak` and an enum. Measure whether it pays; if not,
      use one `Arc` variant.
- [ ] A typed native call costs about 0.5 ns in `HostFn` dispatch and the
      argument slice (`fannkuch` and `results` in `bench/programs` show
      it). Find where the time goes.

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
