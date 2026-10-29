# `json.decode<Config>(text)` fails with an error about chained comparisons

TODO item 072 (Diagnostics): "Accept explicit type arguments in calls, or make the parser error…"

**Pain.** I write `json.decode<Config>(text)`, as I would in TypeScript, Swift, or Rust (`::<>`). Sand reports `comparison operators cannot be chained`. The error does not mention types, so I do not know that the fix is `let config: Config = json.decode(text)`.
**Why it matters.** Typed JSON decoding is one of the most common things a script does, and LLMs write this form first. Each attempt costs a round trip, and the error points the fix in the wrong direction.

**Reproduce.**

    sand run repro.sand
    error: comparison operators cannot be chained
      --> repro.sand:5:32

**Aspirational.** `aspirational.sand` runs today. Sand accepts `json.decode<Config>(text)`, or the parser error suggests the annotated `let` form.
