# `deny env "AWS_*"` leaks AWS secrets, and `sand lint` says nothing

TODO item 032 (Policy): "Expand `lint_policy` to detect shadowed and redundant `env`, `stdin`…"

**Pain.** I write `deny env "AWS_*"` to keep cloud credentials out. The quotes make `*` a literal character, so the rule matches only a variable named `AWS_*`, and the script reads `AWS_SECRET_ACCESS_KEY`. In the same way, `deny net localhost` silently overrides `allow net *.localhost`, so my dev servers are unreachable. `sand lint` warns about neither.
**Why it matters.** For `env`, `stdin`, `stdout`, `stderr`, `timezone`, `custom`, and `listen` rules, the linter reports only exact duplicates. Shell users and LLMs often quote wildcards, and here the result is a credential leak.

**Reproduce.**

    sand lint repro.policy localhost.policy        # no output, exit 0
    AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --policy=repro.policy --no-prompt
    HOME: /Users/alex
    AWS_SECRET_ACCESS_KEY: hunter2                 # with unquoted AWS_*: <denied>
    sand run localhost.sand --policy=localhost.policy --no-prompt
    app.localhost:8080: permission_denied: permission denied: connect to app.localhost:8080 (plaintext)

**Aspirational.** Lint warns that `"AWS_*"` matches only the literal name and suggests `AWS_*`. It reports `allow net *.localhost` as `overridden-rule`.
