# The race (reasoned, not executed; no Windows host available)

Actors:
- The Sand script, confined by `allow write C:/work/in C:/work/out`.
- A second local process with ordinary write access to `C:/work/out`.
  It is not malicious toward the host; it is the untrusted script's
  helper, or any program the host also runs.

The window. `rename` (src/files/windows.rs:839) resolves both paths, holds
every directory on both paths open without `FILE_SHARE_DELETE`, checks that
each held parent is not a symlink/junction, calls `std::fs::rename` by path,
then checks the parents again. Holding an open handle stops a directory from
being deleted or replaced, but an *empty* directory can still be converted
into a junction while a handle to it stays open: the conversion sets a
reparse point on the existing directory rather than replacing it.

The sequence:
1. `C:/work/out` is an empty, allowed folder. The walk resolves it, the
   policy allows the destination, and `rename` holds it open.
2. Between the "before" check and `std::fs::rename`, the second process sets
   a junction reparse point on `C:/work/out` pointing at, say, `C:/Users`.
3. `std::fs::rename` renames by path, so it follows the junction and writes
   `item.txt` under the junction target, outside `C:/work`.
4. The "after" check then notices the parent is now a junction and returns
   an error, but the file already moved. The module comment at line 20-22
   admits this: "detects this change after the operation instead of
   preventing it."

Result: a script allowed to write only `C:/work` plants a file outside it.
The reverse (renaming a victim file *into* the sandbox, or over a sensitive
path) is the same race with the source side.

Why it is hard to close in safe Rust: Windows has no by-handle rename in
safe Rust (`fs_at`, `winx`, `cap-std` all rename by path), so the final step
must take a path and re-resolves the junction the handle was meant to pin.

This is not executed: the triage host is macOS. The reasoning follows the
code and the module's own documentation.
