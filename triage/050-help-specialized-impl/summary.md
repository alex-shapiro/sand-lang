# `help` lists specialized trait implementations without their target type arguments

If a trait is implemented for a concrete instantiation of a generic type such
as `impl Describe for [int]`, `help([str])` lists `Describe` in the `Implements`
section without its target type argument `[int]`. Calling `["a"].describe()`
then fails type checking because `[str]` does not implement `Describe`.

Run:

```
$ sand run repro.sand
Implements: ... Close where T: Close / Describe

$ sand check repro-call.sand
error: [str] does not implement `Describe`
```

`help` should display the target type arguments for specialized implementations,
such as `Describe for [int]`.
