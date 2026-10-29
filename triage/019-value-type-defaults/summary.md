# `#[sand(default)]` panics on `ValueType` fields such as `Date` and `Timestamp`

`NativeModule::bind` panics if a `#[derive(SandStruct)]` field uses
`#[sand(default)]` with a `ValueType` such as `Date` or `Timestamp`, because
`ConstVal` cannot represent value-type constants. Without default support,
every caller and JSON document must supply those fields explicitly.

Running `repro.rs` panics at startup:

```
thread 'main' panicked at src/native/mod.rs:72:13:
the default of field `since` of `Retention` is not a Sand constant, or has more than 65536 parts or 64 levels
```

Representing `ValueType` constants in `ConstVal` by their text form allows
`repro.rs` to match `aspirational.sand`:

```
Retention(name: "logs", since: 2000-01-01, last_run: 1970-01-01T00:00:00Z)
```
