# Native-to-Sand callbacks: research report

## 1. Summary

Most callbacks in SwiftUI and Jetpack Compose go from the host into Sand:
the UI framework calls a script function at a time that it chooses, and
sometimes it needs the answer at once. Sand needs three things for that:

- handles to Sand functions that a host can keep, safe across garbage
  collection;
- an async entry and a sync entry from the host into Sand;
- a timer and I/O reactor on the host's UI thread.

These three do not need a native function to call back into the VM. One
open question can change that. In a custom layout, a script's measure code
calls the host, which can measure a child whose layout is also script code
(J-S10b, J-S11b). Then the host calls Sand while a native is on the Rust
stack. That case needs a restricted nested call (section 8.3), and we did
not test it on a UI framework. For a native that calls back from a Sand
task (Sand → native → Sand), the best mechanism is a **stackless
continuation**:

1. The native does not call the Sand function itself. It returns a request
   to the VM.
2. The VM runs the Sand function as an ordinary frame on the current task.
3. The VM gives the result to a continuation that the native supplied.

Lua's `lua_callk`, piccolo's `Sequence`, and Erlang's `enif_schedule_nif`
are forms of this idea.

We built two prototypes in the Sand VM and tested them against each other:

- **P1, nested calls.** The native calls `vm.call_nested(f, args)` and gets
  the result on the Rust stack. Lua's `lua_call` and CPython's
  `PyObject_Call` work this way.
- **P2, stackless continuations.** The native returns
  `Err(Ctl::call(f, args, state, then))`. The VM runs `f`, then calls
  `then` with the result.

P2 kept every Sand product priority that we tested. P1 failed one, and
needs a rule change for another:

- A P1 callback cannot suspend, so any callback that does I/O fails. Sand
  does async I/O by default.
- Under today's retry rule, a P1 native that calls back cannot follow SPEC
  §16.3. The rule says that a native must check its limits before any side
  effect, because the engine can retry the native from the start. In P1,
  the callback is a side effect before the native's later checks, so a
  retry runs the callback again. P1 would need a different rule: collect
  garbage in place, without a restart.

Both prototypes need a rooting rule for heap handles that native Rust code
holds. In P1, every handle in a Rust variable is at risk during each
callback. In P2, the VM roots the continuation's state and the callback's
result. Our first P2 prototype did not root the result in its retry path;
test E13b found the bug, and the fix is in the patch. Handles that a
continuation captures are still at risk, and an API can prevent those
captures.

The cost of P2 is ergonomic. Code that calls back many times must be a
state machine. Our merge sort with a Sand comparator needs 58 lines in P2
and 24 lines in P1. We expect that a Rust `async` adapter can reduce this
cost, but we did not build or test one.

Callbacks across the native boundary are slower than Sand calls. In each
binary, a native merge sort with a Sand comparator took about 1.6 times as
long as the same merge sort written in Sand. So higher-order library code
should stay in Sand, as it does today. Callbacks are for host APIs that
must call script code, such as UI frameworks.

Most UI callbacks go the other way: SwiftUI and Jetpack Compose call
script code from the UI thread at times that they choose. Some of those
calls must return a value at once. P2 cannot serve them, because no Sand
task is waiting. Two host-side tests set the order of the work:

- Sand's native build uses tokio for timers. A session on a plain run loop,
  as on a UI thread, failed as soon as a script slept. With a tokio runtime
  on a background thread, the same loop worked. Sand's web build uses
  JavaScript timers instead, so a host-supplied timer is also possible.
- An event that the host starts with `spawn` runs during the host's next
  `eval` until it suspends. It resumes inside any later entry that waits,
  or that runs for more than 2 ms, and under `Session::run_tasks_until`.
  So any entry, sync entries included, can see a handler that has run only
  partway.

Our recommendations, in order:

0. **[necessary]** Build a spike that runs a session on a real UI run loop
   (CFRunLoop or Android's Looper), with a timer, an async entry, and a
   sync entry. The two tests above ran on Linux only. The spike must also
   choose how the host supplies the timer and reactor (F-S7), and it must
   include a nested custom layout: script measure code that measures a
   script child. That result decides whether section 8.3 is necessary.

1. **[necessary]** Add GC-rooted handles that a host can keep. Today, the
   VM resets the session if a host uses a handle after a garbage
   collection.
2. **[necessary]** Add two host → Sand entry points:
   - an async entry that runs a handler as a task;
   - a sync entry that runs a handler to completion within a budget, and
     that fails if the handler suspends.

   React Native, Flutter, and Valdi have the same two entry points. Our
   spike of a sync entry took 41 µs per call, including the compilation
   of the call expression. The spike also showed two
   problems that a real sync entry must solve: it failed a handler that
   only computed for more than 2 ms, and a failed handler left its partial
   changes.
3. **[necessary]** Add P2 continuations for Sand → native → Sand callbacks.
4. **[blocker]** Do not use nested calls on the Rust stack for Sand →
   native → Sand callbacks. A restricted nested call stays a candidate for
   a sync host entry while a native runs (section 8.3).
5. **[blocker]** Do not use separate native stacks (fibers).

## 2. Method

### 2.1 Product priorities and tag rules

The priorities come from `README.md` and `AGENTS.md`. Each class has its
own tag rule.

- **Hard constraints.** If a design violates one, the design gets
  **[blocker]**.
  - 100% safe Rust.
  - Full sandboxing, with no escape mechanisms.
  - Hard, user-set limits on memory and CPU.
  - A single-threaded engine, with many engines in one process.
  - Support for iOS, macOS, Windows, Linux, Android, and the web.
- **Core properties.** If a design breaks one, the design gets
  **[blocker]**. If Sand must do a thing to keep one, the thing gets
  **[necessary]**.
  - Async I/O by default.
  - Maximum performance without a JIT.
  - Native Rust modules with type interop.
  - An ergonomic runtime for untrusted code.
- **The target use case.** This is scripting over callback-heavy native UI
  APIs, such as SwiftUI and Jetpack Compose. If Sand must do a thing to
  support it, the thing gets **[necessary]**.

### 2.2 Tags

Each fact and each judgment has at least one tag.

- **[blocker]**: Sand's priorities prohibit this.
- **[necessary]**: Sand must do this to keep its priorities.
- **[nice to have]**: good to do, but its absence does not substantively
  hurt the priorities.
- **[tangential]**: not important for Sand.
- **[hazard]**: a failure mode that a design must prevent. We use it with
  one of the tags above.
- **[source-only]**: we could not run the system on this machine. The
  empirical check is a machine check that the quoted text is in the primary
  source. Section 9 lists every fact with this tag.

### 2.3 Evidence

Each fact gives a source ID and a test.

- **Source IDs** (for example `L-S3`) point to `sources/checked_quotes.md`.
  Each entry gives a URL and the verbatim quote.
  - Web sources are pinned to a tag or a commit. The Apple and Android
    pages have no versions, so we give the retrieval date (2026-11-09).
  - `lab/verify_sources.py` reads every source again. It checks the exact
    text of each quote that this report prints, after it removes markup
    and comment prefixes. **[necessary]**
  - The script also checks that every quoted string of 25 or more
    characters in this report is a checked quote or a test output.
  - Result: 72 of 72 quotes found, 0 report quotes without a check, and
    0 cited paths that git does not track
    (`lab/results/sources.txt`).
- **Tests** have IDs such as `E2`. Each runtime has one program for its
  tests, and its output is in `lab/results/<runtime>.txt`. Section 10 lists
  every test. **[necessary]**

The tests follow these rules for fairness. **[necessary]**

- **Same workload.** Every runtime runs the same workload with the same
  callback body, `x => x + 1`. All sort tests sort the same 200,000 ints.
- **Same timing method.** Each time is the median of 5 runs. Each timed
  run ran alone on an otherwise idle machine (`lab/run_all.sh`, then
  `lab/overhead.sh`). We ran the Sand programs again after the second
  review, also alone. All times are wall-clock
  times, except Lua's sort, which uses `os.clock` (CPU time).
- **Same algorithm within Sand.** The three Sand merge sorts use one
  algorithm and one comparator call per comparison. Only the sort is
  timed. Other runtimes use their own sort algorithms (section 6).
- **Same binary for ratios.** Each Sand comparison is a ratio to a
  baseline in the same binary, because a rebuild changed the same Sand
  code's speed by up to 15%.
- **No JIT in the speed comparison.** Sand prohibits a JIT. Wasmtime
  compiles WebAssembly to machine code, so its numbers are for reference
  only. wasmi is the interpreter to compare with.
- **Loop costs are visible.** The "native → script" loop runs in the host
  language, and the "script → script" loop runs in the interpreter. Each
  table also gives a loop without a call.

The machine: Linux 6.18, x86_64, 4 vCPUs, rustc 1.97.0, gcc 13.3.
Section 10 gives the runtime versions.

## 3. Sand today

- **F-S1. A native cannot call a Sand function.** A native gets a function
  as `Val<fn(..)>`, an opaque handle, and `Vm` has no method that calls
  it. Source: `src/native/ty.rs`, `src/vm/mod.rs`. Test: `sand_current`
  E1. **[necessary]** to change for the UI use case.
- **F-S2. The standard library writes higher-order functions in Sand.**
  `map` and `filter` are Sand code. `sort_by` computes every key in Sand,
  then calls a native sort (S-S3). This pattern needs no callback. On
  200,000 ints, `sort_by(key)` took 65 ms and `sort()` took 12 ms. Test:
  `sand_p2` E6. **[necessary]** to keep.
- **F-S3. A native can already run a Sand closure later, as a task.**
  `Vm::spawn_task` is public. A native can spawn the closure and return
  the `Task`. The script waits for the task, and the closure can suspend
  on I/O. Test: `sand_current` E-S1 returned `55` with 351 ns per
  round trip. E-S1b ran a closure that sleeps and returned `7`.
  **[necessary]** to keep. This test covers a native that schedules a
  callback. It does not cover an event from the host, which has no `Vm`
  outside a native call (F-S6).
- **F-S6. An event from the host runs partly during the next `eval`, and
  finishes only while the host drives the session's tasks.** The host
  started two handlers with `spawn`. In the next `eval`, the handler
  without a suspension finished (`plain 1`). The other handler started
  and stopped at its sleep (`started 1, taps 0`). Two short `eval` calls
  did not resume it, but `Session::run_tasks_until` did (`taps 1`). It
  also resumed inside an `eval` that slept 5 ms, and inside an `eval` that
  computed for more than 2 ms (E17e, E17f). A sync entry right after a new
  event ran that handler up to its sleep (`started 2, taps 1`). The
  `Session` docs promise that "Tasks spawned by an input continue to run during later
  inputs" (S-S7). Test: `sand_host` E17. **[necessary] [hazard]**: any entry,
  sync entries included, can see a handler that has run only partway, and
  a handler's later steps can interleave with unrelated entries.
- **F-S7. Sand's native build needs a tokio runtime for its timers.** On a
  plain run loop without a tokio runtime, an `eval` that computed worked,
  but an `eval` that slept failed with "there is no reactor running", and
  the VM reset the session. With a tokio runtime entered, its timer driven
  by another thread, the same loop returned `5`. Test: `sand_host` E18.
  The web build uses JavaScript timers instead (S-S6). E18 tested timers
  only, not network I/O, which also uses tokio. **[necessary]**: a UI host
  must supply a timer and an I/O reactor. Two options:
  - enter a tokio runtime on the UI thread, as E18c does, at the cost of
    an extra thread and the tokio dependency in each app;
  - let the host plug in its own timer and reactor, as the web build
    does, at the cost of a new embedding API.

  Choosing between them is a **product tradeoff**.
- **F-S4. A host cannot keep a handle.** The `Val` documentation says that
  "a `Val` is valid only for the duration of the synchronous native call"
  (S-S1). Our host kept a handle and then caused a garbage collection.
  When the host used the handle again, the VM stopped with "spawn: not a
  closure" and reset the session. Test: `sand_current` E-S3. The garbage
  collector gives a freed slot to a new object (E11 in section 7), so a
  kept handle can also name a different, live object.
  **[necessary] [hazard]**: UI hosts must keep event handlers.
- **F-S5. The engine retries a native from the start.** After a permission
  prompt or a garbage collection, "the engine re-runs a `vm_function` from
  the start" (S-S2). A native must check limits before its side effects.
  Test: E13 (section 7). **[necessary] [hazard]**: a callback design must
  not run a callback twice.

## 4. What native UI frameworks need

- **F-U1. SwiftUI calls synchronous closures on the main actor.** `View` is
  `@MainActor`, and `Layout` has a synchronous
  `sizeThatFits(proposal:subviews:cache:)` that returns a size. Source:
  J-S10, J-S10b. **[necessary] [source-only]**: Sand must answer some host calls
  at once, on the host's thread.
- **F-U2. Jetpack Compose has the same split.**
  - `MeasureScope.measure` is a synchronous function that returns a value.
  - `LaunchedEffect` takes a `suspend` block for async work.
  - Composable functions run "as often as every frame of an animation".

  Source: J-S11, J-S11b, J-S11c. **[necessary] [source-only]**
- **F-U3. React Native replaced its async-only bridge with synchronous
  calls.** Its team says that "some updates need to render synchronously in
  response to user input", and "Since the old architecture was only
  asynchronous, we needed to rewrite it". Also, before the change, "layout
  could not be read synchronously". Source: J-S1, J-S1b, J-S1c.
  **[necessary] [source-only]**: async delivery alone, which is Sand's
  status quo (F-S3), is not enough for UI.
- **F-U4. Production bridges have a sync entry and an async entry.**
  - React Native's `CallInvoker` has `invokeAsync` and `invokeSync` (J-S3).
  - Dart's `NativeCallable.isolateLocal` is synchronous: "It will abort
    the process if invoked on any other thread". `NativeCallable.listener`
    is async, so "only functions returning void are supported" (J-S7,
    J-S7b).
  - Valdi says that "calling a JS function will automatically
    asynchronously dispatch to the JS thread if needed" (J-S13). For a
    synchronous result, it says: "If your function need to return a value
    synchronously to platform code, make sure you call this function
    within a createScopedJSRuntime scope" (J-S13b).

  **[necessary] [source-only]**: Sand needs both entry points (section 8).
- **F-U5. Frameworks moved script code onto the UI thread for synchronous
  calls.**
  - Flutter merged its UI and platform threads, because "The split-thread
    design prevented Flutter apps and plugins from using Dart FFI" (J-S8).
  - Lynx added main-thread scripts, because background handlers caused
    "animations lagging behind gestures" (J-S12).
  - NativeScript: "JavaScript executes on the main thread" (J-S9).

  **[necessary] [source-only]**: Sand must run on the host's UI thread. A
  single-threaded engine can.
- **F-U6. Each engine that we checked runs one thread at a time.**
  - QuickJS: "Inside a given runtime, no multi-threading is supported"
    (J-S4).
  - Hermes: "The APIs must not be called from multiple threads
    concurrently" (J-S2).
  - JavaScriptCore: "other threads attempting to use the same virtual
    machine must wait" (J-S5).

  **[necessary] [source-only]**: Sand's single-threaded engine follows
  industry practice.
- **F-U7. UI frameworks call script handlers that the host stores.** Roact
  passes a script closure to `event:Connect` (L-S11). A Lua host keeps a
  closure with `luaL_ref`, which "Creates and returns a reference" (L-S8).
  Test: Lua E7, where the host called a stored handler after the script
  ended. **[necessary]**: see F-S4.

## 5. The design space

We found five designs. Each subsection gives the runtimes that use the
design, the facts, and a verdict for Sand.

### 5.1 Forbid callbacks, and deliver events asynchronously

Wren, Erlang NIFs, and Sand today use this design. Flutter platform
channels, React Native's legacy bridge, and Valdi's default path use its
async half.

- **F-A1.** Wren's guide says not to call into the VM from a foreign
  method, because "The VM is not re-entrant". Its check is an assert,
  "Cannot already be in foreign call.", and asserts run only in debug
  builds. Source: R-S8, R-S8b.
  **[tangential] [source-only]**: a precedent, not a design to copy.
- **F-A2.** Erlang asks a NIF to "return to its caller within 1
  millisecond". For long work, it prefers `enif_schedule_nif`, a tail
  continuation: "This approach is always preferred over the other
  alternatives". Source: R-S7, R-S7b. **[nice to have] [source-only]**: a tail
  continuation is a subset of P2.
- **F-A3.** Flutter platform channels pass messages "asynchronously through
  channels to ensure the user interface remains responsive". Source: J-S6.
  **[necessary] [source-only]** for events. F-U3 shows that this is not
  enough for layout and rendering.

**Verdict.** Keep async delivery for events (F-S3). **[necessary]**. Add a
synchronous path for the UI use case. **[necessary]**.

### 5.2 Nested calls on the native stack

The native calls the script function and waits for the result on the
native stack. Lua (`lua_call`), CPython (`PyObject_Call`), QuickJS
(`JS_Call`), Hermes JSI, Rhai, Rune, starlark-rust, wasmtime (sync API),
wasmi, and Sand P1 use this design.

**Performance.** This table gives per-iteration times in ns (median of 5).
"Native → script" is a host loop that calls the script callback.
**[nice to have]**

| Runtime                    | Loop, no call | Script → script | Native → script |
| -------------------------- | ------------: | --------------: | --------------: |
| Lua 5.4.8 (C API)          |           5.0 |            19.5 |            14.5 |
| piccolo 0.3.3 (stackless)  |          30.5 |            97.9 |            74.7 |
| Lua 5.5.0 (C API)          |           4.6 |            18.3 |            12.6 |
| Lua 5.4 through mlua       |           4.6 |            17.8 |            50.7 |
| CPython 3.13.18            |          59.0 |            84.6 |            42.3 |
| CPython 3.14.6             |          53.5 |            72.7 |            34.3 |
| QuickJS-NG 0.16.2          |          26.6 |            37.5 |            66.1 |
| wasmi 2.0.0                |             – |            16.6 |            71.4 |
| starlark-rust 0.14.2       |          64.8 |            78.9 |            32.9 |
| Rune 0.14.2                |          69.8 |            86.3 |           134.6 |
| Rhai 1.27.0                |         172.2 |           440.4 |           323.9 |
| Sand (unpatched)           |          13.0 |            30.2 |               – |
| Sand P1 (nested)           |             – |            31.0 |           101.3 |
| Sand P2 (stackless)        |             – |            27.1 |            69.9 |
| wasmtime 50 (JIT, ref.)    |             – |             1.8 |            12.5 |

Sources: `results/<runtime>.txt` for each row. The Sand "script →
script" column calls through a function value. A
direct call to a small function costs about the same as the loop alone
(12.4 ns vs 13.0 ns, `sand_current` E1).

- **F-N1. A nested call is cheap if the host language owns the API.** In
  Lua's C API, a host loop that calls a Lua callback (14.5 ns) costs less
  than a Lua loop that calls a Lua function (19.5 ns). Source: L-S5c, the
  C API that the test uses. Test: Lua E1. **[nice to have]**
- **F-N2. A safe Rust binding over a longjmp API costs speed.** mlua says
  that each place where Lua can longjmp "is protected by `lua_pcall`"
  (L-S13). Its callback loop took 50.7 ns, and raw C on Lua 5.4 took
  14.5 ns. Part of the difference is mlua's value conversion; we did not
  separate the two costs. Test: mlua E1. **[tangential]**: Sand has no
  longjmp.
- **F-N3. An error in Lua skips the native's cleanup.** "Lua uses the C
  longjmp facility to handle errors" (L-S1). Our native acquired a
  resource, then the callback raised an error. The result: acquired 1,
  released 0. In CPython, the call returned NULL, and the cleanup ran.
  Test: Lua E4, CPython E4. **[tangential]**: Sand natives return
  `Result`, so this cannot happen in Sand.

**Depth.** A nested call uses the native stack, so each runtime must bound
native ↔ script recursion. If it does not, a script can crash the host
process.

- **F-N4. Lua, QuickJS, CPython, Rhai, starlark-rust, and wasmtime bound
  it with a counter or a stack check.** These depths are for native ↔
  script mutual recursion. Script-only recursion goes deeper.
  - Lua: 197 levels, then "C stack overflow" (L-S3b). Script-only:
    499,989.
  - QuickJS: 347 levels, then `RangeError`. Script-only: 1,362.
  - CPython 3.13, with the recursion limit raised to 1,000,000: 3,333
    levels. Script-only: 999,998. CPython 3.13 has
    `Py_C_RECURSION_LIMIT 10000` (P-S3b).
  - CPython 3.14: 10,214 levels, then a `RecursionError` about stack
    overflow. CPython 3.14 measures the stack (P-S4).
  - wasmtime: 546 levels, then the trap "call stack exhausted". Its
    `max_wasm_stack` counts host frames (W-S6).
  - Rhai: 32 levels, with the default `max_call_levels` of 64 (R-S3).
  - starlark-rust: 16 levels, with its default of 50 frames (R-S6).

  Tests: E2 and E2b in each runtime. **[necessary]**: a nested design must
  have such a bound.
- **F-N5. Without a bound, the host process aborts.** The scripts did
  nothing but recurse through a native.
  - wasmi aborted with a Rust stack overflow (exit code 134).
  - Rune aborted the same way.
  - Rhai aborted after the host raised `max_call_levels`.

  Wasmtime warns: "Exhausting the thread stack typically leads to an abort
  of the process" (W-S6b). Tests: `wasmi_cb` E2, `rune_cb` E2,
  `rhai_cb_deep`. **[blocker] [hazard]**: an untrusted script must never
  crash its host. The bounds in F-N4 range from 16 to 10,214 levels and
  depend on the native stack size.

**Suspension.** No one can suspend a plain native frame, so the callback
cannot wait for I/O.

- **F-N6. No nested-call runtime lets a callback suspend through a plain
  native frame.**
  - Lua raised "attempt to yield across a C-call boundary" (L-S5c).
  - mlua raised the same error for an async Rust function inside a
    callback inside a sync native.
  - CPython gave the native an un-awaited coroutine and warned
    "coroutine 'handler' was never awaited". PEP 219 gives the cause: "the
    C stack now holds a nested execution of the byte-code interpreter"
    (P-S6).
  - QuickJS gave the native a promise. The callback resumed only after the
    host ran the job queue.
  - Rune gave the native a `Future` and failed with "Expected number
    type".
  - Sand P1 failed with "a native callback cannot suspend".

  Tests: E3 in each runtime. **[blocker]** for Sand → native → Sand
  callbacks: Sand does async I/O by default. A nested callback that calls
  `http.get` or `task.sleep` fails at run time.

**Limits.** Fuel and memory limits must apply inside callbacks.

- **F-N7. Most runtimes keep their limits inside callbacks.**
  - Lua's count hook fired inside a callback, and its allocator limit gave
    "not enough memory". The count hook fires only "while Lua is executing
    a Lua function" (L-S6).
  - QuickJS's interrupt stopped the callback with an error that the
    script could not catch.
  - The fuel of wasmtime and wasmi trapped.
  - Rhai's progress callback stopped the script.
  - Sand P1 and P2 kept their instruction limit.

  Tests: E4 and E5 in each runtime. **[necessary]**
- **F-N8. Rune's budget does not reach nested calls.** `Function::call`
  makes a new VM (R-S4). Rune says that "without explicit co-operation from
  native functions the budget cannot be enforced" (R-S4b). The budget
  stopped a loop that the VM called directly. The same loop, reached
  through a native's `Function::call`, ran until our 60-second timeout.
  Test: `rune_cb_budget`. **[blocker] [hazard]**: a callback must charge
  the same meter as its caller. P1 and P2 do.
- **F-N9. QuickJS lets a script catch an out-of-memory error.** Inside a
  callback, the script caught the error as `null` and continued. Test:
  QuickJS E5. Source: J-S4 documents the runtime memory limit.
  **[tangential] [hazard]**: Sand ends the run at its memory limit, so a
  script cannot catch it.

**Re-entrancy.** A callback can change state that the native relies on.

- **F-N10. Each runtime protects its sort from a comparator that changes
  the list, in different ways.**
  - CPython empties the list during the sort and raises
    `ValueError('list modified during sort')`. Its source explains why:
    "allowing mutations during sorting is a core-dump factory" (P-S8).
  - Lua raised "attempt to compare number with nil", and for an
    inconsistent comparator, "invalid order function for sorting".
  - Sand P2 sorts a snapshot of the list. It lost the comparator's
    `push`, and the result is sorted (`[1, 2, 3]`).

  Tests: CPython E6, Lua E6, `sand_p2` E6b. **[necessary]**: each native
  that calls back must define what happens if the callback changes its
  inputs. Section 8.2 gives the choice.
- **F-N11. Rust bindings detect re-entry at run time.** mlua reports that
  Lua "called the same mutable callback again" (L-S13b). PyO3 reports
  "Already borrowed" (P-S13). Test: mlua E9 printed "mutable callback
  called recursively". **[necessary]**: Sand natives that hold `&mut`
  state across a callback need the same guard.
- **F-N12. In CPython, a callback can reach the host process.** A callback
  reached `os` through `__globals__`. With empty builtins, a script still
  reached more than 100 classes through `__subclasses__()`. PEP 578 says:
  "Sandboxing CPython has been attempted many times in the past, and each
  past attempt has failed" (P-S10). Test: CPython E8 and E8b.
  **[tangential]**: this comes from Python's object model. Sand has no
  reflection.

**Verdict.** **[blocker]** for Sand → native → Sand callbacks, because of
F-N5, F-N6, and the Sand results E3 and E13 in section 7. Section 8.3
keeps a restricted form for one case of host entry.

### 5.3 Nested calls with continuations

Lua 5.4 (`lua_callk`, `lua_pcallk`, `lua_yieldk`) and Luau
(`lua_pushcclosurek`, L-S10) use this design.

- **F-K1. A continuation lets a callback yield across a native frame.**
  With `lua_call`, the yield failed. With `lua_callk`, the coroutine
  yielded, and on resume Lua ran our continuation. The manual explains
  that the native's "frame in the C stack was destroyed by the yield", so
  "Lua calls a continuation function" instead (L-S5, L-S5b). Test: Lua E3.
  **[necessary]**: a resumable callback needs a continuation that the VM,
  not the Rust stack, holds.

**Verdict.** In C, this design is a nested call that the VM can abandon
with `longjmp` and resume through a continuation. Safe Rust has no
`longjmp`, so a Sand native must return to the VM to allow a yield. That
is design 5.4. **[tangential]** as a separate design for Sand.

### 5.4 Stackless continuations (trampoline)

The native returns a request. The VM calls the script function on the
script's own stack, then resumes the native's continuation. piccolo and
Sand P2 use this design. Erlang's `enif_schedule_nif` is a tail-only form
of it.

- **F-T1. Depth does not use the Rust stack.** piccolo says: "It does not
  rely on the rust stack" (R-S1). It recursed through a native to
  1,000,000 levels. Sand P2 recursed to 9,990 levels, then failed at
  10,000 with "limit: stack overflow", which is the VM's
  `max_call_depth` default. Neither aborted. Tests: `piccolo_cb` E2,
  `sand_p2` E2. **[necessary]**
- **F-T2. A callback can suspend.** In piccolo, a yield inside a callback
  reached the coroutine's resumer and returned with the value (`42`). In
  Sand P2, a callback slept 10 ms while another task ran three ticks, and
  returned `300`. Tests: `piccolo_cb` E3, `sand_p2` E3. **[necessary]**:
  this keeps async I/O by default.
- **F-T3. The host gets control back after each fuel slice.** piccolo's
  executor runs with a "fuel" budget (R-S1d). It ran an infinite loop
  inside a callback for 1,000 slices of 10,000 fuel, and the host got
  control after each slice. Sand P2 stopped the same loop with
  "instruction limit exceeded", and stopped unbounded allocation with
  "memory limit exceeded". Tests: `piccolo_cb` E4, `sand_p2` E4, E5.
  **[necessary]**
- **F-T4. Stackless code is harder to write.** piccolo's author says:
  "writing things as a `Sequence` implementation is much more difficult
  than writing in normal, straight control flow" (R-S1b). Our merge sort
  with a Sand comparator needs 58 lines in P2: a 40-line state machine and
  the registration. The same algorithm in P1 needs 24 lines of direct
  code. Test: line counts in `sandlab/src/bin/sand_*.rs` (section 7.2).
  **[nice to have]** to reduce, through the async adapter in section 8.2.
- **F-T5. piccolo still has `unsafe` code.** Its README says "there are a
  few sources of unsafety, but crucially these sources of unsafety are
  isolated" (R-S1c). Test: `unsafe_count.sh` counted 21 `unsafe` sites in
  piccolo and 119 in its GC library. **[tangential]**: Sand P2 has none
  (F-P2).

**Verdict.** **[necessary]**. Of the designs we tested, this is the only
one that keeps every hard constraint and async I/O. Section 7 gives the
Sand measurements.

### 5.5 Separate native stacks (fibers and stack switching)

Wasmtime's async API, Python's greenlet, and the WebAssembly
stack-switching and JSPI proposals use this design.

- **F-F1. A fiber lets a nested callback suspend.** In wasmtime, a guest
  called an async host function, which called the guest again, which
  called an async host function that slept 5 ms. The call returned `42`
  in 6.2 ms. Wasmtime runs async calls on a "separately allocated native
  stack" (W-S4). A blocking `call` on the same store failed with "store
  configuration requires that `*_async` functions are used instead".
  Test: `wasmtime_async_cb` E3, E3b. **[nice to have]** as a precedent.
- **F-F2. Fibers need per-CPU assembly and `unsafe` code.** Wasmtime's
  fiber crate has stack-switch assembly for each CPU. For any other
  target, including wasm32, it chooses to "defer the error to happen at
  runtime when a fiber is created" (W-S5). greenlet warns about stacks
  that include "native (C) frames" (P-S7). Tests: `unsafe_count.sh` (109
  `unsafe` sites in the fiber crate) and F-P3. **[blocker]**: fibers break
  the safe-Rust rule and the web target.
- **F-F3. JSPI suspends only WebAssembly frames, and it warns about
  re-entrancy.** It says: "Not all applications can equally tolerate
  being reentrant in this way" (W-S10). **[tangential] [source-only]**

**Verdict.** **[blocker]**.

### 5.6 Component Model re-entrance rules

- **F-C1. The Component Model first forbade re-entrance, then allowed it.**
  - The 2024 spec said its guard "ensures that components cannot be
    externally reentered" (W-S7).
  - The current spec says that the callee "may reenter the caller's
    component instance" (W-S8). It adds that a host "must be careful to
    only perform recursive reentrance on components when explicitly
    supported by the component's documented API" (W-S8b).
  - Wasmtime 50 traps only after a trap has poisoned the instance (W-S8c).

  Test: `wasmtime_async_cb` E10. Our host re-entered a component during an
  import call, and the call returned `Ok(())`. **[nice to have]**: Sand can
  follow this rule and document re-entrance for each native.

## 6. Sorting with a script comparator, across runtimes

Each runtime sorts the same 200,000 pseudo-random ints. Times are medians
of 5. **[nice to have]**

| Runtime and method                                     |   Time |
| ------------------------------------------------------ | -----: |
| Lua 5.4.8, `table.sort` with a Lua comparator          | 123 ms |
| Lua 5.4.8, `table.sort`                                |  56 ms |
| Lua 5.5.0, `table.sort` with a Lua comparator          | 136 ms |
| Lua 5.5.0, `table.sort`                                |  51 ms |
| CPython 3.13, `sort(key=cmp_to_key(comparator))`       | 252 ms |
| CPython 3.13, `sort()`                                 |  47 ms |
| CPython 3.14, `sort(key=cmp_to_key(comparator))`       | 246 ms |
| CPython 3.14, `sort()`                                 |  38 ms |
| Sand P2 binary, merge sort written in Sand             | 205 ms |
| Sand P2 binary, native merge sort, Sand comparator     | 329 ms |
| Sand P1 binary, merge sort written in Sand             | 218 ms |
| Sand P1 binary, native merge sort, Sand comparator     | 359 ms |
| Sand, `sort_by(key)`                                   |  65 ms |
| Sand, `sort()`                                         |  12 ms |

Sources: `sort_lua-*.txt`, `sort_python-*.txt`, `sand_p2.txt`,
`sand_p1.txt`. The runtimes use different algorithms. Lua's `table.sort`
is a quicksort (L-S14). CPython's sort is "an adaptive, stable, natural
mergesort" (P-S14), and the comparator goes through a `cmp_to_key`
wrapper. The Sand rows are a bottom-up merge sort. So compare the
ratios within each runtime, not the times across runtimes.

- **F-X1. In Sand, a higher-order function in Sand is faster than a native
  that calls back.** In each binary, the native version of the same merge
  sort took about 1.6 times as long as the Sand version (P2: 329 ms vs
  205 ms; P1: 359 ms vs 218 ms). A P2 callback costs 70 ns, and a Sand call
  through a function value costs 27 ns (`sand_p2` E1). **[necessary]**: keep
  higher-order library code in Sand, and use callbacks for host APIs. An
  optimized P2 could narrow the gap (section 9), so measure again after
  the optimization.
- **F-X2. A comparator callback per comparison is always the slow path.**
  The comparator sort took about 2.2 to 2.7 times as long as the plain
  sort in Lua, and 5.3 to 6.5 times in CPython. In Sand, P2's comparator
  sort took about 27 times as long as `sort()`, but those are different
  algorithms; the like-for-like ratio is F-X1's 1.6. **[nice to have]**:
  Sand's `sort_by(key)` already avoids the comparator.

## 7. Sand prototypes

The patch is `prototypes/callbacks-p1-p2.patch`. It changes `Cargo.toml`,
`src/vm/mod.rs`, `src/vm/interp.rs`, and `src/vm/sched.rs`. The tests are
in `sandlab/src/bin/`. **[necessary]**: the prototypes are the main
evidence for the recommendation.

### 7.1 How the prototypes work

**P1, nested calls.** `Vm::call_nested(f, args)` runs the callback on a
second fiber, on the Rust stack under the native's frame.

- If the callback suspends, P1 returns an error.
- P1 allows 200 nested callbacks, as Lua does.
- P1 reuses its callback fibers.
- With the `nested_callbacks` feature, `call_native` moves the caller's
  fiber to a list that the GC scans. We call this "parking". Without it,
  the GC cannot see the caller's stack while a callback runs.

**P2, stackless continuations.** A native returns
`Err(Ctl::call(f, args, state, then))`.

1. The VM pushes a `Continuation` on the current fiber.
2. The VM pushes a frame for `f` above the live registers.
3. The callback runs as ordinary Sand code on the same task. It can call
   natives, allocate, and suspend.
4. When `f` returns, the VM calls `then(vm, result, state)`. The
   continuation returns the native's result, or `Ctl::call_again` for the
   next callback.

The GC roots `state` and the callback's result. The VM retries a
continuation after a garbage collection, as it retries a native, but a
retry repeats only the continuation step. The only change to the interpreter's
hot path is one comparison when a frame returns.

### 7.2 Results

Each item gives the test, then the P1 and P2 results.

- **E1, cost of one callback.** **[nice to have]**
  - P1: 101.3 ns.
  - P2: 69.9 ns.
  - A Sand call through a function value: 27 ns to 31 ns.
- **E3, a callback that sleeps 10 ms while another task runs.**
  **[blocker]** for P1.
  - P1 failed with "a native callback cannot suspend".
  - P2 returned `300`.
- **E2, native ↔ script recursion.** **[necessary]**
  - P1 succeeded at 200 levels and failed at 1,000 levels with "native
    callback nesting too deep", its own limit.
  - P2 succeeded at 9,990 levels and failed at 10,000 levels with "limit:
    stack overflow", the VM's own limit.
- **E4, an infinite loop in a callback.** Both stopped it with
  "instruction limit exceeded". **[necessary]**
- **E5, unbounded allocation in a callback.** P2 stopped it with "memory
  limit exceeded". **[necessary]**
- **E11, the caller's locals after a GC inside a callback.** One script
  for both designs, under `gc_stress`. **[blocker] [hazard]** for P1
  without parking.
  - P1 without parking: the GC freed the caller's list and gave its slot to
    another object. The script then read that object: "index 49, length 1".
  - P1 with parking: correct.
  - P2: correct (`results/sand_p2_e11b.txt`). A second P2 test sorted with
    an allocating comparator, also correctly.
- **E12, a comparator that empties the list and allocates, under
  `gc_stress`.** **[necessary] [hazard]**
  - P1 keeps the elements in Rust `Vec`s, which are not GC roots. The GC
    freed them, then its tracer panicked with "index out of bounds", and
    the VM reset the session.
  - P2 keeps the elements in rooted `state`, and the sort was correct.
  - Both designs need a rule (section 7.4). P2's prototype also keeps the
    callee in `state`, not in a capture, for the same reason.
- **E13, a callback with a side effect, then a native step that checks
  memory before it allocates.** The size of the allocation depends on the
  callback's result, so the check must come after the callback. The test
  forces the first memory check to fail. **[necessary] [hazard]** under
  Sand's current retry rule.
  - P1: "callback ran 2 times". The engine retried the whole native, so
    the native cannot follow SPEC §16.3. P1 needs a different retry rule:
    collect garbage in place, without a restart. That rule needs parking
    and branded handles (section 7.4).
  - P2: "callback ran 1 times", and "the continuation ran 2 times". The
    engine retried only the step after the callback. SPEC §16.3 must
    change to say this.
- **E13b, a callback that returns a heap value, then a retried
  continuation, under `retry_stress` and `gc_stress`.** **[necessary]
  [hazard]**
  - P2: the script got `[["view", "tree"]]`, the correct value.
  - P2 without the fix that roots the result: the script got a corrupted
    value and failed with "value too deeply nested to display"
    (`results/sand_p2_e13b_without_fix.txt`).
- **E14, cancel a task while its callback is suspended.** P2 ran the
  task's `defer` and then reused fibers correctly. **[necessary]**
- **E6, sort 200,000 ints with the same merge sort.** **[nice to have]**
  - P1: 359 ms, 1.65 times the Sand code in the same binary.
  - P2: 329 ms, 1.61 times the Sand code in the same binary.
  - The difference between P1 and P2 is within the noise between builds.
- **Native source size for the merge sort.** **[nice to have]** to reduce
  (F-T4).
  - P1: 24 lines in direct style.
  - P2: 58 lines with a state machine.
- **E16, a spike of a sync host entry.** The host polls `Session::eval`
  once and treats "not ready" as a failure. **[necessary]**
  - A handler that returns gave `320`.
  - A handler that sleeps gave "not ready".
  - The session worked correctly afterwards.
  - Each sync entry, including the compilation of the call expression,
    took 41.1 µs.
- **E16c, a handler that only computes.** **[necessary] [hazard]**
  - It returned after 0.17 ms and 0.86 ms for small inputs.
  - With a larger input, it gave "not ready" after 2.19 ms, without any
    I/O. The VM's scheduler yields to the host every 2 ms (S-S5), and one
    poll treats the yield as a suspension.
- **E16d, a handler that changes state, then sleeps.** The entry failed,
  and the change stayed: the list was `[1]`. **[necessary] [hazard]**

Raw output: `lab/results/sand_p1.txt`, `sand_p1_noparking.txt`,
`sand_p2.txt`.

### 7.3 Cost to programs that do not use callbacks

We ran the `bench/` programs suite (18 programs) with three `sand`
binaries: unpatched, P2, and P2 with P1's parking. There were 3
interleaved rounds of 5 runs each (`lab/overhead.sh`). For each binary
and program, we used the best of the per-round medians.

- **F-O1. P2 has no consistent cost.** The geomean P2/base ratio was
  1.004, and no program was slower in all 3 rounds. Three programs were
  10% to 17% slower in 2 of the 3 rounds: `dijkstra` (0.96, 1.17, 1.15),
  `fannkuch` (1.10, 1.17, 0.96), and `ray` (1.13, 1.15, 0.93). A small
  cost on those three is possible;
  more rounds would settle it. **[necessary]**
- **F-O2. P1's parking costs about 7.5% overall, and up to 87% on one
  program.** The geomean P1/base ratio was 1.075. In every round,
  `fannkuch` was 1.50 to 1.87 times as slow, and `dijkstra` 1.20 to 1.43
  times. Parking moves the fiber on every native call.
  **[nice to have]**: if Sand ever uses P1, it should park only for
  natives that can call back.
- **Noise.** One program's baseline varied by up to 23% between
  rounds. So a ratio for a single program is significant only if it holds
  in every round. The binaries came from the patch before the E13b fix,
  which changes only the retry path, not the hot path.
  **[necessary]** for the reading of F-O1 and F-O2.

Full table, with per-round ratios: `lab/results/overhead_summary.txt`.

### 7.4 Handles that native code holds

- **F-L1. Today's `Val` is `Copy` and has no lifetime.** So the compiler
  cannot stop E12 in P1, or a capture in a P2 continuation. Source:
  `src/native/ty.rs`. Test: E12. **[necessary] [hazard]**
- **F-L2. Lifetimes can stop a native from holding a raw handle across a
  call that collects garbage.** In the E15 model, `Val<'vm>` borrows the
  VM, and a nested call needs `&mut Vm`. rustc rejected a native that held
  a `Val` across the call with error E0502. A native that first made a
  rooted `Handle` compiled and read the right value. Test: `lab/e15/`,
  `results/e15.txt`. **[necessary]** for any design that runs callbacks
  while a native holds handles.

### 7.5 Platforms, safety, and regressions

- **F-P1. P2 builds for the web.** With the final patch, `sandlang-wasm` built
  for `wasm32-unknown-unknown`. Test: `results/wasm32_sandlang_p2.txt`.
  **[necessary]**
- **F-P2. P2 keeps Sand free of `unsafe` code.** With the final patch, Sand's
  `src/` has no `unsafe` blocks, functions, or impls, and `lib.rs` keeps
  `#![forbid(unsafe_code)]` (S-S4). Every other Rust runtime that we
  tested has `unsafe` code. `results/unsafe_count.txt` gives the counts.
  Test: `lab/unsafe_count.sh`. **[necessary]**
- **F-P3. Wasmtime fibers fail at run time on wasm32.** For wasm32, the
  fiber crate fails with "fibers unsupported on this host architecture"
  when it creates a fiber (W-S5b). Our wasm32 build of an async wasmtime
  call succeeded, because the error happens only at run time. We could not
  run the build: no_std wasmtime needs platform hooks that the test did not
  supply. Test: `results/wasm32_wasmtime_fiber.txt`.
  **[blocker] [source-only]**
- **F-P4. The patch did not change Sand's test results.** With the patch,
  1,127 tests of the `sandlang` package passed and 3 failed. The same 3
  tests also fail without the patch:
  - `listeners_bind_exactly_the_checked_address`: this machine has no
    IPv6.
  - `names_that_are_not_utf8`: the file system of this machine.
  - `peak_memory_stays_near_the_memory_limit`: peak memory 1.32 times the
    limit.

  Tests: `results/sand_tests_with_patch.txt`,
  `results/sand_tests_baseline_failures.txt`. We did not run the suite
  with the `nested_callbacks` feature. **[necessary]**

## 8. Recommendation

### 8.1 The two directions of a callback

A UI binding needs two kinds of call into Sand.

1. **Sand → native → Sand.** A script calls a native, and the native calls
   a script function before it returns. Examples are a comparator, or a
   builder that a script passes to a view constructor. A Sand task waits
   for the native, so the callback can suspend. **Use P2.**
   **[necessary]**
2. **Host → Sand.** The UI framework calls a script function at a time
   that it chooses, for example SwiftUI's `body`, Compose's `measure`, or a
   button action. Swift or Kotlin code on the UI thread waits, not a Sand
   task, so P2 cannot serve this direction. **[necessary]**
   - **Async entry:** run the handler as a task. Today a host can only
     `eval` a `spawn`, and the task runs only while the host drives
     `run_tasks_until` (F-S6). Use the async entry for events.
   - **One owner.** The host needs one loop that both drives tasks and
     makes sync calls (F-S6), with a timer and reactor (F-S7). Sand can
     ship that loop as a host-side driver.
   - **Partial handlers.** Any entry can see a handler that has run only
     partway (F-S6). The UI thread cannot wait for a handler that waits on
     I/O. This is a **product tradeoff**:
     - accept it, as JavaScript does, and document that handlers must keep
       their invariants across every wait; or
     - give the sync entry a mode in which no other task runs, which needs
       scheduler work and removes the 2 ms yield (E16c).
   - **Sync entry:** run the handler to completion and return its value.
     If the handler suspends, fail with a clear error (E16). The host API
     demands an immediate answer, so Sand cannot avoid this failure. The
     host can then use an async update instead, as Compose code does with
     `LaunchedEffect`. This is the model of `invokeSync` and
     `NativeCallable.isolateLocal` (F-U4).
   - The sync entry needs a run mode without the 2 ms yield to the host
     (E16c), bounded by an instruction or CPU budget instead. This is a
     **product tradeoff**: a long handler either blocks the UI thread up
     to the budget, or fails.
   - A failed sync entry leaves partial changes (E16d). This is also a
     **product tradeoff**: document the partial changes, or let the type
     checker reject sync handlers that can suspend.

The ban on suspension in the sync entry does not conflict with F-N6. F-N6
is a blocker for Sand → native → Sand callbacks, where P2 lets the
callback suspend. In the sync entry, the host gives Sand no way to
suspend.

### 8.2 Work items

1. **[necessary]** Add rooted handles that a host can keep and must
   release (F-S4). The VM's `pinned` list is a root set that this API can
   use.
2. **[necessary]** Add the async and sync host entry points of 8.1, with
   the run mode and the partial-change rule that 8.1 describes. E16 polls
   `eval`; a real API should call a kept handle directly, without
   compilation.
3. **[necessary]** Land P2. Close these gaps first:
   - A continuation must be able to return `Async`, `NeedPermission`, and
     `Yield`. A continuation must also go through the checks that
     `call_native` does: limits, cancellation, and preemption.
   - Unwind continuations correctly through `defer` and cancellation. E14
     passes, but the prototype clears continuations only when it recycles a
     fiber.
   - Count continuations in `Fiber::footprint`.
   - Allow native callees, not only Sand closures.
   - Show native frames in stack traces.
   - Remove the allocations on each callback, to approach the cost of a
     Sand call.
   - Let the VM collect garbage and retry if `enter_callback` cannot grow
     the stack. Today that failure ends the run.
   - Change SPEC §16.3: a retry repeats only the current continuation
     step (E13).
4. **[necessary]** Prevent unrooted handles in continuations. Give `Val` a
   lifetime (F-L2), or pass all heap state through a typed, rooted
   container.
5. **[necessary]** Decide what a native does if a callback changes its
   inputs (F-N10). This is a product tradeoff:
   - fail with an error, as CPython does; or
   - work on a snapshot and lose the change, as P2's sort does.

   We have no evidence about which choice users prefer. CPython chose to
   fail.
6. **[necessary]** Guard `NativeObject` borrows against re-entry (F-N11).
7. **[necessary]** Keep higher-order library functions in Sand (F-X1).
8. **[nice to have]** Add an adapter that turns a Rust `async` block into a
   P2 continuation, so that `cx.call(f, args).await` replaces the state
   machine. The block must get VM access only between awaits. We did not
   build or test this adapter.
9. **[blocker]** Do not use P1 for Sand → native → Sand callbacks.
10. **[blocker]** Do not use fibers or stack switching.

### 8.3 A sync host entry while a native runs

One case remains. A Sand native can call a host API that calls back into
Sand before it returns, for example a synchronous layout pass. That is a
sync host entry while a native is on the Rust stack. It needs P1's
mechanics:

- parking the active fiber (E11);
- a nesting bound (F-N4, F-N5);
- a ban on suspension, as for any sync entry (8.1);
- lifetime-branded handles (F-L2), against E12;
- a retry rule that collects garbage in place, without a restart (E13).

The API shapes suggest that custom layout needs this case: a Compose
measure policy measures its children inside `MeasureScope.measure`
(J-S11b), and a SwiftUI `Layout` asks its subviews for sizes inside
`sizeThatFits` (J-S10b). If a script child has script layout code, the
host calls Sand while the parent's native is on the Rust stack. We could
not test this on a UI framework. **[necessary]** to test in the spike of
recommendation 0. The product choice:
- design the layout binding so that script measure code returns a
  description and never measures a child synchronously, which keeps one
  callback path but limits scripts; or
- accept a restricted nested sync entry, which adds a second callback path
  to the VM. If they
do, Sand needs P1's parking. F-O2 measures parking on every native call;
parking only for natives that can call back would cost less, but we did
not measure that.

## 9. Limits of this research

- **Facts without a run.** These facts have the **[source-only]** tag,
  because we could not run the system on this machine: F-U1 to F-U6,
  F-A1 to F-A3, F-F3, and F-P3. **[necessary]** to know before you rely on
  them.
  - This machine has no Swift, Xcode, Android, Flutter, Erlang, Go, or
    Wren toolchain.
  - We ran QuickJS (F-N4 to F-N9) but did not test its threading rule
    (F-U6).
- **Host event loop.** No test embeds Sand in a real UI run loop. E16
  shows that one poll of a session works for short handlers, and E16c
  shows that it fails for long ones. E17 and E18 ran a plain loop on
  Linux, not CFRunLoop or Looper. **[necessary]**: recommendation 0.
  A host must not call the sync entry while it already polls the same
  session. Rust's `&mut Session` borrow prevents that for a single owner.
  We did not test a host that shares a session through other means.
  **[necessary]** to test before work on the sync entry starts.
- **Prototype scope.** The prototypes handle only the cases in section
  7.2. Section 8.2 lists the gaps. Two more: P1's callbacks run without
  the CPU-time deadline (`call_nested` passes none), and no test covers
  the CPU limit in either prototype. P1's E1 and E6 numbers come from the
  parking build, so they include parking's cost on every native call.
  E16 ran on the P2 binary, but it uses only today's `Session` API.
  **[necessary]**
- **Reproduction side effect.** `sandlab/build.sh` applies the patch to
  this repository's `src/` and leaves it there. Revert it with
  `git checkout -- Cargo.toml src`. **[nice to have]**
- **Unoptimized prototypes.** The P1 and P2 per-call costs come from
  unoptimized code, so the ranking in E1 can change. **[nice to have]**
- **Test scope.** We ran the tests of the `sandlang` package, not of `cli/`
  or `wasm/`, and not with the `nested_callbacks` feature.
  **[tangential]**
- **Reproduction.** The patch applies to commit `1520314` (origin/main at
  the time of this research). Later changes to `src/` can stop it from
  applying. **[nice to have]** to keep in mind.

## 10. Experiment index

All paths are under `lab/` unless noted. Runtime versions:

- Lua 5.4.8 and 5.5.0; mlua 0.11.6.
- CPython 3.13.18 and 3.14.6.
- QuickJS-NG 0.16.2 through rquickjs 0.14.0.
- wasmtime 50.0.1 and wasmi 2.0.0.
- piccolo 0.3.3, Rhai 1.27.0, Rune 0.14.2, starlark-rust 0.14.2.

| ID         | Question                                       | Location                    |
| ---------- | ---------------------------------------------- | --------------------------- |
| E1         | Cost of a callback, and of a script call       | every runtime               |
| E2, E2b    | Native ↔ script recursion depth                | all but mlua                |
| E3, E3b    | Can a callback suspend on I/O?                 | see F-N6, F-T2, F-F1        |
| E4         | Fuel or instruction limit inside a callback    | see F-N7, F-N8, F-T3        |
| E5         | Memory limit inside a callback                 | Lua, QuickJS, Sand P2       |
| E6         | Sort with a comparator                         | `sort/`, Lua, CPython, Sand |
| E7         | The host keeps a handler and calls it later    | Lua                         |
| E8, E8b    | Callback isolation in CPython                  | `py/cb.py`                  |
| E9         | Re-entrant Rust callback guards                | `mlua_cb.rs`                |
| E10        | Component Model re-entrance                    | `wasmtime_async_cb.rs`      |
| E11, E11b  | Caller's locals after a GC in a callback       | `sandlab`                   |
| E12        | Native-held handles after a GC in a callback   | `sandlab`                   |
| E13, E13b  | Retry of a native after a callback             | `sandlab`                   |
| E14        | Cancel a task during a suspended callback      | `sandlab`                   |
| E15        | Compile-time ban on handles across a callback  | `e15/`                      |
| E16, c, d  | Sync host entry by one poll                    | `sandlab`                   |
| E17, E18   | Host event; plain run loop without tokio       | `sandlab` (`sand_host.rs`)  |
| E-S1, E-S3 | Sand today: deferred callback; kept handle     | `sandlab`                   |
| Overhead   | Prototype cost on `bench/` programs            | `overhead.sh`               |
| Sources    | Every printed quote is in its source           | `verify_sources.py`         |

In the table, `sandlab` means `../sandlab/src/bin/`, and each Rust file
name is under `src/bin/`. The results of each runtime are in
`results/<runtime>.txt`.
