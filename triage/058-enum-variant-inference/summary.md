# Enum variants in expressions require full qualification even if the expected type is known

Pattern matching infers the enum type of unqualified variants such as `Red =>`,
but expressions reject unqualified variants such as `Write` and leading-dot
variants such as `.Write`. Callers must qualify every enum variant with its
module and type identifier, such as `fs.Mode.Write`, `time.Unit.Month`, and
`Color.Green`, in function arguments, annotated bindings, and comparisons.

Run:

```
$ sand check aspirational.sand
error: undefined name `Write`
error: undefined name `Month`
error: undefined name `Day`
error: undefined name `Green`
error: undefined name `Red`
```

Inferring the enum type of a variant expression from the expected type allows
`aspirational.sand` to compile without repeating the enum path at each use.
