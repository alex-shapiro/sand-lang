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
