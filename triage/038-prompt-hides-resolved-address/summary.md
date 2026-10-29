# CLI network prompts omit resolved IP addresses and plaintext warnings

If the CLI prompts the user for an HTTP or network connection, the prompt
displays only the requested URL or hostname. It does not display the resolved
IP addresses or warn that the connection is unencrypted, so a user who approves
`http://status-7f000001.nip.io:8765/` grants access to `127.0.0.1:8765` without
seeing the loopback address.

Start a local server with `python3 -m http.server 8765 --bind 127.0.0.1 &` and
run on a terminal:

```
$ sand run repro.sand
sand: allow the script to GET http://status-7f000001.nip.io:8765/? [y]es once, [a]lways, [n]o, [N]ever: y
status 200: ...
```

The CLI prompter should display the resolved IP addresses and warn before
allowing plaintext network connections.
