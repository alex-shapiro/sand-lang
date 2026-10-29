# Passing a function with default parameters to `map` fails; I must wrap it in a lambda

TODO item 064 (Language): "Preserve default parameter values in function types so that calls…"

**Pain.** `greet("Ann")` works because `greeting` has a default, but `names.map(greet)` and `msgs.map(Error)` fail with a type mismatch. A function stored in a variable also loses its defaults: `let f = greet; f("Cy")` needs two arguments. The same happens with standard constructors, for example `[9, 12].map(time.PlainTime)`.

**Why it matters.** Point-free `map(Error)` / `map(str)` style is common in Rust, Swift, and Python code, and the error message (`found fn(str, str) -> Error`) does not tell me that a default is the cause. The lambda workaround is easy but adds noise.

**Reproduce.**
```
sand check aspirational.sand
error: type mismatch: expected fn(str) -> _, found fn(str, str) -> Error
error: type mismatch: expected fn(str) -> _, found fn(str, str) -> str
error: function expected 2 arguments, found 1
sand run repro.sand      # workaround: `m => Error(m)`; prints the expected output
```

**Aspirational.** `aspirational.sand`: the function value keeps its defaults. §6.4 documents that constructor values ignore defaults, so this is a known limit, not a regression.
