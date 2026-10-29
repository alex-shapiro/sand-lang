# Follow-on parser errors obscure the initial syntax error and report spurious brace errors

A single syntax mistake produces follow-on parser errors on later lines. In
`repro.sand`, an unterminated string literal on line 2 produces three errors on
lines 6, 7, and 11, ending with a spurious `expected }`. In `repro2.sand`, a
missing `:` in a parameter list also reports a spurious `unexpected }` at the
end of the function.

Run:

```
$ sand check repro.sand
error: expected newline or `;`, found identifier   --> repro.sand:6:18
error: unterminated string                         --> repro.sand:7:20
error: expected `}`, found end of file             --> repro.sand:11:1

$ sand check repro2.sand
error: expected `:`, found identifier   --> repro2.sand:1:11
error: unexpected `}`                   --> repro2.sand:1:39
```

The parser should report a single diagnostic at the site of the syntax error
and suppress cascading errors caused by incomplete recovery.
