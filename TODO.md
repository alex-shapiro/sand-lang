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
- [ ] `Config::retry_stress` fails only the first check in a `vm_function`
      body, so it does not detect a side effect between the first and second
      checks. Run stressed tests for each check index reached during a call.
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

- [ ] Convert `Json` values to and from Rust without recursion so that
      conversion accepts the full 512 nesting levels of `json.parse` without
      overflowing a 512 KiB thread stack (the iOS default).
- [ ] In `#[derive(SandEnum)]` and `#[derive(SandStruct)]`, support fields
      of type `Self` and of nested generic types whose type parameters require
      traits (`struct Sorted<T: Ord>`). The derive macros substitute
      `Param<N>` for each type parameter when describing field types, and
      `Param<N>` does not satisfy Rust trait bounds such as `Ord`.
- [ ] Represent `ValueType` constants in `ConstVal` by their text form so that
      `#[sand(default)]` supports value-type fields and `json.decode` evaluates
      constant value-type defaults directly.
- [ ] Pass `Num<N>` evidence to native `impl` methods by extracting the `Num`
      dictionary from the `impl` dictionary.
- [ ] The native API uses "bound" for a Rust type that `NativeModule::bind`
      attaches to a Sand type (`BoundKind`, `BoundType`, `Type::bound`,
      `declare_bound_types`), next to `Binding<T>`. Use one name for this
      concept.
- [ ] Implement `SandType`, `FromValue`, and `IntoValue` for
      `std::ops::Range<i64>` and `RangeInclusive<i64>` so that `SandStruct`
      fields can use Rust range types and `#[sand(default)]`.

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
- [ ] Report a missing trait once per expression: with `x: float?`,
      `set([need(x), x])` (where `need<T: Hash>`) reports
      `float? does not implement Hash` twice, at overlapping spans.
- [ ] Show the type of impls for some instances of a constructor in the
      `Implements` section of `help` (`impl Describe for [int]` is listed as
      `Describe` in `help(List)`).
- [ ] Qualify types from other modules in `help` output (for example,
      `conn: net.Conn` in `help(http.Request)` and `modified: time.Instant?` in
      `help(fs.Metadata)`).

### Performance

- [ ] Clear dead temporaries before native calls so that inlined allocations
      (such as `fn junk() -> int { "x".repeat(12_000_000).len() }` in
      `junk() + host.call()`) do not remain rooted in caller frame registers
      during garbage collection.
- [ ] Benchmark `HostFn::Static` against `HostFn::Shared` to determine whether
      avoiding the `Arc` dereference justifies `Box::leak` and the enum
      dispatch; if not, keep only `Arc`.
- [ ] Profile `HostFn` dispatch and argument slice construction (`fannkuch` and
      `results` in `bench/programs`) to reduce the ~0.5 ns typed native call
      overhead.
- [ ] Reduce `json.decode` dispatch overhead (~76 ns per call to decode `"1"`,
      of which 15 ns is the Sand wrapper around `decode_raw`).
- [ ] Cache instantiated `TypeDesc` objects in `Op::Describe` by their
      template and argument descriptions to avoid ~90 ns of allocation per
      generic `reflect` call.
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
- [ ] **Check that written types are well formed.** The checker does not
      check type annotations and `reflect` type arguments against the traits
      that their constructors require: set elements and map keys must
      implement `Hash`, and `struct Bag<T: Hash>` requires `T: Hash`.
      `let s: Set<[int]> = json.decode("[[1], [1]]")?` gives a set that holds
      `[1]` twice (also on `main`), and `fn size(b: Bag<[int]>)` is accepted.
      Calls, struct literals and variants check the traits, so no method runs
      without its dictionary. Decide whether generic signatures
      (`fn f<T>(b: Bag<T>)`) must declare `T: Hash` or imply it, then check
      every written type.
- [ ] Let local functions require traits with methods (such as `Num`) and
      have `reflect` type parameters, by passing hidden dictionary and
      type-description arguments to closures.
- [ ] `+` is not a trait. `require_plus` (`src/check/unify.rs`) checks the
      operands of `+` against a fixed list of types, and a type parameter
      supports `+` only through `Num`, so generic code cannot concatenate
      `str`, `bytes` or lists. Declare an `Add` trait in the prelude and
      derive or implement it like the other built-in traits.
- [ ] Non-constant trait method defaults.
