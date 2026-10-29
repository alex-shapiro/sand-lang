# `deny http LOCALHOST 127.0.0.1` blocks nothing, and `allow http purge HOST` allows every method

TODO item 036 (Policy): "Warn in `lint_policy` if an uppercase hostname in an `http` rule is…"

**Pain.** Under `allow http plaintext *`, I add `deny http LOCALHOST 127.0.0.1`. The parser reads `LOCALHOST` as an HTTP method, so the rule denies only `LOCALHOST` requests, and `GET http://localhost/admin` and `GET http://127.0.0.1/admin` both go through. In the other direction, `allow http plaintext purge localhost:9/cache` reads `purge` as a hostname and allows DELETE. `sand lint` warns about neither.
**Why it matters.** One uppercase or lowercase word silently changes what a rule means. One case blocks nothing and the other allows more methods than intended.

**Reproduce.**

    sand lint repro.policy method.policy           # no output, exit 0
    sand run repro.sand --policy=repro.policy --no-prompt
    http://localhost:9/admin: io: Connection refused (os error 61)    # reached the network
    http://127.0.0.1:9/admin: io: Connection refused (os error 61)
    # with lowercase `localhost`: both are permission_denied
    sand run method.sand --policy=method.policy --no-prompt
    DELETE: io: Connection refused (os error 61)   # with `PURGE`: DELETE is permission_denied

**Aspirational.** Lint warns: "`LOCALHOST` is parsed as an HTTP method; write `localhost` for the host", and "`purge` is parsed as a hostname; write `PURGE` for the method".
