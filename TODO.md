# TODO

Follow-up work ordered by impact on script authors and host developers.

Triage status:

- ❓ not triaged
- 🔴 triaged: significant user impact; link opens the scenario in `triage/`
- ⚪ triaged: without significant user impact

## Tier 1: trust

Untrusted code violates a valid policy.

- [ ] 🔴 [030](triage/030-windows-env-case/summary.md) _Policy._ Fold environment variable identifiers case-insensitively on Windows in
      `RulePolicy` to match Windows environment variable lookup.
- [ ] 🔴 [002](triage/002-windows-rename-junction-race/summary.md) _Sandbox._ **Windows rename.** Windows lacks a safe API to rename files by
      handle. `rename` keeps every directory on both paths open so that
      another process cannot rename or replace the directories. However, a
      process can still empty a parent directory and convert it into a
      junction during the call; `rename` detects this change after the
      operation instead of preventing it.

## Tier 2: host reliability

The host cannot rely on engine limits, stability, or control.

- [ ] ❓ _Memory._ Allocate small object buffers from fixed-size block pools so that the
      engine accounts for retained pool capacity against `max_memory`. If a
      script frees small buffers and then allocates larger ones, the system
      allocator retains the freed blocks and peak process memory can reach
      1.3x `max_memory`.
- [ ] ❓ _Memory._ Account for `remembered` and `gray` growth in the write barrier and
      trigger garbage collection if they exceed `max_memory`. A loop that
      mutates old objects without allocating does not trigger collection or
      table measurement, so `remembered` and `gray` can grow by 20 bytes per
      object plus spare capacity without a limit check.
- [ ] ❓ _Memory._ Store the object arena in fixed-size segments instead of a single `Vec`
      so that collections can release any empty segment and growing the arena
      does not copy existing slots. A contiguous `Vec` can release only
      trailing free slots after a full sweep, and incremental sweeps prepend
      freed slots to the free list in descending index order so that new
      allocations reuse high-index slots first.
- [ ] 🔴 [052](triage/052-dead-temporaries-rooted/summary.md) _Performance._ Clear dead temporaries before native calls so that inlined allocations
      do not stay rooted in caller frame registers during garbage collection.
- [ ] ❓ _Embedding._ Convert derived `SandEnum` and `SandStruct` values with an explicit
      stack, as `Json` conversion does. Derived conversion recurses once per
      nesting level, and the 256-level limit does not bound its stack use: in
      an unoptimized build, derived `Json` conversion overflowed a 512 KiB
      thread stack at 100 levels, which aborts the host process.
- [ ] ❓ _Embedding._ Make `json.parse`, `json.encode`, `json.decode`, and `json.to_json`
      use constant stack without loss of speed. They recurse once per nesting
      level. At 512 levels, an unoptimized build needs 2 MiB of stack for
      `parse` and `decode` and more than 2 MiB for `encode`, so a script can
      abort a debug host that runs it on a default 2 MiB thread. A release
      build needs 256 KiB. Rewrites on the explicit-stack `walk` in
      `src/stdlib/json.rs` were 10–35% slower in release builds: one loop for
      all container kinds keeps its state in memory and dispatches on each
      child, where recursion has a loop for each kind.
- [ ] 🔴 [019](triage/019-value-type-defaults/summary.md) _Embedding._ Represent `ValueType` constants in `ConstVal` by their text form so that
      `#[sand(default)]` supports value-type fields and `json.decode` evaluates
      constant value-type defaults directly.
- [ ] 🔴 [027](triage/027-netaddr-missing-request-context/summary.md) _Policy._ Include the originating HTTP method and URL on `Capability::NetAddr` so
      that `CustomPolicy` and `RulePolicy::check` evaluate resolved addresses
      with full request context.
- [ ] 🔴 [040](triage/040-live-policy-needs-custom-policy/summary.md) _Policy._ Support shared, dynamically updatable `RulePolicy` instances and remove
      `CustomPolicy` after host applications migrate to rule policies.
- [ ] 🔴 [015](triage/015-web-stop-busy-engine/summary.md) _Web._ Stopping a busy engine in the browser terminates its worker because
      `Engine::interrupt` only reaches an engine that is waiting. With
      cross-origin isolation, store the interrupt flag in a
      `SharedArrayBuffer` that the page sets directly.

## Tier 3: ordinary scripts

Common scripts fail, require workarounds, or produce misleading diagnostics.

- [ ] 🔴 [058](triage/058-enum-variant-inference/summary.md) _Language._ Infer the enum type of a variant expression from the expected type, as
      pattern matching does.
- [ ] 🔴 [064](triage/064-default-params-fn-values/summary.md) _Language._ Preserve default parameter values in function types so that calls through
      function values can omit defaulted arguments.
- [ ] 🔴 [066](triage/066-trait-objects/summary.md) _Language._ Support trait objects, associated types and constants, and
      blanket implementations.
- [ ] 🔴 [070](triage/070-generic-plus/summary.md) _Language._ Replace the hardcoded `+` operand check in `src/check/unify.rs`
      with a prelude `Add` trait so that generic functions can concatenate
      `str`, `bytes`, and lists in addition to adding numbers.
- [ ] 🔴 [063](triage/063-decode-with-init/summary.md) _Language._ Support `Decode` for structs that declare `init` by calling `init` with
      the JSON object members as named arguments:

      - Propagate `Err` results from deferred `Decoded::Pending` thunks in
        `src/stdlib/json.rs` so that a fallible `init` fails `json.decode`.
      - Pass trait dictionaries to `Decoded::Pending` thunks so that a generic
        `init` can call trait methods on its type parameters.
      - Execute `init` before inserting decoded structs into a `Set`, as
        `json.decode` does for computed field defaults.
      - Verify that `init` is visible at the `json.decode` call site.

- [ ] 🔴 [068](triage/068-generic-field-defaults/summary.md) _Language._ Allow struct field defaults to call methods from traits required by the
      struct type parameters by passing trait dictionaries to default-value
      helpers and `json.decode` field initializers.
- [ ] 🔴 [062](triage/062-error-kind-enum/summary.md) _Language._ Evaluate replacing string error kinds with an enum.
- [ ] 🔴 [072](triage/072-explicit-type-args/summary.md) _Diagnostics._ Accept explicit type arguments in calls, or make the parser error suggest
      `let config: Config = json.decode(text)` instead of `comparison operators cannot be chained`.
- [ ] 🔴 [045](triage/045-parser-follow-on-errors/summary.md) _Diagnostics._ Suppress follow-on parser errors after an initial syntax error
      and remove the follow-on expectations from the spec tests.
- [ ] 🔴 [046](triage/046-match-arm-recovery/summary.md) _Diagnostics._ Recover at the next `match` arm after an invalid pattern
      instead of reporting a spurious `unexpected }` at the end of the `match`.
- [ ] 🔴 [067](triage/067-bound-diagnostics/summary.md) _Language._ **Verify that all written types satisfy constructor trait bounds.**
      Extend well-formedness checking and implied bound inference to all
      remaining declarations and type annotations:

      - Infer implied trait bounds on functions, methods, and `impl` blocks from
        type parameters that appear as direct arguments to constrained type
        constructors in parameter types, return types, trait arguments, or
        `impl` target types.
      - Do not infer bounds from types nested inside trait arguments or on
        `native fn` declarations.
      - Verify constructor trait bounds on all remaining type annotations,
        including `native fn` signatures, concrete type arguments, and
        non-parameter type arguments.
      - Include the origin of an inferred bound in diagnostic messages if a
        caller does not satisfy it.

## Tier 4: polish

Visible defects that rarely block users.

- [ ] 🔴 [018](triage/018-derive-bounded-generics/summary.md) _Embedding._ Support fields of type `Self` and nested generic types with trait bounds
      in `#[derive(SandEnum)]` and `#[derive(SandStruct)]`. These derive
      macros substitute `Param<N>` for each type parameter to describe field
      types, and `Param<N>` does not implement Rust traits such as `Ord`.
- [ ] 🔴 [022](triage/022-rust-range-interop/summary.md) _Embedding._ Implement `SandType`, `FromValue`, and `IntoValue` for
      `std::ops::Range<i64>` and `RangeInclusive<i64>` so that `SandStruct`
      fields can use Rust range types and `#[sand(default)]`.
- [ ] 🔴 [024](triage/024-encoded-slash-rules/summary.md) _Policy._ Support HTTP paths with encoded separators `%2F` and `%5C` in `rule_for`
      so that "always" prompt answers can generate rules for those requests.
- [ ] 🔴 [025](triage/025-matrix-param-deny-bypass/summary.md) _Policy._ Evaluate HTTP paths that contain `;` parameters both with and without
      matrix parameters in `url_segments` in `src/policy/net.rs` so that `deny http a.com/admin/**` blocks `/admin;x/secret` and `/public/..;/admin`.
      Also distinguish raw non-UTF-8 bytes from percent-encoded `%25` sequences in `percent_decode` so that `allow http a.com/%25FF` does not match `/%FF`.
- [ ] 🔴 [026](triage/026-prompt-ip-rule-never-prompts/summary.md) _Policy._ Support interactive prompts for resolved IP addresses that match a
      `prompt` address rule instead of denying them during address resolution.
- [ ] 🔴 [028](triage/028-hint-grants-subtree/summary.md) _Policy._ Add exact-file and exact-path rule syntax so that `rule_for` generates
      rules that match only the prompted file or URL path instead of a directory
      subtree or host prefix.
- [ ] 🔴 [029](triage/029-env-listing/summary.md) _Policy._ Separate the `env.vars()` listing capability from `Capability::Env("*")`
      or return `None` from `rule_for(&Capability::Env("*"))`, because `allow env *`
      does not override existing `deny` or `prompt` rules on individual
      variables.
- [ ] 🔴 [032](triage/032-lint-quoted-env-wildcard/summary.md) _Policy._ Expand `lint_policy` to detect shadowed and redundant `env`, `stdin`,
      `stdout`, `stderr`, `timezone`, `custom`, and `listen` rules, `allow net *.localhost`
      rules overridden by `deny net localhost`, and quoted wildcard
      literals such as `deny env "AWS_*"`.
- [ ] 🔴 [033](triage/033-relative-builder-paths/summary.md) _Policy._ Resolve or reject relative paths in `RulePolicy` file builder methods so
      that `Display` always emits valid, parseable policy syntax.
- [ ] 🔴 [034](triage/034-always-answer-exact-capability/summary.md) _Policy._ Store "always" prompt answers as `RulePolicy` rules rather than exact
      `Capability` values so that an "always" grant for `write /x/o.txt` also
      permits `read /x/o.txt`.
- [ ] 🔴 [035](triage/035-denied-address-missing-from-error/summary.md) _Policy._ Include any policy-denied IP addresses in the error message if
      `resolve_allowed` fails to connect to the remaining allowed addresses.
- [ ] 🔴 [036](triage/036-lint-http-method-host-confusion/summary.md) _Policy._ Warn in `lint_policy` if an uppercase hostname in an `http` rule is
      parsed as an HTTP method, or if a lowercase method is parsed as a
      hostname.
- [ ] 🔴 [038](triage/038-prompt-hides-resolved-address/summary.md) _Policy._ Warn in the CLI prompter before allowing plaintext network connections,
      and display the resolved IP addresses in the prompt.
- [ ] 🔴 [041](triage/041-fmt-comment-drift/summary.md) _Tooling._ Make `sand fmt` idempotent if a multi-line expression or nested tuple
      index precedes line comments.
- [ ] 🔴 [043](triage/043-lint-overridden-listen-env/summary.md) _Tooling._ Report `env` and `listen` policy rules that are subsumed by broader rules.
- [ ] 🔴 [047](triage/047-mixed-list-enum-hint/summary.md) _Diagnostics._ Suggest an enum in addition to a struct or tuple for mixed-type list
      literals.
- [ ] 🔴 [048](triage/048-assert-top-level-frame/summary.md) _Diagnostics._ Include the `<top level>` stack frame if a failing `assert` is the
      final top-level statement.
- [ ] 🔴 [050](triage/050-help-specialized-impl/summary.md) _Diagnostics._ Display the target type arguments for specialized `impl` blocks in the
      `Implements` section of `help`.
- [ ] 🔴 [051](triage/051-help-qualify-types/summary.md) _Diagnostics._ Qualify types from other modules in `help` output.
- [ ] 🔴 [059](triage/059-ordering-enum/summary.md) _Language._ Add an `Ordering` enum to the prelude and return it from
      `time.Span.compare` instead of `-1`, `0`, or `1`.
- [ ] 🔴 [060](triage/060-weekday-enum/summary.md) _Language._ Evaluate returning a `time.Weekday` enum instead of an `int`
      from `day_of_week()` in the `time` module.
- [ ] 🔴 [061](triage/061-fs-kind-enum/summary.md) _Language._ Evaluate replacing `is_file` and `is_dir` on `fs.Metadata` with an
      `fs.Kind` enum.
- [ ] 🔴 [065](triage/065-import-position/summary.md) _Language._ Reject `import` statements after non-import items or statements, as
      specified in §3.1.

## Items without significant user impact

### Miscellaneous

- [ ] ⚪ Move all crates into a `crates/` directory and standardize crate metadata to use workspace values.

### Test coverage

- [ ] ⚪ Run the CI workflow on Windows and Linux, including the ConPTY variant of
      `cli/tests/prompt_tty.rs` and the REPL line editor.
- [ ] ⚪ Run the Windows path resolver and file operations in `files/windows.rs` on
      Windows; CI currently only cross-compiles them.
- [ ] ⚪ Run the Linux path resolver in `files/unix.rs` and the non-UTF-8 file name
      tests in `tests/sandbox_io.rs` on Linux.
- [ ] ⚪ Replace wall-clock bounds with work bounds such as fuel, instruction counts,
      or input-size ratios in `hostile_runtime_programs_are_contained` in
      `tests/fuzz.rs`, `code_generation_is_covered_by_the_compile_budget`,
      `many_loop_constants_compile_in_linear_time`, and
      `many_locals_resolve_in_linear_time` in `tests/compile_limits.rs`, and
      `keys_chosen_to_collide_do_not_slow_down_maps` in `tests/metering.rs`.
- [ ] ⚪ Run the playground page tests in CI across Chrome, Firefox, and Safari.
- [ ] ⚪ `Config::retry_stress` fails only the first check in a `vm_function`
      body, so it does not detect a side effect between the first and second
      checks. Run stressed tests for each check index reached during a call.
- [ ] ⚪ Schedule the `cargo fuzz` targets and update the `run` target to compare
      optimized and unoptimized executions.
- [ ] ⚪ Extend `src/check/verify.rs` to verify full HIR expression typing in
      addition to trait evidence and written type bounds. Record the type of
      each local in `LocalInfo` to verify local variable uses.
- [ ] ⚪ Extend `tests/props/generics.rs` to generate enums with payloads, generic
      traits, method type parameters, `where` clauses, `extend` blocks, generic
      function values, local generic functions, and Rust-bound types.
- [ ] ⚪ Mutate typed ASTs in `tests/fuzz.rs` instead of mutating raw source
      characters.

### Spec

- [ ] ⚪ Document in §14.4 that `time.local_zone()` returns `UTC` if the
      `TimeZone` capability is denied.

### Web

- [ ] ⚪ Compare the `wasm-release` profile against `opt-level = "s"` for
      WebAssembly binary size and execution speed.

### Embedding

- [ ] ⚪ Pass `Num<N>` evidence to native `impl` methods by extracting the `Num`
      dictionary from the `impl` dictionary.
- [ ] ⚪ Unify the terminology for Rust types attached via `NativeModule::bind`
      across `BoundKind`, `BoundType`, `Type::bound`, `declare_bound_types`, and
      `Binding<T>`.

### Policy

- [ ] ⚪ Connect directly to parsed IP literals in `resolve_allowed` without
      checking `Capability::NetAddr`, and remove the IP-literal shortcut from
      `RulePolicy::resolved_verdict`.
- [ ] ⚪ Store the source file path or CLI flag on `Origin` so that
      `RulePolicy::describe_rule` reports the exact file or flag instead of only
      a line number.
- [ ] ⚪ Add a callback for persisting "always" prompt rules from `rule_for` so
      that host applications can save granted rules across sessions.
- [ ] ⚪ Rename `Capability::Custom` to `Capability::Module` to distinguish
      native-module capabilities from `CustomPolicy`.

### Tooling

- [ ] ⚪ Fix broken intra-doc links to private items and run `cargo doc` with
      `-D warnings` in CI.
- [ ] ⚪ Index opposing rules in the policy linter to avoid quadratic comparisons
      between narrow rules and opposing rules.

### Diagnostics

- [ ] ⚪ Deduplicate missing trait diagnostics within a single expression.

### Performance

- [ ] ⚪ Benchmark `HostFn::Static` against `HostFn::Shared` to determine whether
      avoiding the `Arc` dereference justifies `Box::leak` and the enum
      dispatch; if not, keep only `Arc`.
- [ ] ⚪ Profile `HostFn` dispatch and argument slice construction on `fannkuch` and
      `results` in `bench/programs` to reduce typed native call overhead.
- [ ] ⚪ Reduce `json.decode` dispatch overhead in the Sand wrapper around
      `decode_raw`.
- [ ] ⚪ Cache instantiated `TypeDesc` objects in `Op::Describe` by their
      template and argument descriptions to avoid allocation on each generic
      `reflect` call.
- [ ] ❓ Align the VM dispatch loop or build benchmarks with
      `-C llvm-args=-align-loops=64` so that unrelated code changes do not
      shift loop alignment. On Apple silicon, code placement changes alter
      `integer loop 10M`, `game of life`, and `option ? chains` in
      `benches/vm.rs` by up to 13% at identical instruction counts.
- [ ] ⚪ Replace the VM dispatch loop with a threaded dispatch table once Rust
      stabilizes the `become` keyword for guaranteed tail calls.

### Language

- [ ] ⚪ Allow local functions to require traits with methods and to declare
      `reflect` type parameters by passing hidden dictionary and
      type-description arguments to closures.
- [ ] ⚪ Support non-constant default expressions on trait method parameters.
