# Connection errors omit policy-denied IP addresses if remaining addresses fail to connect

If a hostname is resolved to multiple IP addresses and the policy denies some of
them, `resolve_allowed` attempts to connect only to the allowed addresses. If
those connections fail, the error message reports only the socket error and
omits the addresses that the policy blocked.

Start an IPv6-only server with `python3 -m http.server 8765 --bind ::1 &` and
run:

```
$ sand run repro.sand --policy=repro.policy --no-prompt
io: Connection refused (os error 61)
```

Because `repro.policy` denies `[::]/0`, `resolve_allowed` skips `::1` and tries
only `127.0.0.1:8765`, where a server is not listening. The error message should
include the denied addresses and the rules that blocked them alongside the
connection failure.
