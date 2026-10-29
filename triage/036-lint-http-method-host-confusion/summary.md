# `sand lint` does not warn if an uppercase hostname is parsed as an HTTP method or a lowercase method is parsed as a hostname

The HTTP rule parser treats leading uppercase tokens as HTTP methods and
lowercase tokens as host patterns. In `deny http LOCALHOST 127.0.0.1`, the
parser treats `LOCALHOST` as a method identifier, so `GET http://localhost/admin`
and `GET http://127.0.0.1/admin` are both allowed under `allow http plaintext *`.
Conversely, in `allow http plaintext purge localhost:9/cache`, the parser
treats `purge` as a host pattern and allows all HTTP methods, including
`DELETE`, to `localhost:9/cache`.

Run:

```
$ sand lint repro.policy method.policy
$ sand run repro.sand --policy=repro.policy --no-prompt
http://localhost:9/admin: io: Connection refused (os error 61)
http://127.0.0.1:9/admin: io: Connection refused (os error 61)

$ sand run method.sand --policy=method.policy --no-prompt
DELETE: io: Connection refused (os error 61)
```

`lint_policy` should warn if an uppercase token in an `http` rule looks like a
hostname or if a lowercase host pattern matches a known HTTP method.
