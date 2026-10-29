# Windows `rename` junction race

`rename` in `src/files/windows.rs` checks for directory junctions before and
after calling `std::fs::rename`, which leaves a race window during the call. If
a second local process converts an empty destination directory into a junction
during this window, `std::fs::rename` follows the junction and moves the file
outside the sandbox before the post-check returns an error.

`rename` resolves both paths, opens every directory on both paths without
`FILE_SHARE_DELETE`, verifies that each parent directory is not a symlink or
junction, calls `std::fs::rename` by path, and verifies the parents again. An
open handle prevents another process from deleting or replacing a directory,
but another process can still set a junction reparse point on an empty
directory while the handle stays open.

Attack sequence with a Sand script confined by `allow write C:/work/in C:/work/out`
and a second local process that has write access to `C:/work/out`:

1. `C:/work/out` is an empty directory. The path is resolved, the policy allows
   the destination, and `rename` holds the directory open.
2. Between the pre-check and `std::fs::rename`, the second process sets a
   junction reparse point on `C:/work/out` that targets `C:/Users`.
3. `std::fs::rename` renames by path, follows the junction, and writes
   `item.txt` under `C:/Users`, outside `C:/work`.
4. The post-check detects the junction and returns an error after the file has
   already moved.

The same race applies to the source parent directory, which allows a process to
rename an external file into the sandbox. Windows lacks a safe Rust API to
rename files by handle, so the final step takes a path and re-resolves the
junction.
