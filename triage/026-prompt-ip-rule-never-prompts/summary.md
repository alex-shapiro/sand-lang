# `prompt net` IP and CIDR rules deny connections if a hostname is resolved into that range

If a connection uses an IP literal that matches a `prompt net` rule, the engine
prompts the user. If a connection uses a hostname that is resolved to the same
IP address, the engine denies the connection during address resolution without
calling the prompter.

Start an HTTP server on port 18931 and run `repro.rs`:

```
PROMPT: GET http://127.0.0.1:18931/  -> answering y
http://127.0.0.1:18931/ -> status 200
http://localhost:18931/ -> permission_denied: permission denied: connect to localhost:18931:
    address 127.0.0.1 is not allowed by the rule `prompt net 127.0.0.0/8`
```

Address resolution should invoke the prompter if a resolved IP address matches
a `prompt` rule so that both requests prompt and succeed if approved.
