# Native-to-Sand callbacks: research report

## 1. Summary

Sand can give native functions a safe way to call Sand functions. The
callback can suspend on I/O, the engine limits still apply, and the code
stays in 100% safe Rust. The best mechanism is a **stackless continuation**:
the native does not call the Sand function itself. Instead, it returns a
request to the VM. The VM runs the Sand function as a normal frame on the
current task, then gives the result to a continuation that the native
supplied. Lua's `lua_callk`, piccolo's `Sequence`, and Erlang's
`enif_schedule_nif` are versions of this idea.

We built two prototypes in the Sand VM and tested them against each other:

- **P1, nested calls.** The native calls `vm.call_nested(f, args)` and gets
  the result back on the Rust stack. This is the Lua `lua_call` and CPython
  `PyObject_Call` model.
- **P2, stackless continuations.** The native returns
  `Err(Ctl::call(f, args, state, then))`, and the VM resumes `then` with the
  result.

P2 upholds every Sand product priority that we tested. P1 breaks three of
them:

- A P1 callback cannot suspend, so any callback that does I/O fails.
- P1 runs a callback's side effects twice if the engine retries the native.
- P1 lets the GC free values that a native holds in Rust variables.

The cost of P2 is ergonomic. A native that calls back more than once must be
written as a state machine. Our sort with a Sand comparator needs 58 lines
in P2 and 19 lines in P1.

Callbacks across the native boundary are slow compared with Sand-to-Sand
calls. A merge sort written in Sand ran in 221 ms. The same sort as a native
function with a Sand comparator ran in 401 ms (P2) and 521 ms (P1). So
higher-order library code should stay in Sand, as it does today. Callbacks
are for host APIs that must call script code, such as UI frameworks.

UI frameworks need a second, separate mechanism. SwiftUI and Jetpack Compose
call script code from the UI thread at times that they choose, and some of
those calls must return a value at once. Our recommendations:

1. **[necessary]** Add P2 continuations for Sand → native → Sand callbacks
   (section 7.2).
2. **[necessary]** Add GC-rooted handles that a host can keep. Today, a
   kept handle becomes stale after a GC and the session resets (section 3).
3. **[necessary]** Add two host-to-Sand entry points:
   - an async entry that runs a handler as a task;
   - a synchronous entry that runs to completion and fails if the
     callback suspends.

   React Native, Flutter, and Valdi expose the same two entry points
   (section 4).
4. **[blocker]** Do not adopt nested calls on the Rust stack as the general
   mechanism.
5. **[blocker]** Do not adopt separate native stacks (fibers).
6. **[nice to have]** Let natives use Rust `async` blocks as continuations,
   so authors do not write state machines by hand. We did not prototype
   this.

## 2. Method

### 2.1 Product priorities and how we weigh them

The priorities come from `README.md` and `AGENTS.md`. We use them in three
weight classes.

- **Hard constraints.** A design that violates one gets a **[blocker]** tag.
  - 100% safe Rust.
  - Full sandboxing, with no escape mechanisms.
  - Hard, user-set limits on memory and CPU.
  - A single-threaded engine, with many engines in one process.
  - Support for iOS, macOS, Windows, Linux, Android, and the web.
- **Core properties.** A design must keep these. A feature that keeps them
  gets a **[necessary]** tag.
  - Async I/O by default.
  - Maximum performance without a JIT.
  - Native Rust modules with type interop.
  - An ergonomic runtime for untrusted code.
- **The target use case.** Scripting over callback-heavy native UI APIs,
  such as SwiftUI and Jetpack Compose. This is the reason for the request.

### 2.2 Tags

Each fact and each judgment has at least one tag.

- **[blocker]**: Sand's priorities prohibit this.
- **[necessary]**: Sand must do this to keep its priorities.
- **[nice to have]**: good to do, but its absence does not substantively
  hurt the priorities.
- **[tangential]**: not important for Sand.
- **[hazard]**: a failure mode that a design must prevent. Used with one of
  the tags above.
- **[source-only]**: the empirical check is a mechanical check of the
  primary source, not a run. We use this only for Apple and Android APIs,
  because this Linux machine has no Swift, Xcode, or Android toolchain.

### 2.3 Evidence

Each fact gives a source ID and an experiment ID.

- **Source IDs** (for example `L-S3`) point to `sources/*.md`. Each entry
  has a URL at a pinned tag or commit and a short verbatim quote.
  `lab/verify_sources.py` downloads every web source and reads every crate
  source again, then checks that each quote is present. Result: 52 of 52
  web quotes and 14 of 14 crate-source quotes found
  (`lab/results/sources.txt`).
- **Experiment IDs** (for example `E2`) name a test in the program for that
  runtime. The output is in `lab/results/<runtime>.txt`. Section 10 lists
  every experiment.

To keep the tests fair, we used these rules:

- **Same workload everywhere.** Every runtime runs the same workload with
  the same callback body, `x => x + 1`.
- **Same timing method.** Each timing is the median of 5 runs. All timed
  runs ran one after another on a quiet machine (`lab/run_all.sh`).
- **Fixed machine.** Linux 6.18, x86_64, 4 vCPUs, rustc 1.97.0, gcc 13.3,
  CPython 3.13.18 and 3.14.6.
- **No JIT in the speed comparison.** Sand prohibits a JIT. Wasmtime
  compiles WebAssembly to machine code, so its timings are context only.
  wasmi is the interpreter to compare with.
- **Loop overhead is visible.** The "native→script" loop runs in the host
  language, and the "script→script" loop runs in the interpreter, so the
  two rows show different loop costs. Each table also shows a loop without
  a call.

Runtime versions: Lua 5.4.8 and 5.5.0, CPython 3.13.18 and 3.14.6,
QuickJS-NG 0.16.2 (rquickjs 0.14.0), wasmtime 50.0.1, wasmi 2.0.0,
piccolo 0.3.3, Rhai 1.27.0, Rune 0.14.2, starlark-rust 0.14.2, mlua 0.11.6.

## 3. Sand today

- **F-S1. A native cannot call a Sand function.** A native receives a
  function as `Val<fn(..)>`, an opaque handle. `Vm` has no method that
  calls it. Source: `src/native/ty.rs` (`Val`), `src/vm/mod.rs` (`HostFn`).
  Test: `sand_current` E1. **[necessary]** to change for the UI use case.
- **F-S2. The standard library writes higher-order functions in Sand.**
  `map`, `filter`, and `sort_by` are Sand code. `sort_by` computes all keys
  in Sand, then calls the native `sort_by_keys`
  (`src/stdlib/prelude.sand:908`). Test: `sand_p2` E6 measures this
  pattern: `sort_by(key)` takes 81 ms for 200,000 ints, and `sort()` takes
  30 ms. **[necessary]** to keep: the pattern is fast and needs no
  callback.
- **F-S3. A native can already run a Sand closure later, as a task.**
  `Vm::spawn_task` is public. A native can spawn the closure and return the
  `Task`. The script waits for it, and the callback can suspend on I/O.
  Test: `sand_current` E-S1 (result `55`, 355 ns per round trip) and
  E-S1b (a callback that sleeps returns `7`). **[necessary]** to keep as
  the async event path, for example a button action.
- **F-S4. A host cannot keep a handle.** `Val` handles are not GC roots. We
  kept a handle past the native call and caused a GC. When the host then
  used the handle, the VM failed with "spawn: not a closure" and reset the
  session. Source: the `Val` doc says "a `Val` is valid only for the
  duration of the synchronous native call". Test: `sand_current` E-S3.
  **[necessary] [hazard]**: UI hosts must keep event handlers.
- **F-S5. The engine re-runs a `vm_function` from the start after a
  permission prompt or a GC retry.** SPEC §16.3 states this rule. Natives
  must check limits before side effects. Test: E13 in `sand_p1` and
  `sand_p2` (section 7). **[necessary] [hazard]**: any callback design must
  not run callbacks twice.

## 4. What native UI frameworks need

These facts set the requirements. Their tests are mechanical source checks
(`lab/verify_sources.py`) unless an experiment is named.

- **F-U1. SwiftUI calls synchronous closures on the main actor.**
  - `View` is `@MainActor`, and `body` is a synchronous property.
  - `Layout.sizeThatFits` returns a `CGSize` synchronously.
  - `Button(action:)` takes a synchronous `@MainActor` closure.
  - `task(_:)` takes an `async` closure.

  Source: J-S10. **[necessary] [source-only]**: Sand must answer some host
  calls synchronously, on the host's thread.
- **F-U2. Jetpack Compose has the same split.**
  - `MeasurePolicy.measure` is a synchronous function that returns a value.
  - `LaunchedEffect` takes a `suspend` block.
  - Composables "might be run quite frequently, as often as every frame of
    an animation".

  Source: J-S11. **[necessary] [source-only]**
- **F-U3. React Native replaced its async-only bridge with synchronous
  calls.** Its team gives the reason: "some updates need to render
  synchronously in response to user input… Since the old architecture was
  only asynchronous, we needed to rewrite it." Layout also "could not be
  read synchronously". Source: J-S1. **[necessary] [source-only]**:
  async-only callbacks, which is Sand's status quo (F-S3), are not enough
  for UI.
- **F-U4. Production bridges offer both a sync entry and an async entry.**
  - React Native's `CallInvoker` has `invokeAsync` and `invokeSync` (J-S3).
  - Dart's `NativeCallable.isolateLocal` is synchronous and aborts the
    process if a foreign thread calls it. `NativeCallable.listener` is
    asynchronous and void-only (J-S7).
  - Valdi dispatches calls into JS asynchronously by default and needs an
    explicit scope for a synchronous return (J-S13).

  **[necessary] [source-only]**: Sand needs both entry points (section 8).
- **F-U5. Frameworks moved script code onto the UI thread to make
  synchronous calls possible.**
  - Flutter merged its UI and platform threads because "the split-thread
    design prevented… Dart FFI to interoperate with native APIs that must
    be called on the platform thread" (J-S8).
  - Lynx added main-thread scripts because background-thread handlers caused
    "animations lagging behind gestures" (J-S12).
  - NativeScript runs JavaScript on the UI thread (J-S9).

  **[necessary] [source-only]**: Sand must be able to run on the host's UI
  thread. A single-threaded engine allows this.
- **F-U6. Every engine we checked allows one thread at a time per runtime.**
  - QuickJS: "Inside a given runtime, no multi-threading is supported"
    (J-S4).
  - Hermes JSI: "The APIs must not be called from multiple threads
    concurrently" (J-S2).
  - JavaScriptCore makes other threads wait (J-S5).

  **[necessary] [source-only]**: Sand's single-threaded engine matches
  industry practice.
- **F-U7. UI frameworks call script handlers that the host stores.** Roact
  passes a script closure to `event:Connect` (L-S11), and Lua hosts keep
  closures with `luaL_ref` (L-S8). Test: Lua E7, a handler that the host
  stored and called after the script ended. **[necessary]**: see F-S4.

## 5. The design space

We found five families of design. Each subsection gives the runtimes that
use the design, the facts, and a verdict for Sand.

### 5.1 Forbid callbacks; deliver events asynchronously

Wren, Erlang NIFs, and Sand today use this design. Flutter platform
channels, React Native's legacy bridge, and Valdi's default path use the
async half of it.

- **F-A1. [source-only]** Wren's documentation says to "*not* try to resume the VM from
  within a foreign method… The VM is not re-entrant". Its check is a
  debug-only assert. Source: R-S8. **[tangential]**: this is a precedent,
  not a design to copy.
- **F-A2. [source-only]** Erlang tells NIF authors to return "within 1 millisecond". It
  prefers `enif_schedule_nif`, a tail continuation, over all other ways to
  split long work. The NIF API has no call into Erlang. Source: R-S7.
  **[nice to have]**: a tail-only continuation is a subset of P2.
- **F-A3.** Flutter platform channels pass messages "asynchronously
  through channels to ensure the user interface remains responsive"
  (J-S6). **[necessary]** for events, but F-U3 shows that this is not
  enough for layout or rendering.

**Verdict.** Keep async delivery for events (F-S3). It does not meet UI
needs alone. **[blocker]** for the UI use case if it stays the only
mechanism.

### 5.2 Nested calls on the native stack

The native calls the script function and waits for the result on the
native stack. Lua (`lua_call`), CPython (`PyObject_Call`), QuickJS
(`JS_Call`), JavaScriptCore, Hermes JSI, Rhai, Rune, starlark-rust,
wasmtime (sync API), wasmi, and Sand P1 use this design.

**Performance.** These are per-iteration times in ns, with the median of 5.
"Native→script" is a loop in the host that calls the script callback.

| Runtime                  | Loop, no call | Script→script | Native→script |
| ------------------------ | ------------: | ------------: | ------------: |
| Lua 5.4.8 (C API)        |           5.4 |          20.0 |          15.5 |
| Lua 5.5.0 (C API)        |           5.5 |          21.2 |          14.9 |
| Lua 5.4 via mlua (Rust)  |           6.3 |          19.0 |          63.3 |
| CPython 3.13.18          |          66.3 |          85.8 |          43.5 |
| CPython 3.14.6           |          66.6 |          77.1 |          36.3 |
| QuickJS-NG 0.16.2        |          27.2 |          52.7 |          64.5 |
| wasmi 2.0.0              |             – |          17.2 |          89.3 |
| starlark-rust 0.14.2     |          67.9 |          75.7 |          29.5 |
| Rune 0.14.2              |          92.3 |         113.9 |         161.5 |
| Rhai 1.27.0              |         197.6 |         451.7 |         270.1 |
| Sand P1 (nested)         |             – |          32.4 |         106.6 |
| wasmtime 50 (JIT; ref.)  |             – |           2.5 |          17.1 |

Sources: `lua-*.txt`, `mlua_cb.txt`, `python-*.txt`, `quickjs_cb.txt`,
`wasmi_cb.txt`, `starlark_cb.txt`, `rune_cb.txt`, `rhai_cb.txt`,
`sand_p1.txt`, `wasmtime_cb.txt`. The Sand script→script column calls
through a function value. A direct call to a small function costs nearly
nothing more than the loop (12.8 ns vs 13.1 ns, `sand_current.txt`).

- **F-N1. A nested call is cheap where the host language owns the API.**
  In Lua's C API, a host loop that calls a Lua callback costs less than a
  Lua loop that calls a Lua function (15.5 ns vs 20.0 ns). Test: Lua E1.
  **[nice to have]**
- **F-N2. Making a longjmp-based API safe in Rust costs speed.** mlua wraps
  each call in `lua_pcall` (L-S13) and takes 63.3 ns, against 15.5 ns for
  raw C on the same Lua 5.4. Test: mlua E1. **[necessary]**: Sand's safe-Rust
  rule excludes longjmp, so this cost does not apply to Sand.
- **F-N3. An error in Lua skips the native's cleanup.** Lua uses `longjmp`
  for errors (L-S1). Our native acquired a resource, then the callback
  raised an error. The result was acquired 1, released 0. CPython returns
  NULL and the cleanup ran (1, 1). Test: Lua E4, CPython E4.
  **[tangential]**: Sand natives return `Result`, so the problem cannot
  occur in Sand.

**Depth.** Nested calls use the native stack. Every runtime must bound
native↔script recursion, or a script can crash the whole host process.

- **F-N4. A counter keeps the bound in Lua, QuickJS, CPython, Rhai,
  starlark-rust, and wasmtime.** These depths are for native↔script mutual
  recursion; the script-only limits are much higher.
  - Lua: 197 levels, "C stack overflow". The script-only limit is 499,989.
    Lua uses `LUAI_MAXCCALLS 200` (L-S3).
  - QuickJS: 347 levels, `RangeError`. The script-only limit is 1,362.
  - CPython 3.13 with `recursionlimit=1e6`: 3,333 levels. The script-only
    limit is 999,998. CPython 3.13 uses `Py_C_RECURSION_LIMIT 10000`
    (P-S3).
  - CPython 3.14: 10,221 levels, "Stack overflow (used 8148 kB)". 3.14
    measures the real stack pointer (P-S4).
  - wasmtime: 546 levels, the trap "call stack exhausted". Host frames
    count toward `max_wasm_stack` (W-S6).
  - Rhai: 32 levels with the default `max_call_levels` of 64.
  - starlark-rust: 16 levels with its 50-frame default (R-S6).

  Tests: E2 for each runtime. **[necessary]**: a nested design needs such
  a bound.
- **F-N5. Without a bound, the host process aborts.** The callback scripts
  did nothing but recurse.
  - wasmi aborted with "has overflowed its stack" (exit 134).
  - Rune aborted the same way.
  - Rhai aborted after the host raised `max_call_levels`.

  Tests: `wasmi_cb` E2, `rune_cb` E2, `rhai_cb_deep`. **[blocker]
  [hazard]**: an untrusted script must never crash its host. A Sand design
  that uses the Rust stack would need a hard nesting bound, and
  F-N4 shows the bounds are low (16 to 3,333 levels).

**Suspension.** A nested native frame cannot be suspended, so the callback
cannot wait for I/O.

- **F-N6. No nested-call runtime lets a callback suspend through a plain
  native frame.**
  - Lua: "attempt to yield across a C-call boundary" (L-S5).
  - mlua: an async Rust function inside a callback inside a sync native
    fails with the same error.
  - CPython: the native gets an un-awaited coroutine, and Python warns
    "coroutine 'handler' was never awaited". PEP 219 explains the cause:
    "the C stack now holds a nested execution of the byte-code
    interpreter" (P-S6).
  - QuickJS: the native gets a promise. The callback resumes only after the
    host drains the job queue.
  - Rune: the native gets a `Future` and fails with "Expected number type".
  - Sand P1: "a native callback cannot suspend".

  Tests: E3 in each runtime. **[blocker]**: Sand does async I/O by
  default. In a nested design, every callback that calls `http.get`,
  `task.sleep`, or a prompted capability fails at run time.

**Limits.** Each runtime must apply its fuel and memory limits inside
nested callbacks.

- **F-N7. Most runtimes keep their limits inside callbacks.**
  - Lua's count hook fires inside a callback (L-S6), and its allocator
    limit gives "not enough memory".
  - QuickJS's interrupt ends the run with an error that the script cannot
    catch.
  - wasmtime and wasmi fuel trap.
  - Rhai's progress callback ends the script.
  - Sand P1 keeps its instruction limit.

  Tests: E4 and E5 for each runtime. **[necessary]**
- **F-N8. Rune's budget does not reach nested calls.** `Function::call`
  creates a new VM (R-S4). The budget stopped a loop that the VM called
  directly. The same loop, reached through a native's `Function::call`,
  ran until our 60 s timeout. Test: `rune_cb_budget`. **[blocker] [hazard]**:
  a callback design must charge the same meter. P1 and P2 do this.
- **F-N9. QuickJS lets a script catch an out-of-memory error.** Inside a
  callback, a script caught the error as `null` and continued. Test:
  QuickJS E5. **[tangential] [hazard]**: Sand's memory limit ends the run,
  so a script cannot catch it.

**Re-entrancy.** A callback can change state that the native relies on.

- **F-N10. Each runtime protects its own sort from a comparator that
  changes the list, but in different ways.**
  - CPython empties the list during a sort and raises
    `ValueError('list modified during sort')`. Its source calls mutation
    during sorting "a core-dump factory" (P-S8).
  - Lua raised "attempt to compare number with nil". With an inconsistent
    comparator, it raised "invalid order function for sorting".
  - Sand P2 sorts a snapshot. Changes that the comparator makes to the list
    are lost, and the result is sorted (`[1, 2, 3]`).

  Tests: CPython E6, Lua E6, `sand_p2` E6b. **[necessary]**: every native
  that calls back must define what happens if the callback changes its
  inputs.
- **F-N11. Rust bindings detect re-entry at run time.** mlua reports "mutable
  callback called recursively" (L-S13). PyO3 reports "Already borrowed"
  (P-S13). Test: mlua E9. **[necessary]**: Sand natives that hold
  `&mut` state across a callback need the same guard. `NativeObject`
  borrows are the case to check.
- **F-N12. CPython callbacks reach the host process.** A callback reached
  `os` through `__globals__`. With empty builtins, the object graph still
  reached more than 100 classes through `__subclasses__()`. PEP 578 says:
  "Sandboxing CPython has been attempted many times in the past, and each
  past attempt has failed" (P-S10). Test: CPython E8, E8b. **[tangential]**:
  this is a property of Python's object model, not of callbacks. Sand has no
  reflection, so the problem does not transfer.

**Verdict for Sand.** **[blocker]**. F-N5, F-N6, and the Sand results in
section 7 (E11, E12, E13) rule this design out as the general mechanism.
Section 8 keeps a restricted form for synchronous host entry.

### 5.3 Nested calls with continuations

Lua 5.4 (`lua_callk`, `lua_pcallk`, `lua_yieldk`), Luau
(`lua_pushcclosurek`), and wasmi resumable calls use this design.

- **F-K1. A continuation lets a callback yield across a native frame.**
  With `lua_call`, the yield failed. With `lua_callk`, the coroutine
  yielded, and on resume Lua ran the continuation. The manual explains
  that the native's "frame in the C stack was destroyed by the yield" and
  that Lua "calls a continuation function" instead (L-S5). Test: Lua E3.
  **[necessary]**: this confirms that resumable callbacks need a
  continuation that the VM, not the Rust stack, holds.
- **F-K2. wasmi resumes only the outermost host frame.** A host trap two
  levels deep surfaced at the outer frame, and the inner state was lost.
  Source: W-S9. This test was run by our WebAssembly research agent; we did
  not repeat it. **[tangential]**

**Verdict.** In C, this design is a nested call that the VM can abandon
with `longjmp` and resume through a continuation. Safe Rust has no
`longjmp`, so in Sand a continuation-based native must return to the VM.
That is design 5.4. **[tangential]** as a separate design for Sand.

### 5.4 Stackless continuations (trampoline)

The native returns a request. The VM calls the script function on the
script's own stack, then resumes the native's continuation. piccolo and
Sand P2 use this design. Erlang's `enif_schedule_nif` is the tail-only
form.

- **F-T1. Depth does not use the Rust stack.** piccolo recursed through a
  native to 1,000,000 levels (R-S1: "It does not rely on the rust stack").
  Sand P2 reached 1,000 levels, then stopped at the VM's own
  `max_call_depth` with "limit: stack overflow". It did not abort. Tests:
  `piccolo_cb` E2, `sand_p2` E2. **[necessary]**
- **F-T2. A callback can suspend.** In piccolo, a yield inside a callback
  reached the coroutine's resumer and came back with the value (`42`).
  In Sand P2, a callback slept 10 ms while another task ran three ticks,
  and returned `300`. Tests: `piccolo_cb` E3, `sand_p2` E3. **[necessary]**:
  this keeps async I/O by default.
- **F-T3. The host gets control back after each fuel slice.** piccolo ran
  an infinite loop inside a callback for 1,000 slices of 10,000 fuel, and
  the host got control after each slice. Sand P2 stopped the same loop with
  "instruction limit exceeded", and stopped an allocating loop with
  "memory limit exceeded". Tests: `piccolo_cb` E4, `sand_p2` E4, E5.
  **[necessary]**
- **F-T4. Stackless code is harder to write.** piccolo's README says:
  "writing things as a `Sequence` implementation is much more difficult
  than writing in normal, straight control flow" (R-S1). In Sand, our sort
  with a Sand comparator needs 58 lines in P2: a 40-line merge-sort state
  machine plus the registration. In P1 it needs 19 lines, because P1 can
  call Rust's `sort_by`. Test: line counts of `sandlab/src/bin/sand_*.rs`
  (section 7). **[nice to have]** to fix, through the async adapter in
  section 8.
- **F-T5. piccolo still has `unsafe` code.** Its README says "there are a
  few sources of unsafety, but crucially these sources of unsafety are
  *isolated*", and its GC library has an "unsafe `Collect` trait" (R-S1).
  **[tangential]**: Sand P2 needs no `unsafe`. The patch compiles under
  `#![forbid(unsafe_code)]`.

**Verdict.** **[necessary]**. This is the only design that keeps every
hard constraint and async I/O. Section 7 gives the Sand measurements.

### 5.5 Separate native stacks (fibers and stack switching)

Wasmtime's async API, Python's greenlet, and the WebAssembly
stack-switching and JSPI proposals use this design.

- **F-F1. Fibers let a nested callback suspend.** In wasmtime, a guest
  called an async host function, which called back into the guest, which
  called an async host function that slept 5 ms. The call returned `42`
  in 6.2 ms. A blocking `call` on the same store failed with "store
  configuration requires that `*_async` functions are used instead". Test:
  `wasmtime_async_cb` E3, E3b. **[nice to have]** as a precedent.
- **F-F2. Fibers need per-architecture assembly and `unsafe`.**
  Wasmtime's fiber crate has stack-switch code for each CPU. For any other
  target, wasm32 included, it says "No support for this platform" and
  fails at run time (W-S5). greenlet copies C stack slices to the heap and
  warns about "native (C) frames" (P-S7). Test: `verify_sources.py` and
  the wasm32 build in section 7.5. **[blocker]**: this breaks the safe-Rust
  rule and the web target.
- **F-F3. JSPI suspends only WebAssembly frames, and it warns about
  re-entrancy.** **[source-only]** It notes "Not all applications can equally tolerate being
  reentrant in this way" (W-S10). **[tangential]**

**Verdict.** **[blocker]**.

### 5.6 Component Model re-entrance rules

- **F-C1. The Component Model at first forbade re-entrance, then allowed
  it.**
  - The 2024 spec said the `may_enter` guard "ensures that components
    cannot be externally reentered" (W-S7).
  - The current spec says the callee "may **reenter** the caller's
    component instance", and adds that a host "must be careful to only
    perform recursive reentrance on components when explicitly supported
    by the component's documented API" (W-S8).
  - Wasmtime 50 now traps only after a trap has poisoned the instance
    (W-S8c).

  Test: `wasmtime_async_cb` E10. Our host re-entered a component during an
  import call, and the call returned `Ok(())`. **[nice to have]**: a
  standards body chose to allow re-entrance and to document it per API.
  Sand can do the same: each native that calls back states its behavior.

## 6. Sorting with a script comparator, across runtimes

This workload sorts 200,000 pseudo-random ints with the same generator in
each language. Times are medians of 5 runs.

| Runtime and method                                    |   Time |
| ----------------------------------------------------- | -----: |
| Lua 5.4.8, `table.sort` with a Lua comparator         | 150 ms |
| Lua 5.4.8, `table.sort`                               |  71 ms |
| Lua 5.5.0, `table.sort` with a Lua comparator         | 117 ms |
| Lua 5.5.0, `table.sort`                               |  55 ms |
| CPython 3.13, `sort(key=cmp_to_key(comparator))`      | 282 ms |
| CPython 3.13, `sort(key=lambda)`                      |  65 ms |
| CPython 3.14, `sort(key=cmp_to_key(comparator))`      | 276 ms |
| CPython 3.14, `sort(key=lambda)`                      |  58 ms |
| Sand, merge sort written in Sand, Sand comparator     | 221 ms |
| Sand P2, native merge sort, Sand comparator           | 401 ms |
| Sand P1, Rust `sort_by`, nested Sand comparator       | 521 ms |
| Sand, `sort_by(key)` (keys first, then native sort)   |  81 ms |
| Sand, `sort()`                                        |  30 ms |

Sources: `sort_lua-*.txt`, `sort_python-*.txt`, `sort_sand.txt`,
`sand_p2.txt`, `sand_p1.txt`.

- **F-X1. In Sand, a higher-order function written in Sand is faster than a
  native that calls back.** The Sand merge sort beat both prototypes.
  A P2 callback costs about 74 ns, and a Sand call through a function value
  costs about 32 ns (`sand_p2` E1). **[necessary]**: keep higher-order
  library code in Sand. Use callbacks where the host must call script
  code.
- **F-X2. The cheapest sort APIs avoid a callback per comparison.** In
  every runtime, a key function or no callback was 2 to 5 times faster
  than a comparator. **[nice to have]**: Sand's `sort_by(key)` already
  follows this pattern.

## 7. Sand prototypes

The patch is `prototypes/callbacks-p1-p2.patch`. It changes
`src/vm/mod.rs`, `src/vm/interp.rs`, and `src/vm/sched.rs`. P2 adds about
120 lines to the VM, and P1 adds about 70. The tests are in
`sandlab/src/bin/`.

### 7.1 How the prototypes work

**P1, nested calls.** `Vm::call_nested(f, args)` creates a fiber for the
callback and runs the interpreter on it until the callback returns. The
call is on the Rust stack, under the native's frame.

- If the callback suspends, P1 returns an error.
- P1 bounds nesting at 200 levels, as Lua does.
- With the `nested_callbacks` feature, `call_native` moves the caller's
  fiber to a list that the GC scans. Without the feature, the GC cannot
  see the caller's stack while a callback runs.

**P2, stackless continuations.** A native returns
`Err(Ctl::call(f, args, state, then))`.

1. The VM pushes a `Continuation` on the current fiber.
2. The VM pushes a frame for `f` above the live registers.
3. The callback runs as ordinary Sand code on the same task. It can call
   natives, allocate, and suspend.
4. On return, the VM calls `then(vm, result, state)`. The continuation
   returns the native's result, or calls `Ctl::call_again` for the next
   callback.

The GC roots `state`. The only change to the interpreter's hot path is one
comparison in the return sequence.

### 7.2 Results

Each item names the test, then gives the P1 result and the P2 result.

- **E1, cost of one callback.**
  - P1: 106.6 ns.
  - P2: 74.2 ns.
  - For comparison, a Sand call through a function value costs 32 ns.
  - **[necessary]**: P2 is faster.
- **E3, a callback that sleeps 10 ms while another task runs.**
  - P1 fails: "a native callback cannot suspend".
  - P2 returns `300`.
  - **[blocker]** for P1, because Sand does async I/O by default.
- **E2, native↔script recursion.**
  - P1 fails at 200 levels.
  - P2 reaches 1,000 levels and stops at `max_call_depth` with a clean
    error.
  - **[necessary]**
- **E4, an infinite loop in a callback.** P1 and P2 both stop it with
  "instruction limit exceeded". **[necessary]**
- **E5, unbounded allocation in a callback.** P2 stops it with "memory
  limit exceeded". **[necessary]**
- **E11, the caller's locals after a GC inside a callback.**
  - P1 without parking: the caller's list was freed and its slot was
    reused. The script read another object's data: "index 49, length 1".
  - P1 with parking: correct.
  - P2 under `gc_stress`: correct.
  - **[blocker] [hazard]** for P1 without parking.
- **E12, a comparator that empties the list under `gc_stress`.**
  - P1 copies element handles into a Rust `Vec`, which is not a GC root.
    The GC tracer panicked with "index out of bounds", and the session
    reset.
  - P2 keeps its handles in rooted `state`, and the sort stays correct.
  - **[blocker] [hazard]** for P1.
- **E13, a callback side effect, then a retried memory check.**
  - P1 ran the callback 2 times, because the engine re-ran the native.
  - P2 ran it once.
  - **[blocker]** for P1 under Sand's retry rule (F-S5).
- **E14, cancelling a task during a suspended callback.** P2 ran the
  task's `defer`, then reused fibers correctly. **[necessary]**
- **E6, sort 200,000 ints.**
  - P1: 521 ms.
  - P2: 401 ms.
  - **[nice to have]**
- **E6c, an inconsistent comparator.** P1 passed it to Rust's `sort_by`,
  which returned without a panic. **[tangential]**
- **Native source size for a comparator sort.**
  - P1: 19 lines.
  - P2: 58 lines.
  - **[nice to have]** to reduce (F-T4).

Raw output: `lab/results/sand_p1.txt`, `sand_p1_noparking.txt`, and
`sand_p2.txt`.

### 7.3 Cost to programs that do not use callbacks

We ran the `bench/` programs suite (18 programs) with three `sand`
binaries: unpatched, P2, and P2 with P1's parking. There were 3
interleaved rounds of 5 runs each (`lab/overhead.sh`). An earlier pass with
2 rounds gave the same conclusions (`overhead_summary_first_pass.txt`). Each binary's time
for a program is the best of its per-round medians.

- **F-O1. P2 costs nothing measurable.** The geomean P2/base ratio is
  0.994 (first pass: 0.993). **[necessary]**
- **F-O2. P1's parking costs a few percent.** The geomean is 1.070 (first pass:
  1.033). One program, `fannkuch`, is 40% slower in both passes (1.401
  and 1.482), which is above the noise.
  Parking moves the fiber on every native call. **[nice to have]**: if
  P1 is ever used, park only for natives that can call back.
- **Noise.** One program's baseline varied by up to 17% between
  rounds (first pass: 23%), so ratios for single programs are not significant.

Full table: `lab/results/overhead_summary.txt`.

### 7.4 Lifetime-branded handles

- **F-L1. Rust's borrow checker can stop a native from holding a raw
  handle across a call that can collect garbage.** In the E15 model, `Val<'vm>`
  borrows the VM, and a nested call needs `&mut Vm`. rustc rejected a native
  that held a `Val` across the call (`error[E0502]: cannot borrow *vm as
  mutable because it is also borrowed as immutable`). A native that rooted
  the value first, as a `Handle`, compiled and read the right value. Test:
  `lab/e15/`, `results/e15.txt`. **[necessary]** if Sand ever runs
  callbacks while a native is on the Rust stack (section 8.3). gc-arena,
  which piccolo uses, applies the same idea with its `'gc` lifetime (R-S1).
- **F-L2. Today's `Val` is `Copy` and has no lifetime.** So the compiler
  cannot catch E12 in P1. Source: `src/native/ty.rs`. Test: E12.
  **[necessary] [hazard]**

### 7.5 Platforms

- **F-P1. P2 builds for the web.** With the patch applied, `sandlang-wasm`
  built for `wasm32-unknown-unknown`. Test: `results/wasm32_sandlang_p2.txt`.
  **[necessary]**
- **F-P2. P2 keeps Sand free of `unsafe`.** With the patch applied, Sand's
  `src/` has 0 `unsafe` blocks, functions, or impls, and `lib.rs` keeps
  `#![forbid(unsafe_code)]`. Every other Rust runtime that we tested has
  `unsafe` code: Rhai 4 sites, piccolo 21 (plus 119 in gc-arena), wasmi 290,
  Rune 219, mlua 435, rquickjs 632, and starlark-rust 781. Test:
  `lab/unsafe_count.sh`, `results/unsafe_count.txt`. **[necessary]**
- **F-P3. Wasmtime's async API compiles for wasm32, but its fibers fail at
  run time there.** The fiber crate selects its "unsupported" module for any
  CPU without stack-switch assembly. It defers the error "to happen at
  runtime when a fiber is created", and then fails with "fibers unsupported
  on this host architecture" (W-S5, W-S5b). Our wasm32 build of an async
  wasmtime call succeeded. We could not run it, because no_std wasmtime
  needs platform hooks that this test did not supply. Test:
  `results/wasm32_wasmtime_fiber.txt`. **[blocker] [source-only]** for
  fibers on the web.
- **F-P4. The patch passes Sand's test suite.** TESTS_TEXT

## 8. Recommendation

### 8.1 The two directions of a callback

A UI binding needs two kinds of call into Sand.

1. **Sand → native → Sand.** A script calls a native, and the native calls a
   script function before it returns. Examples are a comparator, or a
   builder that the script passes to a native view constructor. The caller
   of the native is a Sand task. **Use P2 continuations.** **[necessary]**
2. **Host → Sand.** The UI framework calls a script function at a time that
   it chooses. Examples are SwiftUI `body`, a Compose `measure`, and a
   button action. The caller is Swift or Kotlin code on the UI thread, not
   a Sand task. P2 cannot serve this direction, because no Sand frame is
   waiting. **[necessary]**:
   - **Async entry:** run the handler as a task, as `Vm::spawn_task` does
     today (F-S3). Use it for event handlers.
   - **Sync entry:** run the handler to completion and return its value.
     If the handler suspends, fail with a clear error. The host can then
     fall back to an async update, as SwiftUI's `task(_:)` does. This
     matches `invokeSync` and `NativeCallable.isolateLocal` (F-U4).

### 8.2 Work items

1. **[necessary]** Land P2. Before it ships, these gaps need work:
   - Allow a continuation to return `Async`, `NeedPermission`, and
     `Yield`.
   - Unwind continuations correctly through `defer` and cancellation. E14
     passes, but the prototype clears continuations only when a fiber is
     recycled.
   - Allow native callees, not only Sand closures.
   - Show native frames in stack traces.
   - Remove the `Vec` and `Box` allocations on each callback to approach
     the 32 ns cost of a Sand call.
2. **[necessary]** Add rooted handles that a host can keep, and that the
   host must release (F-S4). Today, the VM's `pinned` list is a root set
   that a host could use.
3. **[necessary]** Add the async and sync host entry points of 8.1.
4. **[necessary]** Document re-entrancy behavior for each native that calls
   back (F-N10, F-N11, F-C1). Use snapshot semantics for collections, as
   P2's sort does, and guard `NativeObject` borrows.
5. **[necessary]** Keep higher-order library functions in Sand (F-X1).
6. **[nice to have]** An adapter that turns a Rust `async` block into a P2
   continuation, so that `cx.call(f, args).await` replaces the state
   machine. The adapter must give the block VM access only between
   awaits. We did not build it; its cost and safety are untested.
7. **[blocker]** Do not add P1 as the general mechanism.
8. **[blocker]** Do not use fibers or stack switching.

### 8.3 Synchronous host entry while a native runs

A rare case remains. A Sand native can call a host API that calls back into
Sand before it returns, for example a synchronous layout pass. That is a
host → Sand sync entry while a native is on the Rust stack. It needs P1's
mechanics:

- parking the active fiber (E11);
- a nesting bound (F-N4);
- a ban on suspension (F-N6).

It also needs protection against E12. Section 7.4 shows that lifetimes can
enforce that protection at compile time. **[nice to have]** until a UI
binding needs it.

## 9. Limits of this research

- We could not run SwiftUI, UIKit, Compose, Flutter, or JavaScriptCore on
  this Linux machine. Facts about them are **[source-only]**.
- We did not run starlark-go, Wren, Erlang, LuaJIT, or Luau. Their facts
  are source checks only.
- The WebAssembly agent's resumable-call results for wasmi (F-K2) were not
  repeated here.
- The P2 prototype handles only the cases in section 7.2. Section 8.2 lists
  the gaps.
- The P2 merge sort is our own code. A tuned state machine could be
  faster, but it would also be longer.

## 10. Experiment index

All paths are under `lab/` unless noted.

| ID         | Question                                       | Programs and results                 |
| ---------- | ---------------------------------------------- | ------------------------------------ |
| E1         | Cost of a callback vs a script call            | each runtime, `results/*.txt`        |
| E2, E2b    | Native↔script recursion depth and its failure  | each runtime; `*_deep`, `*-depth`    |
| E3, E3b    | Can a callback suspend on I/O?                 | each runtime                         |
| E4         | Fuel or instruction limit inside a callback    | each runtime; `rune_cb_budget.txt`   |
| E5         | Memory limit inside a callback                 | Lua, QuickJS, Sand P2                |
| E6, E6b, c | Sort with a comparator; mutation; consistency  | `sort/`, Lua, CPython, Sand          |
| E7         | Host keeps a handler and calls it later        | Lua                                  |
| E8, E8b    | Callback isolation in CPython                  | `py/cb.py`                           |
| E9         | Re-entrant Rust callback guards                | `src/bin/mlua_cb.rs`                 |
| E10        | Component Model re-entrance                    | `src/bin/wasmtime_async_cb.rs`       |
| E11        | Caller's locals after a GC in a callback       | `../sandlab/src/bin/sand_*.rs`       |
| E12        | Native's own handles after a GC in a callback  | `../sandlab/src/bin/sand_nested.rs`  |
| E13        | Retry of a native after a callback ran         | `../sandlab/src/bin/sand_*.rs`       |
| E14        | Cancel a task during a suspended callback      | `../sandlab/src/bin/sand_stackless.rs` |
| E15        | Compile-time ban on handles across a callback  | `e15/`                               |
| E-S1, E-S3 | Sand today: deferred callback; kept handle     | `../sandlab/src/bin/sand_current.rs` |
| Overhead   | Prototype cost on `bench/` programs            | `overhead.sh`, `results/overhead/`   |
| Sources    | Every quote present in its source              | `verify_sources.py`                  |
