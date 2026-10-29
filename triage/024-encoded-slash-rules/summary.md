# I cannot write a narrow rule for a URL that contains `%2F`, and the error message gives wrong advice

TODO item 024 (Policy): "Support HTTP paths with encoded separators `%2F` and `%5C`…"

**Pain.** APIs such as GitLab put `%2F` in paths (`/api/v4/projects/group%2Fproject`). When such a request is denied, sand does not show the `--allow` hint that it shows for other URLs. If I write the rule myself, the parser rejects `group%2Fproject` and tells me to write `group/project`. That rule does not allow the request. The narrowest rule that works is `projects/*`, which allows every project.
**Why it matters.** The policy cannot express "this one resource" for a common API style, and its own advice fails. Embedders that store "always" answers with `rule_for` get no rule for these requests. (The CLI's interactive `a` answer still works for the rest of the run, because the engine stores the exact request.)

**Reproduce.** Run any HTTP server on 127.0.0.1:18931, for example `../025-matrix-param-deny-bypass/server.py`.

    sand run repro.sand --no-prompt
    sand: denied: GET http://127.0.0.1:18931/api/v4/projects/group%2Fproject        (no --allow hint)

    sand run repro.sand --no-prompt --allow='http plaintext GET 127.0.0.1:18931/api/v4/projects/group%2Fproject'
    sand: invalid rule ...: `group%2Fproject` contains an encoded or backslash path separator: write `group/project`

    sand run repro.sand --no-prompt --allow='http plaintext GET 127.0.0.1:18931/api/v4/projects/group/project'
    permission_denied: permission denied: GET http://127.0.0.1:18931/api/v4/projects/group%2Fproject

    sand run repro.sand --no-prompt --allow='http plaintext GET 127.0.0.1:18931/api/v4/projects/*'
    status 200

**Aspirational.** `aspirational.policy` allows only `group%2Fproject`. Sand accepts it, the denial message suggests it, and it matches the request under both interpretations of `%2F`.
