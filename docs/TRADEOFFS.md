# Open Tradeoffs

## Network checks use two precedence orders

**Problem.** The same connection can get different decisions, because the two
network check stages rank rules in different orders:

- Stage 1 (`Best::offer`) ranks rules by tier and then by specificity, which
  includes the port.
- Stage 2 (`resolve` and `resolution_key`) ranks rules by host specificity
  only. It ignores tiers and ports.
- An IP literal skips stage 2.

For example, take the primary rule `allow net *` and the default rule
`deny net 169.254.0.0/16` from `with_defaults`:

| Request                                  | Decision |
| ---------------------------------------- | -------- |
| IP literal `169.254.169.254`             | Allow    |
| `meta.example` that resolves to the same address | Deny     |

SPEC §14.5 promises that "a user domain wildcard cannot bypass a default IP
deny rule". The IP literal breaks this promise. Ports have the same problem:
`allow net 10.0.0.5:5432` overrides `deny net 10.0.0.5` for the literal, but
`allow net db.internal:5432` does not override it for a name that resolves to
`10.0.0.5`.

`deny_private_net` makes this problem easy to reach, because a host can put the
rules in the defaults tier.

**Fix A: rank by tier, then by full specificity, in both stages.**

- `with_defaults` then means "fallback" for every capability.
- A host that wants a floor calls `deny_private_net()` or `deny_net` on the
  primary policy, after `with_defaults`.
- This agrees with the decision that the host controls every rule, so no rule
  needs to rank above another tier.
- Cost: a default IP deny rule stops protecting against a primary wildcard. The
  §14.5 promise and the `floor()` cases in `tests/sandbox_io.rs` change.

**Fix B: rank network rules by specificity only, in both stages.**

- The floor then holds for IP literals too.
- Cost: `with_defaults` stops meaning "fallback" for network rules. For
  example, a default `allow net example.com` overrides a primary
  `deny net *`.

**Rejected: send IP literals through stage 2.** This removes the symptom, but it
keeps two orders and makes stage 2 larger.

**Recommendation.** Fix A. It removes the second order and the special case,
and it gives `with_defaults` one meaning.

## An exact host name rule allows private addresses

**Problem.** An exact host name rule turns off `deny_private_net` for every
address that the name resolves to. Take `allow net api.partner.com` plus
`deny_private_net()`. If DNS for `api.partner.com` returns `127.0.0.1`, `::1`,
or `169.254.169.254`, the engine connects. A list of exact API hosts plus
`deny_private_net` is the main use of the floor, so DNS rebinding reaches
loopback and cloud metadata.

Stage 2 compares the host rank of the name rule with the IP rules, and an exact
host name ranks equal to a single IP address. SPEC §14.6 documents this on
purpose: `allow net db.internal` permits `10.0.0.5`.

Industry practice differs. Stripe's Smokescreen and ash's own gate
(`ash-sand/src/gate.rs`) need a rule for the private address itself.

**Fix A: only address rules decide about resolved addresses.**

- Stage 2 runs the stage-1 ranking over IP rules only. This removes
  `resolution_key`, and with it the second precedence order of the first
  section.
- `localhost` rules must count as address rules, because RFC 6761 maps them to
  loopback. Otherwise `allow net localhost:8080` stops working.
- Cost: a host that connects to internal names must also allow their
  addresses, such as `allow net db.internal 10.0.0.5`.

**Fix B: keep the current behavior.** The `deny_private_net` doc and SPEC now
say that an exact host name rule allows the private addresses of the name.

**Recommendation.** Fix A. It agrees with industry practice and it makes stage
2 one ranking.
