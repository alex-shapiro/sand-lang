# Open Tradeoffs

This file tracks product and system tradeoffs found during branch review. Each section states the problem, the options, and the tradeoffs between them.

## Derived `Decode` for `pub struct` with private fields or private `init`

**Problem:** The compiler derives `Decode` for every Sand struct, including a `pub struct` with private fields or a private `init`. Callers outside the defining module can decode arbitrary input directly into private fields, bypassing module invariants that private fields or a private `init` normally protect.

- **Option A — Withhold auto-derived `Decode` on structs with private fields or private `init`:** Require the defining module to opt in with `impl Decode for T {}` (which uses the native default walker) or a custom `impl Decode`. This preserves module encapsulation by default, at the cost of requiring a one-line `impl` on public types that hide fields.
- **Option B — Keep auto-deriving `Decode` and document the rule in `SPEC.md`:** Every struct remains decodable with zero boilerplate, matching `Eq` and `Hash`. Module authors must write a custom `impl Decode` (or make `init` validate inputs) to block external decoding into private state.

## `ValueSource` representation of `Option.None` and `Json`

**Problem:** `ValueSource::kind()` returns `InputKind::Null` for `Value::Variant("Option", "None")`, while `ValueSource::unit()` rejects `Option.None` and accepts only `Value::Unit`. As a result, `Json.decode` fails when a host passes `Option.None` inside a `Value`. Separately, `Json::to_value()` encodes a `Json` tree as `Value::Variant("json.Json", ...)` rather than unwrapping it into plain `Value` nodes.

- **Option A — Keep `Option.None` distinct from `InputKind::Null` and unwrap `Json` in `ValueSink`:** `ValueSource::kind()` returns `InputKind::Variant` for `Option.None` (matching `ValueSource::unit()`), and `Value` preserves the distinction between `()` and `None`. `Json.encode` already emits plain `EncodeSink` calls (`unit`, `bool`, `list`, `map`), so `CallContext::to_value` produces unwrapped `Value` data while Rust `Json::to_value()` can be updated to match.
- **Option B — Treat `Option.None` as `InputKind::Null` in `ValueSource::unit()`:** `Json.decode` accepts `Option.None` as `Json.Null`, and `Value::Unit` and `Option.None` behave identically in `ValueSource::unit()`. This blurs the distinction between `()` and `None` when decoding `Value` into `Json` or `()`.

## Public codec streaming API and `NativeContext` value conversions

**Problem:** `EncodeSink` and `DecodeSource` are public traits in `sandlang::native::codec`, so host crates can implement them, but `codec::encode` and `codec::decode` are `pub(crate)`. External crates cannot drive a custom format (such as TOML or CBOR) through Sand's codec walker. In addition, `CallContext` exposes `to_value` and `from_value`, while async `NativeContext` does not.

- **Option A — Keep `EncodeSink` and `DecodeSource` crate-private until the streaming API settles:** Shrink the public surface to `Encoder::write`, `Decoder::read`, `InputKind`, `PayloadForm`, and `CallContext::to_value`/`from_value`. Add public `CallContext::encode_with` and `decode_with` only when a second format (such as TOML or YAML) validates the trait methods.
- **Option B — Expose `CallContext::encode_with` and `CallContext::decode_with` now:** Host crates can build custom formats immediately, at the cost of stabilizing the full 40-method `EncodeSink` and `DecodeSource` traits. Adding `NativeContext::to_value` and `from_value` is a separate choice: synchronous `CallContext` keeps codec walks simple, while `NativeContext` would need synchronous-callback dispatch from async host code.

## `json.to_json` return type and derived `Encode` when `init` differs from fields

**Problem:** Two ergonomics questions surfaced in the JSON API:

1. `json.to_json<T: Encode>(v: T) -> Json` aborts execution if a value contains `NaN`, `inf`, an invalid map key, or a custom `Encode.encode` that returns `Err(...)`, whereas `json.encode` returns `Result<str>`.
2. When a struct defines `init` with parameters that differ from its fields, the compiler derives `Decode` (using `init`) but withholds `Encode` (requiring a manual `impl Encode for T {}` or custom impl).

- **Option A — Keep `json.to_json -> Json` infallible and withhold derived `Encode` on mismatched `init`:** `json!` interpolations remain ergonomic without `?`, and structs with a custom `init` cannot silently emit a wire layout that their own `Decode` cannot parse back.
- **Option B — Return `Result<Json>` from `json.to_json` or derive `Encode` on all structs:** Returning `Result<Json>` makes custom `Encode` errors catchable in Sand code (matching `json.encode`), at the cost of changing `json.to_json` and `json!` error handling. Auto-deriving `Encode` regardless of `init` removes boilerplate for one-way encoding, at the cost of asymmetric round-trips.
