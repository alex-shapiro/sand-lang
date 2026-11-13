//! The decoders that implement the Sand `Decode` trait.
//!
//! `json.decode` reads JSON text with the decoder in the parent module, and a
//! host passes a [`Value`] to [`decode_value`]. Both decoders build VM values
//! with a [`DecodeTarget`], which holds the parts that do not depend on the
//! input: error paths, field defaults, computed-default thunks, and deferred
//! set and map fills.

use super::{Builder, Decoded, Error, JsonIds, PResult, is_json, json_number, write_str};
use crate::native::ty::VmVal;
use crate::stdlib::{base64, core, truncate};
use crate::syntax::lexer::is_ident;
use crate::types::ConstVal;
use crate::value::{Value, VariantValue, integral_float_to_int, is_option_variant, key_text, kind_name, without_json_variant};
use crate::vm::bytecode::RtIds;
use crate::vm::compare::hash_key;
use crate::vm::container_children::MAX_NESTING_DEPTH;
use crate::vm::meter::Meter;
use crate::vm::value::{FieldDefault, FieldDesc, FxHashMap, Obj, ParseValue, ReflectVariantDesc, TypeDesc, VmValue};
use crate::vm::{Fail, Vm};
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;

/// A segment of the path of the part being decoded.
pub(super) enum Seg<'a> {
    Key(Cow<'a, str>),
    Index(usize),
}

/// Elements and entries to insert into a decoded set or map after the heap
/// holds them, because hashing reads the heap.
pub(super) enum Fill {
    Set(VmValue, Vec<VmValue>),
    Map(VmValue, Vec<(VmValue, VmValue)>),
}

const INDEXED_FIELDS: usize = 8;
const LOOKUPS_PER_UNIT: usize = 16;
pub(super) const QUOTED_KEY: usize = 64;
/// Strings longer than this many bytes are checked against the memory limit
/// before the `Value` decoder copies them.
const CHECKED_TEXT_LEN: usize = 1024;

pub(super) fn lookup<T>(meter: &Meter, items: &[T], pred: impl Fn(&T) -> bool) -> Result<Option<usize>, Fail> {
    meter.charge(1 + (items.len() / LOOKUPS_PER_UNIT) as u64)?;
    Ok(items.iter().position(pred))
}

/// The VM values that a decoder builds, and the work that waits until the heap
/// holds them.
pub(super) struct DecodeTarget<'a, 'd, 's, 'h> {
    pub(super) b: Builder<'a, 's, 'h>,
    pub(super) ids: RtIds,
    /// The path of the part being decoded.
    pub(super) path: Vec<Seg<'a>>,
    /// The structs and enums being decoded, from the outermost.
    pub(super) stack: Vec<&'d TypeDesc>,
    /// The decoded fields of the open structs.
    pub(super) slots: Vec<Option<VmValue>>,
    indexes: FxHashMap<u32, FxHashMap<&'d str, usize>>,
    pub(super) fills: Vec<Fill>,
    /// The closures that compute omitted field defaults after decoding.
    pub(super) thunks: Vec<VmValue>,
}

/// A decoded value and the list of thunks that compute its omitted field
/// defaults, if any.
type DecodedParts = (VmValue, Option<VmValue>);

impl<'a, 'd, 's, 'h> DecodeTarget<'a, 'd, 's, 'h> {
    pub(super) fn new(b: Builder<'a, 's, 'h>, ids: RtIds) -> Self {
        let (path, stack, slots, indexes, fills, thunks) = Default::default();
        DecodeTarget { b, ids, path, stack, slots, indexes, fills, thunks }
    }

    /// Returns the decoded value of `r` with its thunks, and the fills to
    /// apply after the heap holds the value.
    pub(super) fn finish(mut self, r: PResult<VmValue>) -> (PResult<DecodedParts>, Vec<Fill>) {
        let r = r.map(|v| (v, (!self.thunks.is_empty()).then(|| self.b.list(std::mem::take(&mut self.thunks)))));
        (r, self.fills)
    }

    pub(super) fn path_str(&self) -> String {
        let mut s = String::from("$");
        for seg in &self.path {
            match seg {
                Seg::Index(i) => {
                    let _ = write!(s, "[{i}]");
                }
                Seg::Key(k) if is_ident(k) => {
                    s.push('.');
                    s.push_str(truncate(k, QUOTED_KEY));
                }
                Seg::Key(k) => {
                    s.push('[');
                    write_str(&mut s, truncate(k, QUOTED_KEY));
                    s.push(']');
                }
            }
        }
        s
    }

    pub(super) fn type_name(&self, t: &TypeDesc) -> String {
        t.format(&|id| {
            let found = self.stack.iter().rev().find_map(|d| match d {
                TypeDesc::Struct { id: i, name, .. } | TypeDesc::Enum { id: i, name, .. } if *i == id => Some(name),
                _ => None,
            });
            found.cloned().unwrap_or_default()
        })
    }

    pub(super) fn fail(&self, msg: &str) -> Error {
        Error::Parse(format!("{}: {msg}", self.path_str()))
    }

    pub(super) fn unsupported(&self, what: &str) -> Error {
        Error::Invalid(format!("{}: cannot decode {what}", self.path_str()))
    }

    /// Returns the enclosing struct or enum that the recursive type `id`
    /// refers to.
    pub(super) fn resolve_recur(&self, id: u32) -> PResult<&'d TypeDesc> {
        self.b.meter.charge(1 + (self.stack.len() / LOOKUPS_PER_UNIT) as u64)?;
        let found = self.stack.iter().rev().find(|d| match d {
            TypeDesc::Struct { id: i, .. } | TypeDesc::Enum { id: i, .. } => *i == id,
            _ => false,
        });
        match found {
            Some(&d) => Ok(d),
            None => Err(Error::Fail(Fail::internal("json.decode: unresolved recursive type"))),
        }
    }

    pub(super) fn field_index(&mut self, id: u32, fields: &'d [FieldDesc], key: &str, guess: usize) -> Result<Option<usize>, Fail> {
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

    /// Builds the struct `def` from the fields in `self.slots[base..]`, and
    /// fills the omitted fields with their defaults.
    pub(super) fn finish_struct(&mut self, def: u32, fields: &'d [FieldDesc], base: usize) -> PResult<VmValue> {
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

    /// Fills each omitted field of a named payload with `None`, or fails if
    /// the field is not an `Option`.
    pub(super) fn fill_omitted_payload(&self, slots: &mut [Option<VmValue>], types: &[Arc<TypeDesc>], names: &[String]) -> PResult<()> {
        for (i, slot) in slots.iter_mut().enumerate() {
            if slot.is_none() {
                if !matches!(*types[i], TypeDesc::Option(_)) {
                    return Err(self.fail(&format!("missing field `{}`", names[i])));
                }
                *slot = Some(VmValue::Tag(self.ids.none));
            }
        }
        Ok(())
    }

    /// Builds a set of `items`. The heap hashes the items after decoding, and
    /// after the thunks from index `thunks` onward have run.
    pub(super) fn new_set(&mut self, items: Vec<VmValue>, fill: u32, thunks: usize) -> PResult<VmValue> {
        let set = self.b.st.alloc(Obj::Set(Box::default()));
        if self.thunks.len() == thunks {
            self.fills.push(Fill::Set(set, items));
        } else {
            // Defer set insertion until element field default thunks have run, so elements are hashed with their initialized fields.
            let items = self.b.list(items);
            self.thunk(fill, Box::new([set, items]))?;
        }
        Ok(set)
    }

    pub(super) fn thunk(&mut self, proto: u32, captures: Box<[VmValue]>) -> PResult<()> {
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

    /// Parses the JSON object key `key` as a map key of type `t`, and returns
    /// the key, its hash, and its text for string comparison.
    pub(super) fn map_key_from_text(&mut self, t: &TypeDesc, key: Cow<'a, str>) -> PResult<(VmValue, u64, Option<Cow<'a, str>>)> {
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
}

/// Inserts the deferred set and map entries of a successful decode, and
/// converts its result.
pub(super) fn finish_decode(vm: &mut Vm, r: PResult<DecodedParts>, fills: Vec<Fill>) -> Result<Decoded<VmValue>, Fail> {
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
        Err(Error::Invalid(msg)) => failed("invalid", msg),
        Err(Error::Fail(f)) => Err(f),
    }
}

/// Decodes `value` as `desc`. The value can also have the JSON form of `desc`,
/// such as a base64 `Value::Str` for `bytes`.
#[cfg_attr(not(test), expect(dead_code, reason = "host function calls decode their arguments with it"))]
pub(crate) fn decode_value(vm: &mut Vm, desc: &TypeDesc, value: &Value) -> Result<Decoded<VmValue>, Fail> {
    let (ids, meter) = (vm.prog.ids, &vm.meter);
    let (r, fills) = vm.heap.staged(|heap, st| {
        let mut d = ValueDecoder { t: DecodeTarget::new(Builder::new(heap, st, meter), ids), depth: 0 };
        let r = d.decode(desc, value);
        d.t.finish(r)
    });
    finish_decode(vm, r, fills)
}

/// Returns `true` if a JSON object key can hold each key of type `t`.
fn has_text_keys(t: &TypeDesc) -> bool {
    match t {
        TypeDesc::Str | TypeDesc::Int | TypeDesc::Duration | TypeDesc::Bool | TypeDesc::Value { .. } => true,
        TypeDesc::Enum { variants, .. } => variants.iter().all(|v| v.payload_types.is_empty()),
        _ => false,
    }
}

/// Decodes a [`Value`] into a VM value of a reflected type.
///
/// The decoder accepts the exact form of each type and the form that JSON
/// parsing gives, such as a `Value::Map` with `str` keys for a struct. It reads
/// each `json.Json` variant as its JSON form.
struct ValueDecoder<'a, 'd, 's, 'h> {
    t: DecodeTarget<'a, 'd, 's, 'h>,
    /// The number of open containers.
    depth: usize,
}

/// The payload of a variant in either form.
#[derive(Clone, Copy)]
enum PayloadInput<'a> {
    Exact(&'a VariantValue),
    Json(&'a Value),
}

impl<'a, 'd> ValueDecoder<'a, 'd, '_, '_> {
    fn with_seg<T>(&mut self, seg: Seg<'a>, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.t.path.push(seg);
        let r = f(self);
        self.t.path.pop();
        r
    }

    /// Runs `f` inside a container, within the nesting limit.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(Error::Parse("value too deeply nested to decode".into()));
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn mismatch(&self, expected: &str, found: &Value) -> Error {
        self.t.fail(&format!("expected {expected}, found {}", kind_name(found)))
    }

    fn type_mismatch(&self, t: &TypeDesc, found: &Value) -> Error {
        self.mismatch(&self.t.type_name(t), found)
    }

    fn int(&self, t: &TypeDesc, v: &Value) -> PResult<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => match integral_float_to_int(*f) {
                Some(i) => Ok(i),
                None if f.is_finite() && f.fract() == 0.0 => Err(self.t.fail("integer out of range")),
                None => Err(self.type_mismatch(t, v)),
            },
            v => Err(self.type_mismatch(t, v)),
        }
    }

    fn bool(&self, v: &Value) -> PResult<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            v => Err(self.mismatch("bool", v)),
        }
    }

    /// Checks `len` bytes against the memory limit if they are many, and
    /// charges fuel for copying them.
    fn copy_bytes(&mut self, len: usize) -> PResult<()> {
        if len > CHECKED_TEXT_LEN {
            self.t.b.st.check_memory(self.t.b.pending.saturating_add(len))?;
        }
        Ok(self.t.b.meter.charge_bytes(len)?)
    }

    fn new_str(&mut self, s: &str) -> PResult<VmValue> {
        self.copy_bytes(s.len())?;
        Ok(self.t.b.st.new_str(s))
    }

    fn parse_native(&mut self, parse: ParseValue, text: &str) -> PResult<VmValue> {
        self.t.b.meter.charge_bytes(text.len())?;
        match parse(text) {
            Ok(v) => Ok(self.t.b.st.alloc(v)),
            Err(msg) => Err(self.t.fail(&msg)),
        }
    }

    fn decode(&mut self, t: &'d TypeDesc, v: &'a Value) -> PResult<VmValue> {
        self.t.b.meter.charge(1)?;
        self.t.b.reserve()?;
        let v = without_json_variant(v);
        match (t, v) {
            (TypeDesc::Int, _) => Ok(VmValue::Int(self.int(t, v)?)),
            (TypeDesc::Duration, Value::Duration(d)) => match i64::try_from(d.as_nanos()) {
                Ok(nanos) => Ok(VmValue::Dur(nanos)),
                Err(_) => Err(self.t.fail("integer out of range")),
            },
            (TypeDesc::Duration, _) => Ok(VmValue::Dur(self.int(t, v)?)),
            (TypeDesc::Float, Value::Float(f)) => Ok(VmValue::Float(*f)),
            (TypeDesc::Float, Value::Int(i)) => Ok(VmValue::Float(*i as f64)),
            (TypeDesc::Bool, Value::Bool(b)) => Ok(VmValue::Bool(*b)),
            (TypeDesc::Unit, Value::Unit) => Ok(VmValue::Unit),
            (TypeDesc::Str, Value::Str(s)) => self.new_str(s),
            (TypeDesc::Bytes, Value::Bytes(b)) => {
                self.copy_bytes(b.len())?;
                Ok(self.t.b.st.new_bytes(b.as_slice()))
            }
            (TypeDesc::Bytes, Value::Str(s)) => {
                self.copy_bytes(s.len())?;
                let b = base64::decode(s, false).map_err(|_| self.t.fail("invalid base64"))?;
                Ok(self.t.b.st.new_bytes(b))
            }
            (TypeDesc::Option(_), _) => self.decode_option(t, v),
            (TypeDesc::List(elem), Value::List(items)) => {
                let items = self.decode_items(elem, items)?;
                Ok(self.t.b.list(items))
            }
            (TypeDesc::Set(elem, fill), Value::Set(items) | Value::List(items)) => {
                let thunks = self.t.thunks.len();
                let items = self.decode_items(elem, items)?;
                self.t.new_set(items, *fill, thunks)
            }
            (TypeDesc::Tuple(elems), Value::Tuple(items) | Value::List(items)) => {
                if items.len() != elems.len() {
                    return Err(self.t.fail(&format!("expected list of length {}", elems.len())));
                }
                let items = self.nested(|d| {
                    items
                        .iter()
                        .zip(elems)
                        .enumerate()
                        .map(|(i, (item, elem))| d.with_seg(Seg::Index(i), |d| d.decode(elem, item)))
                        .collect()
                })?;
                Ok(self.t.b.st.new_tuple(items))
            }
            (TypeDesc::Map(key_type, value_type, fill), Value::Map(entries)) => {
                self.nested(|d| d.decode_map(key_type, value_type, *fill, entries))
            }
            (TypeDesc::Struct { id, def, fields, .. }, Value::Struct(_) | Value::Map(_)) => {
                self.nested(|d| d.decode_struct(t, *id, *def, fields, v))
            }
            (TypeDesc::Enum { variants, .. }, _) if is_json(t) => {
                self.decode_json(&JsonIds::at(variants.first().map_or(0, |v| v.runtime_id)), v)
            }
            (TypeDesc::Enum { name, variants, .. }, Value::Str(_) | Value::Variant(_) | Value::Map(_)) => {
                self.t.stack.push(t);
                let r = self.decode_enum(name, variants, v);
                self.t.stack.pop();
                r
            }
            (TypeDesc::Recur(id), _) => {
                let d = self.t.resolve_recur(*id)?;
                self.decode(d, v)
            }
            (TypeDesc::Value { parse, .. }, Value::Native(native)) => self.parse_native(*parse, &native.text),
            (TypeDesc::Value { parse, .. }, Value::Str(text)) => self.parse_native(*parse, text),
            (TypeDesc::Range, Value::Range { start, end, inclusive }) => Ok(self.t.b.st.alloc(Obj::Range(*start, *end, *inclusive))),
            (TypeDesc::Range, Value::Map(entries)) => self.decode_range(entries),
            (TypeDesc::Unsupported(name), _) => Err(self.t.unsupported(&format!("values of type {name}"))),
            (TypeDesc::Param(_), _) => Err(Error::Fail(Fail::internal("decode: uninstantiated type parameter"))),
            (t, v) => Err(self.type_mismatch(t, v)),
        }
    }

    fn decode_items(&mut self, elem: &'d TypeDesc, items: &'a [Value]) -> PResult<Vec<VmValue>> {
        self.nested(|d| {
            let mut out = d.t.b.list_buffer(items.len())?;
            for (i, item) in items.iter().enumerate() {
                out.push(d.with_seg(Seg::Index(i), |d| d.decode(elem, item))?);
            }
            Ok(out)
        })
    }

    fn decode_option(&mut self, t: &'d TypeDesc, mut v: &'a Value) -> PResult<VmValue> {
        let (mut inner, mut layers) = (t, 0);
        while let TypeDesc::Option(e) = inner {
            (inner, layers) = (e, layers + 1);
        }
        self.t.b.meter.charge(layers)?;
        // Count the `Some` layers that `v` holds exactly. A value of another form is the payload of every remaining layer, as in JSON.
        let mut somes = 0;
        let mut decoded = loop {
            if somes == layers {
                break self.decode(inner, v)?;
            }
            match without_json_variant(v) {
                Value::Unit => break VmValue::Tag(self.t.ids.none),
                Value::Variant(option) if is_option_variant(option) && option.payload.len() <= 1 => match option.payload.first() {
                    None => break VmValue::Tag(self.t.ids.none),
                    Some(payload) => {
                        somes += 1;
                        v = payload;
                    }
                },
                _ => {
                    somes = layers;
                    break self.decode(inner, v)?;
                }
            }
        };
        for _ in 0..somes {
            self.t.b.reserve()?;
            decoded = self.t.b.st.new_variant(self.t.ids.some, &[decoded]);
        }
        Ok(decoded)
    }

    fn decode_map(
        &mut self,
        key_type: &'d TypeDesc,
        value_type: &'d TypeDesc,
        fill: u32,
        entries: &'a [(Value, Value)],
    ) -> PResult<VmValue> {
        if has_text_keys(key_type) {
            let mut m = self.t.b.start_map();
            for (key, value) in entries {
                let (k, hash, text) = self.map_key(key_type, key)?;
                let v = self.with_seg(Seg::Key(key_text(key)), |d| d.decode(value_type, value))?;
                self.t.b.put(&mut m, hash, text, k, v)?;
            }
            return Ok(self.t.b.map(m));
        }
        let thunks = self.t.thunks.len();
        let mut decoded = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let k = self.decode_key(key_type, key)?;
            let v = self.with_seg(Seg::Key(key_text(key)), |d| d.decode(value_type, value))?;
            decoded.push((k, v));
        }
        let map = self.t.b.st.alloc(Obj::Map(Box::default()));
        if self.t.thunks.len() == thunks {
            self.t.fills.push(Fill::Map(map, decoded));
        } else {
            // Defer map insertion until key field default thunks have run, so keys are hashed with their initialized fields.
            let keys_and_values = self.t.b.list(decoded.into_iter().flat_map(|(k, v)| [k, v]).collect());
            self.t.thunk(fill, Box::new([map, keys_and_values]))?;
        }
        Ok(map)
    }

    /// Converts `key` into a map key of a type with a JSON text form, and
    /// returns the key, its hash, and its text for string comparison.
    fn map_key(&mut self, key_type: &'d TypeDesc, key: &'a Value) -> PResult<(VmValue, u64, Option<Cow<'a, str>>)> {
        match without_json_variant(key) {
            Value::Str(text) => self.t.map_key_from_text(key_type, Cow::Borrowed(text)),
            Value::Native(native) if matches!(key_type, TypeDesc::Value { .. }) => {
                self.t.map_key_from_text(key_type, Cow::Borrowed(&native.text))
            }
            key => match self.decode_key(key_type, key)? {
                k @ (VmValue::Int(_) | VmValue::Bool(_) | VmValue::Dur(_) | VmValue::Tag(_)) => {
                    Ok((k, hash_key(self.t.b.heap, self.t.b.meter, k)?, None))
                }
                _ => Err(self.invalid_map_key(key)),
            },
        }
    }

    /// Decodes the map key `key` as `key_type`. A key that does not match
    /// fails at the path of the map.
    fn decode_key(&mut self, key_type: &'d TypeDesc, key: &'a Value) -> PResult<VmValue> {
        let path = std::mem::take(&mut self.t.path);
        let r = self.decode(key_type, key);
        self.t.path = path;
        r.map_err(|e| match e {
            Error::Parse(_) => self.invalid_map_key(key),
            e => e,
        })
    }

    fn invalid_map_key(&self, key: &Value) -> Error {
        self.t.fail(&format!("invalid map key `{}`", truncate(&key_text(key), QUOTED_KEY)))
    }

    fn decode_struct(&mut self, t: &'d TypeDesc, id: u32, def: u32, fields: &'d [FieldDesc], v: &'a Value) -> PResult<VmValue> {
        self.t.stack.push(t);
        let base = self.t.slots.len();
        self.t.slots.resize(base + fields.len(), None);
        let mut next = 0;
        let r = match v {
            Value::Struct(s) => s.named_fields().try_for_each(|(name, field)| self.struct_field(id, fields, base, &mut next, name, field)),
            Value::Map(entries) => entries.iter().try_for_each(|(key, field)| match without_json_variant(key) {
                Value::Str(name) => self.struct_field(id, fields, base, &mut next, name, field),
                key => Err(self.mismatch("str key", key)),
            }),
            v => Err(self.type_mismatch(t, v)),
        };
        self.t.stack.pop();
        if let Err(e) = r {
            self.t.slots.truncate(base);
            return Err(e);
        }
        self.t.finish_struct(def, fields, base)
    }

    fn struct_field(
        &mut self,
        id: u32,
        fields: &'d [FieldDesc],
        base: usize,
        next: &mut usize,
        name: &'a str,
        field: &'a Value,
    ) -> PResult<()> {
        if let Some(i) = self.t.field_index(id, fields, name, *next)? {
            *next = i + 1;
            self.t.slots[base + i] = Some(self.with_seg(Seg::Key(Cow::Borrowed(name)), |d| d.decode(&fields[i].1, field))?);
        }
        Ok(())
    }

    fn decode_range(&mut self, entries: &'a [(Value, Value)]) -> PResult<VmValue> {
        let (mut start, mut end, mut inclusive) = (None, None, false);
        for (key, value) in entries {
            let key = match without_json_variant(key) {
                Value::Str(key) => key.as_str(),
                key => return Err(self.mismatch("str key", key)),
            };
            let value = without_json_variant(value);
            let seg = || Seg::Key(Cow::Borrowed(key));
            match key {
                "start" => start = Some(self.with_seg(seg(), |d| d.int(&TypeDesc::Int, value))?),
                "end" => end = Some(self.with_seg(seg(), |d| d.int(&TypeDesc::Int, value))?),
                "inclusive" => inclusive = self.with_seg(seg(), |d| d.bool(value))?,
                _ => {}
            }
        }
        let (Some(start), Some(end)) = (start, end) else {
            return Err(self.t.fail(&format!("missing field `{}`", if start.is_none() { "start" } else { "end" })));
        };
        Ok(self.t.b.st.alloc(Obj::Range(start, end, inclusive)))
    }

    fn decode_enum(&mut self, name: &str, variants: &'d [ReflectVariantDesc], v: &'a Value) -> PResult<VmValue> {
        let (tag, payload) = match v {
            Value::Str(tag) => (tag.as_str(), None),
            Value::Variant(variant) if variant.payload.is_empty() => (variant.variant.name.as_str(), None),
            Value::Variant(variant) => (variant.variant.name.as_str(), Some(PayloadInput::Exact(variant))),
            Value::Map(entries) => match &entries[..] {
                [(key, payload)] => match without_json_variant(key) {
                    Value::Str(tag) => (tag.as_str(), Some(PayloadInput::Json(payload))),
                    key => return Err(self.mismatch("str key", key)),
                },
                [] => return Err(self.t.fail(&format!("expected a variant of {name}"))),
                _ => return Err(self.t.fail(&format!("expected a single variant of {name}"))),
            },
            v => return Err(self.mismatch(name, v)),
        };
        let Some(i) = lookup(self.t.b.meter, variants, |v| v.name == tag)? else {
            return Err(self.t.fail(&format!("unknown variant `{}` of {name}", truncate(tag, QUOTED_KEY))));
        };
        let variant = &variants[i];
        match payload {
            None if variant.payload_types.is_empty() => Ok(VmValue::Tag(variant.runtime_id)),
            None => Err(self.t.fail(&format!("variant `{tag}` of {name} needs a payload"))),
            Some(payload) => self.with_seg(Seg::Key(Cow::Borrowed(tag)), |d| d.nested(|d| d.decode_payload(variant, payload))),
        }
    }

    fn decode_payload(&mut self, variant: &'d ReflectVariantDesc, payload: PayloadInput<'a>) -> PResult<VmValue> {
        let ReflectVariantDesc { runtime_id: id, payload_types: types, field_names: names, .. } = variant;
        let mut slots: Vec<Option<VmValue>> = vec![None; types.len()];
        let positional = match (payload, names) {
            (PayloadInput::Exact(v), Some(names)) if v.variant.fields.is_some() => {
                for (key, value) in v.variant.fields.iter().flatten().zip(v.payload.iter()) {
                    self.payload_field(&mut slots, types, names, key, value)?;
                }
                None
            }
            (PayloadInput::Exact(v), _) => Some(&*v.payload),
            (PayloadInput::Json(p), _) => match (without_json_variant(p), names) {
                (Value::Map(entries), Some(names)) => {
                    for (key, value) in entries {
                        match without_json_variant(key) {
                            Value::Str(key) => self.payload_field(&mut slots, types, names, key, value)?,
                            key => return Err(self.mismatch("str key", key)),
                        }
                    }
                    None
                }
                (Value::List(items), _) => Some(&items[..]),
                (p, _) => return Err(self.mismatch(if names.is_some() { "list or map" } else { "list" }, p)),
            },
        };
        match (positional, names) {
            (Some(items), _) if items.len() != types.len() => return Err(self.t.fail(&format!("expected list of length {}", types.len()))),
            (Some(items), _) => {
                for (i, (item, ty)) in items.iter().zip(types).enumerate() {
                    slots[i] = Some(self.with_seg(Seg::Index(i), |d| d.decode(ty, item))?);
                }
            }
            (None, Some(names)) => self.t.fill_omitted_payload(&mut slots, types, names)?,
            (None, None) => {}
        }
        let fields: Vec<VmValue> = slots.into_iter().map(|s| s.unwrap_or(VmValue::Unit)).collect();
        Ok(self.t.b.st.new_variant(*id, &fields))
    }

    fn payload_field(
        &mut self,
        slots: &mut [Option<VmValue>],
        types: &'d [Arc<TypeDesc>],
        names: &[String],
        key: &'a str,
        value: &'a Value,
    ) -> PResult<()> {
        if let Some(i) = lookup(self.t.b.meter, names, |n| n == key)? {
            slots[i] = Some(self.with_seg(Seg::Key(Cow::Borrowed(key)), |d| d.decode(&types[i], value))?);
        }
        Ok(())
    }

    fn decode_json(&mut self, ids: &JsonIds, v: &'a Value) -> PResult<VmValue> {
        self.t.b.meter.charge(1)?;
        self.t.b.reserve()?;
        Ok(match without_json_variant(v) {
            Value::Unit => VmValue::Tag(ids.null),
            Value::Bool(b) => self.t.b.st.new_variant(ids.bool_, &[VmValue::Bool(*b)]),
            Value::Int(i) => json_number(self.t.b.st, ids, VmValue::Int(*i)),
            Value::Float(f) => json_number(self.t.b.st, ids, VmValue::Float(*f)),
            Value::Str(s) => {
                let s = self.new_str(s)?;
                self.t.b.st.new_variant(ids.str_, &[s])
            }
            Value::List(items) => {
                let items = self.nested(|d| {
                    let mut out = d.t.b.list_buffer(items.len())?;
                    for (i, item) in items.iter().enumerate() {
                        out.push(d.with_seg(Seg::Index(i), |d| d.decode_json(ids, item))?);
                    }
                    Ok(out)
                })?;
                let list = self.t.b.list(items);
                self.t.b.st.new_variant(ids.arr, &[list])
            }
            Value::Map(entries) => {
                let m = self.nested(|d| {
                    let mut m = d.t.b.start_map();
                    for (key, value) in entries {
                        let key = match without_json_variant(key) {
                            Value::Str(key) => key.as_str(),
                            key => return Err(d.mismatch("str key", key)),
                        };
                        let v = d.with_seg(Seg::Key(Cow::Borrowed(key)), |d| d.decode_json(ids, value))?;
                        let hash = d.t.b.hash(key)?;
                        let k = d.t.b.key(&Cow::Borrowed(key), hash);
                        d.t.b.put(&mut m, hash, Some(Cow::Borrowed(key)), k, v)?;
                    }
                    Ok(m)
                })?;
                let map = self.t.b.map(m);
                self.t.b.st.new_variant(ids.obj, &[map])
            }
            v => return Err(self.mismatch("Json", v)),
        })
    }
}

#[cfg(test)]
mod tests;
