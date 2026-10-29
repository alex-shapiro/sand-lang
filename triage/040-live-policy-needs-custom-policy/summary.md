# To change rules while an engine runs, a host must give up part of `RulePolicy`'s protection

TODO item 040 (Policy): "Support shared, dynamically updatable `RulePolicy` instances and remove…"

**Pain.** My app lets the user change permissions while a long session runs. `Config::policy` fixes the rules when I create the engine, so I wrap `Arc<RwLock<RulePolicy>>` in a `CustomPolicy`. With the same rules (`allow http plaintext *`, `deny http 127.0.0.1/admin [::1]/admin`), the wrapper lets `http://localhost:9/admin` through, while `Config::policy` blocks it.
**Why it matters.** Hosts that need live updates (REPL sessions, agents) must use the wrapper, and it loses the address checks that depend on the request (see `triage/027-netaddr-missing-request-context`). Nothing warns the host about this.

**Reproduce.** `repro.rs`:

    Config::policy(rules):        permission_denied: … address 127.0.0.1 is not allowed by the rule `deny http 127.0.0.1/admin` on line 2
    custom_policy(shared rules):  io: Connection refused (os error 61)    # reached 127.0.0.1/admin

**Aspirational.** A shared handle such as `SharedPolicy::new(rules)`, passed to `Config::policy`, that the host can `replace(rules)` or `extend(rule)` while the engine runs, with the full `RulePolicy` behaviour.
