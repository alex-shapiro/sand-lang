# TODO

Follow-up work, roughly in priority order.

## Open

### Miscellaneous

- [ ] Move all crates into a `crates/` directory and standardize all crate metadata (versions, licenses, etc.) to use workspace values.

### Sandbox

- [ ] **Windows rename.** Windows does not provide a safe API to rename files
      by handle (`fs_at`, `winx`, and `cap-std` all rename by path). `rename`
      keeps every directory on both paths open so that the directories cannot
      be renamed or replaced. However, a caller can still empty a parent
      directory and convert it into a junction during the call; `rename`
      detects this change after the operation instead of preventing it.

### Test coverage

- [ ] Run the CI workflow on Windows and Linux (including the ConPTY variant of
      `cli/tests/prompt_tty.rs` and the REPL line editor).
- [ ] Run the Windows path resolver and file operations (`files/windows.rs`,
      via `fs_at` and `winx`) on Windows; CI currently only cross-compiles them.
- [ ] Run the Linux path resolver (`openat2` with `RESOLVE_NO_SYMLINKS`, the
      fallback if `openat2` is unavailable or blocked by seccomp, and
      `/proc/self/fd` paths) and the non-UTF-8 file name tests in
      `tests/sandbox_io.rs` on Linux.
- [ ] **Wall-clock bounds make tests flaky under load:**
      `hostile_runtime_programs_are_contained` (`tests/fuzz.rs`, 10s bound),
      `code_generation_is_covered_by_the_compile_budget`,
      `many_loop_constants_compile_in_linear_time`
      (`tests/compile_limits.rs`), `keys_chosen_to_collide_do_not_slow_down_maps`
      (`tests/metering.rs`), `many_locals_resolve_in_linear_time`
      (`tests/compile_limits.rs`). Bound the work instead (fuel, instructions,
      the ratio between two input sizes in the same run).
- [ ] Run the playground page tests in CI across Chrome, Firefox, and Safari.
- [ ] `Config::retry_stress` fails only the first check in a `vm_function`
      body, so it does not detect a side effect between the first and second
      checks. Run stressed tests for each check index reached during a call.
- [ ] Run the `cargo fuzz` targets on a schedule (or on OSS-Fuzz). Update the
      `run` target to compare optimized and unoptimized executions.
- [ ] Extend `src/check/verify.rs` to verify full HIR expression typing in
      addition to trait evidence and written type bounds. Recording the type of
      each local in `LocalInfo` is required to verify local variable uses.
- [ ] Extend `tests/props/generics.rs` to generate enums with payloads, generic
      traits (`Into<T>`), method type parameters, `where` clauses, `extend`
      blocks, generic function values, local generic functions, and Rust-bound
      types.
- [ ] Mutate typed ASTs in `tests/fuzz.rs` (for example, by replacing a type,
      an expression of the same type, or a call) instead of mutating raw source
      characters, which mostly produces syntax or type errors.

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
- [ ] Support fields of type `Self` and nested generic types with trait bounds
      (`struct Sorted<T: Ord>`) in `#[derive(SandEnum)]` and
      `#[derive(SandStruct)]`. These derive macros substitute `Param<N>` for
      each type parameter to describe field types, and `Param<N>` does not
      implement Rust traits such as `Ord`.
- [ ] Represent `ValueType` constants in `ConstVal` by their text form so that
      `#[sand(default)]` supports value-type fields and `json.decode` evaluates
      constant value-type defaults directly.
- [ ] Pass `Num<N>` evidence to native `impl` methods by extracting the `Num`
      dictionary from the `impl` dictionary.
- [ ] Unify the terminology for Rust types attached via `NativeModule::bind`
      (`BoundKind`, `BoundType`, `Type::bound`, `declare_bound_types`, and
      `Binding<T>`).
- [ ] Implement `SandType`, `FromValue`, and `IntoValue` for
      `std::ops::Range<i64>` and `RangeInclusive<i64>` so that `SandStruct`
      fields can use Rust range types and `#[sand(default)]`.

### Tooling

- [ ] Fix broken intra-doc links to private items and run `cargo doc` with
      `-D warnings` in CI.
- [ ] Report `net` policy rules that broader `net` rules already decide (a
      `net` rule decides both connections and http requests), and `env` and
      `listen` rules that broader rules already decide. The linter now reports
      only exact repeats of these rules.
- [ ] Make the policy linter near-linear when many rules of the other verb lie
      below a broad rule: with 20,000 `allow read /r/hN`, 5,000 `deny
      /r/*/aN` and `allow read /r`, `sand lint` takes 12 s, because it compares
      each narrow rule with each deny rule.
- [ ] Stop `sand lint` (and other commands that print) from panicking when
      stdout closes early (`sand lint a.policy | head`).

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
- [ ] Deduplicate missing trait diagnostics within a single expression (for
      example, `set([need(x), x])` with `x: float?` and `need<T: Hash>` reports
      `float? does not implement Hash` twice at overlapping spans).
- [ ] Display the target type arguments for specialized `impl` blocks in the
      `Implements` section of `help` (for example, `help(List)` currently lists
      `impl Describe for [int]` as `Describe`).
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
- [ ] Replace the VM dispatch loop with a threaded dispatch table (a separate
      function per instruction using guaranteed tail calls via Rust's `become`
      keyword once stabilized) to eliminate register-allocation variance across
      builds.

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
- [ ] Support trait objects (`dyn Trait`), associated types and constants, and
      blanket implementations (`impl<T> X for T`).
- [ ] **Check that all written types satisfy constructor trait bounds.** The
      type checker currently verifies constructor trait bounds on struct fields,
      enum variant payloads, and `reflect` type arguments, and infers implied
      bounds for structs, enums, and `extend` blocks. Extend well-formedness
      checking and implied bound inference to all remaining declarations and
      type annotations:
      - Infer implied trait bounds on functions, methods, and `impl` blocks from
        type parameters that appear as direct arguments to constrained type
        constructors in parameter types, return types, trait arguments, or
        `impl` target types (for example, `fn size<T>(b: Bag<T>)` infers
        `T: Hash`).
      - Do not infer bounds from types nested inside trait arguments (to ensure
        termination on recursive bounds such as `struct S<T: Tr<S<[T]>>>`) or
        on `native fn` declarations.
      - Verify constructor trait bounds on all remaining type annotations,
        including `native fn` signatures, concrete type arguments
        (`Set<[int]>`, `Bag<float>`), and non-parameter type arguments
        (`Bag<[T]>`).
      - Include the origin of an inferred bound in diagnostic messages if a
        caller does not satisfy it (for example, "required because `size` takes
        a `Bag<T>`").
- [ ] Allow struct field defaults to call methods from traits required by the
      struct's type parameters (`struct Bag<T: Make> { x: T = T.make() }`) by
      passing trait dictionaries to default-value helpers and `json.decode`
      field initializers.
- [ ] Allow local functions to require traits with methods (such as `Num`) and
      to declare `reflect` type parameters by passing hidden dictionary and
      type-description arguments to closures.
- [ ] Replace the hardcoded `+` operand check (`require_plus` in
      `src/check/unify.rs`) with a prelude `Add` trait so that generic functions
      can concatenate `str`, `bytes`, and lists in addition to adding numbers.
- [ ] Support non-constant default expressions on trait method parameters.
