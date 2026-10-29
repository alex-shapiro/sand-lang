# A narrow `allow listen` or `allow env` rule is dead, and `sand lint` does not say so

TODO item 043 (Tooling): "Report `env` and `listen` policy rules that are subsumed by broader rules."

**Pain.** I write `deny listen localhost:8080` and then `allow listen 127.0.0.1:8080` as an exception. `localhost` has the same specificity as an IP address, so the deny wins and my server cannot bind. In an env policy, a later `allow env AWS_*` loses a tie to an earlier `deny env AWS_*`. Lint reports neither.
**Why it matters.** The author thinks that the exception works. The linter already reports the same mistakes for file and `net` rules, so users expect it for `listen` and `env` rules too.

**Reproduce.**

    sand lint repro.policy env.policy              # no output, exit 0
    sand run repro.sand --policy=repro.policy --no-prompt
    127.0.0.1:8080: permission_denied: permission denied: listen on 127.0.0.1:8080
    AWS_REGION=us-east-1 sand run env.sand --policy=env.policy --no-prompt
    AWS_REGION: <denied>

**Aspirational.** `overridden-rule`: "`localhost:8080` on line 1 takes precedence everywhere this rule applies".
