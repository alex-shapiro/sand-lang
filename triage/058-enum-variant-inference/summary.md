# I must spell out the full enum path for every variant, even where the type is known

TODO item 058 (Language): "Infer the enum type of a variant expression from the expected type…"

**Pain.** `match` lets me write `Red =>`, but an argument, an annotated `let`, or a comparison rejects `Red` with `undefined name`. I must write `fs.Mode.Write`, `time.Unit.Month`, and `Color.Green` at every use. Swift users write `.write`, Rust users import the variants; neither works.

**Why it matters.** Calls into `fs` and `time` with option enums get long and noisy. The rule differs between patterns and expressions, so the first try of an LLM or a new user fails to compile.

**Reproduce.**
```
sand run repro.sand --allow='write /tmp/triage-lang1' --no-prompt   # full paths: prints P2M14D, red true
sand check aspirational.sand
error: undefined name `Write`
error: undefined name `Month`
error: undefined name `Day`
error: undefined name `Green`
error: undefined name `Red`
```
`fs.open(path, .Write)` fails with `expected expression, found '.'`.

**Aspirational.** `aspirational.sand`: the expected type selects the enum, as patterns already do.
