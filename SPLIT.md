# Split of `claude/sahara-sandlang-service-marbel`

This temporary file tracks how we split
`origin/claude/sahara-sandlang-service-marbel` into smaller branches. Delete
this file after you merge the final changeset.

The source branch has 82 commits after `c6703df`. Most commits revise the
journal. Changesets 1 to 7 group these commits into self-contained changes.
Changesets 8 to 26 implement the journal design in this file, and use the
source branch only as a reference.

## How to carve a changeset

- Create each branch from the latest `main`. Rewrite each change on `main`,
  because the source commits depend on journal code and an older `main`.
- Use the source commits as a reference. Let the compiler identify every call
  site that needs an update.
- Build to the final design of the changeset. Do not replay the source commits
  in order, because later commits amend earlier ones.
- Exclude changes that belong to other changesets.
- Update the status list below after you merge a changeset.

## Status

1. Module path cleanup: merged in #26 from `module-paths`
2. Typed policy builders: merged in #27 from `typed-policy-builders`
3. Rule and limit positions in policy: merged in #28 from `policy-positions`
4. `deny_private_net`: merged in #29 from `deny-private-net`
5. Precompiled host modules: merged in #30 from `precompiled-host-modules`
6. `InterruptHandle::new`: merged in #31 from `interrupt-handle`
7. Linter budget and interrupt errors: merged in #32 from `lint-budget-errors`
8. Single exit path: merged in #34 from `single-exit-path`
9. Explicit VM control flow: merged in #35 from `explicit-vm-control-flow`
10. `Fail` failure type: merged in #37 from `fail-failure-type`
11. Deterministic fuel: merged in #38 from `deterministic-fuel`
12. `VmValue` rename: in review on `vm-value-rename`
13. Single native function registration: not started
14. `Value` and the shared encode path: not started; requires 12
15. Shared decode path and `Value` input: not started; requires 14
16. `serde` support for `Value`: not started; requires 14
17. Single run method and `Outcome`: not started; requires 14
18. Host function calls: not started; requires 15 and 17
19. Host context: not started; requires 13 and 17
20. Run statistics: not started
21. Journal core: not started; requires 10, 11, 18, and 19
22. Journaled operations: not started; requires 21
23. Host function classes: not started; requires 13 and 22
24. Suspension and parked host calls: not started; requires 23
25. File system restriction in journaled runs: not started; requires 21
26. Journal corpus and Sahara acceptance: not started; requires 24 and 25

## Independent changesets

### 1. Module path cleanup

Import each item from the module that defines it, such as
`sandlang::engine::Engine`. Keep only the derive macros at the crate root. Add
`macros` to the workspace `default-members` so `cargo test` runs the macro
expansion tests.

Source commits: `2c21305`, `003daed`, and the two documentation link fixes from
`87342a7`.

### 2. Typed policy builders

`RulePolicy` builder methods take typed arguments instead of raw strings.
`PathPattern` rejects relative file paths to close issue 033. This changeset
replaces the `bitflags` set `Transports` with an enum. It removes the `bitflags`
dependency.

Public types:

- `policy::PatternError`
- `policy::net::ListenPattern`
- `policy::net::NetPattern`
- `policy::net::UrlPattern`
- `policy::rules::AbsolutePath`
- `policy::rules::CustomMatch`
- `policy::rules::HttpMethod`
- `policy::rules::Methods`
- `policy::rules::NamePattern`
- `policy::rules::PathPattern`
- `policy::rules::Transports`

Source commits: `464b03a`, `c5ca2d7`, `b111b33`, `335ae8b`, `89186fa`, `265e762`.

### 3. Rule and limit positions in policy

`PolicyFile::rule_directives` gives the position, decision, and capability of
each rule line. `PolicyFile::limit_directives` gives the position and value of
each `limit` directive. `PolicyFile::apply_limits` applies them to the limits of
the caller.

Source commit: `d4a2e5c`. Skip `HostPattern` from `d859942`, because `d4a2e5c`
removes it.

### 4. `deny_private_net`

`RulePolicy::deny_private_net` adds `deny net` rules for `localhost` and for
the ranges in `is_private_address`. Skip `Tier::Absolute` from `50dae44`. The
host controls all rules and can rank them by specificity.

Source commit: the `deny_private_net` changes in `d859942`.

### 5. Precompiled host modules

`HostModules::new` compiles host modules ahead of time. `CompiledProgram`
includes these compiled host modules so any engine can run the program. This
makes `Engine::new` infallible. Remove `Config::module`. Add `compile_file`,
`run_program`, and a public `STD_MODULES` constant.

Source commits: `5fbd07f`, `5bd8966`, `f9a4e60`.

### 6. `InterruptHandle::new` and `Config::interrupt`

Create an `InterruptHandle` with `InterruptHandle::new`. Pass the handle to
`Config::interrupt` before constructing an engine. Remove the interrupt handle
getters from `Engine` and `Session`.

Source commit: `c0d9652`.

### 7. Linter budget and interrupt errors

Return `Error::Compile`, `Error::Limit`, or `Error::Interrupted` if linting
exhausts compile fuel, exceeds the time limit, or receives an interrupt.

Source commit: the linter error changes in `a31c9fb`. The rest of `a31c9fb`
belongs to the journal changesets.

## The journal design

A journal makes a run durable. The run records each input from outside the
script. If the run stops, the host starts it again with the journal. The engine
replays the recorded inputs, reaches the same state, and continues live.

We judge each decision by two questions:

1. Does the change make the product more useful, more usable, more reliable,
   safer, or faster?
2. After the change, does each affected feature work as a user expects?

The source branch built the journal first and then corrected it. Changesets 8
to 26 land the foundations first and the journal last, so that each changeset
builds on finished work. Each changeset merges on its own and has its own tests.

**Keep the replay model of the source branch.** Scripts need no special syntax.
Plain and journaled runs share a single scheduler, which switches tasks when a fuel
slice runs out. The journal records the slice number at which each result arrives,
and replay delivers the result after the same slice. A release that moves slice
boundaries makes paused runs incompatible. Across 22 commits that touched the
compiler, the interpreter, or the Sand standard library, fuel and task interleaving
did not change for the tested workloads.

**Separate engine-scoped data from run-scoped data.** `Config` and `Session::new`
hold data for the lifetime of an engine or session. A value is run-scoped if two
runs on the same engine need different values for correct behavior. The program,
the journal, and the host context are run-scoped, so they are arguments of the
run methods:

```rust
engine.run(target, ctx).await?;
engine.run_journaled(target, journal, ctx).await?;
```

Output sinks, the seed, the network provider, and the request filter stay in
`Config`.

**Keep these decisions from the source branch:**

- The entry kinds, including the scheduling step in a `Done` entry.
- The divergence check and `Error::Diverged`.
- A save before each external action. Effects occur at least once, and the
  sequence number of an effect is the same in each execution.
- Suspension with `Journal::suspend_after`, and parked host calls.
- Whole-message HTTP. `net`, listeners, HTTP servers, and `http.upload` fail with
  kind `"unsupported"`.
- A wall clock that does not go back, and flush time outside `max_wall_time`.
- Compile-time checks that journaled arguments and results are plain data.
- The output of each execution starts from the beginning of the run.

### 8. Single exit path

Users get a fix for a wrong test result: a test failed if a finisher called
`exit(0)`. Give `exit` a single path through the engine, and remove `Trap::Exit`.

Source commit: `d84df4d`.

### 9. Explicit VM control flow

Replace control-flow flags on `Vm` with explicit return values and parameters.
`Vm::cancel_task` returns a `Result` that native modules must propagate.
`CtlKind` represents racing and self-cancellation as variants.
`CtlKind::Blocked` includes the wait generation.

This changeset also fixes four task defects:

- Cancelling a task in `task.race` clears its race list so a later wake-up in
  `defer` does not cancel the raced tasks.
- `task.race` fails immediately if every task is cancelled or only the caller
  remains.
- The scheduler keeps a failing fiber in the fiber table so session cleanup
  cancels its task.
- A cancelled task runs its deferred code once if it stopped in or just after a
  native call.

Source commits: `fc27559`, `fb9367e`, `5bdac47`, `61bfe31`.

### 10. `Fail` failure type

Host authors get compile errors for mistakes that are internal errors today.

- Add `Fail`, the public type for the end of a run. A `Ctl` is a `Fail` or a
  request to the scheduler.
- Add `HostError<C = Ctl>`. Finishers and the closure of `Later::new` return
  `HostError<Fail>`, so they cannot make a scheduler request.
- Make `CtlKind` private to the crate, so a host cannot build an internal
  signal.

Exclude `Divergence`, parked calls, suspension, and `JournaledFn`. Later
changesets add them.

Source commits: `c690c03`, `906ac91`, `4792919`.

### 11. Deterministic fuel

Fuel must be a function of the program alone, because replay depends on it. Fuel
slices decide when tasks switch, so a difference in fuel can move a task switch and
change what a replay does. Hosts also get repeatable fuel reports.

- Charge fuel once if the VM retries a native function after garbage
  collection.
- Make map and set lookups independent of the hash seed of the process. A false
  match of two 32-bit hash tags must cost no fuel.

Source commit: `925ba18`.

### 12. `VmValue` rename

Rename `vm::value::Value` to `VmValue`, and rename `FromValue` and `IntoValue` to
`FromVmValue` and `IntoVmValue`. Native function authors then see a clear
difference between the VM's representation and the host-owned `Value`.

This changeset has no source commits.

### 13. Single native function registration

Replace `function`, `vm_function`, and `async_function` with a single
`NativeModule::function`. The closure's parameters and return type select the
behavior, so an author does not choose among methods.

```rust
module.function("len", |s: &str| s.len() as i64);
module.function("touch", |vm: &mut Vm, path: String| { /* uses the VM */ });
module.function("slow", |url: String| async move { fetch(url).await });
```

- A single trait replaces `HostFunction`, `VmFunction`, and `AsyncHostFunction`.
- Each parameter is a script argument or a value that the engine provides. A
  later changeset can add a provided value without a new method.
- A closure that takes `&mut Vm` with a borrowed argument remains a compile error.
  An async closure with a borrowed argument also remains a compile error.
- Reject VM handles in async closures and `Later` futures at compile time. Add
  a marker trait for types that do not reference the VM heap. Require this
  trait on async parameters and results so that a handle cannot cross a garbage
  collection.
- Check `Later`, retry stress, and trait dictionary arguments against this trait.
  A standalone prototype covered only the closure forms.

This changeset has no source commits.

### 14. `Value` and the shared encode path

`sandlang::value::Value` is the Rust form of every Sand value that implements
`Encode`. It holds unit, bool, int, float, duration, string, bytes, list, tuple,
map, set, range, structs and enums with their names, and native value types. A
`Value` does not reference the VM, so it remains valid after the run ends. `Value`
is `#[non_exhaustive]`, so a new variant does not break hosts.

The Sand trait `Encode` defines what can leave the VM, and `json` already implements
it twice: in `Encoder` and in `Converter`, which is `to_json`. Replace both walks with
a single heap walk that writes to a sink. The sinks are JSON text, the Sand `Json`
value, and `Value`.

- The walk reports what each value is: list, tuple, set, map with its key values,
  struct or variant with its name, and native value.
- Rules that are specific to JSON exist only in the JSON sinks. These rules flatten
  `Some`, write `None` as `null`, pass the `Json` enum through, write map keys as
  text, and use Base64 for `bytes` and text for native value types.
- The value types of the `time` module have their own variants, such as
  `Value::Instant(jiff::Timestamp)` and `Value::PlainDate(jiff::civil::Date)`,
  because dates and times are central to Sand programs.
- Every other native value type, from the standard library or a host, is
  `Value::Native { type_name, text }`. The type name includes the module. The text
  is lossless, because `ValueType::parse` must turn the `Display` output back into
  an equal value. `value.native::<Money>()` checks the type name and returns a
  `Money`.
- `ValueObj` gains a method that makes the `Value`, and the `time` types override
  it, so the heap walk needs no list of special types.
- A result whose type does not implement `Encode` is `Value::Opaque`, which holds
  the type name. Native objects, such as files and connections, are not `Encode`.
  A native object lives on the heap of the VM, and its resources count against the
  limits of the run, so it cannot leave the VM.
- `value::Encode` mirrors the Sand trait. Primitives, collections, and `ValueType`
  types implement it. `#[derive(SandStruct)]`, `#[derive(SandEnum)]`, and
  `#[derive(SandValue)]` generate it.
- The copy out of the heap uses the fuel, memory, and 512-level depth bounds of
  `json.encode`. A cycle reaches the depth bound. A copy that exceeds a bound
  returns a limit error.
- A benchmark must show that `json.encode` keeps its speed.
- Rename `Vm::new_value` and `Heap::value` to reflect `ValueType` rather than
  `Value`.
- Decide how `Constant` relates to `Value`. `IntoVmValue::to_const` builds
  compile-time constants. The derive macros generate a separate walk for
  `to_const`. Building constants from `Encode` or `Value` removes that walk so
  that `IntoVmValue` only converts to `VmValue`.

This changeset has no source commits. `03076c5` returned `Json`, which loses the
difference between `bytes` and a string, and between `()` and `None`.

### 15. Shared decode path and `Value` input

The Sand trait `Decode` defines what can enter the VM, and `Decoder` implements it
for JSON text. Make `Decoder` read from a source. The sources are JSON text and
`Value`.

- A source answers requests for a type, such as "give me bytes" or "give me this
  map key as this type". The JSON source parses Base64 or key text, and the `Value`
  source matches a variant.
- Error paths, field defaults, computed-default thunks, and deferred set and map
  fills stay in the shared decoder.
- The `Value` source also accepts the JSON form of each type, so a `Value` that
  comes from JSON is valid input.
- `value::Decode: Encode` mirrors the Sand trait. Primitives, collections, and
  `ValueType` types implement it, and the derives in changeset 14 generate it. A
  host that knows a type writes `let receipt: Receipt = value.decode()?`.
- `stdlib::core::Error` is public, and implements `value::Decode`.
- A benchmark must show that `json.decode` keeps its speed.

Do not convert a `Value` to JSON text and decode the text. `NaN` and infinity encode
as `null`, which fails to decode as a float.

This changeset has no source commits.

### 16. `serde` support for `Value`

The only effect of this optional feature is to implement `Serialize` and
`Deserialize` for `Value`. A host can write a `Value` directly to JSON without an
intermediate form. In a human-readable format the mapping matches `json.encode`,
so `bytes` is a Base64 string and `duration` is a nanosecond count. A test must
compare the two outputs. Serializing `Value::Opaque` fails.

This changeset has no source commits.

### 17. Single run method and `Outcome`

`Engine::run` replaces `run_source`, `run_file`, and `run_program`. It accepts a
script or a compiled program. Every run returns a single type, which replaces the
`exit_code` struct on `main`:

```rust
match engine.run(target).await? {
    Outcome::Returned(value) => {}
    Outcome::Failed(payload) => {}
    Outcome::Exited(code) => {}
}
```

- `Returned(Value)` holds the result. A result of `()` is `Value::Unit`.
- `Failed(Value)` holds the payload of a returned `Err`, for a script or a
  function.
- `Exited(i32)` holds the code of a call to `exit`.
- `Outcome::exit_code` gives a process exit code for any outcome.
- The result of a script is its top-level `return` value or its final expression.
  The engine discards this value on `main`. Confirm that a final expression
  reaches the engine, and change the compiler if it does not.

This changeset has no source commits. The generic `Outcome<R, S>` of the source
branch is not used.

### 18. Host function calls

Hosts can call a function of a script and receive its result. Today a host can
only run a script to an exit code. Sahara spliced an entry point into user source
to work around this.

```rust
let program = engine.compile_file("main.sand").await?;
let function = program.function("main")?;
let outcome = engine.run(function.with_args([order])).await?;
```

A function call is a script run with an extra step, so both use the same path and
the same result type.

- `CompiledProgram::function` returns a `Function` or a `FunctionError`.
  `Function::signature` describes the parameters and the return type.
- `with_args` takes `Value` arguments and cannot fail. `run` converts each argument
  to its parameter type with the shared decoder, including computed defaults.
- A bad argument returns `Error::Argument`, and a wrong number of arguments returns
  `Error::ArgumentCount`. Both are host input errors with exit code 2.

Source commits: `4ebd15b`, `f43460f`, and the `Error::Argument` part of
`29e1f37`.

### 19. Host context

Host functions can read per-run data, such as a run ID. Changeset 5 shares
compiled host modules across engines, so this data cannot live in the module.
The `sahara` and `kv` host modules in Sahara need it.

```rust
let mut module = NativeModule::<RunCtx>::with_context("sahara");
module.function("run_id", |ctx: Context<RunCtx>| ctx.run_id.to_string());

engine.run(target, run_ctx).await?;
let mut session = Session::new(config, run_ctx);
```

- `Context<C>` is a value that the engine provides to a native function, as
  changeset 13 allows. A function that does not ask for it pays no cost.
- `C` is a parameter of `NativeModule`, `HostModules`, `Config`, `Engine`,
  `CompiledProgram`, and `Function`. It defaults to `()`, so a host without a
  context does not name it.
- `run` takes the context as `impl Into<Arc<C>>`. The Rust compiler rejects a
  missing context or a context of the wrong type.
- A session holds a single context from `Session::new`.

The per-run binder and `context_function` of the source branch are not used.

Source commit: the parameter `C` from `18f7bf7`.

### 20. Run statistics

`Engine::run_stats` reports the fuel, peak memory, output bytes, and garbage
collection statistics of the last run.

Source commit: `run_stats` from `eeda440`.

### 21. Journal core

```rust
let journal = Journal::new(storage);           // a new run
let journal = Journal::load(storage, history); // a run to resume

match engine.run_journaled(target, journal, ctx).await? {
    JournalOutcome::Ended(outcome) => {}
    JournalOutcome::Suspended(suspension) => {}
}
```

This changeset records and replays synchronous inputs: clock readings, random
seeds, and the environment.

- **Pass the journal to the run.** `run_journaled` takes the `Journal` by value,
  so a second run cannot reuse it. Only `run_journaled` can return `Suspended`.
  Scripts and function calls can both be journaled. Sessions and `run_tests` do
  not have a journaled form.
- **Replace `record` and `flush` with a single method.**
  `JournalStorage::append(first_seq, entries)` saves a batch. The engine holds
  unsaved entries itself. This fixes the bug in which `MemoryJournal` keeps
  unsaved entries of an interrupted run and builds a journal that differs from
  every execution.
- **Bound the memory of the journal.** Unsaved entries count against the budget
  of live operations. The engine also saves them if they pass a fixed size. On
  the source branch a loop of `time.now()` grows a host buffer outside every
  limit, which violates the hard memory limit of the sandbox.
- **Record regex fuel that depends on the machine.** The engine charges regex
  fuel from the elapsed time and the size of the compiled program. A replay
  reads these charges from the journal.
- **Detect an incompatible journal before replay.** The first entry holds a
  format number, a hash of the script source and its imports, and the arguments
  of the run. A mismatch returns an error before the script starts. Bump the format
  number for any change to the encoding, and for any change to fuel costs, the
  compiler, or the scheduler that moves slice boundaries. The host finishes a paused
  run on the old engine or marks it as failed.
- **Leave room for a history reset.** A journal grows with each operation, and
  replay starts from the beginning. Other systems bound the history and offer a
  reset, such as Temporal's continue-as-new. The format must allow a later entry
  that starts a new history. The host sets the size limit in `JournalStorage`.

Source commits: `4aad4b0`, `4b9a0ea`, `b6ca288`, `610e371`, `98727a1`, `f7bbeb6`,
`19e04e3`, `667e651`, `f858796`, `ae3c293`, `2c037fe`, `be1871b`, `733c8dd`, and
the `HostAbort` part of `a31c9fb`.

### 22. Journaled operations

This changeset adds timers, HTTP, standard input, and local futures to journaled
runs. `main` has a single path for timers, standard input, and `http.send`. The
journal keeps this property, so each operation has one implementation with or
without a journal.

The journal records the slice number at which each result arrives, and replay
delivers the result after the same slice.

Source commits: `d4a4ac0`, `16da2fb`, `483ac31`, `e541a7a`, `3bf577b`, `5c5068e`,
`385677e`, and `3a59f06`, because replay decodes values inside a finisher.

### 23. Host function classes

**Require a class for each host function in a journaled call.** On the source
branch an unmarked function runs again in replay, so a function with a side effect
silently repeats it. `run_journaled` returns an error that identifies an
unclassified host function before the script starts. Hosts that do not use the
journal are not affected.

| Class       | Meaning                       | In replay                 |
| ----------- | ----------------------------- | ------------------------- |
| `Pure`      | Same result in each execution | The engine calls it again |
| `ReadOnly`  | Reads the outside world       | Returns the recorded data |
| `Effectful` | Changes the outside world     | Returns the recorded data |

The docs of `Pure` must say that a `Pure` function cannot depend on anything that
differs between executions, such as a time zone setting. The engine saves the
journal before an `Effectful` function starts. A `ReadOnly` function does not need
this save, so read-heavy runs avoid a storage write for each read.

**Give `ReadOnly` and `Effectful` functions plain arguments only.** The body gets its
arguments, its context, and an optional `JournalSeq`, and it does not get
`&mut Vm`. A body therefore cannot read the journaled clock or random numbers, and
it cannot ask for a retry after garbage collection. Every journaled function in
Sahara already ignores its `Vm`. A body returns a value or a future, and the engine
converts the result. A Sand wrapper that calls a `Pure` native checks a permission
before it calls the body. This sketch shows Sahara's functions in this form:

```rust
module.function("get", ReadOnly(|ctx: Context<RunCtx>, key: String| async move {
    ctx.effects.kv_get(key).await.map_err(kv_error)
}));

module.function("put", Effectful(|ctx: Context<RunCtx>, seq: JournalSeq, key: String, value: Json| async move {
    ctx.effects.kv_write(key, Some(value), seq).await.map_err(kv_error)
}));
```

**Let a host choose the behavior after an unknown outcome.** An `Effectful` function
runs at least once by default. An `Effectful` function can instead fail the run if a
crash leaves its outcome unknown, for an operation that cannot be repeated safely.

Source commits: `722ea6d`, `f46c14c`, `5cf6fae`, `9b0a692`, `9630b0e`, `ff1d63e`,
`c24783a`, `12895c8`, `561cff0`, `b490c8e`, `4672531`.

### 24. Suspension and parked host calls

A journaled run can pause if every task waits for a timer or a parked host call
that is not due soon. The run returns `Suspended`, and the host resumes it later.

```rust
module.function("wait_until", ReadOnly(|ctx: Context<RunCtx>, name: String, deadline: Timestamp| {
    Parked::until(deadline, name.clone(), async move { ctx.poll_signal(name, deadline).await })
}));
```

`Parked::until` is a sketch. The design must also let a function return at once if
its result is ready, as Sahara's `wait_until` does if a signal has arrived.

Source commits: `e791c8c`, `e23f85a`, `e06eb83`, `3aa2937`, `a49201d`, `bedb787`.

### 25. File system restriction in journaled runs

`fs` operations on the operating-system file system fail with kind
`"unsupported"` in a journaled run. On the source branch replay repeats each write
and rereads changed files, and only the docs warn about this. A `MemFs` remains
valid, and the host resets it for each execution.

This changeset has no source commits.

### 26. Journal corpus and Sahara acceptance

Keep a corpus of journals from earlier engine versions in the repository, and run
it as a release gate. A change that moves slice boundaries without a format number
bump fails the gate.

Port Sahara to the final API. Its simulator, replay oracles, and bug zoo must pass.

This changeset has no source commits.

### Source commits that this plan does not use

| Commits             | Change                  | Reason                                  |
| ------------------- | ----------------------- | --------------------------------------- |
| `33beccd`,`e4ed4cb` | `Call<'a>` for natives  | Adds a parameter to fix internal state  |
| `35f7476`           | `AfterEffect` VM view   | A flag already gives this behavior      |
| `2c23b85`           | `IntoValue` depth       | Adds work to manual `IntoValue` impls   |
| `18f7bf7`           | `Run` and `D`           | Run-scoped values are arguments         |
| `300c107`           | `JournaledCall` borrow  | A sequence number is a plain value      |
| `fe2dd83`           | Non-suspending end type | `run` cannot suspend                    |
| `03076c5`           | `Json` results          | See changeset 14                        |
| `29e1f37`           | `bind` and `BoundCall`  | `run` reports the argument count error  |

## Other commits on the source branch

- Formatting, lint, and build fixes: `52a3fff`, `d39a7ff`, `61e9587`, `f5ef1da`,
  `90282ca`, `87342a7`. Fold each fix into the changeset that needs it.
- TODO entries from Sahara: `bdad37d`. Include the two journal TODO entries in
  changeset 21.

## Merge conflicts with `main`

Commit `eeee3b6` on `main` streams standard input from the host. It also pauses
`max_wall_time` during user waits. Reconcile `eeee3b6` with these changes:

- `483ac31` in changeset 22, which modifies standard input scheduling
- `19e04e3` in changeset 21, which modifies the wall-time deadline
