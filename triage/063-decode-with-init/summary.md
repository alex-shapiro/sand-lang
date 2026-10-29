# Structs that declare `init` do not implement `Decode`

Declaring an `init` constructor on a struct removes its `Decode` implementation
and prevents `json.decode` from decoding that struct or any enclosing type. To
decode JSON into a validated struct, a script must declare duplicate structs
without `init` and convert each value manually.

Run:

```
$ sand check aspirational.sand
error: Job does not implement `Decode` because `Window` declares `init`, which decoding does not call

$ sand run repro.sand
error: invalid: start 9 is after end 5
```

Supporting `Decode` for structs that declare `init` by calling `init` with the
JSON object members as named arguments allows `json.decode` to construct and
validate `Window` directly, as in `aspirational.sand`.
