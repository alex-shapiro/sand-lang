# Updating a `RulePolicy` while an engine runs requires `CustomPolicy`, which loses request-scoped address checks

`Config::policy` takes ownership of a static `RulePolicy` at engine creation.
To update rules dynamically during a session, a host must wrap a shared
`RulePolicy` in `Config::custom_policy`, which evaluates `Capability::NetAddr`
without the originating HTTP request context (see
`triage/027-netaddr-missing-request-context`) and bypasses path-scoped IP
rules.

Run `repro.rs`:

```
Config::policy(rules):        permission_denied: … address 127.0.0.1 is not allowed by the rule `deny http 127.0.0.1/admin` on line 2
custom_policy(shared rules):  io: Connection refused (os error 61)
```

Supporting a shared, dynamically updatable `RulePolicy` handle on `Config`
allows hosts to update rules at runtime while preserving full `RulePolicy`
evaluation.
