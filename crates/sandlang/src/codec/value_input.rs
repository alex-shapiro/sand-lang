//! The rules of host [`Value`] input, and [`ValueReader`], which reads a
//! `Value` with them.
//!
//! A host `Value` can have the data model of the target type, or its JSON
//! form, such as a Base64 `Value::Str` for `bytes`. The Rust `Decode` and the
//! Sand `Decoder` of a `Value` read through `ValueReader`, so that both
//! accept the same forms and give the same messages.

use super::{DecodeFail, MAX_NESTING_DEPTH, QUOTED_KEY, SeqKind};
use crate::stdlib::base64;
use crate::value::{
    Decode, DecodeError, PathSegment, Value, integral_float_to_int, is_json_variant, is_option_variant, key_text, kind_name,
    without_json_variant,
};
use std::borrow::Cow;

/// A node of the input that does not match the type that a read expects,
/// with the message of the error.
#[derive(Debug)]
pub(crate) struct InputMismatch(pub(crate) String);

impl From<InputMismatch> for DecodeFail {
    fn from(m: InputMismatch) -> DecodeFail {
        DecodeFail::Mismatch(m.0)
    }
}

/// The result of a read of a scalar node.
pub(crate) type InputResult<T> = Result<T, InputMismatch>;

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
        })
    }

    /// The error for a node that is not a value of `expected`.
    pub(crate) fn mismatch(&self, expected: &str) -> InputMismatch {
        InputMismatch(format!("expected {expected}, found {}", self.kind()))
    }

    pub(crate) fn unit(&self) -> InputResult<()> {
        match self {
            InputNode::Unit => Ok(()),
            _ => Err(self.mismatch("()")),
        }
    }

    pub(crate) fn bool(&self, in_key: bool) -> InputResult<bool> {
        match *self {
            InputNode::Bool(b) => Ok(b),
            InputNode::Str(s) if in_key && (s == "true" || s == "false") => Ok(s == "true"),
            _ => Err(self.mismatch("bool")),
        }
    }

    /// Reads an integer, or an integral float, as a value of `expected`.
    fn integer(&self, expected: &str, in_key: bool) -> InputResult<i64> {
        match *self {
            InputNode::Int(i) => Ok(i),
            InputNode::Float(f) => match integral_float_to_int(f) {
                Some(i) => Ok(i),
                None if f.is_finite() && f.fract() == 0.0 && !in_key => Err(InputMismatch("integer out of range".into())),
                None => Err(self.mismatch(expected)),
            },
            InputNode::Str(s) if in_key => s.parse().map_err(|_| invalid_key(s)),
            _ => Err(self.mismatch(expected)),
        }
    }

    pub(crate) fn int(&self, in_key: bool) -> InputResult<i64> {
        self.integer("int", in_key)
    }

    pub(crate) fn float(&self) -> InputResult<f64> {
        match *self {
            InputNode::Float(f) => Ok(f),
            InputNode::Int(i) => Ok(i as f64),
            _ => Err(self.mismatch("float")),
        }
    }

    /// Reads a duration, or an integer count of nanoseconds.
    pub(crate) fn duration(&self, in_key: bool) -> InputResult<i64> {
        match *self {
            InputNode::Duration(d) => i64::try_from(d.as_nanos()).map_err(|_| InputMismatch("integer out of range".into())),
            _ => self.integer("duration", in_key),
        }
    }

    pub(crate) fn str(&self) -> InputResult<&'a str> {
        match *self {
            InputNode::Str(s) => Ok(s),
            _ => Err(self.mismatch("str")),
        }
    }

    /// Reads bytes, or their standard Base64 text.
    pub(crate) fn bytes(&self) -> InputResult<Cow<'a, [u8]>> {
        match *self {
            InputNode::Bytes(b) => Ok(Cow::Borrowed(b)),
            InputNode::Str(s) => base64::decode(s, false).map(Cow::Owned).map_err(|_| InputMismatch("invalid base64".into())),
            _ => Err(self.mismatch("bytes")),
        }
    }

    /// Reads the text of a value of the native value type `name`.
    pub(crate) fn native_text(&self, name: &str) -> InputResult<&'a str> {
        match *self {
            InputNode::Native(_, text) | InputNode::Str(text) => Ok(text),
            _ => Err(self.mismatch(name)),
        }
    }
}

/// The error for the map key text `key` that the key type cannot read.
pub(crate) fn invalid_key(key: &str) -> InputMismatch {
    InputMismatch(format!("invalid map key `{}`", crate::stdlib::truncate(key, QUOTED_KEY)))
}

/// Reads a `Range` from the entries of its JSON form, an object with the
/// fields `start`, `end`, and `inclusive`. A key that is not a `str` is an
/// error.
pub(crate) fn range_of<'a>(entries: impl Iterator<Item = (InputNode<'a>, InputNode<'a>)>) -> InputResult<(i64, i64, bool)> {
    let (mut start, mut end, mut inclusive) = (None, None, false);
    for (key, value) in entries {
        match key {
            InputNode::Str("start") => start = Some(value.int(false)?),
            InputNode::Str("end") => end = Some(value.int(false)?),
            InputNode::Str("inclusive") => inclusive = value.bool(false)?,
            InputNode::Str(_) => {}
            key => return Err(key.mismatch("str key")),
        }
    }
    match (start, end) {
        (Some(s), Some(e)) => Ok((s, e, inclusive)),
        (None, _) => Err(InputMismatch("missing field `start`".into())),
        _ => Err(InputMismatch("missing field `end`".into())),
    }
}

/// The message for a tuple or positional payload without `len` elements.
pub(crate) fn length_mismatch(len: usize) -> String {
    format!("expected list of length {len}")
}

/// A failure of a [`ValueReader`] operation.
#[derive(Debug)]
pub(crate) enum ReadFail {
    /// The input does not match the target type.
    Mismatch(String),
    /// The caller used the reader out of order, such as `next_item` without
    /// an open sequence.
    Misuse(&'static str),
}

impl From<InputMismatch> for ReadFail {
    fn from(m: InputMismatch) -> ReadFail {
        ReadFail::Mismatch(m.0)
    }
}

impl From<ReadFail> for DecodeFail {
    fn from(f: ReadFail) -> DecodeFail {
        match f {
            ReadFail::Mismatch(m) => DecodeFail::Mismatch(m),
            ReadFail::Misuse(what) => DecodeFail::Fail(crate::vm::Fail::internal(what)),
        }
    }
}

pub(crate) type ReadResult<T> = Result<T, ReadFail>;

/// A step from a container to one of its parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// An item of a list, tuple, or set, a field of a struct, or an element
    /// of a variant payload.
    Part(usize),
    /// The key of a map entry.
    Key(usize),
    /// The value of a map entry.
    Value(usize),
}

/// The parts of an open container of a [`ValueReader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parts {
    /// The items of a list, tuple, or set.
    Items,
    /// The entries of a map, each a key and then a value.
    Entries,
    /// The fields of a struct.
    Fields,
    /// The entries of a map in the form of a struct, with `str` keys.
    MapFields,
    /// The elements of the positional payload of a `Value::Variant`.
    Payload,
    /// The fields of the named payload of a `Value::Variant`.
    PayloadFields,
    /// The single entry of a map in the form of a variant, whose value is the
    /// payload.
    VariantEntry,
    /// The payload of an `Option.Some` variant.
    Some,
}

/// An open container of a [`ValueReader`].
struct Open<'v> {
    /// The container, with its `json.Json` variants. The reader of an owned
    /// value moves each open container out of its parent, and puts it back
    /// when the container closes.
    node: Cow<'v, Value>,
    parts: Parts,
    /// The index of the next part.
    next: usize,
    len: usize,
    /// The current part, if any.
    at: Option<Step>,
}

/// The state of a [`ValueReader`] that a mark keeps.
#[derive(Clone, Copy)]
struct ReaderMark {
    open: usize,
    next: usize,
    at: Option<Step>,
    ready: bool,
    payload: bool,
    key: Option<usize>,
}

/// Reads a host [`Value`] part by part, with the rules of host `Value`
/// input. Both the Rust [`Decode`](crate::value::Decode) and a Sand
/// `Decoder` over a `Value` read through it, so they accept the same forms
/// and give the same messages.
///
/// A `Decode` implementation usually reads one value with
/// [`ValueReader::decode`]. A failed `decode` leaves the reader at the start
/// of its value, so the implementation can read the value as another type.
pub struct ValueReader<'v> {
    root: Cow<'v, Value>,
    open: Vec<Open<'v>>,
    /// Whether the current part waits for a read.
    ready: bool,
    /// Whether the current part is a `Value::Variant` whose payload the next
    /// operation reads.
    payload: bool,
    /// The index of the open map whose current key the reader reads.
    key: Option<usize>,
    marks: Vec<ReaderMark>,
}

static UNIT: Value = Value::Unit;

/// Returns part `step` of `node`, without the `json.Json` variants of `node`.
fn part(node: &Value, step: Step) -> &Value {
    let found = match (without_json_variant(node), step) {
        (Value::List(items) | Value::Tuple(items) | Value::Set(items), Step::Part(i)) => items.get(i),
        (Value::Struct(s), Step::Part(i)) => s.fields.get(i),
        (Value::Variant(v), Step::Part(i)) => v.payload.get(i),
        (Value::Map(entries), Step::Key(i)) => entries.get(i).map(|e| &e.0),
        (Value::Map(entries), Step::Value(i)) => entries.get(i).map(|e| &e.1),
        _ => None,
    };
    found.unwrap_or(&UNIT)
}

/// Returns part `step` of `node` to change, without the `json.Json` variants
/// of `node`.
fn part_mut(node: &mut Value, step: Step) -> Option<&mut Value> {
    match (without_json_variant_mut(node), step) {
        (Value::List(items) | Value::Tuple(items) | Value::Set(items), Step::Part(i)) => items.get_mut(i),
        (Value::Struct(s), Step::Part(i)) => s.fields.get_mut(i),
        (Value::Variant(v), Step::Part(i)) => v.payload.get_mut(i),
        (Value::Map(entries), Step::Key(i)) => entries.get_mut(i).map(|e| &mut e.0),
        (Value::Map(entries), Step::Value(i)) => entries.get_mut(i).map(|e| &mut e.1),
        _ => None,
    }
}

/// Returns the JSON form of `value` to change, as [`without_json_variant`]
/// does.
fn without_json_variant_mut(value: &mut Value) -> &mut Value {
    if !matches!(&*value, Value::Variant(v) if is_json_variant(v) && !v.payload.is_empty()) {
        return value;
    }
    match value {
        Value::Variant(v) => without_json_variant_mut(&mut v.payload[0]),
        value => value,
    }
}

impl<'v> ValueReader<'v> {
    /// Returns a reader of `value`.
    pub fn new(value: &'v Value) -> ValueReader<'v> {
        ValueReader::of(Cow::Borrowed(value))
    }

    fn of(root: Cow<'v, Value>) -> ValueReader<'v> {
        ValueReader { root, open: Vec::new(), ready: true, payload: false, key: None, marks: Vec::new() }
    }

    /// Reads one value of type `T`. An error from `T` gets the path of the
    /// value, and a failed read leaves the reader at the start of the value.
    pub fn decode<T: Decode>(&mut self) -> Result<T, DecodeError> {
        let mark = self.mark();
        let r = T::decode(self).map_err(|e| {
            self.rewind(mark);
            if e.has_path() { e } else { e.at(self.path()) }
        });
        self.release(mark);
        r
    }

    /// Reads the current value whole.
    pub fn read_value(&mut self) -> Result<&Value, DecodeError> {
        self.node().map_err(|e| self.fail(e))?;
        self.ready = false;
        Ok(self.current())
    }

    /// Returns an error with `message` and the path of the current value.
    pub fn error(&self, message: impl Into<String>) -> DecodeError {
        DecodeError::new(message).at(self.path())
    }

    /// Returns the error of the failed operation `f`, with the path of the
    /// current part.
    pub(crate) fn fail(&self, f: ReadFail) -> DecodeError {
        match f {
            ReadFail::Mismatch(m) => self.error(m),
            ReadFail::Misuse(what) => self.error(format!("the decode used the reader out of order: {what}")),
        }
    }

    /// Returns the current part, with its `json.Json` variants.
    fn current(&self) -> &Value {
        match self.open.last() {
            None => &self.root,
            Some(Open { node, at: Some(step), .. }) => part(node, *step),
            Some(_) => &UNIT,
        }
    }

    /// Returns the current part, which the next operation reads.
    pub(crate) fn node(&self) -> ReadResult<&Value> {
        if !self.ready {
            return Err(ReadFail::Misuse("no value to read"));
        }
        Ok(without_json_variant(self.current()))
    }

    /// Returns `true` if the reader reads a map key.
    fn in_key(&self) -> bool {
        self.key.is_some()
    }

    /// Returns the error for the current part, `node`, which is not a value
    /// of `expected`. In a map key, it is the error of an invalid key.
    fn mismatch(&self, node: &Value, expected: &str) -> ReadFail {
        match self.key_root() {
            Some(key) => invalid_key(&key_text(key)).into(),
            None => InputNode::of(node).mismatch(expected).into(),
        }
    }

    /// Returns the map key that the reader reads, if any.
    fn key_root(&self) -> Option<&Value> {
        let k = self.key?;
        // An open container in the key is the key, which it moved out of the
        // map if the reader owns its value.
        if let Some(key) = self.open.get(k + 1) {
            return Some(&key.node);
        }
        let map = &self.open[k];
        Some(part(&map.node, map.at?))
    }

    /// Returns the result of the scalar rule `read` for the current part,
    /// without a change to the reader.
    pub(crate) fn peek<'s, T>(&'s self, read: impl FnOnce(InputNode<'s>, bool) -> InputResult<T>) -> ReadResult<T> {
        let node = self.node()?;
        // Only a whole key has a key text, such as `"1"` for the `int` key `1`.
        let whole_key = self.key.is_some_and(|k| k + 1 == self.open.len());
        // Each failure in a map key is the failure of the whole key.
        read(InputNode::of(node), whole_key).map_err(|e| match self.key_root() {
            Some(key) => invalid_key(&key_text(key)).into(),
            None => e.into(),
        })
    }

    /// Reads the current part with the scalar rule `read`.
    pub(crate) fn read<T>(&mut self, read: impl FnOnce(InputNode<'_>, bool) -> InputResult<T>) -> ReadResult<T> {
        let r = self.peek(read)?;
        self.ready = false;
        Ok(r)
    }

    /// Reads a range, or the entries of its JSON form.
    pub(crate) fn range(&mut self) -> ReadResult<(i64, i64, bool)> {
        let in_key = self.in_key();
        self.read(|node, _| match node {
            InputNode::Range(start, end, inclusive) => Ok((start, end, inclusive)),
            InputNode::Value(Value::Map(entries)) if !in_key => range_of(entries.iter().map(|(k, v)| (InputNode::of(k), InputNode::of(v)))),
            node => Err(node.mismatch("Range")),
        })
    }

    /// Opens the current part as a container of `len` `parts`.
    fn enter(&mut self, parts: Parts, len: usize) -> ReadResult<()> {
        if self.open.len() >= MAX_NESTING_DEPTH {
            return Err(ReadFail::Mismatch("value too deeply nested to decode".into()));
        }
        let node = match self.open.last_mut() {
            None => std::mem::replace(&mut self.root, Cow::Borrowed(&UNIT)),
            Some(parent) => {
                let Some(step) = parent.at else { return Err(ReadFail::Misuse("no value to open")) };
                match &mut parent.node {
                    Cow::Borrowed(node) => Cow::Borrowed(part(node, step)),
                    Cow::Owned(node) => Cow::Owned(part_mut(node, step).map_or(Value::Unit, |p| std::mem::replace(p, Value::Unit))),
                }
            }
        };
        self.open.push(Open { node, parts, next: 0, len, at: None });
        (self.ready, self.payload) = (false, false);
        Ok(())
    }

    /// Closes the innermost container, and puts it back in its parent.
    fn close(&mut self) {
        let Some(open) = self.open.pop() else { return };
        if self.key == Some(self.open.len()) {
            self.key = None;
        }
        match self.open.last_mut() {
            None => self.root = open.node,
            Some(parent) => {
                if let (Cow::Owned(node), Cow::Owned(parent_node), Some(step)) = (open.node, &mut parent.node, parent.at)
                    && let Some(slot) = part_mut(parent_node, step)
                {
                    *slot = node;
                }
            }
        }
    }

    /// Closes the `Some` payloads above the innermost other container, whose
    /// values the reader read.
    fn close_options(&mut self) {
        while self.open.last().is_some_and(|o| o.parts == Parts::Some) {
            self.close();
        }
    }

    /// Moves to the next part of the innermost container, which must have
    /// one of `parts`, with `step`. Returns the index of the part, or `None`
    /// after the last, which closes the container.
    fn advance(&mut self, parts: &[Parts], step: fn(usize) -> Step) -> ReadResult<Option<usize>> {
        self.close_options();
        let Some(top) = self.open.last_mut().filter(|o| parts.contains(&o.parts)) else {
            return Err(ReadFail::Misuse("no open container"));
        };
        (self.payload, self.key) = (false, None);
        if top.next >= top.len {
            // A variant payload stays open until `end_variant`, so that a
            // later error has the path of the variant.
            match top.parts {
                Parts::Payload | Parts::PayloadFields => top.at = None,
                _ => self.close(),
            }
            self.ready = false;
            return Ok(None);
        }
        let i = top.next;
        top.next += 1;
        top.at = Some(step(i));
        self.ready = true;
        Ok(Some(i))
    }

    /// Reads whether an `Option` is `Some`, and moves to its payload. `()`
    /// and `Option.None` are `None`.
    pub fn some(&mut self) -> Result<bool, DecodeError> {
        self.read_some().map_err(|e| self.fail(e))
    }

    pub(crate) fn read_some(&mut self) -> ReadResult<bool> {
        if self.payload {
            return Ok(true);
        }
        match self.node()? {
            Value::Unit => {
                self.ready = false;
                Ok(false)
            }
            Value::Variant(option) if is_option_variant(option) && option.payload.len() <= 1 => {
                if option.payload.is_empty() {
                    self.ready = false;
                    return Ok(false);
                }
                self.enter(Parts::Some, 1)?;
                if let Some(top) = self.open.last_mut() {
                    (top.next, top.at) = (1, Some(Step::Part(0)));
                }
                self.ready = true;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    /// Opens a list.
    pub fn begin_list(&mut self) -> Result<(), DecodeError> {
        self.open_seq(SeqKind::List).map_err(|e| self.fail(e))
    }

    /// Opens a set, or a list as a set.
    pub fn begin_set(&mut self) -> Result<(), DecodeError> {
        self.open_seq(SeqKind::Set).map_err(|e| self.fail(e))
    }

    /// Opens a tuple of `len` elements, a list of `len` items as a tuple, or
    /// the positional payload of the current variant.
    pub fn begin_tuple(&mut self, len: usize) -> Result<(), DecodeError> {
        self.open_seq(SeqKind::Tuple(len)).map_err(|e| self.fail(e))
    }

    pub(crate) fn open_seq(&mut self, kind: SeqKind) -> ReadResult<()> {
        if self.payload {
            // The payload of a `Value::Variant` is open since `read_variant`.
            let Some(top) = self.open.last().filter(|o| o.parts == Parts::Payload) else { return Err(ReadFail::Misuse("no open payload")) };
            return match kind {
                SeqKind::Tuple(n) if n == top.len => {
                    self.payload = false;
                    Ok(())
                }
                SeqKind::Tuple(n) => Err(ReadFail::Mismatch(length_mismatch(n))),
                _ => Err(ReadFail::Misuse("a payload is a tuple")),
            };
        }
        let node = self.node()?;
        let (parts, len) = match (kind, node) {
            (SeqKind::List, Value::List(items)) => (Parts::Items, items.len()),
            (SeqKind::Set, Value::Set(items) | Value::List(items)) => (Parts::Items, items.len()),
            (SeqKind::Tuple(_), Value::Tuple(items) | Value::List(items)) => (Parts::Items, items.len()),
            (SeqKind::List, v) => return Err(self.mismatch(v, "list")),
            (SeqKind::Set, v) => return Err(self.mismatch(v, "set")),
            (SeqKind::Tuple(n), v) => return Err(self.mismatch(v, &format!("tuple of {n} elements"))),
        };
        if let SeqKind::Tuple(n) = kind
            && len != n
        {
            return Err(match self.key_root() {
                Some(key) => invalid_key(&key_text(key)).into(),
                None => ReadFail::Mismatch(length_mismatch(n)),
            });
        }
        self.enter(parts, len)
    }

    /// Moves to the next item of the innermost sequence. Returns `false`
    /// after the last.
    pub fn next_item(&mut self) -> Result<bool, DecodeError> {
        self.advance_item().map_err(|e| self.fail(e))
    }

    pub(crate) fn advance_item(&mut self) -> ReadResult<bool> {
        let key = self.key;
        let more = self.advance(&[Parts::Items, Parts::Payload], Step::Part)?.is_some();
        // The parts of a tuple key are in the key.
        self.key = key.filter(|&k| k < self.open.len());
        Ok(more)
    }

    /// Reads the next element of the innermost tuple of `len` elements.
    pub fn element<T: Decode>(&mut self, len: usize) -> Result<T, DecodeError> {
        if !self.next_item()? {
            return Err(self.error(length_mismatch(len)));
        }
        self.decode()
    }

    /// Closes the innermost tuple of `len` elements, which must have no more
    /// elements.
    pub fn end_tuple(&mut self, len: usize) -> Result<(), DecodeError> {
        if self.next_item()? {
            return Err(self.error(length_mismatch(len)));
        }
        Ok(())
    }

    /// Opens a map.
    pub fn begin_map(&mut self) -> Result<(), DecodeError> {
        self.open_map().map_err(|e| self.fail(e))
    }

    pub(crate) fn open_map(&mut self) -> ReadResult<()> {
        match self.node()? {
            Value::Map(entries) if !self.payload => {
                let len = entries.len();
                self.enter(Parts::Entries, len)
            }
            v => Err(self.mismatch(v, "map")),
        }
    }

    /// Moves to the key of the next entry of the innermost map. Returns the
    /// text of the key for a path, or `None` after the last entry.
    pub(crate) fn next_entry(&mut self) -> ReadResult<Option<Cow<'_, str>>> {
        let Some(i) = self.advance(&[Parts::Entries], Step::Key)? else { return Ok(None) };
        self.key = Some(self.open.len() - 1);
        let key = self.open.last().map_or(&UNIT, |o| part(&o.node, Step::Key(i)));
        Ok(Some(key_text(key)))
    }

    /// Moves to the key of the next entry of the innermost map. Returns
    /// `false` after the last entry.
    pub fn next_key(&mut self) -> Result<bool, DecodeError> {
        self.next_entry().map(|key| key.is_some()).map_err(|e| self.fail(e))
    }

    /// Moves from the key to the value of the current map entry.
    pub fn map_value(&mut self) -> Result<(), DecodeError> {
        self.enter_map_value().map_err(|e| self.fail(e))
    }

    pub(crate) fn enter_map_value(&mut self) -> ReadResult<()> {
        self.close_options();
        let Some(top) = self.open.last_mut().filter(|o| o.parts == Parts::Entries && o.next > 0) else {
            return Err(ReadFail::Misuse("no open map"));
        };
        top.at = Some(Step::Value(top.next - 1));
        (self.ready, self.payload, self.key) = (true, false, None);
        Ok(())
    }

    /// Opens a struct of the type `name`, or the named payload of the current
    /// variant.
    pub(crate) fn open_fields(&mut self, name: &str) -> ReadResult<()> {
        if self.payload {
            // The payload of a `Value::Variant` is open since `read_variant`.
            let Some(top) = self.open.last_mut().filter(|o| o.parts == Parts::Payload) else {
                return Err(ReadFail::Misuse("no open payload"));
            };
            let node = without_json_variant(&top.node);
            if !matches!(node, Value::Variant(v) if v.variant.fields.is_some()) {
                return Err(InputNode::of(node).mismatch("map").into());
            }
            top.parts = Parts::PayloadFields;
            self.payload = false;
            return Ok(());
        }
        let (parts, len) = match self.node()? {
            Value::Struct(s) => (Parts::Fields, s.fields.len()),
            Value::Map(entries) => {
                if let Some((key, _)) = entries.iter().find(|(k, _)| !matches!(without_json_variant(k), Value::Str(_))) {
                    return Err(InputNode::of(key).mismatch("str key").into());
                }
                (Parts::MapFields, entries.len())
            }
            v => return Err(self.mismatch(v, if name.is_empty() { "map" } else { name })),
        };
        self.enter(parts, len)
    }

    /// Moves to the next field of the innermost struct or named payload.
    /// Returns its name, or `None` after the last field.
    pub(crate) fn next_field_name(&mut self) -> ReadResult<Option<&str>> {
        let parts = [Parts::Fields, Parts::MapFields, Parts::PayloadFields];
        let step = match self.open.last().map(|o| o.parts) {
            Some(Parts::MapFields) => Step::Value,
            _ => Step::Part,
        };
        let Some(i) = self.advance(&parts, step)? else { return Ok(None) };
        Ok(Some(self.open.last().map_or("", |o| field_name(&o.node, i))))
    }

    /// Opens a struct of the type `name`, or the named payload of the current
    /// variant.
    pub fn begin_fields(&mut self, name: &str) -> Result<(), DecodeError> {
        self.open_fields(name).map_err(|e| self.fail(e))
    }

    /// Moves to the next field of the innermost struct or named payload
    /// whose name is in `fields`, and skips the others. Returns the index of
    /// the field in `fields`, or `None` after the last field.
    pub fn next_field(&mut self, fields: &[&str]) -> Result<Option<usize>, DecodeError> {
        self.next_field_by(|name| fields.iter().position(|f| *f == name))
    }

    /// Moves to the next field for which `find` returns an index, as
    /// [`ValueReader::next_field`] does.
    pub(crate) fn next_field_by(&mut self, find: impl Fn(&str) -> Option<usize>) -> Result<Option<usize>, DecodeError> {
        loop {
            let found = match self.next_field_name() {
                Ok(None) => return Ok(None),
                Ok(Some(name)) => find(name),
                Err(e) => return Err(self.fail(e)),
            };
            match found {
                Some(i) => return Ok(Some(i)),
                None => self.skip(),
            }
        }
    }

    /// Skips the current value, such as an unknown field.
    pub fn skip(&mut self) {
        self.ready = false;
    }

    /// Reads a variant of the enum `name`. Returns its name, and `true` if a
    /// payload follows.
    pub(crate) fn read_variant(&mut self, name: &str) -> ReadResult<(&str, bool)> {
        let payload = match self.node()? {
            Value::Str(_) => false,
            Value::Variant(variant) => !variant.payload.is_empty(),
            Value::Map(entries) if entries.len() == 1 => match without_json_variant(&entries[0].0) {
                Value::Str(_) => true,
                key => return Err(InputNode::of(key).mismatch("str key").into()),
            },
            Value::Map(entries) => {
                let what = if entries.is_empty() { "expected a variant" } else { "expected a single variant" };
                return Err(ReadFail::Mismatch(format!("{what} of {name}")));
            }
            v => return Err(self.mismatch(v, name)),
        };
        match self.node()? {
            Value::Map(_) => {
                self.enter(Parts::VariantEntry, 1)?;
                if let Some(top) = self.open.last_mut() {
                    (top.next, top.at) = (1, Some(Step::Value(0)));
                }
                self.ready = true;
            }
            Value::Variant(v) if payload => {
                let len = v.payload.len();
                self.enter(Parts::Payload, len)?;
                self.payload = true;
            }
            _ => self.ready = false,
        }
        let tag = match self.open.last() {
            Some(Open { parts: Parts::VariantEntry, node, .. }) => part(node, Step::Key(0)),
            Some(Open { parts: Parts::Payload, node, .. }) if self.payload => node,
            _ => self.current(),
        };
        let tag = match without_json_variant(tag) {
            Value::Str(tag) => tag.as_str(),
            Value::Variant(variant) => variant.variant.name.as_str(),
            _ => "",
        };
        Ok((tag, payload))
    }

    /// Reads a variant of the enum `name`, whose variants have the names and
    /// the payload lengths of `variants`. Returns the index of the variant.
    /// The reader then reads its payload, if any, which
    /// [`ValueReader::end_variant`] ends.
    pub fn variant(&mut self, name: &str, variants: &[(&str, usize)]) -> Result<usize, DecodeError> {
        self.variant_by(name, |tag| variants.iter().position(|(v, _)| *v == tag).map(|i| (i, variants[i].1)))
    }

    /// Reads a variant as [`ValueReader::variant`] does. `find` returns the
    /// index and the payload length of the variant with a name.
    pub(crate) fn variant_by(&mut self, name: &str, find: impl Fn(&str) -> Option<(usize, usize)>) -> Result<usize, DecodeError> {
        let (found, payload) = match self.read_variant(name) {
            Ok((tag, payload)) => (find(tag).ok_or_else(|| tag.to_string()).map(|(i, len)| (i, len, tag.to_string())), payload),
            Err(e) => return Err(self.fail(e)),
        };
        let (i, len, tag) = match found {
            Ok(found) => found,
            Err(tag) => {
                // The error is at the path of the variant, not of its payload.
                if payload {
                    self.end_variant();
                }
                return Err(self.error(super::fast::unknown_variant(&tag, name)));
            }
        };
        match (payload, len) {
            (false, 0) => {}
            (false, _) => return Err(self.error(super::fast::needs_payload(&tag, name))),
            (true, 0) => {
                // A payload-less variant in object form has an empty payload.
                self.begin_tuple(0)?;
                self.end_tuple(0)?;
                self.end_variant();
            }
            (true, _) => {}
        }
        Ok(i)
    }

    /// Ends the payload of the variant that [`ValueReader::variant`] read.
    pub fn end_variant(&mut self) {
        self.close_options();
        if self.open.last().is_some_and(|o| matches!(o.parts, Parts::VariantEntry | Parts::Payload | Parts::PayloadFields)) {
            self.close();
        }
        (self.ready, self.payload) = (false, false);
    }

    /// Returns the path of the current part, such as `$.items[2]`. In a map
    /// key, it is the path of the map.
    fn path(&self) -> Vec<PathSegment> {
        let open = match self.key {
            Some(k) => &self.open[..=k],
            None => &self.open[..],
        };
        let mut out = Vec::new();
        for o in open {
            let node = without_json_variant(&o.node);
            let key = |s: &str| PathSegment::Key(s.to_string());
            match (o.parts, o.at) {
                (Parts::Items, Some(Step::Part(i))) => out.push(PathSegment::Index(i)),
                (Parts::Entries, Some(Step::Value(i))) => out.push(PathSegment::Key(key_text(part(node, Step::Key(i))).into_owned())),
                (Parts::Fields, Some(Step::Part(i))) | (Parts::MapFields, Some(Step::Value(i))) => out.push(key(field_name(node, i))),
                (Parts::Payload, at) => {
                    out.push(key(variant_name(node)));
                    if let Some(Step::Part(i)) = at {
                        out.push(PathSegment::Index(i));
                    }
                }
                (Parts::PayloadFields, at) => {
                    out.push(key(variant_name(node)));
                    if let Some(Step::Part(i)) = at {
                        out.push(key(field_name(node, i)));
                    }
                }
                (Parts::VariantEntry, _) => out.push(PathSegment::Key(key_text(part(node, Step::Key(0))).into_owned())),
                _ => {}
            }
        }
        out
    }

    fn save(&self) -> ReaderMark {
        let top = self.open.last();
        ReaderMark {
            open: self.open.len(),
            next: top.map_or(0, |o| o.next),
            at: top.and_then(|o| o.at),
            ready: self.ready,
            payload: self.payload,
            key: self.key,
        }
    }

    /// Keeps the state of the reader before a value, and returns the index
    /// of the mark for [`ValueReader::rewind`].
    pub(crate) fn mark(&mut self) -> usize {
        let m = self.save();
        self.marks.push(m);
        self.marks.len() - 1
    }

    /// Returns to the state of mark `mark`, after a read failed before it
    /// ended. The mark stays. A read after the mark changes only the
    /// containers that it opens and the position in the innermost one.
    pub(crate) fn rewind(&mut self, mark: usize) {
        let Some(&m) = self.marks.get(mark) else { return };
        while self.open.len() > m.open {
            self.close();
        }
        if let Some(top) = self.open.last_mut() {
            (top.next, top.at) = (m.next, m.at);
        }
        (self.ready, self.payload, self.key) = (m.ready, m.payload, m.key);
    }

    /// Forgets mark `mark` and the marks after it.
    pub(crate) fn release(&mut self, mark: usize) {
        self.marks.truncate(mark);
    }

    /// Returns to the start of the input.
    pub(crate) fn rewind_to_start(&mut self) {
        while !self.open.is_empty() {
            self.close();
        }
        (self.ready, self.payload, self.key) = (true, false, None);
    }

    /// Bytes of Rust memory that the reader holds besides its value.
    pub(crate) fn heap_size(&self) -> usize {
        self.open.capacity() * size_of::<Open<'_>>() + self.marks.capacity() * size_of::<ReaderMark>()
    }
}

impl ValueReader<'static> {
    /// Returns a reader that owns `value`.
    pub(crate) fn owned(value: Value) -> ValueReader<'static> {
        ValueReader::of(Cow::Owned(value))
    }
}

/// Returns the name of the variant `node`, or the key of the map `node` in
/// the form of a variant.
fn variant_name(node: &Value) -> &str {
    match node {
        Value::Variant(v) => &v.variant.name,
        _ => "",
    }
}

/// Returns the name of field `i` of the struct, named payload, or map
/// `node`.
fn field_name(node: &Value, i: usize) -> &str {
    let name = match without_json_variant(node) {
        Value::Struct(s) => s.ty.fields.get(i).map(String::as_str),
        Value::Variant(v) => v.variant.fields.as_ref().and_then(|names| names.get(i)).map(String::as_str),
        Value::Map(entries) => entries.get(i).and_then(|(k, _)| match without_json_variant(k) {
            Value::Str(k) => Some(k.as_str()),
            _ => None,
        }),
        _ => None,
    };
    name.unwrap_or_default()
}
