# A Rust struct cannot give a default to a date or instant field

TODO item 019 (Embedding): "Represent `ValueType` constants in `ConstVal` by their text form…"

**Pain.** I bind a settings struct with `#[derive(SandStruct)]` and give the field `since: Date` the default `Date::constant(2000, 1, 1)`. `NativeModule::bind` panics when the host starts. The same struct written in Sand works, and `json.decode` fills the defaults.

**Why it matters.** Dates, instants, spans, and time zones are usual fields in host settings and records. Without a default, every script and every JSON document must supply the field.

**Reproduce.** `cargo run` with `repro.rs` in a scratch crate that depends on `sandlang`:
```
thread 'main' panicked at src/native/mod.rs:72:13:
the default of field `since` of `Retention` is not a Sand constant, or has more than 65536 parts or 64 levels
```
The Sand version: `sand run aspirational.sand`
```
Retention(name: "logs", since: 2000-01-01, last_run: 1970-01-01T00:00:00Z)
```

**Aspirational.** `repro.rs` prints the same line as `aspirational.sand`.
