# `sand lint` does not report `listen` or `env` rules that are overridden by broader or equal-specificity rules

In `repro.policy`, `deny listen localhost:8080` has the same specificity as
`allow listen 127.0.0.1:8080`, so the `deny` rule wins and blocks the listener.
In `env.policy`, `deny env AWS_*` overrides `allow env AWS_*`. `sand lint`
reports neither rule as overridden, even though it reports equivalent overrides
for file and `net` rules.

Run:

```
$ sand lint repro.policy env.policy
$ sand run repro.sand --policy=repro.policy --no-prompt
127.0.0.1:8080: permission_denied: permission denied: listen on 127.0.0.1:8080
$ AWS_REGION=us-east-1 sand run env.sand --policy=env.policy --no-prompt
AWS_REGION: <denied>
```

`lint_policy` should report `env` and `listen` rules that are subsumed by
opposing rules of equal or higher precedence.
