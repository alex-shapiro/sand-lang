# `fs.Metadata` exposes `is_file` and `is_dir` booleans instead of an `fs.Kind` enum

`fs.Metadata` represents the entry type with two boolean fields, `is_file` and
`is_dir`. For a FIFO, socket, or device node, both fields are `false`, so a
directory walk that branches on `if m.is_file { ... } else { walk(path)? }`
treats special files as directories and fails at runtime.

Run:

```
$ ./setup.sh
$ sand run bug.sand --allow='read ./' --no-prompt
file tree/a.txt
error: io: tree/pipe: Not a directory (os error 20)

$ sand run repro.sand --allow='read ./' --no-prompt
file tree/a.txt
skip tree/pipe
```

Replacing `is_file` and `is_dir` with an `fs.Kind` enum with `File`, `Dir`, and
`Other` variants requires `match` expressions to handle non-file, non-directory
entries explicitly, as in `aspirational.sand`.
