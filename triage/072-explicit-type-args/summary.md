# Explicit type arguments in calls fail with a chained comparison error

Sand does not support explicit type arguments on function calls, so the parser
interprets `<` and `>` in `json.decode<Config>(text)` as comparison operators
and reports `comparison operators cannot be chained`. The diagnostic does not
mention type arguments or suggest adding a type annotation on the `let`
binding.

Run:

```
$ sand run repro.sand
error: comparison operators cannot be chained
  --> repro.sand:5:32
```

The parser should either accept explicit type arguments in calls or suggest
writing `let config: Config = json.decode(text)`, as in `aspirational.sand`.
