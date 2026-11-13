//! The rules of host [`Value`] input, for each scalar that a decoder reads.
//!
//! A host `Value` can have the data model of the target type, or its JSON
//! form, such as a Base64 `Value::Str` for `bytes`. The `Value` decoder and
//! the `json.Json` decoder read their scalars with these rules, so that both
//! accept the same forms and give the same messages.

use super::{DecodeFail, DecodeResult, QUOTED_KEY};
use crate::stdlib::base64;
use crate::value::{Value, integral_float_to_int, key_text, kind_name, without_json_variant};
use std::borrow::Cow;

/// A node of a decoder input, as the rules see it.
#[derive(Clone, Copy)]
pub(crate) enum InputNode<'a> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Duration(jiff::SignedDuration),
    Str(&'a str),
    Bytes(&'a [u8]),
    Range(i64, i64, bool),
    /// A value of a native value type: its qualified type name and its text.
    Native(&'a str, &'a str),
    /// Another host value, such as a list or a struct.
    Value(&'a Value),
    /// A list or a map of a `json.Json` value, by its kind name.
    Container(&'static str),
}

impl<'a> InputNode<'a> {
    /// Returns the node of `value`, without its `json.Json` variants.
    pub(crate) fn of(value: &'a Value) -> InputNode<'a> {
        match without_json_variant(value) {
            Value::Unit => InputNode::Unit,
            Value::Bool(b) => InputNode::Bool(*b),
            Value::Int(i) => InputNode::Int(*i),
            Value::Float(f) => InputNode::Float(*f),
            Value::Duration(d) => InputNode::Duration(*d),
            Value::Str(s) => InputNode::Str(s),
            Value::Bytes(b) => InputNode::Bytes(b),
            &Value::Range { start, end, inclusive } => InputNode::Range(start, end, inclusive),
            Value::Native(native) => InputNode::Native(&native.type_name, &native.text),
            value => InputNode::Value(value),
        }
    }

    /// The kind of the node for a message, such as `str` or `shop.Order`.
    fn kind(&self) -> Cow<'a, str> {
        Cow::Borrowed(match *self {
            InputNode::Unit => "()",
            InputNode::Bool(_) => "bool",
            InputNode::Int(_) => "int",
            InputNode::Float(_) => "float",
            InputNode::Duration(_) => "duration",
            InputNode::Str(_) => "str",
            InputNode::Bytes(_) => "bytes",
            InputNode::Range(..) => "range",
            InputNode::Native(type_name, _) => type_name,
            InputNode::Value(v) => return kind_name(v),
            InputNode::Container(kind) => kind,
        })
    }

    /// The JSON object key text of the node, for a message.
    fn key_text(&self) -> Cow<'a, str> {
        match *self {
            InputNode::Str(s) | InputNode::Native(_, s) => Cow::Borrowed(s),
            InputNode::Int(i) => Cow::Owned(i.to_string()),
            InputNode::Bool(b) => Cow::Borrowed(if b { "true" } else { "false" }),
            InputNode::Duration(d) => Cow::Owned(d.as_nanos().to_string()),
            InputNode::Value(v) => key_text(v),
            _ => self.kind(),
        }
    }

    /// The error for a node that is not a value of `expected`. A map key
    /// (`in_key`) gets the message of an invalid key.
    pub(crate) fn mismatch(&self, expected: &str, in_key: bool) -> DecodeFail {
        if in_key {
            return invalid_key(&self.key_text());
        }
        DecodeFail::Mismatch(format!("expected {expected}, found {}", self.kind()))
    }

    pub(crate) fn unit(&self, in_key: bool) -> DecodeResult<()> {
        match self {
            InputNode::Unit => Ok(()),
            _ => Err(self.mismatch("()", in_key)),
        }
    }

    pub(crate) fn bool(&self, in_key: bool) -> DecodeResult<bool> {
        match *self {
            InputNode::Bool(b) => Ok(b),
            InputNode::Str(s) if in_key && (s == "true" || s == "false") => Ok(s == "true"),
            _ => Err(self.mismatch("bool", in_key)),
        }
    }

    /// Reads an integer, or an integral float, as a value of `expected`.
    fn integer(&self, expected: &str, in_key: bool) -> DecodeResult<i64> {
        match *self {
            InputNode::Int(i) => Ok(i),
            InputNode::Float(f) => match integral_float_to_int(f) {
                Some(i) => Ok(i),
                None if f.is_finite() && f.fract() == 0.0 && !in_key => Err(DecodeFail::Mismatch("integer out of range".into())),
                None => Err(self.mismatch(expected, in_key)),
            },
            InputNode::Str(s) if in_key => s.parse().map_err(|_| invalid_key(s)),
            _ => Err(self.mismatch(expected, in_key)),
        }
    }

    pub(crate) fn int(&self, in_key: bool) -> DecodeResult<i64> {
        self.integer("int", in_key)
    }

    pub(crate) fn float(&self, in_key: bool) -> DecodeResult<f64> {
        match *self {
            InputNode::Float(f) => Ok(f),
            InputNode::Int(i) => Ok(i as f64),
            _ => Err(self.mismatch("float", in_key)),
        }
    }

    /// Reads a duration, or an integer count of nanoseconds.
    pub(crate) fn duration(&self, in_key: bool) -> DecodeResult<i64> {
        match *self {
            InputNode::Duration(d) => i64::try_from(d.as_nanos()).map_err(|_| DecodeFail::Mismatch("integer out of range".into())),
            _ => self.integer("duration", in_key),
        }
    }

    pub(crate) fn str(&self, in_key: bool) -> DecodeResult<&'a str> {
        match *self {
            InputNode::Str(s) => Ok(s),
            _ => Err(self.mismatch("str", in_key)),
        }
    }

    /// Reads bytes, or their standard Base64 text.
    pub(crate) fn bytes(&self, in_key: bool) -> DecodeResult<Cow<'a, [u8]>> {
        match *self {
            InputNode::Bytes(b) => Ok(Cow::Borrowed(b)),
            InputNode::Str(s) => base64::decode(s, false).map(Cow::Owned).map_err(|_| DecodeFail::Mismatch("invalid base64".into())),
            _ => Err(self.mismatch("bytes", in_key)),
        }
    }

    /// Reads the text of a value of the native value type `name`.
    pub(crate) fn native_text(&self, name: &str, in_key: bool) -> DecodeResult<&'a str> {
        match *self {
            InputNode::Native(_, text) | InputNode::Str(text) => Ok(text),
            _ => Err(self.mismatch(name, in_key)),
        }
    }
}

/// The error for the map key text `key` that the key type cannot read.
pub(crate) fn invalid_key(key: &str) -> DecodeFail {
    DecodeFail::Mismatch(format!("invalid map key `{}`", crate::stdlib::truncate(key, QUOTED_KEY)))
}

/// Reads a `Range` from the entries of its JSON form, an object with the
/// fields `start`, `end`, and `inclusive`. A key that is not a `str` is an
/// error.
pub(crate) fn range_of<'a>(entries: impl Iterator<Item = (InputNode<'a>, InputNode<'a>)>) -> DecodeResult<(i64, i64, bool)> {
    let (mut start, mut end, mut inclusive) = (None, None, false);
    for (key, value) in entries {
        match key {
            InputNode::Str("start") => start = Some(value.int(false)?),
            InputNode::Str("end") => end = Some(value.int(false)?),
            InputNode::Str("inclusive") => inclusive = value.bool(false)?,
            InputNode::Str(_) => {}
            key => return Err(key.mismatch("str key", false)),
        }
    }
    match (start, end) {
        (Some(s), Some(e)) => Ok((s, e, inclusive)),
        (None, _) => Err(DecodeFail::Mismatch("missing field `start`".into())),
        _ => Err(DecodeFail::Mismatch("missing field `end`".into())),
    }
}

/// The message for a tuple or positional payload without `len` elements.
pub(crate) fn length_mismatch(len: usize) -> String {
    format!("expected list of length {len}")
}
