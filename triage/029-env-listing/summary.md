# With `allow env *` and one `deny env` rule, `env.vars()` silently returns an empty map

TODO item 029 (Policy): "Separate the `env.vars()` listing capability from `Capability::Env(\"*\")`…"

**Pain.** I want the script to see my whole environment except one secret, so I write `allow env *` and `deny env AWS_SECRET_ACCESS_KEY`. `env.get` works as expected, but `env.vars()` returns `{}` with no message and no prompt. One deny rule disables the listing completely, and no rule can turn it back on. The CLI hint and `rule_for` suggest `allow env *` for a denied listing, and that rule is already in my policy.
**Why it matters.** `env.vars()` is a common idiom (dump the config, find all `APP_*` variables). An empty map looks like a valid result, so scripts continue with wrong data instead of failing.

**Reproduce.**

    AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --policy=repro.policy --no-prompt
    HOME = Some("/Users/alex")
    secret = None
    env.vars() has 0 entries

    AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --allow='env *' --no-prompt
    env.vars() has 62 entries        (without the deny rule)

`sand lint repro.policy` reports no warning.

**Aspirational.** With the same policy, `env.vars()` returns all variables except `AWS_SECRET_ACCESS_KEY`. The listing filters out each variable that `env.get` would deny.
