# Function values discard default parameter values

If a function or constructor with default parameters is used as a value, such
as an argument to `List.map` or a variable binding, its function type requires
all parameters explicitly. Callers cannot pass `Error` or `greet` to `map`
without wrapping the call in a closure.

Run:

```
$ sand check aspirational.sand
error: type mismatch: expected fn(str) -> _, found fn(str, str) -> Error
error: type mismatch: expected fn(str) -> _, found fn(str, str) -> str
error: function expected 2 arguments, found 1

$ sand run repro.sand
[Error(message: "disk full", kind: "error"), Error(message: "timeout", kind: "error")]
["Hello, Ann!", "Hello, Bo!"]
Hello, Cy!
```

Preserving default parameter values in function types allows calls through
function values to omit defaulted arguments, as in `aspirational.sand`.
