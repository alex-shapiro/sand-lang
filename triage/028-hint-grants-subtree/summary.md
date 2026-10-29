# `rule_for` and CLI `--allow` hints grant access to an entire directory subtree or URL prefix

Because `RulePolicy` lacks syntax for exact file or URL path matches,
`rule_for` and the CLI `--allow` hint generate prefix rules. Approving the
suggested `--allow='read <home>'` rule for `fs.list_dir(home)` grants read
access to every file under `<home>`, including `<home>/.ssh/id_ed25519`.
Similarly, the suggested rule for `GET host/v1/users` also allows
`GET host/v1/users/**`.

Run:

```
$ mkdir -p home/.ssh && echo "PRIVATE KEY" > home/.ssh/id_ed25519
$ sand run repro.sand --no-prompt -- home
sand: denied: read /private/tmp/.../home; pass --allow='read /private/tmp/.../home' to allow it
list: Err(... permission denied ...)

$ sand run repro.sand --no-prompt --allow='read /private/tmp/.../home' -- home
list: Ok([".ssh", "notes.txt"])
key: Ok("PRIVATE KEY\n")
```

Adding exact-file and exact-path rule syntax allows `rule_for` and CLI hints to
grant access only to the requested path.
