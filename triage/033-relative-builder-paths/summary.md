# Relative paths in `RulePolicy` file builder methods fail to match and emit invalid policy text

`RulePolicy` file builder methods such as `allow_write("data")` and
`deny_read("data/secrets")` accept unanchored relative paths without resolving
or rejecting them. At runtime, the relative `allow_write` rule never matches
resolved paths, while `Display` emits unanchored path rules that
`PolicyFile::parse` rejects.

Create `/tmp/triage-policy2/app/data` and run `repro.rs`:

```
Display:
allow stdout
allow stderr
allow write data
deny read data/secrets
parse back, line 3: path `data` is not anchored: write `./data` for `/tmp/triage-policy2/app/data`, …
parse back, line 4: path `data/secrets` is not anchored: …
script in /tmp/triage-policy2/app: Err(Error(message: "permission denied: write data/out.txt", kind: "permission_denied"))
```

`RulePolicy` file builder methods should reject relative paths or resolve them
against an explicit base directory so that `Display` always emits valid policy
syntax.
