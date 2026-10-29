# Case-sensitive `env` rules on Windows allow scripts to bypass `deny env` rules

Windows looks up environment variables case-insensitively, but `RulePolicy`
compares environment variable identifiers case-sensitively on all platforms.
Under `allow env *` and `deny env AWS_SECRET_ACCESS_KEY`, calling
`env.get("aws_secret_access_key")` on Windows passes the policy check and
returns the value of `AWS_SECRET_ACCESS_KEY`. Conversely, `allow env Path`
denies `env.get("PATH")`.

Run `repro.rs`:

```
AWS_SECRET_ACCESS_KEY: Deny
aws_secret_access_key: Allow
Aws_Secret_Access_Key: Allow
```

On Windows, `RulePolicy` should fold environment variable identifiers
case-insensitively so that policy evaluation matches the operating system
lookup.
