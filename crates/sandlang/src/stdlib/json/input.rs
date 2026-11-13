//! The rules of JSON input, which the JSON text decoder and the `json.Json`
//! decoder share, so that `json.decode` and `json.from_json` accept the same
//! input and give the same messages.

use crate::codec::{DecodeFail, DecodeResult, QUOTED_KEY, SeqKind};
use crate::stdlib::base64;

/// The kind of a JSON value, which a message names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JsonKind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl JsonKind {
    fn name(self) -> &'static str {
        match self {
            JsonKind::Null => "null",
            JsonKind::Bool => "bool",
            JsonKind::Number => "number",
            JsonKind::String => "string",
            JsonKind::Array => "array",
            JsonKind::Object => "object",
        }
    }
}

/// The message for a value of kind `found` that is not a value of
/// `expected`, such as "expected int, found string".
pub(super) fn mismatch_message(expected: &str, found: JsonKind) -> String {
    format!("expected {expected}, found {}", found.name())
}

/// The error for a value of kind `found` that is not a value of `expected`.
pub(super) fn mismatch(expected: &str, found: JsonKind) -> DecodeFail {
    DecodeFail::Mismatch(mismatch_message(expected, found))
}

/// The message for a number with the float value `float`, which is not an
/// `int`.
pub(super) fn not_int(float: f64) -> String {
    match float.is_finite() && float.fract() == 0.0 {
        true => "integer out of range".into(),
        false => "expected int, found number".into(),
    }
}

/// Reads `bytes` from their standard Base64 text.
pub(super) fn bytes_of(text: &str) -> Result<Vec<u8>, String> {
    base64::decode(text, false).map_err(|_| "invalid base64".into())
}

/// What a sequence of `kind` expects, for a message.
pub(super) fn seq_expected(kind: SeqKind) -> std::borrow::Cow<'static, str> {
    match kind {
        SeqKind::Tuple(n) => format!("array of length {n}").into(),
        SeqKind::List | SeqKind::Set => "array".into(),
    }
}

/// The error for the map key text `key` that the key type cannot read.
pub(super) fn invalid_key(key: &str) -> DecodeFail {
    DecodeFail::Mismatch(format!("invalid map key `{}`", crate::stdlib::truncate(key, QUOTED_KEY)))
}

/// Reads a `bool` map key from its text.
pub(super) fn key_bool(key: &str) -> DecodeResult<bool> {
    match key {
        "true" => Ok(true),
        "false" => Ok(false),
        key => Err(invalid_key(key)),
    }
}

/// Reads an `int` map key from its text.
pub(super) fn key_int(key: &str) -> DecodeResult<i64> {
    key.parse().map_err(|_| invalid_key(key))
}

/// The error for a map key of the type `what`, which has no JSON key form.
pub(super) fn container_key(what: &str) -> DecodeFail {
    DecodeFail::Invalid(format!("cannot decode map keys of type {what}"))
}

/// The type of a sequence of `kind` as a map key, for a message.
pub(super) fn seq_key_type(kind: SeqKind) -> &'static str {
    match kind {
        SeqKind::List => "list",
        SeqKind::Set => "Set",
        SeqKind::Tuple(_) => "tuple",
    }
}

/// The message for an object of the variant of the enum `name` without
/// a member.
pub(super) fn no_variant(name: &str) -> DecodeFail {
    DecodeFail::Mismatch(format!("expected a variant of {name}"))
}

/// The message for an object of a variant of the enum `name` with more than
/// one member.
pub(super) fn not_single_variant(name: &str) -> DecodeFail {
    DecodeFail::Mismatch(format!("expected a single variant of {name}"))
}

/// The fields of the JSON form of a `Range`, an object with `start`, `end`,
/// and `inclusive`.
#[derive(Default)]
pub(super) struct RangeFields {
    pub(super) start: Option<i64>,
    pub(super) end: Option<i64>,
    pub(super) inclusive: bool,
}

impl RangeFields {
    /// Returns the range, or the error of a missing field.
    pub(super) fn finish(self) -> DecodeResult<(i64, i64, bool)> {
        match (self.start, self.end) {
            (Some(s), Some(e)) => Ok((s, e, self.inclusive)),
            (None, _) => Err(DecodeFail::Mismatch("missing field `start`".into())),
            _ => Err(DecodeFail::Mismatch("missing field `end`".into())),
        }
    }
}
