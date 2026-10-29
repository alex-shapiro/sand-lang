# A misspelled error kind compiles, so my error handler silently never runs

TODO item 062 (Language): "Evaluate replacing string error kinds (`\"not_found\"`, `\"invalid\"`) with…"

**Pain.** To handle a missing file, I compare `e.kind` with a string. If I write `"notfound"`, `"NotFound"`, or `"not-found"` (the Rust, Python, or HTTP habit), `check` and `lint` pass and the fallback never runs. The script fails only when the file is really missing, often in production.

**Why it matters.** Error handling is the code that runs least often in testing, so a silent typo there is the most likely to ship. Kinds are also passed to `Error(..., kind:)` and `fail(..., kind:)`, where the same typo goes unseen.

**Reproduce.**
```
sand check bug.sand && sand lint bug.sand    # ok
sand run bug.sand --allow='read ./' --no-prompt
error: not_found: cannot read config: not_found: missing-config.json: ...
sand run repro.sand --allow='read ./' --no-prompt   # exact string "not_found"
{}
```

**Aspirational.** `aspirational.sand` compares with an enum variant, so a typo is a compile error. The enum needs a case for custom kinds (for example `Other(str)`), because scripts and native modules define their own kinds. A cheaper option: a lint for string literals compared with `Error.kind` that are not standard kinds.
