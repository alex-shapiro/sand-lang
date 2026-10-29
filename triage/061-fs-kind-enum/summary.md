# Two booleans for the entry kind let me treat a FIFO as a directory without a warning

TODO item 061 (Language): "Evaluate replacing `is_file` and `is_dir` on `fs.Metadata`…"

**Pain.** `fs.Metadata` has `is_file` and `is_dir`. The natural walk `if m.is_file { … } else { walk(path)? }` treats every entry that is not a file as a directory. A FIFO, socket, or device is neither, so the walk calls `list_dir` on it and the whole script fails. Nothing in the compiler or linter tells me that a third case exists.

**Why it matters.** Directory walks are a common script task. With an `fs.Kind` enum, `match` forces me to handle the third case.

**Reproduce.**
```
./setup.sh                                          # makes tree/ with a file, a dir, a FIFO
sand run bug.sand --allow='read ./' --no-prompt
file tree/a.txt
error: io: tree/pipe: Not a directory (os error 20)
sand run repro.sand --allow='read ./' --no-prompt   # workaround: check both flags, add an else
file tree/a.txt
skip tree/pipe
```

**Aspirational.** `aspirational.sand` matches on `m.kind` (`File`, `Dir`, `Other`). Today: `error: no field 'kind' on type Metadata`.
