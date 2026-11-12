# `#[sand(default)]` rejects `ValueType` fields such as `Date` and `Timestamp`

`HostModules::new` returns a compile error if a `#[derive(SandStruct)]` field
uses `#[sand(default)]` with a `ValueType` such as `Date` or `Timestamp`,
because `ConstVal` cannot represent value-type constants. Without default
support, every caller and JSON document must supply those fields explicitly.

The struct of `repro.rs` fails with these errors:

```
the Rust field `since` of `jobs.Retention` has a default that is not a constant of `PlainDate` with at most 65536 parts and 64 levels
the Rust field `last_run` of `jobs.Retention` has a default that is not a constant of `Instant` with at most 65536 parts and 64 levels
```

Representing `ValueType` constants in `ConstVal` by their text form allows
`repro.rs` to match `aspirational.sand`:

```
Retention(name: "logs", since: 2000-01-01, last_run: 1970-01-01T00:00:00Z)
```
