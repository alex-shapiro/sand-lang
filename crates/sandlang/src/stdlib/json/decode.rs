//! The decoder that implements the Sand `Decode` trait.
//!
//! [`Decoder`] builds a VM value of a reflected type from a [`DecodeSource`].
//! The sources are JSON text for `json.decode` and a [`Value`] from the host.
//! The decoder owns the parts that both sources share: error paths, field
//! defaults, computed-default thunks, and deferred set and map fills.

use super::{Builder, Decoded, Error, JsonIds, Number, PResult, Parser, is_json, json_number};
use crate::native::ty::VmVal;
use crate::stdlib::core;
use crate::stdlib::{base64, truncate};
use crate::types::ConstVal;
use crate::value::{DecodeError, Value, VariantValue, is_option_variant, key_text, kind_name, without_json_variant};
use crate::vm::bytecode::RtIds;
use crate::vm::compare::hash_key;
use crate::vm::container_children::MAX_NESTING_DEPTH;
use crate::vm::meter::Meter;
use crate::vm::value::{FieldDefault, FieldDesc, FxHashMap, Obj, ReflectVariantDesc, TypeDesc, VmValue};
use crate::vm::{Fail, Vm};
use std::borrow::Cow;

/// The input of a [`Decoder`].
///
/// A source holds a cursor at the next input value. The decoder asks for the
/// value at the cursor in the form of the target type, such as an `int` or the
/// items of a list. A method returns `Ok(None)` or `Ok(false)` if the value at
/// the cursor does not have that form. The decoder then reports a mismatch with
/// [`DecodeSource::found`].
pub(super) trait DecodeSource<'a> {
    /// The word for a sequence in length errors, such as `array` in "expected
    /// array of length 2".
    const SEQUENCE: &'static str;

    /// Moves the cursor to the start of the next value, and charges fuel for
    /// the input that the source read since the last charge.
    fn begin_value(&mut self, meter: &Meter) -> Result<(), Fail>;

    /// Charges fuel for the input that the source read since the last charge.
    fn charge(&mut self, meter: &Meter) -> Result<(), Fail>;

    /// Checks that no input follows the decoded value.
    fn finish(&mut self) -> PResult<()>;

    /// Describes the value at the cursor for a mismatch error.
    fn found(&self) -> Found;

    /// Reads a unit value, which is JSON `null`.
    fn null(&mut self) -> PResult<bool>;

    fn bool(&mut self) -> PResult<Option<bool>>;

    /// Reads a number. A `Value` source also reads a `duration` if `duration`
    /// is true.
    fn number(&mut self, duration: bool) -> PResult<Option<Number<'a>>>;

    fn str(&mut self) -> PResult<Option<Cow<'a, str>>>;

    fn bytes(&mut self) -> PResult<Option<BytesInput<'a>>>;

    /// Reads the text of a native value type, such as `2024-03-10` for a
    /// `time.PlainDate`.
    fn value_text(&mut self) -> PResult<Option<Cow<'a, str>>>;

    /// Reads one layer of an `Option`.
    fn option_layer(&mut self) -> PResult<OptionLayer>;

    /// Reads a range in its exact form. JSON text has no exact form, and the
    /// decoder then reads the range as an object.
    fn range(&mut self) -> Option<(i64, i64, bool)>;

    /// Opens the sequence at the cursor for [`DecodeSource::next_item`].
    fn open_sequence(&mut self, kind: SequenceKind) -> PResult<bool>;

    /// Moves the cursor to item `index` of the innermost open sequence, or
    /// closes the sequence and returns `false` if no items remain.
    fn next_item(&mut self, index: usize) -> PResult<bool>;

    /// Opens the object at the cursor for [`DecodeSource::next_key`].
    fn open_object(&mut self, kind: ObjectKind) -> PResult<bool>;

    /// Reads key `index` of the innermost open object and moves the cursor to
    /// its value, or closes the object and returns `None` if no keys remain.
    fn next_key(&mut self, index: usize) -> PResult<Option<KeyInput<'a>>>;

    /// Reads key `index` of the innermost open map for a key type without a
    /// text form, such as a tuple. A `Value` source moves the cursor to the
    /// key, and [`DecodeSource::enter_entry_value`] then moves it to the
    /// value.
    fn next_whole_key(&mut self, index: usize) -> PResult<Option<KeyInput<'a>>>;

    /// Moves the cursor from the key that [`DecodeSource::next_whole_key`]
    /// read to its value.
    fn enter_entry_value(&mut self);

    /// Skips the value at the cursor.
    fn skip(&mut self) -> PResult<()>;

    /// Reads an enum variant.
    fn variant(&mut self) -> PResult<Option<VariantInput<'a>>>;

    /// Opens the payload at the cursor, as an object if `named` is true and
    /// the input has names, or else as a sequence.
    fn open_payload(&mut self, named: bool) -> PResult<Option<PayloadForm>>;

    /// Reads the kind of the value at the cursor for a `json.Json` target, and
    /// opens it if it is an array or an object.
    fn json_node(&mut self) -> PResult<Option<JsonNode<'a>>>;
}

/// A description of the value at the cursor of a [`DecodeSource`].
pub(super) enum Found {
    /// A JSON value starts at the cursor.
    Json,
    /// The JSON text at the cursor is not valid.
    Syntax(Error),
    /// A `Value` of the named kind is at the cursor, such as `str` or
    /// `shop.Order`.
    Value(String),
}

pub(super) enum BytesInput<'a> {
    Base64(Cow<'a, str>),
    Raw(&'a [u8]),
}

pub(super) enum OptionLayer {
    /// The input is `None`.
    None,
    /// The input is `Some`, and the cursor moved to its payload.
    Some,
    /// The input is the payload of every remaining `Some` layer. JSON
    /// represents `Some(x)` as `x`.
    Payload,
}

pub(super) enum SequenceKind {
    List,
    Set,
    Tuple,
}

pub(super) enum ObjectKind {
    Struct,
    Map,
    Range,
}

/// A key of an object, in the form of the input.
#[derive(Clone)]
pub(super) enum KeyInput<'a> {
    Text(Cow<'a, str>),
    Int(i64),
    Bool(bool),
    /// A `duration` in nanoseconds.
    Duration(i64),
    /// A payload-less variant by name.
    Variant(&'a str),
    /// A `Value` key of another kind. After
    /// [`DecodeSource::next_whole_key`], the cursor is at this key.
    Value(&'a Value),
}

impl KeyInput<'_> {
    /// Returns the JSON object key text of `self`.
    fn text(&self) -> Cow<'_, str> {
        match self {
            KeyInput::Text(text) => Cow::Borrowed(text),
            KeyInput::Int(i) | KeyInput::Duration(i) => Cow::Owned(i.to_string()),
            KeyInput::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            KeyInput::Variant(name) => Cow::Borrowed(name),
            KeyInput::Value(key) => key_text(key),
        }
    }

    fn kind(&self) -> Cow<'_, str> {
        match self {
            KeyInput::Text(_) => "str".into(),
            KeyInput::Int(_) => "int".into(),
            KeyInput::Bool(_) => "bool".into(),
            KeyInput::Duration(_) => "duration".into(),
            KeyInput::Variant(name) => Cow::Borrowed(name),
            KeyInput::Value(key) => kind_name(key),
        }
    }
}

pub(super) enum VariantInput<'a> {
    /// A variant name without a payload, such as JSON `"Red"`.
    Name(Cow<'a, str>),
    /// An open object whose single key is the variant name, such as JSON
    /// `{"Circle": [2.0]}`.
    Object,
    /// A variant with a payload. The cursor is at the payload.
    Payload(Cow<'a, str>),
}

pub(super) enum PayloadForm {
    Named,
    Positional,
}

pub(super) enum JsonNode<'a> {
    Null,
    Bool(bool),
    Number(Number<'a>),
    Str(Cow<'a, str>),
    Array,
    Object,
}

/// The type or form that a mismatch error names.
#[derive(Clone, Copy)]
enum Expected<'t> {
    Type(&'t TypeDesc),
    Payload { named: bool },
    Json,
}

/// The part of a container that a decode error is in.
enum Seg<'a> {
    Key(KeyInput<'a>),
    Index(usize),
}

impl Error {
    /// Moves a decode error into `seg`.
    #[cold]
    fn in_seg(self, seg: Seg<'_>) -> Error {
        let in_seg = |e: DecodeError| match seg {
            Seg::Key(key) => e.in_key(key.text()),
            Seg::Index(i) => e.in_index(i),
        };
        match self {
            Error::Mismatch(e) => Error::Mismatch(in_seg(e)),
            Error::Invalid(e) => Error::Invalid(in_seg(e)),
            e => e,
        }
    }
}

/// Elements and entries to insert into a decoded set or map after the heap
/// holds them, because hashing reads the heap.
enum Fill {
    Set(VmValue, Vec<VmValue>),
    Map(VmValue, Vec<(VmValue, VmValue)>),
}

const INDEXED_FIELDS: usize = 8;
const LOOKUPS_PER_UNIT: usize = 16;
const QUOTED_KEY: usize = 64;
/// Strings longer than this many bytes are checked against the memory limit
/// before the decoder copies them.
const CHECKED_TEXT_LEN: usize = 1024;

fn lookup<T>(meter: &Meter, items: &[T], pred: impl Fn(&T) -> bool) -> Result<Option<usize>, Fail> {
    meter.charge(1 + (items.len() / LOOKUPS_PER_UNIT) as u64)?;
    Ok(items.iter().position(pred))
}

/// Builds a VM value of a reflected type from a [`DecodeSource`].
pub(super) struct Decoder<'a, 'd, 's, 'h, S> {
    src: S,
    b: Builder<'a, 's, 'h>,
    ids: RtIds,
    stack: Vec<&'d TypeDesc>,
    slots: Vec<Option<VmValue>>,
    indexes: FxHashMap<u32, FxHashMap<&'d str, usize>>,
    fills: Vec<Fill>,
    thunks: Vec<VmValue>,
}

/// The value that [`Decoder::run`] decoded and the list of thunks that compute
/// its missing field defaults, if any.
type DecodedParts = (VmValue, Option<VmValue>);

impl<'a, 'd, 's, 'h, S: DecodeSource<'a>> Decoder<'a, 'd, 's, 'h, S> {
    pub(super) fn new(src: S, b: Builder<'a, 's, 'h>, ids: RtIds) -> Self {
        let (stack, slots, indexes, fills, thunks) = Default::default();
        Decoder { src, b, ids, stack, slots, indexes, fills, thunks }
    }

    /// Decodes the whole input as `desc`.
    fn run(mut self, desc: &'d TypeDesc) -> (PResult<DecodedParts>, Vec<Fill>) {
        let r = self.decode(desc).and_then(|v| self.src.finish().map(|()| v));
        let r = self.src.charge(self.b.meter).map_err(Error::Fail).and(r);
        let r = r.map(|v| (v, (!self.thunks.is_empty()).then(|| self.b.list(std::mem::take(&mut self.thunks)))));
        (r, self.fills)
    }

    /// Decodes the whole input as a `json.Json` value.
    pub(super) fn run_json(mut self, ids: &JsonIds) -> PResult<VmValue> {
        let r = self.decode_json(ids).and_then(|v| self.src.finish().map(|()| v));
        self.src.charge(self.b.meter).map_err(Error::Fail).and(r)
    }

    fn type_name(&self, t: &TypeDesc) -> String {
        t.format(&|id| {
            let found = self.stack.iter().rev().find_map(|d| match d {
                TypeDesc::Struct { id: i, name, .. } | TypeDesc::Enum { id: i, name, .. } if *i == id => Some(name),
                _ => None,
            });
            found.cloned().unwrap_or_default()
        })
    }

    #[cold]
    fn fail(&self, msg: &str) -> Error {
        Error::Mismatch(DecodeError::new(msg))
    }

    fn unsupported(&self, what: &str) -> Error {
        Error::Invalid(DecodeError::new(format!("cannot decode {what}")))
    }

    #[cold]
    fn mismatch(&self, expected: Expected<'_>) -> Error {
        match self.src.found() {
            Found::Syntax(e) => e,
            Found::Json => self.fail(&format!("expected {}", json_word(expected))),
            Found::Value(kind) => {
                let expected = match expected {
                    Expected::Type(t) => self.type_name(t),
                    Expected::Payload { named: true } => "list or map".into(),
                    Expected::Payload { named: false } => "list".into(),
                    Expected::Json => "Json".into(),
                };
                self.fail(&format!("expected {expected}, found {kind}"))
            }
        }
    }

    #[inline]
    fn with_seg<T>(&mut self, seg: Seg<'a>, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        f(self).map_err(|e| e.in_seg(seg))
    }

    /// Calls `f` for each item of the open sequence.
    #[inline]
    fn items(&mut self, mut f: impl FnMut(&mut Self, usize) -> PResult<()>) -> PResult<()> {
        let mut i = 0;
        while self.src.next_item(i)? {
            f(self, i)?;
            i += 1;
        }
        Ok(())
    }

    /// Calls `f` for each key of the open object.
    #[inline]
    fn entries(&mut self, mut f: impl FnMut(&mut Self, KeyInput<'a>) -> PResult<()>) -> PResult<()> {
        let mut i = 0;
        while let Some(key) = self.src.next_key(i)? {
            f(self, key)?;
            i += 1;
        }
        Ok(())
    }

    /// Returns the text of a struct field name or variant name.
    #[inline]
    fn text_key(&self, key: KeyInput<'a>) -> PResult<Cow<'a, str>> {
        match key {
            KeyInput::Text(text) => Ok(text),
            key => Err(self.key_kind_error(&key)),
        }
    }

    #[cold]
    fn key_kind_error(&self, key: &KeyInput<'_>) -> Error {
        self.fail(&format!("expected str key, found {}", key.kind()))
    }

    #[inline]
    fn open_sequence(&mut self, t: &TypeDesc, kind: SequenceKind) -> PResult<()> {
        if self.src.open_sequence(kind)? { Ok(()) } else { Err(self.mismatch(Expected::Type(t))) }
    }

    #[inline]
    fn int(&mut self, t: &TypeDesc, duration: bool) -> PResult<i64> {
        let Some(n) = self.src.number(duration)? else { return Err(self.mismatch(Expected::Type(t))) };
        n.int().ok_or_else(|| {
            let f = n.float();
            match (f.is_finite() && f.fract() == 0.0, n) {
                (true, _) => self.fail("integer out of range"),
                (false, Number::Text { .. }) => self.fail("expected int"),
                (false, _) => self.fail("expected int, found float"),
            }
        })
    }

    #[inline]
    fn bool(&mut self) -> PResult<bool> {
        self.src.bool()?.ok_or_else(|| self.mismatch(Expected::Type(&TypeDesc::Bool)))
    }

    /// Copies `s` into a new `str`, after a memory check if `s` is long.
    #[inline]
    fn new_str(&mut self, s: &str) -> PResult<VmValue> {
        if s.len() > CHECKED_TEXT_LEN {
            self.b.st.check_memory(self.b.pending.saturating_add(s.len()))?;
        }
        Ok(self.b.st.new_str(s))
    }

    fn decode(&mut self, t: &'d TypeDesc) -> PResult<VmValue> {
        self.src.begin_value(self.b.meter)?;
        self.b.reserve()?;
        match t {
            TypeDesc::Int => Ok(VmValue::Int(self.int(t, false)?)),
            TypeDesc::Duration => Ok(VmValue::Dur(self.int(t, true)?)),
            TypeDesc::Float => match self.src.number(false)? {
                Some(n) => Ok(VmValue::Float(n.float())),
                None => Err(self.mismatch(Expected::Type(t))),
            },
            TypeDesc::Bool => Ok(VmValue::Bool(self.bool()?)),
            TypeDesc::Unit => match self.src.null()? {
                true => Ok(VmValue::Unit),
                false => Err(self.mismatch(Expected::Type(t))),
            },
            TypeDesc::Str => match self.src.str()? {
                Some(s) => self.new_str(&s),
                None => Err(self.mismatch(Expected::Type(t))),
            },
            TypeDesc::Bytes => {
                let b: Box<[u8]> = match self.src.bytes()? {
                    Some(BytesInput::Base64(s)) => base64::decode(&s, false).map_err(|_| self.fail("invalid base64"))?.into(),
                    Some(BytesInput::Raw(b)) => {
                        if b.len() > CHECKED_TEXT_LEN {
                            self.b.st.check_memory(self.b.pending.saturating_add(b.len()))?;
                        }
                        b.into()
                    }
                    None => return Err(self.mismatch(Expected::Type(t))),
                };
                Ok(self.b.st.new_bytes(b))
            }
            TypeDesc::Option(_) => self.decode_option(t),
            TypeDesc::List(elem) => {
                self.open_sequence(t, SequenceKind::List)?;
                let mut items = Vec::new();
                self.items(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(elem))?;
                    Ok(d.b.push(&mut items, v)?)
                })?;
                Ok(self.b.list(items))
            }
            TypeDesc::Set(elem, fill) => {
                self.open_sequence(t, SequenceKind::Set)?;
                let mut items = Vec::new();
                let thunks = self.thunks.len();
                self.items(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(elem))?;
                    Ok(d.b.push(&mut items, v)?)
                })?;
                let set = self.b.st.alloc(Obj::Set(Box::default()));
                if self.thunks.len() == thunks {
                    self.fills.push(Fill::Set(set, items));
                } else {
                    // Defer set insertion until element field default thunks have run, so elements are hashed with their initialized fields.
                    let items = self.b.list(items);
                    self.thunk(*fill, Box::new([set, items]))?;
                }
                Ok(set)
            }
            TypeDesc::Tuple(elems) => {
                self.open_sequence(t, SequenceKind::Tuple)?;
                let length_error = |d: &Self| d.fail(&format!("expected {} of length {}", S::SEQUENCE, elems.len()));
                let mut items = Vec::with_capacity(elems.len());
                self.items(|d, i| {
                    let Some(elem) = elems.get(i) else { return Err(length_error(d)) };
                    items.push(d.with_seg(Seg::Index(i), |d| d.decode(elem))?);
                    Ok(())
                })?;
                if items.len() != elems.len() {
                    return Err(length_error(self));
                }
                Ok(self.b.st.new_tuple(items))
            }
            TypeDesc::Map(key_type, value_type, fill) => {
                if !self.src.open_object(ObjectKind::Map)? {
                    return Err(self.mismatch(Expected::Type(t)));
                }
                if !has_text_keys(key_type) {
                    return self.decode_map_with_whole_keys(key_type, value_type, *fill);
                }
                let mut m = self.b.start_map();
                self.entries(|d, key| {
                    let seg = Seg::Key(key.clone());
                    let (k, hash, text) = d.map_key(key_type, key)?;
                    let v = d.with_seg(seg, |d| d.decode(value_type))?;
                    Ok(d.b.put(&mut m, hash, text, k, v)?)
                })?;
                Ok(self.b.map(m))
            }
            TypeDesc::Struct { id, def, fields, .. } => self.decode_struct(t, *id, *def, fields),
            TypeDesc::Enum { variants, .. } if is_json(t) => self.decode_json(&JsonIds::at(variants.first().map_or(0, |v| v.runtime_id))),
            TypeDesc::Enum { name, variants, .. } => {
                self.stack.push(t);
                let r = self.decode_enum(t, name, variants);
                self.stack.pop();
                r
            }
            TypeDesc::Recur(id) => {
                self.b.meter.charge(1 + (self.stack.len() / LOOKUPS_PER_UNIT) as u64)?;
                let found = self.stack.iter().rev().find(|d| match d {
                    TypeDesc::Struct { id: i, .. } | TypeDesc::Enum { id: i, .. } => i == id,
                    _ => false,
                });
                match found {
                    Some(&d) => self.decode(d),
                    None => Err(Error::Fail(Fail::internal("json.decode: unresolved recursive type"))),
                }
            }
            TypeDesc::Value { parse, .. } => {
                let Some(s) = self.src.value_text()? else { return Err(self.mismatch(Expected::Type(t))) };
                self.b.meter.charge_bytes(s.len())?;
                match parse(&s) {
                    Ok(v) => Ok(self.b.st.alloc(v)),
                    Err(msg) => Err(self.fail(&msg)),
                }
            }
            TypeDesc::Range => self.decode_range(t),
            TypeDesc::Unsupported(name) => Err(self.unsupported(&format!("values of type {name}"))),
            TypeDesc::Param(_) => Err(Error::Fail(Fail::internal("json.decode: uninstantiated type parameter"))),
        }
    }

    /// Decodes the open map, whose keys have no text form, and inserts its
    /// entries after the heap holds them, because hashing reads the heap.
    fn decode_map_with_whole_keys(&mut self, key_type: &'d TypeDesc, value_type: &'d TypeDesc, fill: u32) -> PResult<VmValue> {
        let mut keys_and_values = Vec::new();
        let thunks = self.thunks.len();
        let mut i = 0;
        while let Some(key) = self.src.next_whole_key(i)? {
            let seg = || Seg::Key(key.clone());
            let k = match &key {
                KeyInput::Value(_) => {
                    let k = self.decode(key_type).map_err(|e| e.in_seg(seg()))?;
                    self.src.enter_entry_value();
                    k
                }
                KeyInput::Text(text) if matches!(key_type, TypeDesc::Enum { .. }) => self.map_key_text(key_type, text.clone())?.0,
                _ => return Err(self.unsupported(&format!("map keys of type {}", self.type_name(key_type)))),
            };
            let v = self.decode(value_type).map_err(|e| e.in_seg(seg()))?;
            self.b.push(&mut keys_and_values, k)?;
            self.b.push(&mut keys_and_values, v)?;
            i += 1;
        }
        let map = self.b.st.alloc(Obj::Map(Box::default()));
        if self.thunks.len() == thunks {
            let entries = keys_and_values.as_chunks::<2>().0.iter().map(|&[k, v]| (k, v)).collect();
            self.b.pending = self.b.pending.saturating_sub(keys_and_values.capacity() * std::mem::size_of::<VmValue>());
            self.fills.push(Fill::Map(map, entries));
        } else {
            // Defer map insertion until key field default thunks have run, so keys are hashed with their initialized fields.
            let keys_and_values = self.b.list(keys_and_values);
            self.thunk(fill, Box::new([map, keys_and_values]))?;
        }
        Ok(map)
    }

    fn decode_option(&mut self, t: &'d TypeDesc) -> PResult<VmValue> {
        // Handle nested `Option` types iteratively because they do not open containers.
        let (mut inner, mut layers) = (t, 0);
        while let TypeDesc::Option(e) = inner {
            (inner, layers) = (e, layers + 1);
        }
        self.b.meter.charge(layers)?;
        let mut somes = 0;
        let mut v = loop {
            if somes == layers {
                break self.decode(inner)?;
            }
            match self.src.option_layer()? {
                OptionLayer::None => break VmValue::Tag(self.ids.none),
                OptionLayer::Some => somes += 1,
                OptionLayer::Payload => {
                    somes = layers;
                    break self.decode(inner)?;
                }
            }
        };
        for _ in 0..somes {
            self.b.reserve()?;
            v = self.b.st.new_variant(self.ids.some, &[v]);
        }
        Ok(v)
    }

    fn decode_json(&mut self, ids: &JsonIds) -> PResult<VmValue> {
        self.src.begin_value(self.b.meter)?;
        self.b.reserve()?;
        let Some(node) = self.src.json_node()? else { return Err(self.mismatch(Expected::Json)) };
        Ok(match node {
            JsonNode::Null => VmValue::Tag(ids.null),
            JsonNode::Bool(b) => self.b.st.new_variant(ids.bool_, &[VmValue::Bool(b)]),
            JsonNode::Number(n) => json_number(self.b.st, ids, n.value()),
            JsonNode::Str(s) => {
                let s = self.new_str(&s)?;
                self.b.st.new_variant(ids.str_, &[s])
            }
            JsonNode::Array => {
                let mut items = Vec::new();
                self.items(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode_json(ids))?;
                    Ok(d.b.push(&mut items, v)?)
                })?;
                let list = self.b.list(items);
                self.b.st.new_variant(ids.arr, &[list])
            }
            JsonNode::Object => {
                let mut m = self.b.start_map();
                self.entries(|d, key| {
                    let key = d.text_key(key)?;
                    let v = d.decode_json(ids).map_err(|e| e.in_seg(Seg::Key(KeyInput::Text(Cow::Borrowed(&key)))))?;
                    let hash = d.b.hash(&key)?;
                    let k = d.b.key(&key, hash);
                    Ok(d.b.put(&mut m, hash, Some(key), k, v)?)
                })?;
                let map = self.b.map(m);
                self.b.st.new_variant(ids.obj, &[map])
            }
        })
    }

    fn decode_struct(&mut self, t: &'d TypeDesc, id: u32, def: u32, fields: &'d [FieldDesc]) -> PResult<VmValue> {
        if !self.src.open_object(ObjectKind::Struct)? {
            return Err(self.mismatch(Expected::Type(t)));
        }
        self.stack.push(t);
        let base = self.slots.len();
        self.slots.resize(base + fields.len(), None);
        let mut next = 0;
        let r = self.entries(|d, key| {
            let key = d.text_key(key)?;
            match d.field_index(id, fields, &key, next)? {
                Some(i) => {
                    next = i + 1;
                    d.slots[base + i] = Some(d.with_seg(Seg::Key(KeyInput::Text(key)), |d| d.decode(&fields[i].1))?);
                    Ok(())
                }
                None => d.src.skip(),
            }
        });
        self.stack.pop();
        if let Err(e) = r {
            self.slots.truncate(base);
            return Err(e);
        }
        let mut values = Vec::with_capacity(fields.len());
        let mut computed: Vec<(u32, i64)> = Vec::new();
        for (i, (field, ty, default)) in fields.iter().enumerate() {
            values.push(match (self.slots[base + i], default, &**ty) {
                (Some(v), _, _) => v,
                (None, FieldDefault::Const(c), _) => match self.constant(c) {
                    Ok(v) => v,
                    Err(e) => {
                        self.slots.truncate(base);
                        return Err(e);
                    }
                },
                (None, FieldDefault::Computed { fill, bit }, _) => {
                    match computed.last_mut() {
                        Some((f, mask)) if f == fill => *mask |= 1 << bit,
                        _ => computed.push((*fill, 1 << bit)),
                    }
                    VmValue::Unit
                }
                (None, FieldDefault::None, TypeDesc::Option(_)) => VmValue::Tag(self.ids.none),
                (None, FieldDefault::None, _) => {
                    self.slots.truncate(base);
                    return Err(self.fail(&format!("missing field `{field}`")));
                }
            });
        }
        self.slots.truncate(base);
        let s = self.b.st.alloc(Obj::Struct(def, values.into_boxed_slice()));
        for (fill, mask) in computed {
            self.thunk(fill, Box::new([s, VmValue::Int(mask)]))?;
        }
        Ok(s)
    }

    fn thunk(&mut self, proto: u32, captures: Box<[VmValue]>) -> PResult<()> {
        self.b.reserve()?;
        let c = self.b.st.alloc(Obj::Closure(proto, captures));
        Ok(self.b.push(&mut self.thunks, c)?)
    }

    fn constant(&mut self, c: &ConstVal) -> PResult<VmValue> {
        self.b.meter.charge(1)?;
        self.b.reserve()?;
        let all = |d: &mut Self, xs: &[ConstVal]| xs.iter().map(|x| d.constant(x)).collect::<PResult<Vec<_>>>();
        Ok(match c {
            ConstVal::Unit => VmValue::Unit,
            ConstVal::Bool(b) => VmValue::Bool(*b),
            ConstVal::Int(i) => VmValue::Int(*i),
            ConstVal::Float(f) => VmValue::Float(*f),
            ConstVal::Dur(d) => VmValue::Dur(*d),
            ConstVal::Str(s) => {
                self.b.meter.charge_bytes(s.len())?;
                self.b.st.new_str(s.as_str())
            }
            ConstVal::Bytes(b) => {
                self.b.meter.charge_bytes(b.len())?;
                self.b.st.new_bytes(b.as_slice())
            }
            ConstVal::Range(a, b, inclusive) => self.b.st.alloc(Obj::Range(*a, *b, *inclusive)),
            ConstVal::Tuple(xs) => {
                let items = all(self, xs)?;
                self.b.st.new_tuple(items)
            }
            ConstVal::List(xs) => {
                let items = all(self, xs)?;
                self.b.st.new_list(items)
            }
            ConstVal::Set(xs) => {
                let items = all(self, xs)?;
                let set = self.b.st.alloc(Obj::Set(Box::default()));
                self.fills.push(Fill::Set(set, items));
                set
            }
            ConstVal::Map(kvs) => {
                let mut entries = Vec::with_capacity(kvs.len());
                for (k, v) in kvs {
                    entries.push((self.constant(k)?, self.constant(v)?));
                }
                let map = self.b.st.alloc(Obj::Map(Box::default()));
                self.fills.push(Fill::Map(map, entries));
                map
            }
            ConstVal::Variant(id, payload) => {
                let payload = all(self, payload)?;
                self.b.st.new_variant(*id, &payload)
            }
            ConstVal::Struct(def, fields) => {
                let fields = all(self, fields)?;
                self.b.st.alloc(Obj::Struct(*def, fields.into_boxed_slice()))
            }
        })
    }

    fn decode_range(&mut self, t: &TypeDesc) -> PResult<VmValue> {
        if let Some((start, end, inclusive)) = self.src.range() {
            return Ok(self.b.st.alloc(Obj::Range(start, end, inclusive)));
        }
        if !self.src.open_object(ObjectKind::Range)? {
            return Err(self.mismatch(Expected::Type(t)));
        }
        let (mut start, mut end, mut inclusive) = (None, None, false);
        self.entries(|d, key| {
            let key = d.text_key(key)?;
            match &*key {
                "start" => start = Some(d.with_seg(Seg::Key(KeyInput::Text(key)), |d| d.int(&TypeDesc::Int, false))?),
                "end" => end = Some(d.with_seg(Seg::Key(KeyInput::Text(key)), |d| d.int(&TypeDesc::Int, false))?),
                "inclusive" => inclusive = d.with_seg(Seg::Key(KeyInput::Text(key)), |d| d.bool())?,
                _ => d.src.skip()?,
            }
            Ok(())
        })?;
        let (Some(start), Some(end)) = (start, end) else {
            return Err(self.fail(&format!("missing field `{}`", if start.is_none() { "start" } else { "end" })));
        };
        Ok(self.b.st.alloc(Obj::Range(start, end, inclusive)))
    }

    fn field_index(&mut self, id: u32, fields: &'d [FieldDesc], key: &str, guess: usize) -> Result<Option<usize>, Fail> {
        if fields.get(guess).is_some_and(|f| f.0 == key) {
            return Ok(Some(guess));
        }
        if fields.len() <= INDEXED_FIELDS {
            return lookup(self.b.meter, fields, |f| f.0 == key);
        }
        self.b.meter.charge(1)?;
        if !self.indexes.contains_key(&id) {
            self.b.meter.charge(fields.len() as u64)?;
            self.b.st.check_memory(self.b.pending.saturating_add(fields.len().saturating_mul(32)))?;
            self.b.pending += fields.len() * 32;
            let index = fields.iter().enumerate().rev().map(|(i, f)| (f.0.as_str(), i)).collect();
            self.indexes.insert(id, index);
        }
        Ok(self.indexes[&id].get(key).copied())
    }

    /// Converts `key` into a map key of type `t`, and returns the key, its
    /// hash, and its text for string comparison.
    fn map_key(&mut self, t: &TypeDesc, key: KeyInput<'a>) -> PResult<(VmValue, u64, Option<Cow<'a, str>>)> {
        let k = match (t, key) {
            (_, KeyInput::Text(text)) => return self.map_key_text(t, text),
            (TypeDesc::Enum { .. }, KeyInput::Variant(name)) => return self.map_key_text(t, Cow::Borrowed(name)),
            (TypeDesc::Int, KeyInput::Int(i)) => VmValue::Int(i),
            (TypeDesc::Duration, KeyInput::Duration(nanos)) => VmValue::Dur(nanos),
            (TypeDesc::Bool, KeyInput::Bool(b)) => VmValue::Bool(b),
            (TypeDesc::Duration, KeyInput::Value(Value::Duration(_))) => return Err(self.fail("integer out of range")),
            (TypeDesc::Str | TypeDesc::Int | TypeDesc::Duration | TypeDesc::Bool | TypeDesc::Enum { .. } | TypeDesc::Value { .. }, key) => {
                return Err(self.fail(&format!("expected {} key, found {}", self.type_name(t), key.kind())));
            }
            _ => return Err(self.unsupported(&format!("map keys of type {}", self.type_name(t)))),
        };
        Ok((k, hash_key(self.b.heap, self.b.meter, k)?, None))
    }

    fn map_key_text(&mut self, t: &TypeDesc, key: Cow<'a, str>) -> PResult<(VmValue, u64, Option<Cow<'a, str>>)> {
        let quoted = truncate(&key, QUOTED_KEY);
        let bad = |d: &Self| d.fail(&format!("invalid map key `{quoted}`"));
        let k = match t {
            TypeDesc::Str => {
                let hash = self.b.hash(&key)?;
                let k = self.b.key(&key, hash);
                return Ok((k, hash, Some(key)));
            }
            TypeDesc::Int => VmValue::Int(key.parse().map_err(|_| bad(self))?),
            TypeDesc::Duration => VmValue::Dur(key.parse().map_err(|_| bad(self))?),
            TypeDesc::Bool => match &*key {
                "true" => VmValue::Bool(true),
                "false" => VmValue::Bool(false),
                _ => return Err(bad(self)),
            },
            TypeDesc::Enum { variants, .. } => match lookup(self.b.meter, variants, |v| v.name == *key && v.payload_types.is_empty())? {
                Some(i) => VmValue::Tag(variants[i].runtime_id),
                None => return Err(bad(self)),
            },
            TypeDesc::Value { parse, .. } => {
                self.b.meter.charge_bytes(key.len())?;
                let v = parse(&key).map_err(|msg| self.fail(&format!("invalid map key `{quoted}`: {msg}")))?;
                let Some(x) = v.value() else { return Err(Error::Fail(Fail::internal("json.decode: not a value"))) };
                let (hash, text) = (x.value_hash(), x.text());
                return Ok((self.b.st.alloc(v), hash, Some(Cow::Owned(text))));
            }
            _ => return Err(self.unsupported(&format!("map keys of type {}", self.type_name(t)))),
        };
        Ok((k, hash_key(self.b.heap, self.b.meter, k)?, None))
    }

    fn decode_enum(&mut self, t: &TypeDesc, name: &str, variants: &'d [ReflectVariantDesc]) -> PResult<VmValue> {
        let find = |d: &Self, tag: &str| match lookup(d.b.meter, variants, |v| v.name == tag)? {
            Some(i) => Ok(&variants[i]),
            None => Err(d.fail(&format!("unknown variant `{}` of {name}", truncate(tag, QUOTED_KEY)))),
        };
        match self.src.variant()? {
            Some(VariantInput::Name(tag)) => {
                let v = find(self, &tag)?;
                if !v.payload_types.is_empty() {
                    return Err(self.fail(&format!("variant `{tag}` of {name} needs a payload")));
                }
                Ok(VmValue::Tag(v.runtime_id))
            }
            Some(VariantInput::Payload(tag)) => {
                let v = find(self, &tag)?;
                self.with_seg(Seg::Key(KeyInput::Text(tag)), |d| d.decode_payload(v))
            }
            Some(VariantInput::Object) => {
                let mut result = None;
                self.entries(|d, tag| {
                    if result.is_some() {
                        return Err(d.fail(&format!("expected a single variant of {name}")));
                    }
                    let tag = d.text_key(tag)?;
                    let v = find(d, &tag)?;
                    result = Some(d.with_seg(Seg::Key(KeyInput::Text(tag)), |d| d.decode_payload(v))?);
                    Ok(())
                })?;
                result.ok_or_else(|| self.fail(&format!("expected a variant of {name}")))
            }
            None => Err(self.mismatch(Expected::Type(t))),
        }
    }

    fn decode_payload(&mut self, v: &'d ReflectVariantDesc) -> PResult<VmValue> {
        let ReflectVariantDesc { runtime_id: id, payload_types: types, field_names: names, .. } = v;
        let mut slots: Vec<Option<VmValue>> = vec![None; types.len()];
        match (self.src.open_payload(names.is_some())?, names) {
            (Some(PayloadForm::Named), Some(names)) => {
                self.entries(|d, key| {
                    let key = d.text_key(key)?;
                    match lookup(d.b.meter, names, |n| *n == *key)? {
                        Some(i) => {
                            slots[i] = Some(d.with_seg(Seg::Key(KeyInput::Text(key)), |d| d.decode(&types[i]))?);
                            Ok(())
                        }
                        None => d.src.skip(),
                    }
                })?;
                for (i, slot) in slots.iter_mut().enumerate() {
                    if slot.is_none() {
                        if !matches!(*types[i], TypeDesc::Option(_)) {
                            return Err(self.fail(&format!("missing field `{}`", names[i])));
                        }
                        *slot = Some(VmValue::Tag(self.ids.none));
                    }
                }
            }
            (Some(PayloadForm::Positional), _) => {
                let length_error = |d: &Self| d.fail(&format!("expected {} of length {}", S::SEQUENCE, types.len()));
                let mut n = 0;
                self.items(|d, i| {
                    let Some(ty) = types.get(i) else { return Err(length_error(d)) };
                    slots[i] = Some(d.with_seg(Seg::Index(i), |d| d.decode(ty))?);
                    n += 1;
                    Ok(())
                })?;
                if n != types.len() {
                    return Err(length_error(self));
                }
            }
            _ => return Err(self.mismatch(Expected::Payload { named: names.is_some() })),
        }
        let fields: Vec<VmValue> = slots.into_iter().map(|s| s.unwrap_or(VmValue::Unit)).collect();
        Ok(self.b.st.new_variant(*id, &fields))
    }
}

/// Returns `true` if a JSON object key can hold each key of type `t`.
fn has_text_keys(t: &TypeDesc) -> bool {
    match t {
        TypeDesc::Str | TypeDesc::Int | TypeDesc::Duration | TypeDesc::Bool | TypeDesc::Value { .. } => true,
        TypeDesc::Enum { variants, .. } => variants.iter().all(|v| v.payload_types.is_empty()),
        _ => false,
    }
}

/// Returns the JSON word for `expected` in a mismatch error.
fn json_word(expected: Expected<'_>) -> Cow<'static, str> {
    let t = match expected {
        Expected::Type(t) => t,
        Expected::Payload { named: true } => return "object".into(),
        Expected::Payload { named: false } => return "array".into(),
        Expected::Json => return "Json".into(),
    };
    match t {
        TypeDesc::Int | TypeDesc::Duration => "int".into(),
        TypeDesc::Float => "float".into(),
        TypeDesc::Bool => "bool".into(),
        TypeDesc::Unit => "null".into(),
        TypeDesc::Str => "str".into(),
        TypeDesc::Bytes => "base64 str".into(),
        TypeDesc::List(_) | TypeDesc::Set(..) => "array".into(),
        TypeDesc::Tuple(elems) => format!("array of length {}", elems.len()).into(),
        TypeDesc::Map(..) | TypeDesc::Struct { .. } | TypeDesc::Range => "object".into(),
        TypeDesc::Enum { name, .. } | TypeDesc::Value { name, .. } => name.clone().into(),
        TypeDesc::Option(_) | TypeDesc::Recur(_) | TypeDesc::Unsupported(_) | TypeDesc::Param(_) => "value".into(),
    }
}

/// Decodes JSON `text` as `desc`.
pub(super) fn decode_json_text(vm: &mut Vm, desc: &TypeDesc, text: VmVal<String>) -> Result<Decoded<VmValue>, Fail> {
    let (ids, meter) = (vm.prog.ids, &vm.meter);
    let (r, fills) = vm.heap.staged(|heap, st| Decoder::new(Parser::new(text.str(heap)), Builder::new(heap, st, meter), ids).run(desc));
    finish_decode(vm, r, fills)
}

/// Decodes `value` as `desc`. The value can also have the JSON form of `desc`,
/// such as a base64 `Value::Str` for `bytes`.
#[cfg_attr(not(test), expect(dead_code, reason = "host function calls decode their arguments with it"))]
pub(crate) fn decode_value(vm: &mut Vm, desc: &TypeDesc, value: &Value) -> Result<Decoded<VmValue>, Fail> {
    let (ids, meter) = (vm.prog.ids, &vm.meter);
    let (r, fills) = vm.heap.staged(|heap, st| Decoder::new(ValueSource::new(value), Builder::new(heap, st, meter), ids).run(desc));
    finish_decode(vm, r, fills)
}

fn finish_decode(vm: &mut Vm, r: PResult<DecodedParts>, fills: Vec<Fill>) -> Result<Decoded<VmValue>, Fail> {
    if r.is_ok() {
        for fill in fills {
            match fill {
                Fill::Set(set, items) => {
                    for x in items {
                        vm.set_add(set, x)?;
                    }
                }
                Fill::Map(map, entries) => {
                    for (k, v) in entries {
                        vm.map_insert(map, k, v)?;
                    }
                }
            }
        }
    }
    let failed = |kind: &str, message| Ok(Decoded::Failed(core::Error { kind: kind.into(), message }));
    match r {
        Ok((v, None)) => Ok(Decoded::Done(v)),
        Ok((v, Some(thunks))) => Ok(Decoded::Pending(v, VmVal::from_raw(thunks))),
        Err(Error::Parse(msg)) => failed("parse", msg),
        Err(Error::Mismatch(e)) => failed("parse", e.to_string()),
        Err(Error::Invalid(e)) => failed("invalid", e.to_string()),
        Err(Error::Fail(f)) => Err(f),
    }
}

/// A [`DecodeSource`] that reads a [`Value`].
///
/// The source accepts the exact form of each type, and the form that JSON
/// parsing produces, such as a `Value::Map` with `str` keys for a struct. It
/// reads each `json.Json` variant as its JSON form.
struct ValueSource<'v> {
    at: Node<'v>,
    /// The open containers, from outermost to innermost.
    open: Vec<OpenContainer<'v>>,
    /// The value of the map entry whose key is at the cursor.
    entry_value: Option<&'v Value>,
    /// Bytes of strings and byte slices read since the last fuel charge.
    unpaid_bytes: usize,
}

#[derive(Clone, Copy)]
enum Node<'v> {
    Value(&'v Value),
    /// The payload of a variant value.
    Payload(&'v VariantValue),
}

enum OpenContainer<'v> {
    Items(std::slice::Iter<'v, Value>),
    Entries(std::slice::Iter<'v, (Value, Value)>),
    Fields(std::iter::Zip<std::slice::Iter<'v, String>, std::slice::Iter<'v, Value>>),
}

impl<'v> ValueSource<'v> {
    fn new(value: &'v Value) -> Self {
        ValueSource { at: Node::Value(value), open: Vec::new(), entry_value: None, unpaid_bytes: 0 }
    }

    /// Returns the value at the cursor in its JSON form if it is a
    /// `json.Json` variant, or `None` if the cursor is at a variant payload.
    fn value(&self) -> Option<&'v Value> {
        match self.at {
            Node::Value(v) => Some(without_json_variant(v)),
            Node::Payload(_) => None,
        }
    }

    fn open(&mut self, container: OpenContainer<'v>) -> PResult<bool> {
        if self.open.len() >= MAX_NESTING_DEPTH {
            return Err(Error::Parse("value too deeply nested to decode".into()));
        }
        self.open.push(container);
        Ok(true)
    }

    fn read_text(&mut self, text: &'v str) -> Cow<'v, str> {
        self.unpaid_bytes += text.len();
        Cow::Borrowed(text)
    }
}

impl<'a, 'v: 'a> DecodeSource<'a> for ValueSource<'v> {
    const SEQUENCE: &'static str = "list";

    fn begin_value(&mut self, meter: &Meter) -> Result<(), Fail> {
        meter.charge(1)?;
        self.charge(meter)
    }

    fn charge(&mut self, meter: &Meter) -> Result<(), Fail> {
        meter.charge_bytes(std::mem::take(&mut self.unpaid_bytes))
    }

    fn finish(&mut self) -> PResult<()> {
        Ok(())
    }

    fn found(&self) -> Found {
        match (self.value(), self.at) {
            (Some(v), _) => Found::Value(kind_name(v).into_owned()),
            (None, Node::Payload(p)) => Found::Value(format!("payload of {}", p.variant.name)),
            (None, Node::Value(_)) => Found::Value("value".into()),
        }
    }

    fn null(&mut self) -> PResult<bool> {
        Ok(matches!(self.value(), Some(Value::Unit)))
    }

    fn bool(&mut self) -> PResult<Option<bool>> {
        Ok(match self.value() {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        })
    }

    fn number(&mut self, duration: bool) -> PResult<Option<Number<'a>>> {
        Ok(match self.value() {
            Some(Value::Int(i)) => Some(Number::Int(*i)),
            Some(Value::Float(f)) => Some(Number::Float(*f)),
            Some(Value::Duration(d)) if duration => Some(match i64::try_from(d.as_nanos()) {
                Ok(nanos) => Number::Int(nanos),
                Err(_) => Number::Float(d.as_nanos() as f64),
            }),
            _ => None,
        })
    }

    fn str(&mut self) -> PResult<Option<Cow<'a, str>>> {
        Ok(match self.value() {
            Some(Value::Str(s)) => Some(self.read_text(s)),
            _ => None,
        })
    }

    fn bytes(&mut self) -> PResult<Option<BytesInput<'a>>> {
        Ok(match self.value() {
            Some(Value::Bytes(b)) => {
                self.unpaid_bytes += b.len();
                Some(BytesInput::Raw(b))
            }
            Some(Value::Str(s)) => Some(BytesInput::Base64(self.read_text(s))),
            _ => None,
        })
    }

    fn value_text(&mut self) -> PResult<Option<Cow<'a, str>>> {
        Ok(match self.value() {
            Some(Value::Native(native)) => Some(Cow::Borrowed(native.text.as_str())),
            Some(Value::Str(s)) => Some(Cow::Borrowed(s.as_str())),
            _ => None,
        })
    }

    fn option_layer(&mut self) -> PResult<OptionLayer> {
        Ok(match self.value() {
            Some(Value::Unit) => OptionLayer::None,
            Some(Value::Variant(v)) if is_option_variant(v) => match &*v.payload {
                [] => OptionLayer::None,
                [payload] => {
                    self.at = Node::Value(payload);
                    OptionLayer::Some
                }
                _ => OptionLayer::Payload,
            },
            _ => OptionLayer::Payload,
        })
    }

    fn range(&mut self) -> Option<(i64, i64, bool)> {
        match self.value() {
            Some(Value::Range { start, end, inclusive }) => Some((*start, *end, *inclusive)),
            _ => None,
        }
    }

    fn open_sequence(&mut self, kind: SequenceKind) -> PResult<bool> {
        let items = match (kind, self.value()) {
            (SequenceKind::List | SequenceKind::Set | SequenceKind::Tuple, Some(Value::List(items)))
            | (SequenceKind::Set, Some(Value::Set(items)))
            | (SequenceKind::Tuple, Some(Value::Tuple(items))) => items,
            _ => return Ok(false),
        };
        self.open(OpenContainer::Items(items.iter()))
    }

    fn next_item(&mut self, _: usize) -> PResult<bool> {
        let Some(OpenContainer::Items(items)) = self.open.last_mut() else {
            return Err(Error::Fail(Fail::internal("decode: no open sequence")));
        };
        match items.next() {
            Some(item) => {
                self.at = Node::Value(item);
                Ok(true)
            }
            None => {
                self.open.pop();
                Ok(false)
            }
        }
    }

    fn open_object(&mut self, kind: ObjectKind) -> PResult<bool> {
        match (kind, self.value()) {
            (ObjectKind::Struct, Some(Value::Struct(s))) => self.open(OpenContainer::Fields(s.ty.fields.iter().zip(s.fields.iter()))),
            (_, Some(Value::Map(entries))) => self.open(OpenContainer::Entries(entries.iter())),
            _ => Ok(false),
        }
    }

    fn next_key(&mut self, _: usize) -> PResult<Option<KeyInput<'a>>> {
        let (key, value) = match self.open.last_mut() {
            Some(OpenContainer::Fields(fields)) => match fields.next() {
                Some((name, value)) => (KeyInput::Text(Cow::Borrowed(name.as_str())), value),
                None => {
                    self.open.pop();
                    return Ok(None);
                }
            },
            Some(OpenContainer::Entries(entries)) => match entries.next() {
                Some((key, value)) => (self.key_input(key), value),
                None => {
                    self.open.pop();
                    return Ok(None);
                }
            },
            _ => return Err(Error::Fail(Fail::internal("decode: no open object"))),
        };
        self.at = Node::Value(value);
        Ok(Some(key))
    }

    fn next_whole_key(&mut self, _: usize) -> PResult<Option<KeyInput<'a>>> {
        let Some(OpenContainer::Entries(entries)) = self.open.last_mut() else {
            return Err(Error::Fail(Fail::internal("decode: no open map")));
        };
        let Some((key, value)) = entries.next() else {
            self.open.pop();
            return Ok(None);
        };
        (self.at, self.entry_value) = (Node::Value(key), Some(value));
        Ok(Some(KeyInput::Value(key)))
    }

    fn enter_entry_value(&mut self) {
        if let Some(value) = self.entry_value.take() {
            self.at = Node::Value(value);
        }
    }

    fn skip(&mut self) -> PResult<()> {
        Ok(())
    }

    fn variant(&mut self) -> PResult<Option<VariantInput<'a>>> {
        Ok(Some(match self.value() {
            Some(Value::Str(name)) => VariantInput::Name(Cow::Borrowed(name.as_str())),
            Some(Value::Variant(v)) if v.payload.is_empty() => VariantInput::Name(Cow::Borrowed(v.variant.name.as_str())),
            Some(Value::Variant(v)) => {
                self.at = Node::Payload(v);
                VariantInput::Payload(Cow::Borrowed(v.variant.name.as_str()))
            }
            Some(Value::Map(entries)) => {
                self.open(OpenContainer::Entries(entries.iter()))?;
                VariantInput::Object
            }
            _ => return Ok(None),
        }))
    }

    fn open_payload(&mut self, named: bool) -> PResult<Option<PayloadForm>> {
        let (container, form) = match self.at {
            Node::Payload(v) => match (&v.variant.fields, named) {
                (Some(names), true) => (OpenContainer::Fields(names.iter().zip(v.payload.iter())), PayloadForm::Named),
                _ => (OpenContainer::Items(v.payload.iter()), PayloadForm::Positional),
            },
            Node::Value(v) => match without_json_variant(v) {
                Value::Map(entries) if named => (OpenContainer::Entries(entries.iter()), PayloadForm::Named),
                Value::List(items) => (OpenContainer::Items(items.iter()), PayloadForm::Positional),
                _ => return Ok(None),
            },
        };
        self.open(container)?;
        Ok(Some(form))
    }

    fn json_node(&mut self) -> PResult<Option<JsonNode<'a>>> {
        Ok(Some(match self.value() {
            Some(Value::Unit) => JsonNode::Null,
            Some(Value::Bool(b)) => JsonNode::Bool(*b),
            Some(Value::Int(i)) => JsonNode::Number(Number::Int(*i)),
            Some(Value::Float(f)) => JsonNode::Number(Number::Float(*f)),
            Some(Value::Str(s)) => JsonNode::Str(self.read_text(s)),
            Some(Value::List(items)) => {
                self.open(OpenContainer::Items(items.iter()))?;
                JsonNode::Array
            }
            Some(Value::Map(entries)) => {
                self.open(OpenContainer::Entries(entries.iter()))?;
                JsonNode::Object
            }
            _ => return Ok(None),
        }))
    }
}

impl<'v> ValueSource<'v> {
    fn key_input(&mut self, key: &'v Value) -> KeyInput<'v> {
        match without_json_variant(key) {
            Value::Str(s) => KeyInput::Text(self.read_text(s)),
            Value::Native(native) => KeyInput::Text(Cow::Borrowed(native.text.as_str())),
            Value::Int(i) => KeyInput::Int(*i),
            Value::Bool(b) => KeyInput::Bool(*b),
            Value::Duration(d) => match i64::try_from(d.as_nanos()) {
                Ok(nanos) => KeyInput::Duration(nanos),
                Err(_) => KeyInput::Value(key),
            },
            Value::Variant(v) if v.payload.is_empty() => KeyInput::Variant(v.variant.name.as_str()),
            _ => KeyInput::Value(key),
        }
    }
}

#[cfg(test)]
mod tests;
