# Wrapping a `RulePolicy` in a logging `CustomPolicy` silently removes path-scoped DNS-rebinding protection

TODO item 027 (Policy): "Include the originating HTTP method and URL on `Capability::NetAddr`…"

**Pain.** As an embedder, I wrap my `RulePolicy` in a `CustomPolicy` that logs each decision and then calls `rules.check(cap)`. With the policy `allow http plaintext GET *` + `deny http 127.0.0.1/admin/**` + `deny http [::1]/admin/**`, a script reads `http://localhost:18931/admin/secret` through the wrapper. Without the wrapper, the same policy blocks the request.
**Why it matters.** This is a sandbox escape for a common embedding pattern (audit or metrics wrapper). The wrapper cannot fix it: `NetAddr` carries no method or path, so `RulePolicy::check(NetAddr)` checks the address as a plain `net` connection and ignores the `http` rules. For the same reason, a custom policy cannot write a rule such as "deny POST to internal addresses".

**Reproduce.** `repro.rs`:

    RulePolicy directly:
      GET localhost/admin/secret -> permission_denied: ... address 127.0.0.1 is not allowed by the rule `deny http 127.0.0.1/admin`
    Same RulePolicy behind an audit-logging CustomPolicy:
      audit: GET http://localhost:18931/admin/secret -> Allow
      audit: connect to localhost at 127.0.0.1:18931 (plaintext) -> Allow
      GET localhost/admin/secret -> SECRET (raw /admin/secret -> /admin/secret)

**Aspirational.** `NetAddr` includes the method and URL. `rules.check(&NetAddr { .. })` then returns the same decision as the engine's own stage-2 check, so the wrapper denies this request.
