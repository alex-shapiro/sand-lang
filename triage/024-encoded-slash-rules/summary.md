# HTTP policy rules reject encoded path separators `%2F` and `%5C`

`RulePolicy` rejects HTTP path rules that contain `%2F` or `%5C` and suggests
replacing the encoded separator with `/`, which does not match requests that
contain `%2F`. As a result, a policy cannot allow a single URL with an encoded
slash such as `/api/v4/projects/group%2Fproject`, `rule_for` returns `None`,
and the CLI omits the `--allow` hint on denial.

Start an HTTP server on `127.0.0.1:18931` and run:

```
$ sand run repro.sand --no-prompt
sand: denied: GET http://127.0.0.1:18931/api/v4/projects/group%2Fproject

$ sand run repro.sand --no-prompt --allow='http plaintext GET 127.0.0.1:18931/api/v4/projects/group%2Fproject'
sand: invalid rule ...: `group%2Fproject` contains an encoded or backslash path separator: write `group/project`

$ sand run repro.sand --no-prompt --allow='http plaintext GET 127.0.0.1:18931/api/v4/projects/group/project'
permission_denied: permission denied: GET http://127.0.0.1:18931/api/v4/projects/group%2Fproject
```

The narrowest rule that matches the request today is `projects/*`, which allows
all projects. `RulePolicy` and `rule_for` should accept `%2F` and `%5C` in HTTP
path patterns so that `aspirational.policy` matches only `group%2Fproject`.
