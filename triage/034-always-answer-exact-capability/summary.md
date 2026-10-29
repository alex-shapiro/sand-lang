# "Always" and "Never" prompt answers match only the exact capability instead of applying policy rules

The engine stores `PromptAnswer::AllowAlways` and `PromptAnswer::DenyAlways` as
exact `Capability` values rather than `RulePolicy` rules. Answering "always" to
`write /tmp/sand-034.txt` prompts again if the script reads the same file, even
though `allow write` implies `read`. Similarly, answering "Never" to
`read /etc/ssh` prompts again if the script reads `/etc/ssh/ssh_host_ed25519_key`.

Run on a terminal:

```
$ sand run repro.sand
sand: allow the script to write /private/tmp/sand-034.txt? … : a
sand: allow the script to read /private/tmp/sand-034.txt? … : a

$ sand run never_dir.sand
sand: allow the script to read /private/etc/ssh? … : N
sand: allow the script to read /private/etc/ssh/ssh_host_ed25519_key? … : n
```

Storing "always" and "Never" answers as `RulePolicy` rules ensures that a write
grant covers reads of the same file and a directory denial covers its subtree.
