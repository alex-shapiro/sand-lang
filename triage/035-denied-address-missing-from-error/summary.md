# The error says "Connection refused" when the policy actually blocked the only address the server listens on

TODO item 035 (Policy): "Include any policy-denied IP addresses in the error message if…"

**Pain.** My dev server listens on `[::1]:8765` (Node's default for `localhost`). The policy says `deny net [::]/0  # no IPv6 egress`. `http.get("http://localhost:8765/")` fails with `Connection refused` and does not say that the policy skipped `::1`. I, or the LLM that wrote the script, conclude that the server is down.
**Why it matters.** The message points away from the real cause. When the policy denies every address, the error names the rule; when only some addresses fail to connect, it names nothing.

**Reproduce.** `python3 -m http.server 8765 --bind ::1 &`, then:

    sand run repro.sand --policy=repro.policy --no-prompt
    io: Connection refused (os error 61)
    # with only `allow http plaintext *`: status 200

**Aspirational.** `connection refused at 127.0.0.1:8765; address ::1 is not allowed by the rule `deny net [::]/0` on line 2`.
