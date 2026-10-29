# A relative path in `RulePolicy::allow_write` silently never matches, and the policy text does not parse back

TODO item 033 (Policy): "Resolve or reject relative paths in `RulePolicy` file builder methods…"

**Pain.** As a host, I write `RulePolicy::standard().allow_write("data").deny_read("data/secrets")` and set `Config::cwd(app)`. The script's `fs.write_text("data/out.txt", …)` is denied, even when the process working directory is `app`. The relative `deny` rule does match, against the process working directory. When I save `policy.to_string()` and load it again, `PolicyFile::parse` rejects it.
**Why it matters.** The builder accepts the path without an error, and then `allow` and `deny` behave differently. A host that saves policies through `Display` cannot load them again.

**Reproduce.** `mkdir -p /tmp/triage-policy2/app/data`, then run `repro.rs`:

    Display:
    allow stdout
    allow stderr
    allow write data
    deny read data/secrets
    parse back, line 3: path `data` is not anchored: write `./data` for `/tmp/triage-policy2/app/data`, …
    parse back, line 4: path `data/secrets` is not anchored: …
    script in /tmp/triage-policy2/app: Err(Error(message: "permission denied: write data/out.txt", kind: "permission_denied"))

**Aspirational.** The builder rejects relative paths (panics like an empty path does), or resolves them against an explicit base. `Display` then always emits anchored paths.
