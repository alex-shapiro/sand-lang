# Split of `claude/sahara-sandlang-service-marbel`

This temporary file tracks how we split
`origin/claude/sahara-sandlang-service-marbel` into smaller branches. Delete
this file after you merge the final changeset.

The source branch has 82 commits after `c6703df`. Most commits revise the
journal. The sections below group these commits into self-contained changesets.

## How to carve a changeset

- Create each branch from the latest `main`. Rewrite each change on `main`,
  because the source commits depend on journal code and an older `main`.
- Use the source commits as a reference. Let the compiler identify every call
  site that needs an update.
- Exclude changes that belong to other changesets.
- Update the status list below after you merge a changeset.

## Status

1. Module path cleanup: merged in #26 from `module-paths`
2. Typed policy builders: in review on `typed-policy-builders`
3. Rule and limit positions in policy: not started
4. `deny_private_net` and absolute rules: not started
5. Precompiled host modules: not started
6. `InterruptHandle::new`: not started
7. Linter budget and interrupt errors: not started
8. Host calls to main-module functions: not started; requires 5
9. Explicit VM control flow and call state: not started
10. Fuel accounting for retried host calls: not started
11. Per-run arguments with `Run<D, C>`: not started
12. The journal: not started; requires 9, 11, and 13
13. Unified scheduler path: not started

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

`PolicyFile::rule_sources` records the source span of each rule.
`PolicyFile::limits` holds limits specified in the policy file.
`apply_limits` applies them to the limits of the caller.

Source commit: `d4a2e5c`. Skip `HostPattern` from `d859942`, because `d4a2e5c`
removes it.

### 4. `deny_private_net` and absolute rules

`Tier::Absolute` ensures that `deny_private_net` overrides all other rules for
private IP addresses. Previously, a fallback rule for a specific host could
permit a private address.

Source commits: the `deny_private_net` changes in `d859942`, and `50dae44`.

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

## Journal-related changesets

### 8. Host calls to main-module functions

Hosts can look up and call functions exported by the main script module:

- `CompiledProgram::function` returns a `Function` or a `FunctionError`.
- `Function::bind` returns a `BoundCall` or an `ArityError`.
- `Engine::call` runs a bound call with JSON arguments and returns `Returned`.
- Export `stdlib::core::Error`.

This changeset requires changeset 5. `Outcome` omits the `Suspended` variant
until changeset 12.

Source commits: `4ebd15b`, `03076c5`, `f43460f`, `29e1f37`, and the
corresponding part of `a31c9fb`.

### 9. Explicit VM control flow and call state

Pass state between the VM and native functions through explicit parameters and
return types instead of `Vm` fields. This updates the native module API in four
steps:

- **9a. Control-flow types.** Split `Fail` and engine-only requests out of
  `Ctl`. Add typed exits and `HostError<C>`. This also fixes a bug where a test
  failed if a finisher called `exit(0)`. Source commits: `fc27559`, `fb9367e`,
  `5bdac47`, `61bfe31`, `d84df4d`, `c690c03`, `906ac91`, `4792919`.
- **9b. Native call context.** Pass `Call<'a>` to native functions. Source
  commits: `33beccd`, `e4ed4cb`.
- **9c. Post-effect heap view.** Replace `Heap::unchecked` with `AfterEffect`.
  Source commit: `35f7476`, which supersedes `3a59f06`.
- **9d. Conversion depth.** Pass the recursion depth to `IntoValue`. Source
  commit: `2c23b85`.

Steps 9b, 9c, and 9d are independent of the journal.

### 10. Fuel accounting for retried host calls

Charge fuel once if the VM retries a native function after garbage collection.
This makes fuel consumption identical whether or not a collection occurs.

Source commit: `925ba18`.

### 11. Per-run arguments with `Run<D, C>`

Pass per-execution state to each run through `Run<D, C>` instead of `Config`.
Parameterize `Config<C>`, `Engine<C>`, and `HostModules<C>` by the host context
type `C`. `Run<D, C>` configures:

- the journal
- the typed host context
- the random seed
- the output sinks
- the network provider
- the HTTP request filter

Source commits: `18f7bf7`, and `run_stats` from `eeda440`. `18f7bf7` supersedes
`Config::context` from `eeda440`.

### 12. The journal

Implement the execution journal as a single changeset across these areas:

- Core journal execution: `4aad4b0`
- Entry format and codecs: `4b9a0ea`, `b6ca288`, `610e371`, `98727a1`,
  `f7bbeb6`
- Call identities and effect sequence numbers: `ff1d63e`, `c24783a`
- Clock floor and wall-time accounting during flushes: `19e04e3`
- Suspension and parked calls: `e791c8c`, `e23f85a`, `e06eb83`, `3aa2937`,
  `a49201d`, `bedb787`, `fe2dd83`
- Static checking for journaled functions: `722ea6d`, `f46c14c`, `5cf6fae`,
  `9b0a692`, `300c107`, `9630b0e`
- Replay state machine: `561cff0`, `ae3c293`, `b490c8e`, `5c5068e`, `385677e`,
  `2c037fe`
- Durable configuration and error types: `667e651`, `f858796`, and the
  remaining part of `a31c9fb`
- Documentation: `be1871b`, `733c8dd`, `4672531`

### 13. Unified scheduler path

Route timers, journaled host calls, standard input, and HTTP through a single
scheduler path in all runs. `http.send` uses a single implementation in runs
with and without a journal. The web HTTP client also runs the request filter.

Source commits: `d4a4ac0`, `16da2fb`, `12895c8`, `483ac31`, `e541a7a`,
`3bf577b`.

## Other commits on the source branch

- Formatting, lint, and build fixes: `52a3fff`, `d39a7ff`, `61e9587`, `f5ef1da`,
  `90282ca`, `87342a7`. Fold each fix into the changeset that needs it.
- TODO entries from Sahara: `bdad37d`. Include the two journal TODO entries in
  changeset 12.

## Merge conflicts with `main`

Commit `eeee3b6` on `main` streams standard input from the host. It also pauses
`max_wall_time` during user waits. Reconcile `eeee3b6` with these changes:

- `483ac31` in changeset 13, which modifies standard input scheduling
- `19e04e3` in changeset 12, which modifies the wall-time deadline
- `18f7bf7` in changeset 11, which moves output sinks to `Run`
