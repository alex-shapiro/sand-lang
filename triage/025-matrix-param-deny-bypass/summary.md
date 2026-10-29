# HTTP matrix parameters bypass path-scoped `deny http` rules

`url_segments` in `src/policy/net.rs` treats `;` matrix parameters as literal
characters in path segments, so a script can bypass `deny http host/admin/**`
with `/admin;x/secret` or `/public/..;/admin/secret` against servers such as
Tomcat, Spring, and Jetty that strip `;` parameters before routing. In
addition, `percent_decode` decodes `%25FF` and `%FF` to the same string, so
`allow http host/%25FF` also allows `/%FF`.

Start `python3 -I server.py 18931` and run:

```
$ sand run repro.sand --policy=repro.policy --no-prompt
/admin/secret -> permission_denied: permission denied: GET http://127.0.0.1:18931/admin/secret
/admin;x/secret -> SECRET (raw /admin;x/secret -> /admin/secret)
/public/..;/admin/secret -> SECRET (raw /public/..;/admin/secret -> /admin/secret)

$ sand run pct-repro.sand --policy=pct-repro.policy --no-prompt
/%25FF -> public (raw /%25FF -> /%25FF)
/%FF -> public (raw /%FF -> /%FF)
/%FE -> permission_denied: permission denied: GET http://127.0.0.1:18931/%FE
```

`url_segments` should evaluate HTTP paths both with and without matrix
parameters and apply the stricter decision, and `percent_decode` should
distinguish raw non-UTF-8 bytes from percent-encoded `%25` sequences.
