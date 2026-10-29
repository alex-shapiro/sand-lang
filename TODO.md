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
- [ ] `peak_rss_stays_near_the_memory_limit` in `tests/peak_rss.rs` failed in
      7 of 9 runs on macOS because the "decoded computed defaults" case
      exceeds the 1.25 limit ratio at 1.32.
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

### Policy

- [ ] Connect directly to parsed IP literals in `resolve_allowed` without
      checking `Capability::NetAddr`, and remove the IP-literal shortcut from
      `RulePolicy::resolved_verdict`.
- [ ] Support HTTP paths with encoded separators `%2F` and `%5C` in `rule_for`
      so that "always" prompt answers can save rules for those requests.
- [ ] Prompt for a resolved address that a `prompt` address rule decides
      (for example, a name that `allow net *` allows and that resolves to
      `10.0.0.5`) instead of denying it. The check runs after the native call
      returned to the event loop, so it needs a way to suspend the
      connection for a prompt answer.
- [ ] Give `Capability::NetAddr` the request that its host came from (method
      and URL; it has the transport), so that custom policies and
      `RulePolicy::check` decide resolved addresses as
      `RulePolicy::check_resolved` does.
- [ ] Make "always" answers saved with `rule_for` as narrow as the prompt.
      A file rule is a prefix, so `allow read /x/data` from the prompt
      `read /x/data` also allows `/x/data/secret/key.pem`. An http rule
      without a path allows every path, so the prompt `GET
      https://example.com/` saves a rule that also allows `GET /admin`. The
      engine keeps the exact capability, so the same answer decides
      differently in the run that asked and in the next run (the
      playground). The path pattern language has no "this path only" form.
- [ ] A saved `allow env *` (from an "always" answer to `env.vars()`) does
      not decide the next `Env("*")` request while a deny or prompt rule
      decides some variable: the listing gets the strictest decision of any
      variable. `rule_for(Env("*"))` cannot express "allow the listing", so
      it should return `None` or the listing needs its own rule.
- [ ] Environment rules compare names case-sensitively (`env_names` in
      `rules.rs`), but Windows environment names are case-insensitive, so
      there `env.get("aws_secret_access_key")` reads the variable that `deny
      env AWS_SECRET_ACCESS_KEY` denies. Fold names on Windows as file rules
      do.
- [ ] A denial by a command-line rule cites "(line 1)" for every flag, and
      a denial by a policy file rule gives no file name
      (`RulePolicy::describe_rule` knows only `Origin::line`). Give `Origin`
      a source (`--deny=...` or the file).
- [ ] The lint reports only identical repeats of rules without patterns
      (`env`, `stdin`, `stdout`, `stderr`, `timezone`, `custom`, `listen`).
      It misses overridden pairs (`allow env HOME` + `deny env HOME`, `prompt
      stdin` + `allow stdin`), subsumed rules (`allow env *` + `allow env
      HOME`), and a `prompt net a.com` beside `allow net a.com`. It also
      misses `prompt write **` beside `allow **` (the allow rule always wins),
      an allow rule that a `deny net localhost` always overrides (`allow net
      app.localhost`: `NetPattern::covers` does not know that `localhost`
      also matches the names under it), and a `deny env "AWS_*"` whose quoted
      `*` matches no real variable.
- [ ] The file rule builders keep a relative path as written, so the display
      of `RulePolicy::new().allow_read("data")` (`allow read data`) does
      not parse, and `deny_read("~/x")` or `deny_read("./x")` display as text
      that parses to a home or base relative rule. Relative paths resolve
      against the process (host) or `/` (`memory_path`), so the builders
      need one meaning for them (or must reject them) before the display
      can write them.
- [ ] The engine remembers "always" answers per exact capability
      (`Host::always`), so answering "always" to `write /x/o.txt` asks again
      for `read /x/o.txt`, although a write rule also allows reading.
      Remember the answer as the rule that `rule_for` gives instead.
- [ ] If the policy denies some addresses of a name and the connection to
      the others fails, the error shows only the connection error (`io:
      Connection refused`), not the denied addresses and their rule
      (`resolve_allowed` in `src/stdlib/net.rs`).
- [ ] Lint rules whose words a reader may misread: `deny http LOCALHOST
      a.com` reads `LOCALHOST` as a method (any upper-case word is one),
      and `allow http purge a.com` reads `purge` as a host.
- [ ] Let hosts persist "always" prompt answers: a hook that receives the
      answer as a rule (`rule_for`) to save, so that a host such as Ash
      Control can load it into the policy at the next start instead of
      asking again.
- [ ] Make the CLI prompter (and document for hosts) warn before allowing a
      plaintext connection, and show the addresses that a name resolved to.
- [ ] Simplify the terms of the policy API: "capability" (a request kind),
      `Capability::Custom` (a request kind of a host module) and
      `CustomPolicy` (a host-defined policy) are easy to confuse. For
      example, rename `Capability::Custom` to `Capability::Module`.
- [ ] Remove `CustomPolicy` (`Config::custom_policy`) once Ash Control
      decides with rules: rules then need a shared, updatable form, since
      Ash applies a grant to all conversations and resets the policy without
      restarting the interpreter.

### Tooling

- [ ] Make `sand fmt` idempotent when a multi-line expression contains line
      comments (for example, `//r /\nf.7\t.// \n7/e\nf.//as\nf` shifts comments
      on each formatting pass).

- [ ] Fix broken intra-doc links to private items and run `cargo doc` with
      `-D warnings` in CI.
- [ ] Report `env` and `listen` policy rules that are subsumed by broader rules.
- [ ] Index opposing rules in the policy linter to avoid quadratic comparisons
      between narrow rules and opposing rules.

### Diagnostics

- [ ] Suppress follow-on parser errors after an initial syntax error
      (`unexpected }` after a missing `,` or `:`, `expected )` after an
      unterminated string) and remove the follow-on expectations from the spec
      tests.
- [ ] Recover at the next `match` arm after an invalid pattern
      (`1..5 =>`, `Point { x } =>`) instead of reporting a spurious
      `unexpected }` at the end of the `match`.
- [ ] Suggest an enum in addition to a struct or tuple for mixed-type list
      literals (`[1, "a"]`).
- [ ] Include the `<top level>` stack frame if a failing `assert` is the
      final top-level statement.
- [ ] Deduplicate missing trait diagnostics within a single expression (for
      example, `Set([need(x), x])` with `x: float?` and `need<T: Hash>` reports
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

- [ ] Support `Decode` for structs that declare `init` by calling `init` with
      the JSON object's members as named arguments:
      - Propagate `Err` results from deferred `Decoded::Pending` thunks in
        `src/stdlib/json.rs` so that a fallible `init` fails `json.decode`.
      - Pass trait dictionaries to `Decoded::Pending` thunks so that a generic
        `init` can call trait methods on its type parameters.
      - Execute `init` before inserting decoded structs into a `Set`, as
        `json.decode` already does for computed field defaults.
      - Verify that `init` is visible at the `json.decode` call site.
- [ ] Preserve default parameter values in function types so that calls through
      function values can omit defaulted arguments (for example,
      `["a"].map(Error)`).
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
