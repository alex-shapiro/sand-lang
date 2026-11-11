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

- [ ] ❓ _Embedding._ Reject a raw `VmValue` that a native function holds
      across a wait. The compiler rejects typed VM handles in a future because
      they are not `Send`. However, `IntoVmValue::into_vm_value` and `Vm`
      allocation methods return a raw `VmValue`. `VmValue` is `Send` because the
      heap stores it. The compiler therefore accepts a function that allocates a
      `VmValue` before a wait and reads it in the `Later::new` finisher. After a
      garbage collection during the wait, the read returns the data of another
      object without an error. If the collector shrank the heap, the read or the
      next collection causes a panic. The engine reports the panic as an
      internal error. A `panic = "abort"` build stops the process. Options: a
      host value type that is not `Send`, or a generation check on each heap
      slot.
- [ ] ❓ _Embedding._ Add a rooted handle that keeps a script value alive
      across a wait. A native function cannot safely change an argument object
      or return an early allocation after a wait. `Vm::pinned` already holds
      garbage collection roots.
- [ ] ❓ _Embedding._ Give native functions a narrow call context in place of
      `&mut Vm`. With `&mut Vm`, a native function can call public scheduler and
      collector methods such as `Vm::collect_all_garbage`. Typed accessors also
      return raw `VmValue` slices. A narrow context exposes only host call
      operations.
- [ ] ❓ _Memory._ Share standard library and host module bytecode across
      programs. `compile_program` copies the snapshot `Program` into each
      `CompiledProgram`. A copy takes about 74 KB for `print(1)` and 173 KB
      with a host module. Keeping 1,000 compiled programs uses about 170 MB for
      these copies. Split `Program` into a shared base and per-program tables.
      Benchmark the extra VM lookup. Then remove `Engine::recycle` and
      `Compiled::snapshot`.
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
- [ ] ❓ _Metering._ Make regex fuel independent of the machine. The engine
      charges regex work at least 1 unit per microsecond to bound searches that
      the byte charge underestimates. This time charge changes total fuel and
      task switches between runs. The compile charge also uses `memory_usage`,
      which differs across platforms. Replace both charges with a deterministic
      cost model, or check the time limits without charging fuel.
- [ ] ❓ _Scheduling._ End a slice after an instruction spends the slice's fuel.
      The interpreter syncs instruction charges to the meter only at native
      calls. A task that runs only instructions can spend far more than its
      20,000-unit slice while other tasks wait. Fix this before adding the
      journal, because slice changes break paused runs.
- [ ] ❓ _Testing._ Compare the active fiber and fuel at the start of each slice
      across GC and retry modes. Current tests compare only total fuel.
      Comparing each slice catches task-switch changes and cost changes that
      break paused runs.
- [ ] ❓ _Testing._ Compare fuel across hash seeds in the spec suite. Add a test
      configuration option to set the hash seed.
- [ ] ❓ _Testing._ Compare fuel between a 32-bit and a 64-bit target. No test
      runs a program on both, so the deterministic fuel rule holds across
      platforms only by review. Add a small set of programs with pinned fuel
      totals that runs on a native target and on `wasm32`. Include a host
      function that calls `Vm::to_value`.
- [ ] ❓ _Embedding._ Convert derived `SandEnum` and `SandStruct` values iteratively with an
      explicit stack. The derived `FromVmValue` and `IntoVmValue` implementations
      recurse once per nesting level. Deep values can overflow a small host
      thread stack in debug builds before reaching the 256-level limit.
- [ ] ❓ _Embedding._ Convert `json.parse` and `json.decode` to use an explicit
      stack without regressing throughput. These functions recurse up to the
      512-level nesting limit, which can overflow a 2 MiB thread stack in debug
      builds.
- [ ] ❓ _Embedding._ Drop a deep `Value` without recursion. `Vm::to_value`
      copies at most 512 levels, but a host can build a deeper `Value` in Rust.
      The default drop recurses once per level, so such a value can overflow
      the host thread stack when it drops.
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
- [ ] ❓ _Embedding._ **Decide whether an interrupt before a call starts should
      stop that call.** `InterruptHandle::interrupt` sets the interrupt flags
      of active calls only. Each call creates a new flag at the first poll of
      its future. On the web, `engine.run(src); engine.interrupt();` leaves the
      run active because `engine.run` polls the future asynchronously. A host
      with an engine per job also cannot cancel a queued job before `run`
      starts. Options:
      - Keep this behavior. Optionally create the interrupt flag before
        returning the future so that an immediate interrupt stops the call.
      - Make `InterruptHandle` sticky and add `InterruptHandle::reset()`.
        However, if two engines share a handle, resetting the handle for one
        engine clears a pending interrupt for the other engine.
      - Recommended: make `InterruptHandle` single-use and sticky, like
        `tokio_util::sync::CancellationToken`, `AbortSignal`, and Go's
        `context`. Add `Engine::set_interrupt` and `Session::set_interrupt` so
        the host can replace a used handle. This design also supports item 015.
- [ ] ❓ _Sandbox._ Cap the number of worker threads for blocking I/O jobs. If
      a call stops at a limit or an interrupt during a blocking job, the
      engine abandons the job. The worker thread remains blocked, and
      `Pool::run` in `src/platform.rs` spawns a new thread for the next job.
      Repeated calls against a hung path can exhaust host threads. Cap the
      thread pool or the blocked jobs per engine. Return an I/O error if a new
      job exceeds the cap.
- [ ] ❓ _Embedding._ Share test-execution logic between `Engine::run_tests`
      and `Session::run_tests`, and apply `max_wall_time` consistently.
      `Engine::run_tests` applies `max_wall_time` to the entire test suite.
      `Session::run_tests` applies `max_wall_time` to each test individually.
      `Engine::run_tests` unwinds spawned tasks after each test.
      `Session::run_tests` leaves spawned tasks running. A task from an earlier
      test can then call `exit` or `fail` during a later test. Choose a single
      rule and document it in SPEC §4.8. Keep session tests on the persistent
      session VM. Stop the test suite if an internal error resets the session.
- [ ] ❓ _Sandbox._ Read the main script file through `Engine::discovery_io`
      and `Fs::read_source` in `Engine::read_main`, as module imports do.
      `Engine::read_main` currently reads the entire file on the caller's
      thread before checking `max_source_bytes`. Offloading the bounded read
      enforces `max_source_bytes`, `max_wall_time`, and interrupts on a large
      file, a FIFO, or a hung mount.
- [ ] ❓ _Memory._ Count compiler and linter memory toward `max_memory`.
      SPEC §15.3 excludes compilation from `max_memory`. Only `max_source_bytes`
      and `max_compile_fuel` bound memory use during compilation. Counting
      compiler memory will reject scripts that compile today under a low
      `max_memory` limit.
- [ ] ❓ _Memory._ Bound the size of lint warning messages. Identifiers can be as
      long as the source file. The linter charges fuel per identifier rather
      than per byte of warning text. In `shadowed-variable`, the linter also
      formats the full type before charging fuel. The linter can therefore
      allocate gigabytes of warning text for long identifiers or deep types.
      Either cap identifier length in the lexer or charge fuel per byte before
      formatting warning text.
- [ ] ❓ _Performance._ Index enum variants and struct fields by name in
      `TypeDef`. Variant and field lookups in `src/check/expr.rs` and
      `src/check/pat.rs` scan the member list linearly without charging compile
      fuel. A 16 MiB script with 400,000 variants runs for 91 seconds before
      exhausting compile fuel.
- [ ] ❓ _Embedding._ Charge lint-only checker work to the lint budget. During
      linting, the type checker charges `argument-order` and import checks to
      the compile budget. A script near the compile limit can therefore pass
      `sand check` and fail `sand lint` with `program too complex to compile`.
- [ ] ❓ _Embedding._ Allow hosts to configure the lint fuel limit.
      `lint_module` derives the lint budget only from the source size. Add
      `Limits::max_lint_fuel` or derive the lint budget from
      `Limits::max_compile_fuel`.
- [ ] ❓ _Embedding._ Yield to the executor while waiting for an output sink.
      `Vm::wait_backlog` blocks the current thread in a loop. A stalled sink
      blocks other engines on that thread. It also prevents
      `Session::run_tasks_until` from polling its `until` future. Only a limit
      or an interrupt from another thread can stop the wait.

## Tier 3: ordinary scripts

Common scripts fail, require workarounds, or produce misleading diagnostics.

- [ ] ❓ _Tasks._ Run cleanup if a task is preempted at the first script call
      of its deferred code and then cancelled. `tick_call!` rewinds the frame to
      that call. That instruction is outside the cleanup range. As a result, the
      engine skips `defer` blocks and `use` bindings with a script `close`
      method. In a tight loop with a `defer` in each iteration, 99 of 300 runs
      skipped cleanup. Avoid preemption at that call. Add a scheduling stress
      mode that suspends and cancels tasks at each safe point.
- [ ] ❓ _Tasks._ Decide what a caller listed in its own `task.race` does. If
      another task wins, the race cancels the caller with the other tasks.
- [ ] ❓ _Tasks._ Evaluate owner-scoped tasks. An owner block cannot exit until
      its tasks end. Cancelling the block cancels its tasks. See
      [the task lifetime experiment](experiments/task-lifetime.md).
- [ ] ❓ _Tasks._ Free the module-initializer fiber before each test.
      `Engine::run_tests` calls `run_main` twice and keeps the first fiber
      alive. As a result, `sand test` allows one fewer task than `sand run`.
- [ ] ❓ _Tasks._ Decide how module-level tasks behave during tests. `run_tests`
      clears the ready queue after the module initializer finishes. A `Ready`
      task is never queued again. A test that waits for that task fails with a
      deadlock error.
- [ ] ❓ _Tasks._ Test the fiber lifecycle across every combination of stop kind
      and scheduler event. Check the scheduler invariants after each slice in
      debug builds.
- [ ] ❓ _Tasks._ Run all deferred code at the end of a run. `Vm::end_tasks`
      returns early if deferred code requests a permission prompt. The
      remaining tasks then skip their deferred code.
- [ ] ❓ _Testing._ Decide whether `exit(0)` should pass a test. `exit(0)`
      currently stops a test early and passes it. A test that calls `exit(0)`
      before a failing `assert` therefore passes. If `exit` always fails a test,
      add an API for tests that expect an exit code. Top-level `exit(0)` also
      fails every test in the module. A script that calls `exit(0)` at the top
      level cannot have passing tests.
- [ ] ❓ _Testing._ Measure fuel or probe counts instead of wall-clock time in
      `keys_chosen_to_collide_do_not_slow_down_maps` in `tests/metering.rs`.
      The test currently compares elapsed times and fails if the host is under
      load.
- [ ] ❓ _Testing._ Capture test output in `Engine::run_tests` and display it
      for failed tests, or remove the capture claim from SPEC §17.4. SPEC
      §17.4 states that `sand test` captures output. However,
      `Engine::run_tests` sends output to `NullSink`, and `TestResult` does not
      store output.
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
- [ ] 🔴 [022](triage/022-rust-range-interop/summary.md) _Embedding._ Implement `SandType`, `FromVmValue`, and `IntoVmValue` for
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
- [ ] 🔴 [034](triage/034-always-answer-exact-capability/summary.md) _Policy._ Store "always" prompt answers as `RulePolicy` rules rather than exact
      `Capability` values so that an "always" grant for `write /x/o.txt` also
      permits `read /x/o.txt`.
- [ ] 🔴 [035](triage/035-denied-address-missing-from-error/summary.md) _Policy._ Include any policy-denied IP addresses in the error message if
      `resolve_allowed` fails to connect to the remaining allowed addresses.
- [ ] ❓ _Policy._ Include the deciding rule in all `permission_denied` messages.
      Resolved-address denials already include the rule. A host-check denial
      such as `permission denied: GET http://localhost:8080/` omits it. Return
      the rule from the stage-1 check and format both messages with
      `RulePolicy::describe_rule`.
- [ ] ❓ _Policy._ Parse each host into a canonical `Host` enum with
      `Name(String)` and `Ip(IpAddr)` variants across URLs, `net.connect`, and
      policy rules. Six functions normalize host text and disagree. A rule
      accepts `deny net 169.254.169.254.`, whereas a URL with that host is
      invalid. `Url` also keeps the raw host, such as `api.github.com.`. A
      request filter for `api.github.com` then misses the request.
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

- [ ] ⚪ Check the error codes of `compile_fail` doctests for the host API.
      Stable `rustdoc` ignores error codes, so a snippet that fails for another
      reason still passes.
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
- [ ] ⚪ `Config::retry_stress` fails only the first check in a native function
      that takes `&mut Vm`. It does not detect a side effect between the first
      and second checks. Run stressed tests for each check index that a call
      reaches.
- [ ] ⚪ Schedule the `cargo fuzz` targets and update the `run` target to compare
      optimized and unoptimized executions.
- [ ] ⚪ Extend `src/check/verify.rs` to verify full HIR expression typing in
      addition to trait evidence and written type bounds. Record the type of
      each local in `LocalInfo` to verify local variable uses.
- [ ] ⚪ Extend `tests/props/generics.rs` to generate enums with payloads, generic
      traits, method type parameters, `where` clauses, `extend` blocks, generic
      function values, local generic functions, and Rust-bound types.
- [ ] ⚪ Compile the `triage/` `repro.rs` files in a test. Several `repro.rs`
      files call removed APIs such as `Config::module`.
- [ ] ⚪ Mutate typed ASTs in `tests/fuzz.rs` instead of mutating raw source
      characters.

### Spec

- [ ] ⚪ Document in §14.4 that `time.local_zone()` returns `UTC` if the
      `TimeZone` capability is denied.

### Web

- [ ] ⚪ Compare the `wasm-release` profile against `opt-level = "s"` for
      WebAssembly binary size and execution speed.

### Embedding

- [ ] ⚪ Require at compile time that a native function that takes `&mut Vm`
      makes its scheduler requests before its side effects. The engine runs the
      call again after a prompt or a garbage collection, so it executes any
      earlier side effect twice. A check phase that returns `Ctl` and an effect
      phase that returns `Fail` would prevent this in journaled runs.
- [ ] ❓ Decide whether `Config::stdin_bytes` should share its channel across
      every VM of an engine, as `StdinFeed` does. `Engine::new_vm` creates a
      fresh channel for `Config::stdin_bytes`, so `Session::reset` and each
      test in `run_tests` receive the full input again. Sharing a channel
      across VMs would remove `StdinConfig`, but earlier tests could consume
      input before later tests run.
- [ ] ❓ Advance the wall-time deadline through a single type during
      compilation and execution. During compilation, module discovery adds
      prompt wait time to a `prompted` total and checks `deadline + prompted`.
      During execution, `Vm::exclude_wall_time` shifts `deadline` directly.
- [ ] ❓ Compile a script once to check, lint, and test it. `ash` currently
      compiles each tool three times across `check_file`, `lint_file`, and
      `run_tests`. Return lint warnings from compilation and accept a
      `CompiledProgram` in `run_tests`.
- [ ] ⚪ Pass `Num<N>` evidence to native `impl` methods by extracting the `Num`
      dictionary from the `impl` dictionary.
- [ ] ⚪ Unify the terminology for Rust types attached via `NativeModule::bind`
      across `BoundKind`, `BoundType`, `Type::bound`, `declare_bound_types`, and
      `Binding<T>`.
- [ ] ❓ Return a typed error from `format_source_with`. This function returns a
      `String` error today. Callers must parse the error message to distinguish
      syntax errors, fuel exhaustion, timeouts, and interrupts.
- [ ] ❓ Read a Sand `Json` value into a Rust `Json` through `encode::walk`.
      `stdlib/json.rs` keeps a second walk (`Walk`, `Reader`, `Writer`) for
      `FromVmValue` and `IntoVmValue` of `Json`. `Reader` cannot be a
      `JsonOutput` today without a change of its fuel and memory accounting. It
      charges one unit for each `Json` value, but `JsonSink` reports a `Json`
      variant and its payload as two values. It also charges for the entries
      of an object when the object begins, and `Begin::Object` has no entry
      count.

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

- [ ] ❓ Decide whether `sand run` and `sand test` should surface process stdin
      errors to the running script. `forward_stdin` writes the error to stderr
      and closes the feed, so the script receives end-of-file instead of an I/O
      error. A `StdinFeed::fail(io::Error)` method could fail subsequent reads
      with that error.
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
- [ ] ❓ Add a comparison mode to `benches/codec.rs`. Speed evidence for the
      `json` module and for `Vm::to_value` needs two benchmark executables,
      alternating runs, and a process environment of a different size in each
      round. The size of the environment alone changes one operation by up to
      45%. The repository records none of these steps.
- [ ] ⚪ Replace the VM dispatch loop with a threaded dispatch table once Rust
      stabilizes the `become` keyword for guaranteed tail calls.

### Language

- [ ] ⚪ Allow local functions to require traits with methods and to declare
      `reflect` type parameters by passing hidden dictionary and
      type-description arguments to closures.
- [ ] ⚪ Support non-constant default expressions on trait method parameters.
