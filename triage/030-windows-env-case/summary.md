# On Windows, `deny env AWS_SECRET_ACCESS_KEY` does not block `env.get("aws_secret_access_key")`

TODO item 030 (Policy): "Fold environment variable names case-insensitively on Windows in…"

**Pain.** I write `allow env *` and `deny env AWS_SECRET_ACCESS_KEY`. On Windows, environment names are case-insensitive, so `env.get("aws_secret_access_key")` reads the same variable. The policy compares names case-sensitively and allows the lowercase name, so the script gets the secret.
**Why it matters.** This is a deny bypass on Windows. It also breaks the opposite case: `allow env Path` does not allow a script that asks for `PATH`.

**Reproduce (macOS; the Windows result is reasoned, not executed).**

    repro.rs (policy check only):
    AWS_SECRET_ACCESS_KEY: Deny
    aws_secret_access_key: Allow
    Aws_Secret_Access_Key: Allow

    AWS_SECRET_ACCESS_KEY=hunter2 sand run repro.sand --policy=repro.policy --no-prompt
    exact: None
    lower: None        (allowed by the policy; on macOS the lookup is case-sensitive)

On Windows, `std::env::var("aws_secret_access_key")` returns the value of `AWS_SECRET_ACCESS_KEY`, so the second line would print `Some("hunter2")`.

**Aspirational.** On Windows, the policy compares environment names case-insensitively, so both lines print `None` and the lowercase request is denied.
