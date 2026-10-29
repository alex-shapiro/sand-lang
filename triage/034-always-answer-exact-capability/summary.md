# "Always" and "Never" answers cover only the exact request, so the CLI asks again about the same file or folder

TODO item 034 (Policy): "Store \"always\" prompt answers as `RulePolicy` rules rather than exact…"

**Pain.** I answer "always" when a script writes `/tmp/sand-034.txt`, and it asks again when the script reads the file back. Worse, I answer "Never" to `read /etc/ssh`, and the next prompt asks me about `/etc/ssh/ssh_host_ed25519_key`.
**Why it matters.** The policy rule `allow write X` implies read, and `deny read DIR` covers the whole subtree, but the answers do not. Each extra prompt about a secret is one more chance to press `y` by mistake.

**Reproduce.** (on a pty: `(sleep 1; echo a; sleep 1; echo a; sleep 1) | script -q /dev/null sand run repro.sand`)

    sand: allow the script to write /private/tmp/sand-034.txt? … : a
    sand: allow the script to read /private/tmp/sand-034.txt? … : a
    read back: run 0 …

    sand run never_dir.sand
    sand: allow the script to read /private/etc/ssh? … : N
    sand: allow the script to read /private/etc/ssh/ssh_host_ed25519_key? … : n

**Aspirational.** "Never" on `read /etc/ssh` acts like `deny read /etc/ssh` for the rest of the run, and "always" on a write also covers the read.
