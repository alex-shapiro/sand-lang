# A Windows rename can place a file outside the sandbox when a parent folder is turned into a junction mid-call

TODO item 002 (Sandbox): "Windows rename. Windows does not provide a safe API to rename files…"

**Pain.** On Windows, a script confined to `allow write C:/work/...` can still
move a file outside that area. If another local process converts the empty
destination folder into a junction during the `rename` call, `std::fs::rename`
follows the junction and writes through it. The call then reports an error,
but the file already left the sandbox.

**Why it matters.** This is a sandbox escape: the policy says one tree is
writable, yet a write lands elsewhere. A runtime that sells itself as safe for
untrusted code must not let a confined script reach outside its granted paths,
even under a race.

**Scope.** The escape needs a second process outside the sandbox that converts the folder into a junction. Sand code cannot make junctions or symlinks, so a script cannot do this attack alone.

**Reproduce.** Not executed; the triage host is macOS. The scenario, actors,
and the exact window are in `attack.md`, derived from `src/files/windows.rs`
(the `rename` function at line 839 and the module comment at lines 17-22, which
state the race is detected after the fact, not prevented). `repro.sand` and
`repro.policy` are the script and policy to run on Windows beside the helper
process described in `attack.md`.

**Aspirational.** Prevent rather than detect: rename only when both parents can
be pinned against junction conversion (open with a share mode that blocks the
reparse set, or verify by handle after an atomic exchange). If safe Rust cannot
guarantee this on Windows, document `rename` as best-effort there so hosts do
not rely on it for confinement.
