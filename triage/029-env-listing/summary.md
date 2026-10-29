# A single `deny env` rule causes `env.vars()` to return an empty map under `allow env *`

`env.vars()` checks `Capability::Env("*")`, which fails if the policy contains
any `deny` or `prompt` rule on an individual environment variable. If a policy
specifies `allow env *` and `deny env AWS_SECRET_ACCESS_KEY`, `env.get("HOME")`
succeeds, but `env.vars()` returns `{}` without an error or prompt. In
addition, `rule_for(&Capability::Env("*"))` suggests `allow env *`, which does
not enable `env.vars()` if a `deny` rule is present.

Run:

```
$ AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --policy=repro.policy --no-prompt
HOME = Some("/Users/alex")
secret = None
env.vars() has 0 entries

$ AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --allow='env *' --no-prompt
env.vars() has 62 entries
```

`env.vars()` should use a dedicated listing capability or filter out individual
variables that `env.get` denies, and `rule_for(&Capability::Env("*"))` should
not suggest a rule that fails to grant the capability.
