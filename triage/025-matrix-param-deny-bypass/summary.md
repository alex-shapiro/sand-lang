# `deny http host/admin/**` does not block `/admin;x/secret` or `/public/..;/admin/secret`

TODO item 025 (Policy): "Evaluate HTTP paths that contain `;` parameters both with…"

**Pain.** I deny an admin area on a server and allow the rest. A script adds a `;` parameter to a path segment, and the request gets through. Tomcat, Spring, and Jetty remove `;...` from each segment before routing, so the server returns the denied page.
**Why it matters.** This is a deny bypass. The policy and `sand lint` both report no problem, so I get no warning.

**Scope.** The bypass occurs only on servers that read `;` parameters and `..;` as Tomcat does. A Sand server reads `..;` as a normal segment, the same as the policy, so pure Sand programs are not affected.

**Reproduce.** Start `python3 -I server.py 18931` in an empty directory. This server removes `;...` from each segment, as Tomcat does. Then run:

    sand run repro.sand --policy=repro.policy --no-prompt

    /admin/secret -> permission_denied: permission denied: GET http://127.0.0.1:18931/admin/secret
    /admin;x/secret -> SECRET (raw /admin;x/secret -> /admin/secret)
    /public/..;/admin/secret -> SECRET (raw /public/..;/admin/secret -> /admin/secret)

The policy sees the segments `admin;x` and `..;` as literal names, so the deny rule does not match.

**Second part (small impact).** `%25FF` and the raw byte `%FF` decode to the same text, so `allow http host/%25FF` also allows `/%FF`, which is a different path on the server:

    sand run pct-repro.sand --policy=pct-repro.policy --no-prompt
    /%25FF -> public ...
    /%FF -> public ...        (allowed, but I did not write a rule for it)
    /%FE -> permission_denied ...

**Aspirational.** With the same `repro.policy`, all three requests fail with `permission_denied`. The policy evaluates each path with and without `;` parameters and applies the stricter decision, as it already does for `%2F`.
