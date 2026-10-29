# I approve what looks like an internet host, and the script reaches a service on my machine

TODO item 038 (Policy): "Warn in the CLI prompter before allowing plaintext network connections, and display…"

**Pain.** The prompt asks `allow the script to GET http://status-7f000001.nip.io:8765/?`. I answer `y`, and the script reads my local service on 127.0.0.1. The prompt does not show the resolved address, and with the CLI defaults no rule blocks private addresses. The prompt also gives no warning that the request is unencrypted; it only shows `http://`.
**Why it matters.** The user decides from the prompt text. A hostname can point anywhere, including a router or an admin UI on the LAN.

**Reproduce.** `python3 -m http.server 8765 --bind 127.0.0.1 &` (needs a working DNS lookup of nip.io), then on a pty:

    (sleep 2; echo y; sleep 2) | script -q /dev/null sand run repro.sand
    sand: allow the script to GET http://status-7f000001.nip.io:8765/? [y]es once, [a]lways, [n]o, [N]ever: y
    status 200: router admin page

**Aspirational.** `GET http://status-7f000001.nip.io:8765/ (resolves to 127.0.0.1, a loopback address; unencrypted)?`
