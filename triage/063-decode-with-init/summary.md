# A struct that validates itself in `init` cannot be read from JSON

TODO item 063 (Language): "Support `Decode` for structs that declare `init` by calling `init`…"

**Pain.** I add `init` to `Window` to reject `start > end`. After that, `json.decode` rejects `Window` and every type that contains it (`Job`, `[Window]`, ...), although `json.encode` still accepts it. To load a config, I must declare a copy of each struct without `init` and convert by hand.

**Why it matters.** "Validate at construction, then decode from JSON" is the normal Rust (`serde(try_from)`) and Swift (`init(from:)`) pattern. The workaround doubles the type declarations, and each new field needs an edit in three places. Decoding does not bypass `init` (good), but the rejection is total.

**Reproduce.**
```
sand check aspirational.sand
error: Job does not implement `Decode` because `Window` declares `init`, which decoding does not call
sand run repro.sand      # workaround: RawWindow/RawJob mirror structs
error: invalid: start 9 is after end 5
```

**Aspirational.** `aspirational.sand`: `json.decode` calls `Window.init(start:, end:)`, and the `Err` from `init` fails the decode, preferably with the JSON path (`$.window`).
