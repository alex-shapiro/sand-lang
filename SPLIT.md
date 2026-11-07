# Split of `claude/sahara-sandlang-service-marbel`

This is a temporary file. It tracks how we split the branch
`origin/claude/sahara-sandlang-service-marbel` into smaller branches. Delete
this file when the last changeset merges.

The source branch has 82 commits on top of `c6703df`. Most of its commits are
review passes on the journal. The list below groups the commits into
changesets, which are units that a reader can understand alone.

## How to carve a changeset

- Make each branch from the newest main. Do not cherry-pick a commit as it is:
  the branch commits sit on top of journal code and on an old main.
- Use the branch commits as a reference. Do the change again on main, and let
  the compiler find each place to change.
- Leave out the parts of a commit that belong to another changeset.
- When a changeset merges, update its status here.

## Status

| #   | Changeset                                | Status                        | Branch         |
| --- | ---------------------------------------- | ----------------------------- | -------------- |
| 1   | Module path cleanup                      | In review                     | `module-paths` |
| 2   | Typed policy builders                    | Not started                   |                |
| 3   | Rule and limit positions in policy files | Not started                   |                |
| 4   | `deny_private_net` and absolute rules    | Not started                   |                |
| 5   | Host modules that compile once           | Not started                   |                |
| 6   | `InterruptHandle::new`                   | Not started                   |                |
| 7   | Errors from the linter                   | Not started                   |                |
| 8   | A host calls a main-module function      | Not started (needs 5)         |                |
| 9   | Side-channel state out of the VM         | Not started                   |                |
| 10  | Fuel of a retried native call            | Not started                   |                |
| 11  | Per-run arguments with `Run<D, C>`       | Not started                   |                |
| 12  | The journal                              | Not started (needs 9, 11, 13) |                |
| 13  | One code path with and without a journal | Not started                   |                |

## Changesets that do not need the journal

### 1. Module path cleanup

Each item has one import path: the path of the module that defines it, such as
`sandlang::engine::Engine`. The crate root keeps only the derive macros. The
workspace `default-members` include `macros`, so `cargo test` runs the macro
expansion tests.

Source commits: `2c21305`, `003daed`, and the two doc links in `87342a7` that
the change breaks.

### 2. Typed policy builders

The `RulePolicy` builders take parsed pattern types instead of rule text, so
they cannot panic. The new types are `NetPattern`, `UrlPattern`, `PathPattern`,
`NamePattern`, `Methods`, `CustomName`, `AbsolutePath` and `PatternError`.
`Transports` changes from a bitflags set to an enum, and the `bitflags`
dependency goes. The builders reject relative file paths, which closes TODO 033.

Source commits: `464b03a`, `c5ca2d7`, `b111b33`, `335ae8b`, `89186fa`, `265e762`.

### 3. Rule and limit positions in policy files

`PolicyFile::rule_sources` gives the position of each rule.
`PolicyFile::limits` and `apply_limits` keep the limits that a file sets apart
from the defaults.

Source commit: `d4a2e5c`. This commit also removes `HostPattern`, which
`d859942` added. Do not carve `HostPattern`.

### 4. `deny_private_net` and absolute rules

`Tier::Absolute` makes sure that no other rule allows an address that
`deny_private_net` denies. This fixes a bug: a fallback rule for a specific host
could allow a private address.

Source commits: `d859942` (only `deny_private_net`) and `50dae44`.

### 5. Host modules that compile once

`HostModules::new` compiles the host modules once. A `CompiledProgram` carries
its host modules, so it runs on any engine. `Engine::new` cannot fail, and
`Config::module` goes. `compile_file`, `run_program` and a public `STD_MODULES`
are part of this changeset.

Source commits: `5fbd07f`, `5bd8966`, `f9a4e60`.

### 6. `InterruptHandle::new` and `Config::interrupt`

A host makes the interrupt handle before it makes the engine. The getters on
the engine and the session go.

Source commit: `c0d9652`.

### 7. Errors from the linter

A lint that runs out of fuel, out of time, or that a host interrupts gives a
compile error, a limit error, or an interrupt, not `Error::Io`.

Source: one part of `a31c9fb`. The rest of that commit is journal work.

## Changesets that the journal needs or that started from journal work

### 8. A host calls a function of the main module

`CompiledProgram::function` returns a function, or a `FunctionError` that says
why it cannot. `Function::bind` gives a `BoundCall` or an `ArityError`.
`Engine::call` takes JSON arguments and returns a typed `Returned` value.
`stdlib::core::Error` becomes public.

This changeset needs changeset 5. It uses `Outcome`. Without the journal,
`Outcome` has no `Suspended` variant.

Source commits: `4ebd15b`, `03076c5`, `f43460f`, `29e1f37`, and part of
`a31c9fb`.

### 9. Side-channel state out of the VM

Many `Vm` fields carried data between a native and its caller. These commits
replace each field with a typed parameter or return value. They break the API
for authors of native modules.

- 9a. Control-flow types: `Fail` and engine-only requests in place of one
  `Ctl`, `End` and `Request`, `Exit::Collect`, `Ending`, the `Until` trait, and
  `HostError<C>`. This fixes a bug: a test failed when a finisher called
  `exit(0)`. Source commits: `fc27559`, `fb9367e`, `5bdac47`, `61bfe31`,
  `d84df4d`, `c690c03`, `906ac91`, `4792919`.
- 9b. The `Call<'a>` argument of a native. Source commits: `33beccd`,
  `e4ed4cb`.
- 9c. The `AfterEffect` view in place of `Heap::unchecked`. Source commit:
  `35f7476`. Drop `3a59f06`, which `35f7476` makes obsolete.
- 9d. The conversion depth as a parameter of `IntoValue`. Source commit:
  `2c23b85`.

The journal motivated some of these commits, but 9b, 9c and 9d do not need the
journal.

### 10. Fuel of a retried native call

A native that runs again after a collection costs the same fuel as one that
does not. This changes fuel totals in runs without a journal.

Source commit: `925ba18`.

### 11. Per-run arguments with `Run<D, C>`

The journal, a typed host context, the seed, the output sinks, the network and
the request filter move from `Config` to an argument of each run.
`Config<C>`, `Engine<C>` and `HostModules<C>` carry the context type. This is
the largest change to the public API.

Source commits: `18f7bf7`, and `eeda440` for `run_stats`. `18f7bf7` replaces
`Config::context` from `eeda440`.

### 12. The journal

This is the main feature. Its commits are review passes on one design, so carve
it as one changeset. Its parts are:

- the core feature: `4aad4b0`;
- the entry format and codecs: `4b9a0ea`, `b6ca288`, `610e371`, `98727a1`,
  `f7bbeb6`;
- call identities and effect sequence numbers: `ff1d63e`, `c24783a`;
- the clock floor and the wall-time limit during flushes: `19e04e3`;
- suspension and parked calls: `e791c8c`, `e23f85a`, `e06eb83`, `3aa2937`,
  `a49201d`, `bedb787`, `fe2dd83`;
- journaled functions that the compiler checks: `722ea6d`, `f46c14c`,
  `5cf6fae`, `9b0a692`, `300c107`, `9630b0e`;
- the replay state machine: `561cff0`, `ae3c293`, `b490c8e`, `5c5068e`,
  `385677e`, `2c037fe`;
- durable configuration and error types: `667e651`, `f858796`, and part of
  `a31c9fb`;
- documentation: `be1871b`, `733c8dd`, `4672531`.

### 13. One code path with and without a journal

Timers, journaled host calls, standard input and HTTP go through one scheduler
path in every run. This changes runs without a journal too. For example,
`http.send` has one implementation, and the web client goes through the request
filter.

Source commits: `d4a4ac0`, `16da2fb`, `12895c8`, `483ac31`, `e541a7a`,
`3bf577b`.

## Commits that are not changesets

- Format, lint and build fixes: `52a3fff`, `d39a7ff`, `61e9587`, `f5ef1da`,
  `90282ca`, `87342a7`. Each changeset includes the fixes it needs.
- TODO entries for gaps that Sahara found: `bdad37d`. This commit also adds two
  journal TODOs, which belong to changeset 12.

## Risks

Main has commits that the source branch does not have. `eeee3b6` passes stdin
from the host and pauses `max_wall_time`. It overlaps with the stdin path
(`483ac31`), the wall-time deadline (`19e04e3`) and the output sinks
(`18f7bf7`).
