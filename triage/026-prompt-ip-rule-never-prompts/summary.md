# A `prompt net 10.0.0.0/8` rule never asks me when a hostname resolves into that range; it denies

TODO item 026 (Policy): "Support interactive prompts for resolved IP addresses that match…"

**Pain.** I allow GET everywhere but want sand to ask me before a script reaches loopback or internal addresses, so I write `prompt net 127.0.0.0/8 ::1`. If the script uses the IP literal, sand asks me. If the script uses a hostname that resolves to the same address (`localhost`, `db.internal`, a DNS-rebinding name), sand does not ask. It fails with "not allowed by the rule `prompt net ...`".
**Why it matters.** The same address gets two different behaviours, depending only on how the script spells the host. A `prompt` rule that silently acts as `deny` breaks legitimate scripts and gives me no way to approve them. (SPEC §14.6 documents this limit, but the result surprises users.)

**Reproduce.** `repro.rs` uses the policy `allow http plaintext GET *` + `prompt net 127.0.0.0/8 ::1` and a prompter that prints each prompt and answers `y`.

    PROMPT: GET http://127.0.0.1:18931/  -> answering y
    http://127.0.0.1:18931/ -> status 200
    http://localhost:18931/ -> permission_denied: permission denied: connect to localhost:18931:
        address 127.0.0.1 is not allowed by the rule `prompt net 127.0.0.0/8`

The prompter never receives a request for `localhost`.

**Aspirational.** Both requests prompt, for example "connect to localhost at 127.0.0.1:18931 (plaintext)?". After `y`, both get status 200.
