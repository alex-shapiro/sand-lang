# Delegating `Capability::NetAddr` to `RulePolicy::check` inside a `CustomPolicy` bypasses HTTP IP rules

`Capability::NetAddr` omits the originating HTTP method and URL, so
`RulePolicy::check(&Capability::NetAddr { .. })` evaluates resolved IP
addresses only against `net` rules and ignores `http` rules. If a host wraps a
`RulePolicy` inside a `CustomPolicy`, requests to hostnames that are resolved
to denied HTTP IP paths bypass those `deny http` rules.

Start `../025-matrix-param-deny-bypass/server.py` on port 18931 and run
`repro.rs`:

```
RulePolicy directly:
  GET localhost/admin/secret -> permission_denied: ... address 127.0.0.1 is not allowed by the rule `deny http 127.0.0.1/admin`
Same RulePolicy behind an audit-logging CustomPolicy:
  audit: GET http://localhost:18931/admin/secret -> Allow
  audit: connect to localhost at 127.0.0.1:18931 (plaintext) -> Allow
  GET localhost/admin/secret -> SECRET (raw /admin/secret -> /admin/secret)
```

`Capability::NetAddr` should include the originating HTTP method and URL so
that `RulePolicy::check` and `CustomPolicy` evaluate resolved addresses with
full request context.
