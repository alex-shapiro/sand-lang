# `sand lint` misses quoted wildcards in `env` rules and `*.localhost` rules overridden by `deny net localhost`

Quoting a pattern in a policy file turns `*` into a literal character, so
`deny env "AWS_*"` matches only the literal variable `AWS_*` and allows the
script to read `AWS_SECRET_ACCESS_KEY`. Similarly, `deny net localhost`
overrides `allow net *.localhost` at runtime. `lint_policy` reports neither
issue, and for `env`, `stdin`, `stdout`, `stderr`, `timezone`, `custom`, and
`listen` rules it checks only for exact duplicates.

Run:

```
$ sand lint repro.policy localhost.policy
$ AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --policy=repro.policy --no-prompt
HOME: /Users/alex
AWS_SECRET_ACCESS_KEY: hunter2
$ sand run localhost.sand --policy=localhost.policy --no-prompt
app.localhost:8080: permission_denied: permission denied: connect to app.localhost:8080 (plaintext)
```

`lint_policy` should warn if a quoted literal contains `*` and report shadowed
and redundant rules across all rule kinds, including `*.localhost` rules
overridden by `localhost`.
