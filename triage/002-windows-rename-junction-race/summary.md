# Windows `rename` can move a file outside the sandbox if a parent directory becomes a junction during the call

On Windows, a script confined to `allow write C:/work/in C:/work/out` can move
a file outside `C:/work` if a second local process converts the empty
destination directory into a junction during `fs.rename`. `std::fs::rename`
follows the junction and writes the file to the junction target before the
post-check returns an error.

This race requires a second process outside the sandbox because Sand scripts
cannot create junctions or symlinks. See `attack.md` for the race sequence and
run `repro.sand` with `repro.policy` on Windows alongside a process that
toggles `C:/work/out` between an empty directory and a junction.

To prevent this escape, `rename` must block reparse point changes on open
parent directories or perform an atomic handle-based rename.
