# Parser accepts `import` statements after non-import items and executes them before earlier statements

Although §3.1 requires `import` declarations to appear before all other items
and statements, `sand check`, `sand lint`, and `sand fmt` accept `import`
statements anywhere in a file. At runtime, imported modules execute before the
top-level statements above them, and imported bindings are in scope before
their `import` line.

Run:

```
$ sand run repro.sand
side: module body runs
main: line 1
[1]
hi
```

The parser or type checker should reject `import` statements that appear after
any non-import item or statement, as in `aspirational.sand`.
