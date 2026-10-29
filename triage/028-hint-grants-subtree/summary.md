# The `--allow` hint for listing a directory grants read access to every file below it

TODO item 028 (Policy): "Add exact-file and exact-path rule syntax so that `rule_for` generates…"

**Pain.** A script lists my home directory. Sand denies it and tells me "pass --allow='read <home>' to allow it". I pass that flag to let the script see file names. The flag also lets the script read `<home>/.ssh/id_ed25519`, because a path rule always matches the whole subtree. No rule syntax exists for "this path only". The same applies to HTTP: the hint for `GET host/v1/users` also allows `GET host/v1/users/**`.
**Why it matters.** Sand's own advice grants much more than the request that was denied, and I cannot write a narrower rule. Embedders that store "always" answers with `rule_for` have the same problem. (The CLI's interactive `a` answer is not affected: it covers only the exact request for the rest of the run.)

**Reproduce.**

    mkdir -p home/.ssh && echo "PRIVATE KEY" > home/.ssh/id_ed25519
    sand run repro.sand --no-prompt -- home
    sand: denied: read /private/tmp/.../home; pass --allow='read /private/tmp/.../home' to allow it
    list: Err(... permission denied ...)

    sand run repro.sand --no-prompt --allow='read /private/tmp/.../home' -- home
    list: Ok([".ssh", "notes.txt"])
    key: Ok("PRIVATE KEY\n")

**Aspirational.** An exact-path form (for example a hypothetical `allow read exact <home>`) matches only `<home>`. Sand suggests that form after a denied list, so the key stays denied.
