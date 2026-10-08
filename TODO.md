# TODO

Follow-up work, roughly in priority order.

## Sandbox hardening

These matter most when running untrusted scripts.

- [ ] **Meter native work.** CPU and instruction limits only count bytecode.
      A huge `sort`, regex match or `json.parse` runs to completion and blocks the
      engine's thread. Long-running natives should charge fuel, or check limits and
      yield partway through.
- [ ] **Bound compile time.** Type checking runs before any limit applies, and
      exhaustiveness checking can blow up on large or-patterns over tuples, so a
      hostile script can stall the host during compilation. Add a compile budget.
- [ ] **Check resolved addresses, not just hostnames.** `--allow-net=example.com`
      checks the name, but nothing stops that name from resolving to `127.0.0.1` or
      another internal address. Check the resolved IP, with an option to block
      private ranges.
- [ ] **Close filesystem check-then-use races.** Paths are canonicalized and
      checked, then opened, so a symlink swapped in between could slip through.
- [ ] **Check deny rules inside renamed directories.** Renaming a whole
      directory only checks the two top-level paths, not deny rules on its
      subdirectories.
- [ ] **Tighten memory accounting.** Buffers held by pending I/O and Rust-side
      scratch space aren't counted until they become values.

## Test coverage

- [ ] Tests for the `sand` CLI: flags, exit codes, the terminal prompter.
- [ ] TLS/HTTPS tests against a local TLS server (currently verified by hand
      against example.com only).
- [ ] Tests for permission prompts during `import`.
- [ ] Run on Windows (paths, `/dev/tty` prompter).
- [ ] Run the `cargo fuzz` targets in `fuzz/` (needs nightly).

## Tooling

- [ ] **Real REPL.** It currently re-runs the whole session for each input, so
      side effects repeat. Needs incremental checking with a persistent VM.
- [ ] **Linter precision.** `shadowed-variable` can flag same-named `let`s in
      sibling blocks; `unused-function` is name-based and misses unused methods.
- [ ] **Lazy `help` summaries.** Importing `help` compiles every stdlib module.

## Performance

- [ ] Reduce call overhead (about 35ns per call): leaner frames, fewer stack
      resizes.
- [ ] Incremental or generational GC; the current mark-sweep pauses grow with
      heap size.
- [ ] Avoid heap allocation for `Some(x)` and small enum payloads.
- [ ] Hoist loop-invariant constants in the compiler.

## Language and spec

- [ ] **User-defined interfaces/traits.** Generic code is limited to the
      built-in bounds (`Eq`, `Ord`, `Hash`, `Num`). This is the biggest gap in the
      language.
- [ ] Consider lazy iterators, labeled `break`, and `defer`-style resource
      cleanup.
- [ ] Reconcile `docs/SPEC.md` with implementation decisions:
  - `http.Request` has an extra `conn` field used by `respond`.
  - `test.property` uses a fixed seed.
  - `fs` acts on symlinks themselves (`remove_file`, `rename`), but policy
    checks also require permission on the link target.
- [ ] Document the `"{...}"` interpolation trap more prominently: JSON literals
      and regex quantifiers like `{8}` need raw strings (`r"..."`) or `\{`.
