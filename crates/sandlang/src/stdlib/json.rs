//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::ty::{Param, Reflect, VmVal};
use crate::native::{EnumType, NamedType, NativeModule, variant_index};
use crate::native::{VariantDescCell, encode_variant};
use crate::types::traits;
use crate::value::{Decode, DecodeError, Encode, Value, without_json_variant};
use crate::vm::bytecode::{Program, VariantRt};
use crate::vm::compare::str_hash;
use crate::vm::container_children::{ChildRole, ContainerChildren, INLINE_OPEN_CONTAINERS, MAX_NESTING_DEPTH};
use crate::vm::heap::{Heap, Staging};
use crate::vm::meter::{Meter, SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{
    FieldDesc, FxHashMap, KeyIndex, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, ReflectVariantDesc, TypeDesc, ValueRef, VmValue, slot_fits,
};
use crate::vm::{Fail, Vm, fail, internal, without_trait};
use decode::{DecodeTarget, QUOTED_KEY, Seg, finish_decode, lookup};
use hashbrown::HashTable;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::fmt::Write;

mod decode;

const NODE_BYTES: usize = 64;

/// Dynamic JSON value.
///
/// Integers in the 64-bit signed `int` range without a fractional part,
/// exponent, or `-0` are represented as `Int`, and all other numbers are
/// represented as `Float`. `Json` equality is variant-exact, so
/// `Json.Int(1) != Json.Float(1.0)`; use `as_float()` or `as_int()` to read
/// numbers across both variants. Supports up to 512 levels of nesting.
#[derive(Clone, Debug, PartialEq, crate::SandEnum)]
#[sand(module = "json", manual_conversion, no_value)]
pub enum Json {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// Exact 64-bit signed integer.
    Int(i64),
    /// IEEE-754 floating-point number.
    Float(f64),
    /// UTF-8 string.
    Str(String),
    /// JSON array.
    Arr(Vec<Json>),
    /// Insertion-ordered JSON object.
    Obj(indexmap::IndexMap<String, Json>),
}

impl Encode for Json {
    fn encode(&self) -> Value {
        static VARIANT_DESCS: [VariantDescCell; Json::VARIANTS.len()] = [const { VariantDescCell::new() }; Json::VARIANTS.len()];
        let payload = match self {
            Json::Null => None,
            Json::Bool(b) => Some(b.encode()),
            Json::Int(i) => Some(i.encode()),
            Json::Float(f) => Some(f.encode()),
            Json::Str(s) => Some(s.encode()),
            Json::Arr(items) => Some(items.encode()),
            Json::Obj(members) => Some(members.encode()),
        };
        encode_variant::<Json>(&VARIANT_DESCS, self.index(), payload)
    }
}

/// Accepts a `Json` variant and any `Value` in JSON form.
impl Decode for Json {
    fn decode(value: &Value) -> Result<Json, DecodeError> {
        Ok(match without_json_variant(value) {
            Value::Unit => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(i) => Json::Int(*i),
            Value::Float(f) => Json::Float(*f),
            Value::Str(s) => Json::Str(s.clone()),
            Value::List(_) => Json::Arr(Vec::decode(value)?),
            Value::Map(_) => Json::Obj(indexmap::IndexMap::decode(value)?),
            value => return Err(DecodeError::expected("Json", value)),
        })
    }
}

#[derive(Clone, Copy)]
struct JsonIds {
    first: u32,
    null: u32,
    bool_: u32,
    int: u32,
    float: u32,
    str_: u32,
    arr: u32,
    obj: u32,
}

impl JsonIds {
    fn new(prog: &Program) -> Result<JsonIds, Fail> {
        match prog.enum_types.get(&Json::rust_type()) {
            Some(&first) => Ok(JsonIds::at(first)),
            None => internal("the json module is not loaded"),
        }
    }

    fn at(first: u32) -> JsonIds {
        const INDEXES: [usize; 7] = [
            variant_index::<Json>("Null"),
            variant_index::<Json>("Bool"),
            variant_index::<Json>("Int"),
            variant_index::<Json>("Float"),
            variant_index::<Json>("Str"),
            variant_index::<Json>("Arr"),
            variant_index::<Json>("Obj"),
        ];
        let [null, bool_, int, float, str_, arr, obj] = INDEXES.map(|i| first + i as u32);
        JsonIds { first, null, bool_, int, float, str_, arr, obj }
    }

    fn contains(&self, id: u32) -> bool {
        id.wrapping_sub(self.first) < Json::VARIANTS.len() as u32
    }
}

fn is_json(t: &TypeDesc) -> bool {
    matches!(t, TypeDesc::Enum { rust: Some(r), .. } if *r == Json::rust_type())
}

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("json", ModKind::Std, include_str!("json.sand"));
    m.bind::<Json>();
    m.function("parse", |vm: &mut Vm, text: VmVal<String>| -> Result<VmVal<Json>, HostError> {
        let ids = JsonIds::new(&vm.prog)?;
        let meter = &vm.meter;
        let r = vm.heap.staged(|heap, st| {
            let mut b = Builder::new(heap, st, meter);
            let mut p = Parser::new(text.str(heap));
            let r = p.parse_json(&mut b, &ids).and_then(|v| p.finish().map(|()| v));
            p.charge(meter)?;
            r
        });
        into_result(r)
    });
    m.function("encode", |vm: &mut Vm, value: Param<0, traits::Encode>, indent: i64| -> Result<VmVal<String>, Fail> {
        let (prog, heap, meter, indent) = (&vm.prog, &vm.heap, &vm.meter, indent.clamp(0, 1 << 16) as usize);
        let ids = JsonIds::new(prog)?;
        let out = {
            let (out, scratch, open_brackets) = (String::new(), Scratch::new(), SmallVec::new());
            let mut e = JsonTextEncoder { prog, heap, meter, ids, indent, out, charged: 0, scratch, open_brackets };
            e.encode(value.raw())?;
            e.out
        };
        Ok(VmVal::from_raw(vm.new_str(out)))
    });
    m.bind::<Decoded<()>>();
    m.function("decode_raw", |vm: &mut Vm, ty: Reflect<0>, text: VmVal<String>| -> Result<Decoded<Param<0>>, Fail> {
        let desc = ty.desc(&vm.heap)?.clone();
        let (ids, meter) = (vm.prog.ids, &vm.meter);
        let (r, fills) = vm.heap.staged(|heap, st| {
            let mut d = Decoder { p: Parser::new(text.str(heap)), t: DecodeTarget::new(Builder::new(heap, st, meter), ids) };
            let r = d.decode(&desc).and_then(|v| d.p.finish().map(|()| v));
            let r = d.p.charge(meter).map_err(Error::Fail).and(r);
            d.t.finish(r)
        });
        Ok(match finish_decode(vm, r, fills)? {
            Decoded::Done(v) => Decoded::Done(Param::from_raw(v)),
            Decoded::Pending(v, thunks) => Decoded::Pending(Param::from_raw(v), thunks),
            Decoded::Failed(e) => Decoded::Failed(e),
        })
    });
    m.function("to_json", |vm: &mut Vm, value: Param<0, traits::Encode>| -> Result<VmVal<Json>, Fail> {
        let ids = JsonIds::new(&vm.prog)?;
        let (prog, meter) = (&vm.prog, &vm.meter);
        let v = vm.heap.staged(|heap, st| ToJsonConverter::new(prog, heap, Builder::new(heap, st, meter), ids).build_json(value.raw()))?;
        Ok(VmVal::from_raw(v))
    });
    m
}

#[derive(crate::SandEnum)]
#[sand(module = "json", private, no_value)]
enum Decoded<T> {
    Done(T),
    Pending(T, VmVal<Vec<fn()>>),
    Failed(super::core::Error),
}

enum Error {
    Parse(String),
    Invalid(String),
    Fail(Fail),
}

impl From<Fail> for Error {
    fn from(c: Fail) -> Self {
        Error::Fail(c)
    }
}

type PResult<T> = Result<T, Error>;

fn into_result<T>(r: PResult<VmValue>) -> Result<VmVal<T>, HostError> {
    match r {
        Ok(v) => Ok(VmVal::from_raw(v)),
        Err(Error::Parse(msg)) => Err(HostError::new("parse", msg)),
        Err(Error::Invalid(msg)) => Err(HostError::new("invalid", msg)),
        Err(Error::Fail(c)) => Err(c.into()),
    }
}

const MAX_SHARED_KEY_LEN: usize = 64;
const MAX_SHARED_KEYS: usize = 4096;
const SCANNED_KEYS: usize = 16;

struct Builder<'a, 's, 'h> {
    heap: &'a Heap,
    st: &'s mut Staging<'h>,
    meter: &'a Meter,
    pending: usize,
    keys: HashTable<(u64, Cow<'a, str>, VmValue)>,
    entries: Vec<Entry<'a>>,
}

struct Entry<'a> {
    hash: u64,
    /// String key text used for equality checks before staged strings are committed to the heap.
    text: Option<Cow<'a, str>>,
    key: VmValue,
    value: VmValue,
}

struct MapBuilder {
    base: usize,
    index: Option<Box<KeyTable>>,
}

impl<'a, 's, 'h> Builder<'a, 's, 'h> {
    fn new(heap: &'a Heap, st: &'s mut Staging<'h>, meter: &'a Meter) -> Self {
        Builder { heap, st, meter, pending: 0, keys: HashTable::new(), entries: Vec::new() }
    }

    #[inline]
    fn reserve(&self) -> Result<(), Fail> {
        self.st.check_memory(NODE_BYTES + self.pending)
    }

    /// Checks list buffer growth against the memory limit so that lists of inline values cannot grow unchecked.
    fn push(&mut self, items: &mut Vec<VmValue>, v: VmValue) -> Result<(), Fail> {
        if items.len() == items.capacity() {
            let more = items.capacity().max(4);
            self.st.check_memory((NODE_BYTES + self.pending).saturating_add(more.saturating_mul(2 * std::mem::size_of::<VmValue>())))?;
            let before = items.capacity();
            items.reserve_exact(more);
            self.pending += (items.capacity() - before) * std::mem::size_of::<VmValue>();
        }
        items.push(v);
        Ok(())
    }

    /// Reserves memory for a list of `len` items and returns a vector with
    /// that capacity.
    fn list_buffer(&mut self, len: usize) -> Result<Vec<VmValue>, Fail> {
        let bytes = len.saturating_mul(std::mem::size_of::<VmValue>());
        self.st.check_memory((NODE_BYTES + self.pending).saturating_add(bytes))?;
        let items = Vec::with_capacity(len);
        self.pending += items.capacity() * std::mem::size_of::<VmValue>();
        Ok(items)
    }

    fn list(&mut self, items: Vec<VmValue>) -> VmValue {
        self.pending = self.pending.saturating_sub(items.capacity() * std::mem::size_of::<VmValue>());
        self.st.new_list(items)
    }

    fn hash(&self, text: &str) -> Result<u64, Fail> {
        self.meter.charge_bytes(text.len())?;
        Ok(str_hash(text))
    }

    #[allow(clippy::ptr_arg)]
    fn key(&mut self, key: &Cow<'a, str>, hash: u64) -> VmValue {
        if let Some(&(_, _, v)) = self.keys.find(hash, |e| e.1 == *key) {
            return v;
        }
        let v = self.st.new_str(&**key);
        if key.len() <= MAX_SHARED_KEY_LEN && self.keys.len() < MAX_SHARED_KEYS {
            self.pending += 2 * NODE_BYTES + key.len();
            self.keys.insert_unique(hash, (hash, key.clone(), v), |e| e.0);
        }
        v
    }

    fn start_map(&self) -> MapBuilder {
        MapBuilder { base: self.entries.len(), index: None }
    }

    fn put(&mut self, m: &mut MapBuilder, hash: u64, text: Option<Cow<'a, str>>, key: VmValue, value: VmValue) -> Result<(), Fail> {
        let segment = &self.entries[m.base..];
        let same = |e: &Entry| {
            e.hash == hash
                && match (&e.text, &text) {
                    (Some(x), Some(y)) => x == y,
                    _ => e.key == key,
                }
        };
        let found = match &m.index {
            Some(index) => index.find(hash, |slot| same(&segment[slot])),
            None => segment.iter().position(same),
        };
        if let Some(slot) = found {
            let e = &mut self.entries[m.base + slot];
            (e.key, e.value) = (key, value);
            return Ok(());
        }
        let slot = segment.len();
        if !slot_fits(slot) {
            return fail("limit", "map too large");
        }
        self.pending += ENTRY_PENDING;
        self.entries.push(Entry { hash, text, key, value });
        match &mut m.index {
            Some(index) => {
                let before = index.bytes();
                index.insert(hash, slot);
                self.pending = (self.pending + index.bytes()).saturating_sub(before);
            }
            None if slot >= SCANNED_KEYS => {
                let mut index = Box::<KeyTable>::default();
                index.reserve(2 * SCANNED_KEYS);
                for (i, e) in self.entries[m.base..].iter().enumerate() {
                    index.insert(e.hash, i);
                }
                self.pending += index.bytes();
                m.index = Some(index);
            }
            None => {}
        }
        Ok(())
    }

    /// Appends a unique key and value to `m`. Do not call with duplicate keys
    /// or mix with [`Builder::put`] on the same [`MapBuilder`].
    fn append(&mut self, m: &MapBuilder, hash: u64, key: VmValue, value: VmValue) -> Result<(), Fail> {
        if !slot_fits(self.entries.len() - m.base) {
            return fail("limit", "map too large");
        }
        self.pending += ENTRY_PENDING;
        self.entries.push(Entry { hash, text: None, key, value });
        Ok(())
    }

    fn map(&mut self, m: MapBuilder) -> VmValue {
        let n = self.entries.len() - m.base;
        let index = KeyIndex::for_keys(n, self.entries[m.base..].iter().map(|e| e.key));
        let mut map = MapObj { entries: Vec::with_capacity(n), live: n, index, ..MapObj::default() };
        for (slot, e) in self.entries.drain(m.base..).enumerate() {
            map.index.insert(e.key, e.hash, slot);
            map.entries.push(Some((e.key, e.value)));
        }
        let index = m.index.map_or(0, |ix| ix.bytes());
        self.pending = self.pending.saturating_sub(n * ENTRY_PENDING + index);
        self.st.alloc(Obj::Map(Box::new(map)))
    }
}

const ENTRY_PENDING: usize = std::mem::size_of::<Entry>() + MAP_ENTRY_SIZE;

impl FromVmValue for Json {
    type Item<'a> = Json;
    fn from_vm_value(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<Json, Fail> {
        let ids = JsonIds::new(&vm.prog)?;
        let mut stack = Vec::new();
        let mut node = v;
        loop {
            vm.charge(1)?;
            let id = vm.heap.variant_id(node);
            let mut value = match vm.heap.field(node, 0) {
                _ if id == ids.null => Some(Json::Null),
                Some(VmValue::Bool(b)) if id == ids.bool_ => Some(Json::Bool(b)),
                Some(VmValue::Int(i)) if id == ids.int => Some(Json::Int(i)),
                Some(VmValue::Float(f)) if id == ids.float => Some(Json::Float(f)),
                Some(payload) if id == ids.str_ => Some(Json::Str(rust_string(vm, payload, conv)?)),
                Some(payload) if id == ids.arr => {
                    let Obj::List(items) = vm.heap.get(payload) else { return Err(mismatch::<Json>()) };
                    conv.alloc(vm, items.len().saturating_mul(std::mem::size_of::<Json>()))?;
                    vm.charge(items.len() as u64)?;
                    stack.push(ReadFrame::Arr(Vec::with_capacity(items.len()), items.iter()));
                    None
                }
                Some(payload) if id == ids.obj => {
                    let Obj::Map(m) = vm.heap.get(payload) else { return Err(mismatch::<Json>()) };
                    conv.alloc(vm, m.live.saturating_mul(OBJ_ENTRY_BYTES))?;
                    vm.charge(m.live as u64)?;
                    stack.push(ReadFrame::Obj(indexmap::IndexMap::with_capacity(m.live), m.entries.iter(), String::new()));
                    None
                }
                _ => return Err(mismatch::<Json>()),
            };
            if stack.len() > MAX_NESTING_DEPTH {
                return fail("limit", "value too deeply nested to convert");
            }
            node = loop {
                match stack.last_mut() {
                    None => return value.ok_or_else(missing_root_error),
                    Some(ReadFrame::Arr(values, items)) => {
                        values.extend(value.take());
                        if let Some(&item) = items.next() {
                            break item;
                        }
                    }
                    Some(ReadFrame::Obj(members, entries, key)) => {
                        if let Some(value) = value.take() {
                            members.insert(std::mem::take(key), value);
                        }
                        if let Some((next_key, member)) = entries.find_map(|entry| *entry) {
                            *key = rust_string(vm, next_key, conv)?;
                            break member;
                        }
                    }
                }
                value = stack.pop().map(|frame| match frame {
                    ReadFrame::Arr(values, _) => Json::Arr(values),
                    ReadFrame::Obj(members, ..) => Json::Obj(members),
                });
            };
        }
    }
}

impl IntoVmValue for Json {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        let ids = JsonIds::new(&vm.prog)?;
        let meter = &vm.meter;
        vm.heap.staged(|heap, st| Builder::new(heap, st, meter).json(ids, self))
    }
}

fn missing_root_error() -> Fail {
    Fail::internal("json: missing root value in conversion")
}

/// An open Sand `Json` array or object being converted to a Rust [`Json`].
enum ReadFrame<'h> {
    Arr(Vec<Json>, std::slice::Iter<'h, VmValue>),
    /// Converted members, remaining entries, and the key of the current member.
    Obj(indexmap::IndexMap<String, Json>, std::slice::Iter<'h, Option<(VmValue, VmValue)>>, String),
}

const OBJ_ENTRY_BYTES: usize = std::mem::size_of::<(u64, String, Json)>() + std::mem::size_of::<usize>();

/// Copies the Sand `str` `v` into a Rust `String` and charges fuel and memory.
fn rust_string(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<String, Fail> {
    let s = match vm.heap.get(v) {
        Obj::Str(s) => &**s,
        Obj::StrBuf(s) => s.as_str(),
        _ => return Err(mismatch::<Json>()),
    };
    conv.alloc(vm, s.len())?;
    vm.charge_bytes(s.len())?;
    Ok(s.to_string())
}

/// An open Rust [`Json`] array or object being converted to a Sand `Json`.
enum WriteFrame {
    Arr(std::vec::IntoIter<Json>, Vec<VmValue>),
    /// Remaining members, map builder, and the key and hash of the current
    /// member.
    Obj(indexmap::map::IntoIter<String, Json>, MapBuilder, String, u64),
}

impl Builder<'_, '_, '_> {
    /// Converts `json` into a Sand `Json` value.
    fn json(&mut self, ids: JsonIds, json: Json) -> Result<VmValue, Fail> {
        let mut stack = Vec::new();
        let mut node = json;
        loop {
            self.reserve()?;
            self.meter.charge(1)?;
            let mut value = match node {
                Json::Null => Some(VmValue::Tag(ids.null)),
                Json::Bool(b) => Some(self.st.new_variant(ids.bool_, &[VmValue::Bool(b)])),
                Json::Int(i) => Some(self.st.new_variant(ids.int, &[VmValue::Int(i)])),
                Json::Float(f) => Some(self.st.new_variant(ids.float, &[VmValue::Float(f)])),
                Json::Str(s) => {
                    self.st.check_memory((NODE_BYTES + self.pending).saturating_add(s.len()))?;
                    self.meter.charge_bytes(s.len())?;
                    let s = self.st.new_str(s);
                    Some(self.st.new_variant(ids.str_, &[s]))
                }
                Json::Arr(items) => {
                    stack.push(WriteFrame::Arr(items.into_iter(), Vec::new()));
                    None
                }
                Json::Obj(members) => {
                    stack.push(WriteFrame::Obj(members.into_iter(), self.start_map(), String::new(), 0));
                    None
                }
            };
            if stack.len() > MAX_NESTING_DEPTH {
                return fail("limit", "value too deeply nested to convert");
            }
            node = loop {
                match stack.last_mut() {
                    None => return value.ok_or_else(missing_root_error),
                    Some(WriteFrame::Arr(items, values)) => {
                        if let Some(value) = value.take() {
                            self.push(values, value)?;
                        }
                        if let Some(item) = items.next() {
                            break item;
                        }
                    }
                    Some(WriteFrame::Obj(members, map, key, hash)) => {
                        if let Some(value) = value.take() {
                            let key = self.key(&Cow::Owned(std::mem::take(key)), *hash);
                            self.append(map, *hash, key, value)?;
                        }
                        if let Some((next_key, member)) = members.next() {
                            *hash = self.hash(&next_key)?;
                            *key = next_key;
                            break member;
                        }
                    }
                }
                value = stack.pop().map(|frame| match frame {
                    WriteFrame::Arr(_, values) => {
                        let list = self.list(values);
                        self.st.new_variant(ids.arr, &[list])
                    }
                    WriteFrame::Obj(_, map, ..) => {
                        let map = self.map(map);
                        self.st.new_variant(ids.obj, &[map])
                    }
                });
            };
        }
    }
}

struct Number<'a> {
    text: &'a str,
    is_int: bool,
}

impl Number<'_> {
    fn float(&self) -> f64 {
        self.text.parse().unwrap_or(f64::NAN)
    }

    fn value(&self) -> VmValue {
        match self.text.parse::<i64>() {
            Ok(i) if self.is_int && !(i == 0 && self.text.starts_with('-')) => VmValue::Int(i),
            _ => VmValue::Float(self.float()),
        }
    }

    fn int(&self) -> Option<i64> {
        if self.is_int {
            return self.text.parse().ok();
        }
        let f = self.float();
        (f.fract() == 0.0 && (-9223372036854775808.0..9223372036854775808.0).contains(&f)).then_some(f as i64)
    }
}

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
    depth: usize,
    charged: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Parser<'a> {
        Parser { src, b: src.as_bytes(), pos: 0, depth: 0, charged: 0 }
    }

    fn charge(&mut self, meter: &Meter) -> Result<(), Fail> {
        let consumed = self.pos.saturating_sub(self.charged);
        self.charged = self.pos;
        meter.charge(1 + (consumed / SLOW_BYTES_PER_UNIT) as u64)
    }

    fn err_at(&self, pos: usize, what: &str) -> Error {
        let pos = pos.min(self.b.len());
        let before = &self.b[..pos];
        let line = 1 + before.iter().filter(|&&c| c == b'\n').count();
        let line_start = before.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        let col = 1 + self.src.get(line_start..pos).map_or(pos - line_start, |s| s.chars().count());
        Error::Parse(format!("{what} at line {line}, column {col}"))
    }

    fn unexpected(&self) -> Error {
        match self.src.get(self.pos..).and_then(|s| s.chars().next()) {
            None => self.err_at(self.pos, "unexpected end of input"),
            Some(c) => self.err_at(self.pos, &format!("unexpected character `{}`", c.escape_debug())),
        }
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn finish(&mut self) -> PResult<()> {
        self.ws();
        if self.pos < self.b.len() { Err(self.unexpected()) } else { Ok(()) }
    }

    fn literal(&mut self, word: &[u8]) -> PResult<()> {
        if self.b[self.pos..].starts_with(word) {
            self.pos += word.len();
            return Ok(());
        }
        while self.eat(word[0]) {}
        Err(self.unexpected())
    }

    fn enter(&mut self) -> PResult<()> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.err_at(self.pos, "nesting too deep"));
        }
        self.depth += 1;
        self.pos += 1;
        Ok(())
    }

    fn digits(&mut self) {
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
    }

    fn number(&mut self) -> PResult<Number<'a>> {
        let start = self.pos;
        self.eat(b'-');
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.unexpected()),
        }
        let mut is_int = true;
        if self.eat(b'.') {
            is_int = false;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.unexpected());
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            is_int = false;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.unexpected());
            }
            self.digits();
        }
        Ok(Number { text: &self.src[start..self.pos], is_int })
    }

    fn hex4(&mut self, esc: usize) -> PResult<u32> {
        let digits = self.src.get(self.pos..self.pos + 4).filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()));
        let Some(h) = digits else { return Err(self.err_at(esc, "invalid unicode escape")) };
        self.pos += 4;
        Ok(u32::from_str_radix(h, 16).unwrap_or(0))
    }

    fn string(&mut self, keep: bool) -> PResult<Cow<'a, str>> {
        self.pos += 1;
        let mut out: Option<String> = None;
        loop {
            let start = self.pos;
            while let Some(&c) = self.b.get(self.pos) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            let run = &self.src[start..self.pos];
            match self.peek() {
                None => return Err(self.unexpected()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(match out {
                        None => Cow::Borrowed(run),
                        Some(mut s) => {
                            s.push_str(run);
                            Cow::Owned(s)
                        }
                    });
                }
                Some(b'\\') => {
                    let out = out.get_or_insert_with(String::new);
                    if keep {
                        out.push_str(run);
                    }
                    let esc = self.pos;
                    self.pos += 1;
                    let c = match self.peek() {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            self.pos += 1;
                            let hi = self.hex4(esc)?;
                            let code = if (0xd800..0xdc00).contains(&hi) {
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return Err(self.err_at(esc, "invalid unicode escape"));
                                }
                                let lo = self.hex4(esc)?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.err_at(esc, "invalid unicode escape"));
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            let Some(c) = char::from_u32(code) else { return Err(self.err_at(esc, "invalid unicode escape")) };
                            if keep {
                                out.push(c);
                            }
                            continue;
                        }
                        _ => return Err(self.err_at(esc, "invalid escape")),
                    };
                    self.pos += 1;
                    if keep {
                        out.push(c);
                    }
                }
                Some(_) => return Err(self.err_at(self.pos, "control character in string")),
            }
        }
    }

    fn skip(&mut self) -> PResult<()> {
        self.ws();
        match self.peek() {
            Some(b'n') => self.literal(b"null"),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'"') => self.string(false).map(drop),
            Some(b'-' | b'0'..=b'9') => self.number().map(drop),
            Some(b'[') => self.array(|p, _| p.skip()),
            Some(b'{') => self.object(false, |p, _| p.skip()),
            _ => Err(self.unexpected()),
        }
    }

    fn parse_json(&mut self, b: &mut Builder<'a, '_, '_>, ids: &JsonIds) -> PResult<VmValue> {
        self.ws();
        self.charge(b.meter)?;
        b.reserve()?;
        Ok(match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                VmValue::Tag(ids.null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                b.st.new_variant(ids.bool_, &[VmValue::Bool(true)])
            }
            Some(b'f') => {
                self.literal(b"false")?;
                b.st.new_variant(ids.bool_, &[VmValue::Bool(false)])
            }
            Some(b'"') => {
                let s = self.string(true)?;
                let s = b.st.new_str(&*s);
                b.st.new_variant(ids.str_, &[s])
            }
            Some(b'-' | b'0'..=b'9') => json_number(b.st, ids, self.number()?.value()),
            Some(b'[') => {
                let mut items = Vec::new();
                self.array(|p, _| {
                    let v = p.parse_json(b, ids)?;
                    Ok(b.push(&mut items, v)?)
                })?;
                let list = b.list(items);
                b.st.new_variant(ids.arr, &[list])
            }
            Some(b'{') => {
                let mut m = b.start_map();
                self.object(true, |p, key| {
                    let v = p.parse_json(b, ids)?;
                    let hash = b.hash(&key)?;
                    let k = b.key(&key, hash);
                    Ok(b.put(&mut m, hash, Some(key), k, v)?)
                })?;
                let map = b.map(m);
                b.st.new_variant(ids.obj, &[map])
            }
            _ => return Err(self.unexpected()),
        })
    }
}

trait Source<'a>: Sized {
    fn parser(&mut self) -> &mut Parser<'a>;

    fn array(&mut self, mut f: impl FnMut(&mut Self, usize) -> PResult<()>) -> PResult<()> {
        self.parser().enter()?;
        self.parser().ws();
        if !self.parser().eat(b']') {
            let mut i = 0;
            loop {
                f(self, i)?;
                i += 1;
                let p = self.parser();
                p.ws();
                if p.eat(b']') {
                    break;
                }
                if !p.eat(b',') {
                    return Err(p.unexpected());
                }
            }
        }
        self.parser().depth -= 1;
        Ok(())
    }

    fn object(&mut self, keys: bool, mut f: impl FnMut(&mut Self, Cow<'a, str>) -> PResult<()>) -> PResult<()> {
        self.parser().enter()?;
        self.parser().ws();
        if !self.parser().eat(b'}') {
            loop {
                let p = self.parser();
                p.ws();
                if p.peek() != Some(b'"') {
                    return Err(p.unexpected());
                }
                let key = p.string(keys)?;
                p.ws();
                if !p.eat(b':') {
                    return Err(p.unexpected());
                }
                f(self, key)?;
                let p = self.parser();
                p.ws();
                if p.eat(b'}') {
                    break;
                }
                if !p.eat(b',') {
                    return Err(p.unexpected());
                }
            }
        }
        self.parser().depth -= 1;
        Ok(())
    }
}

impl<'a> Source<'a> for Parser<'a> {
    fn parser(&mut self) -> &mut Parser<'a> {
        self
    }
}

/// Decodes JSON text into a VM value of a reflected type.
struct Decoder<'a, 'd, 's, 'h> {
    p: Parser<'a>,
    t: DecodeTarget<'a, 'd, 's, 'h>,
}

impl<'a> Source<'a> for Decoder<'a, '_, '_, '_> {
    fn parser(&mut self) -> &mut Parser<'a> {
        &mut self.p
    }
}

impl<'a, 'd> Decoder<'a, 'd, '_, '_> {
    fn mismatch(&self, what: &str) -> Error {
        match self.p.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => self.t.fail(&format!("expected {what}")),
            _ => self.p.unexpected(),
        }
    }

    fn number(&mut self, what: &str) -> PResult<Number<'a>> {
        match self.p.peek() {
            Some(b'-' | b'0'..=b'9') => self.p.number(),
            _ => Err(self.mismatch(what)),
        }
    }

    fn int(&mut self) -> PResult<i64> {
        let n = self.number("int")?;
        n.int().ok_or_else(|| {
            let f = n.float();
            self.t.fail(if f.is_finite() && f.fract() == 0.0 { "integer out of range" } else { "expected int" })
        })
    }

    fn bool(&mut self) -> PResult<bool> {
        match self.p.peek() {
            Some(b't') => self.p.literal(b"true").map(|()| true),
            Some(b'f') => self.p.literal(b"false").map(|()| false),
            _ => Err(self.mismatch("bool")),
        }
    }

    fn string(&mut self, what: &str) -> PResult<Cow<'a, str>> {
        if self.p.peek() == Some(b'"') { self.p.string(true) } else { Err(self.mismatch(what)) }
    }

    fn expect_open(&self, open: u8, what: &str) -> PResult<()> {
        if self.p.peek() == Some(open) { Ok(()) } else { Err(self.mismatch(what)) }
    }

    fn with_seg<T>(&mut self, seg: Seg<'a>, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.t.path.push(seg);
        let r = f(self);
        self.t.path.pop();
        r
    }

    fn decode(&mut self, t: &'d TypeDesc) -> PResult<VmValue> {
        self.p.ws();
        self.p.charge(self.t.b.meter)?;
        self.t.b.reserve()?;
        match t {
            TypeDesc::Int => Ok(VmValue::Int(self.int()?)),
            TypeDesc::Duration => Ok(VmValue::Dur(self.int()?)),
            TypeDesc::Float => Ok(VmValue::Float(self.number("float")?.float())),
            TypeDesc::Bool => Ok(VmValue::Bool(self.bool()?)),
            TypeDesc::Unit => match self.p.peek() {
                Some(b'n') => self.p.literal(b"null").map(|()| VmValue::Unit),
                _ => Err(self.mismatch("null")),
            },
            TypeDesc::Str => {
                let s = self.string("str")?;
                Ok(self.t.b.st.new_str(&*s))
            }
            TypeDesc::Bytes => {
                let s = self.string("base64 str")?;
                let b = base64::decode(&s, false).map_err(|_| self.t.fail("invalid base64"))?;
                Ok(self.t.b.st.new_bytes(b))
            }
            TypeDesc::Option(_) => {
                // Handle nested `Option` types iteratively because they do not increment `self.p.depth`.
                let (mut inner, mut layers) = (t, 0);
                while let TypeDesc::Option(e) = inner {
                    (inner, layers) = (e, layers + 1);
                }
                self.t.b.meter.charge(layers)?;
                if self.p.peek() == Some(b'n') {
                    self.p.literal(b"null")?;
                    return Ok(VmValue::Tag(self.t.ids.none));
                }
                let mut v = self.decode(inner)?;
                for _ in 0..layers {
                    self.t.b.reserve()?;
                    v = self.t.b.st.new_variant(self.t.ids.some, &[v]);
                }
                Ok(v)
            }
            TypeDesc::List(elem) => {
                self.expect_open(b'[', "array")?;
                let mut items = Vec::new();
                self.array(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(elem))?;
                    Ok(d.t.b.push(&mut items, v)?)
                })?;
                Ok(self.t.b.list(items))
            }
            TypeDesc::Set(elem, fill) => {
                self.expect_open(b'[', "array")?;
                let mut items = Vec::new();
                let thunks = self.t.thunks.len();
                self.array(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(elem))?;
                    Ok(d.t.b.push(&mut items, v)?)
                })?;
                self.t.new_set(items, *fill, thunks)
            }
            TypeDesc::Tuple(elems) => {
                let what = format!("array of length {}", elems.len());
                self.expect_open(b'[', &what)?;
                let mut items = Vec::with_capacity(elems.len());
                self.array(|d, i| {
                    let Some(elem) = elems.get(i) else { return Err(d.t.fail(&format!("expected {what}"))) };
                    items.push(d.with_seg(Seg::Index(i), |d| d.decode(elem))?);
                    Ok(())
                })?;
                if items.len() != elems.len() {
                    return Err(self.t.fail(&format!("expected {what}")));
                }
                Ok(self.t.b.st.new_tuple(items))
            }
            TypeDesc::Map(key_type, value_type, _) => {
                self.expect_open(b'{', "object")?;
                let mut m = self.t.b.start_map();
                self.object(true, |d, key| {
                    let seg = Seg::Key(key.clone());
                    let (k, hash, text) = d.t.map_key_from_text(key_type, key)?;
                    let v = d.with_seg(seg, |d| d.decode(value_type))?;
                    Ok(d.t.b.put(&mut m, hash, text, k, v)?)
                })?;
                Ok(self.t.b.map(m))
            }
            TypeDesc::Struct { id, def, fields, .. } => self.decode_struct(t, *id, *def, fields),
            TypeDesc::Enum { variants, .. } if is_json(t) => {
                let ids = JsonIds::at(variants.first().map_or(0, |v| v.runtime_id));
                self.p.parse_json(&mut self.t.b, &ids)
            }
            TypeDesc::Enum { name, variants, .. } => {
                self.t.stack.push(t);
                let r = self.decode_enum(name, variants);
                self.t.stack.pop();
                r
            }
            TypeDesc::Recur(id) => {
                let d = self.t.resolve_recur(*id)?;
                self.decode(d)
            }
            TypeDesc::Value { name, parse } => {
                let s = self.string(name)?;
                self.t.b.meter.charge_bytes(s.len())?;
                match parse(&s) {
                    Ok(v) => Ok(self.t.b.st.alloc(v)),
                    Err(msg) => Err(self.t.fail(&msg)),
                }
            }
            TypeDesc::Range => self.decode_range(),
            TypeDesc::Unsupported(name) => Err(self.t.unsupported(&format!("values of type {name}"))),
            TypeDesc::Param(_) => Err(Error::Fail(Fail::internal("json.decode: uninstantiated type parameter"))),
        }
    }

    fn decode_struct(&mut self, t: &'d TypeDesc, id: u32, def: u32, fields: &'d [FieldDesc]) -> PResult<VmValue> {
        self.expect_open(b'{', "object")?;
        self.t.stack.push(t);
        let base = self.t.slots.len();
        self.t.slots.resize(base + fields.len(), None);
        let mut next = 0;
        let r = self.object(true, |d, key| match d.t.field_index(id, fields, &key, next)? {
            Some(i) => {
                next = i + 1;
                d.t.slots[base + i] = Some(d.with_seg(Seg::Key(key), |d| d.decode(&fields[i].1))?);
                Ok(())
            }
            None => d.p.skip(),
        });
        self.t.stack.pop();
        if let Err(e) = r {
            self.t.slots.truncate(base);
            return Err(e);
        }
        self.t.finish_struct(def, fields, base)
    }

    fn decode_range(&mut self) -> PResult<VmValue> {
        self.expect_open(b'{', "object")?;
        let (mut start, mut end, mut inclusive) = (None, None, false);
        self.object(true, |d, key| {
            d.p.ws();
            match &*key {
                "start" => start = Some(d.with_seg(Seg::Key(key), |d| d.int())?),
                "end" => end = Some(d.with_seg(Seg::Key(key), |d| d.int())?),
                "inclusive" => inclusive = d.with_seg(Seg::Key(key), |d| d.bool())?,
                _ => d.p.skip()?,
            }
            Ok(())
        })?;
        let (Some(start), Some(end)) = (start, end) else {
            return Err(self.t.fail(&format!("missing field `{}`", if start.is_none() { "start" } else { "end" })));
        };
        Ok(self.t.b.st.alloc(Obj::Range(start, end, inclusive)))
    }

    fn decode_enum(&mut self, name: &str, variants: &'d [ReflectVariantDesc]) -> PResult<VmValue> {
        let find = |d: &Self, tag: &str| match lookup(d.t.b.meter, variants, |v| v.name == tag)? {
            Some(i) => Ok(&variants[i]),
            None => Err(d.t.fail(&format!("unknown variant `{}` of {name}", super::truncate(tag, QUOTED_KEY)))),
        };
        match self.p.peek() {
            Some(b'"') => {
                let tag = self.p.string(true)?;
                let v = find(self, &tag)?;
                if !v.payload_types.is_empty() {
                    return Err(self.t.fail(&format!("variant `{tag}` of {name} needs a payload")));
                }
                Ok(VmValue::Tag(v.runtime_id))
            }
            Some(b'{') => {
                let mut result = None;
                self.object(true, |d, tag| {
                    if result.is_some() {
                        return Err(d.t.fail(&format!("expected a single variant of {name}")));
                    }
                    let v = find(d, &tag)?;
                    result = Some(d.with_seg(Seg::Key(tag), |d| d.decode_payload(v))?);
                    Ok(())
                })?;
                result.ok_or_else(|| self.t.fail(&format!("expected a variant of {name}")))
            }
            _ => Err(self.mismatch(name)),
        }
    }

    fn decode_payload(&mut self, v: &'d ReflectVariantDesc) -> PResult<VmValue> {
        let ReflectVariantDesc { runtime_id: id, payload_types: types, field_names: names, .. } = v;
        self.p.ws();
        let mut slots: Vec<Option<VmValue>> = vec![None; types.len()];
        match (self.p.peek(), names) {
            (Some(b'{'), Some(names)) => {
                self.object(true, |d, key| match lookup(d.t.b.meter, names, |n| *n == *key)? {
                    Some(i) => {
                        slots[i] = Some(d.with_seg(Seg::Key(key), |d| d.decode(&types[i]))?);
                        Ok(())
                    }
                    None => d.p.skip(),
                })?;
                self.t.fill_omitted_payload(&mut slots, types, names)?;
            }
            (Some(b'['), _) => {
                let what = format!("array of length {}", types.len());
                let mut n = 0;
                self.array(|d, i| {
                    let Some(ty) = types.get(i) else { return Err(d.t.fail(&format!("expected {what}"))) };
                    slots[i] = Some(d.with_seg(Seg::Index(i), |d| d.decode(ty))?);
                    n += 1;
                    Ok(())
                })?;
                if n != types.len() {
                    return Err(self.t.fail(&format!("expected {what}")));
                }
            }
            _ => return Err(self.mismatch(if names.is_some() { "object" } else { "array" })),
        }
        let fields: Vec<VmValue> = slots.into_iter().map(|s| s.unwrap_or(VmValue::Unit)).collect();
        Ok(self.t.b.st.new_variant(*id, &fields))
    }
}

const SPACES: &str = "                                                                ";

fn invalid_key_error<T>() -> Result<T, Fail> {
    fail("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type")
}

fn nesting_limit_error<T>(action: &str) -> Result<T, Fail> {
    fail("limit", format!("value too deeply nested to {action}"))
}

fn runtime_variant(prog: &Program, id: u32) -> Result<&VariantRt, Fail> {
    match prog.variants.get(id as usize) {
        Some(variant) => Ok(variant),
        None => internal("json: unknown variant ID"),
    }
}

/// Returns the runtime variant ID and payload of the inline variant `v`.
fn inline_variant_payload(v: VmValue) -> Result<(u32, VmValue), Fail> {
    match v.as_inline_variant() {
        Some(inline) => Ok(inline),
        None => internal("json: not an inline variant"),
    }
}

/// Formats `key` as a JSON object key.
fn map_key_text<'v>(prog: &'v Program, heap: &'v Heap, key: VmValue) -> Result<Cow<'v, str>, Fail> {
    Ok(match key {
        VmValue::Int(i) | VmValue::Dur(i) => Cow::Owned(i.to_string()),
        VmValue::Bool(b) => Cow::Borrowed(if b { "true" } else { "false" }),
        VmValue::Tag(id) if id != prog.ids.none => Cow::Borrowed(runtime_variant(prog, id)?.name.as_str()),
        VmValue::Obj(_) => match heap.get(key) {
            Obj::Str(s) => Cow::Borrowed(&**s),
            Obj::StrBuf(s) => Cow::Borrowed(s.as_str()),
            o @ (Obj::Value(_) | Obj::Packed(..)) => Cow::Owned(o.value().map(ValueRef::text).unwrap_or_default()),
            _ => return invalid_key_error(),
        },
        _ => return invalid_key_error(),
    })
}

/// Encodes a Sand value into JSON text for `json.encode`.
struct JsonTextEncoder<'v> {
    prog: &'v Program,
    heap: &'v Heap,
    meter: &'v Meter,
    ids: JsonIds,
    indent: usize,
    out: String,
    /// Bytes of `out` already charged to `meter`.
    charged: usize,
    scratch: Scratch,
    /// Open JSON arrays and objects awaiting their remaining members.
    open_brackets: SmallVec<[OpenJsonBracket<'v>; INLINE_OPEN_CONTAINERS]>,
}

/// An open JSON array or object in [`JsonTextEncoder`].
struct OpenJsonBracket<'v> {
    children: ContainerChildren<'v>,
    is_array: bool,
    is_empty: bool,
}

impl<'v> JsonTextEncoder<'v> {
    /// Encodes `root` into JSON text. Charges a fuel unit per Sand value and
    /// charges for written output bytes.
    fn encode(&mut self, root: VmValue) -> Result<(), Fail> {
        let (prog, heap) = (self.prog, self.heap);
        let mut v = root;
        loop {
            self.meter.charge(1)?;
            self.charge_written()?;
            match v {
                VmValue::Unit | VmValue::Uninit => self.out.push_str("null"),
                VmValue::Bool(b) => self.out.push_str(if b { "true" } else { "false" }),
                VmValue::Int(i) | VmValue::Dur(i) => self.out.push_str(itoa::Buffer::new().format(i)),
                VmValue::Float(f) if f.is_finite() => _ = write!(self.out, "{f:?}"),
                VmValue::Float(_) => self.out.push_str("null"),
                VmValue::Tag(id) if id == prog.ids.none || id == self.ids.null => self.out.push_str("null"),
                VmValue::Tag(id) => self.write_json_string(&runtime_variant(prog, id)?.name)?,
                VmValue::Pair(first, second) => self.begin_bracket(true, ContainerChildren::Pair(first, second))?,
                VmValue::Obj(_) => match heap.get(v) {
                    Obj::Str(s) => self.write_json_string(s)?,
                    Obj::StrBuf(s) => self.write_json_string(s)?,
                    Obj::Bytes(b) => {
                        self.check_room(b.len() / 3 * 4 + 4)?;
                        self.meter.charge_slow_bytes(b.len())?;
                        self.out.extend(["\"", &base64::encode(b, false), "\""]);
                    }
                    Obj::List(items) => self.begin_bracket(true, ContainerChildren::Items(items.iter()))?,
                    Obj::Tuple(items) => self.begin_bracket(true, ContainerChildren::Items(items.iter()))?,
                    Obj::Set(set) => self.begin_bracket(true, ContainerChildren::Set(set.entries.iter()))?,
                    Obj::Map(map) => self.begin_bracket(false, ContainerChildren::Map(map.entries.iter(), None))?,
                    Obj::Struct(def, values) => {
                        self.begin_bracket(false, ContainerChildren::Fields(prog.types[*def as usize].fields.iter(), values.iter()))?
                    }
                    Obj::Variant(id, values) => {
                        if let Some(&payload) = values.first()
                            && self.encodes_as_payload(*id)
                        {
                            v = payload;
                            continue;
                        }
                        let variant = runtime_variant(prog, *id)?;
                        self.begin_variant_object(variant, ContainerChildren::from_payload(variant, values))?
                    }
                    Obj::Range(start, end, inclusive) => {
                        self.begin_bracket(false, ContainerChildren::empty())?;
                        for (name, bound) in [("start", start), ("end", end)] {
                            self.begin_member(Some(name))?;
                            self.charge_written()?;
                            self.out.push_str(itoa::Buffer::new().format(*bound));
                        }
                        if *inclusive {
                            self.begin_member(Some("inclusive"))?;
                            self.charge_written()?;
                            self.out.push_str("true");
                        }
                    }
                    o @ (Obj::Value(_) | Obj::Packed(..)) => self.write_json_string(&o.value().map(ValueRef::text).unwrap_or_default())?,
                    o @ (Obj::Closure(..)
                    | Obj::NativeFn(_)
                    | Obj::IterFn(_)
                    | Obj::Task(_)
                    | Obj::Channel(_)
                    | Obj::Native(_)
                    | Obj::TypeDesc(_)
                    | Obj::Cell(_)
                    | Obj::Free(_)) => return Err(without_trait(o.kind_name(), "encoded as JSON", "Encode")),
                },
                VmValue::VUnit(..)
                | VmValue::VBool(..)
                | VmValue::VInt(..)
                | VmValue::VFloat(..)
                | VmValue::VDur(..)
                | VmValue::VTag(..)
                | VmValue::VObj(..) => {
                    let (id, payload) = inline_variant_payload(v)?;
                    if self.encodes_as_payload(id) {
                        v = payload;
                        continue;
                    }
                    let variant = runtime_variant(prog, id)?;
                    self.begin_variant_object(variant, ContainerChildren::from_inline_payload(variant, payload))?
                }
            }
            v = loop {
                let Some(top) = self.open_brackets.last_mut() else { return Ok(()) };
                match top.children.next() {
                    Some((ChildRole::MapKey, key)) => self.begin_member(Some(&map_key_text(prog, heap, key)?))?,
                    Some((ChildRole::MapValue, value)) => break value,
                    Some((ChildRole::Item, item)) => {
                        self.begin_member(None)?;
                        break item;
                    }
                    Some((ChildRole::Field(name), value)) => {
                        self.begin_member(Some(name))?;
                        break value;
                    }
                    None => self.end_bracket()?,
                }
            };
        }
    }

    /// Returns `true` if `json.encode` writes the variant `id` as its payload.
    /// This applies to `Some` and to `Json` variants.
    #[inline(always)]
    fn encodes_as_payload(&self, id: u32) -> bool {
        id == self.prog.ids.some || self.ids.contains(id)
    }

    /// Opens an outer object keyed by `variant.name` and an inner array or
    /// object for `payload`.
    #[inline(always)]
    fn begin_variant_object(&mut self, variant: &VariantRt, payload: ContainerChildren<'v>) -> Result<(), Fail> {
        self.begin_bracket(false, ContainerChildren::empty())?;
        self.begin_member(Some(&variant.name))?;
        self.charge_written()?;
        self.begin_bracket(variant.field_names.is_none(), payload)
    }

    /// Opens a JSON array or object for `children`.
    #[inline(always)]
    fn begin_bracket(&mut self, is_array: bool, children: ContainerChildren<'v>) -> Result<(), Fail> {
        if self.open_brackets.len() >= MAX_NESTING_DEPTH {
            return nesting_limit_error("encode as JSON");
        }
        self.out.push(if is_array { '[' } else { '{' });
        self.open_brackets.push(OpenJsonBracket { children, is_array, is_empty: true });
        Ok(())
    }

    /// Closes the innermost JSON array or object.
    #[inline(always)]
    fn end_bracket(&mut self) -> Result<(), Fail> {
        let Some(ended) = self.open_brackets.pop() else { return Ok(()) };
        if !ended.is_empty {
            self.start_line()?;
        }
        self.out.push(if ended.is_array { ']' } else { '}' });
        Ok(())
    }

    /// Writes the separator and optional `key` for the next member of the
    /// innermost array or object.
    #[inline(always)]
    fn begin_member(&mut self, key: Option<&str>) -> Result<(), Fail> {
        if let Some(top) = self.open_brackets.last_mut()
            && !std::mem::replace(&mut top.is_empty, false)
        {
            self.out.push(',');
        }
        self.start_line()?;
        let Some(key) = key else { return Ok(()) };
        self.write_json_string(key)?;
        self.out.push(':');
        if self.indent > 0 {
            self.out.push(' ');
        }
        Ok(())
    }

    /// Writes a newline and indentation if `self.indent` is positive.
    #[inline(always)]
    fn start_line(&mut self) -> Result<(), Fail> {
        if self.indent == 0 {
            return Ok(());
        }
        let mut left = self.indent.saturating_mul(self.open_brackets.len());
        self.scratch.check(self.heap, self.out.len().saturating_add(left))?;
        self.out.push('\n');
        while left > 0 {
            let spaces = &SPACES[..left.min(SPACES.len())];
            self.out.push_str(spaces);
            left -= spaces.len();
        }
        Ok(())
    }

    /// Writes `s` as a quoted JSON string after checking the memory limit for
    /// long strings.
    #[inline(always)]
    fn write_json_string(&mut self, s: &str) -> Result<(), Fail> {
        if s.len() > 1024 {
            self.check_room(escaped_len(s))?;
        }
        write_str(&mut self.out, s);
        Ok(())
    }

    /// Checks whether `len` additional output bytes fit within the memory
    /// limit.
    #[inline(always)]
    fn check_room(&self, len: usize) -> Result<(), Fail> {
        self.heap.check_memory(self.out.len().saturating_add(len))
    }

    /// Charges fuel for newly written output bytes and checks `out.capacity()`
    /// against the memory limit.
    #[inline(always)]
    fn charge_written(&mut self) -> Result<(), Fail> {
        let written = self.out.len() - self.charged;
        self.charged = self.out.len();
        self.meter.charge_bytes(written)?;
        self.scratch.check(self.heap, self.out.capacity())
    }
}

fn json_number(st: &mut Staging, ids: &JsonIds, n: VmValue) -> VmValue {
    let id = if matches!(n, VmValue::Int(_)) { ids.int } else { ids.float };
    st.new_variant(id, &[n])
}

fn escaped_len(s: &str) -> usize {
    let escapes: usize = s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 1,
            0..0x20 => 5,
            _ => 0,
        })
        .sum();
    s.len() + escapes + 2
}

#[inline(always)]
pub(crate) fn write_str(out: &mut String, s: &str) {
    out.reserve(s.len() + 2);
    out.push('"');
    if has_escape(s.as_bytes()) {
        write_escaped(out, s);
    } else {
        out.push_str(s);
    }
    out.push('"');
}

/// Returns `true` if `bytes` contains a byte that requires JSON escaping.
#[inline(always)]
fn has_escape(bytes: &[u8]) -> bool {
    const LOW_BITS: u64 = u64::from_ne_bytes([0x01; 8]);
    const HIGH_BITS: u64 = u64::from_ne_bytes([0x80; 8]);
    // `(x - LOW_BITS * n) & !x & HIGH_BITS` is nonzero if any byte of `x` is
    // less than `n`.
    let word_has_escape = |word: &[u8; 8]| {
        let word = u64::from_ne_bytes(*word);
        let quote = word ^ (LOW_BITS * u64::from(b'"'));
        let backslash = word ^ (LOW_BITS * u64::from(b'\\'));
        let control = word.wrapping_sub(LOW_BITS * 0x20) & !word;
        (control | (quote.wrapping_sub(LOW_BITS) & !quote) | (backslash.wrapping_sub(LOW_BITS) & !backslash)) & HIGH_BITS != 0
    };
    let (words, rest) = bytes.as_chunks::<8>();
    words.iter().any(word_has_escape)
        || match bytes.last_chunk::<8>() {
            Some(last) => word_has_escape(last),
            None => rest.iter().any(|&b| b < 0x20 || b == b'"' || b == b'\\'),
        }
}

/// Writes `s` with JSON string escapes into `out`.
#[inline(never)]
fn write_escaped(out: &mut String, s: &str) {
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..0x20 => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if esc.is_empty() {
            let _ = write!(out, "\\u{:04x}", b);
        } else {
            out.push_str(esc);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
}

/// Converts a Sand value into a Sand `Json` value for `json.to_json`.
struct ToJsonConverter<'a, 's, 'h> {
    prog: &'a Program,
    heap: &'a Heap,
    b: Builder<'a, 's, 'h>,
    ids: JsonIds,
    /// Innermost `Json.Arr` or `Json.Obj` container being built.
    innermost: PartialJson<'a>,
    /// Enclosing arrays and objects, from outermost to innermost.
    parents: SmallVec<[PartialJson<'a>; INLINE_OPEN_CONTAINERS]>,
    /// Cached field keys for visited structs and named-payload variants.
    field_keys: Vec<FieldKey>,
    /// Starting index in `field_keys` for each visited struct or variant.
    first_field_keys: FxHashMap<FieldsOwner, usize>,
    /// Interned variant name strings by runtime variant ID.
    variant_names: FxHashMap<u32, VmValue>,
}

/// Runtime ID of a struct or named-payload variant.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum FieldsOwner {
    Struct(u32),
    Variant(u32),
}

/// Precomputed map key for a struct or variant field.
struct FieldKey {
    /// Heap `str` value of the field name.
    name: VmValue,
    hash: u64,
    name_len: usize,
}

/// An open `Json.Arr` or `Json.Obj` container and its remaining children.
struct PartialJson<'a> {
    children: ContainerChildren<'a>,
    members: PartialJsonMembers<'a>,
}

/// Accumulated members of a [`PartialJson`] container.
enum PartialJsonMembers<'a> {
    /// Slot for the root result of `json.to_json`.
    Root(VmValue),
    Array(Vec<VmValue>),
    /// Map builder and the pending key for the next object entry.
    Object(MapBuilder, Option<(VmValue, Cow<'a, str>)>),
    /// Map builder for a struct or variant and the index of the next field key
    /// in [`ToJsonConverter::field_keys`].
    Fields(MapBuilder, usize),
}

impl<'a, 's, 'h> ToJsonConverter<'a, 's, 'h> {
    fn new(prog: &'a Program, heap: &'a Heap, b: Builder<'a, 's, 'h>, ids: JsonIds) -> Self {
        let innermost = PartialJson { children: ContainerChildren::empty(), members: PartialJsonMembers::Root(VmValue::Unit) };
        let (parents, field_keys, first_field_keys, variant_names) = Default::default();
        ToJsonConverter { prog, heap, b, ids, innermost, parents, field_keys, first_field_keys, variant_names }
    }

    /// Converts `root` into a Sand `Json` value. Charges a fuel unit per Sand
    /// value and charges for copied byte slices and strings.
    fn build_json(&mut self, root: VmValue) -> Result<VmValue, Fail> {
        let (prog, heap, ids) = (self.prog, self.heap, self.ids);
        self.innermost.children = ContainerChildren::Single(ChildRole::Item, Some(root));
        'next_member: loop {
            let mut v = match self.innermost.children.next() {
                Some((ChildRole::MapKey, key)) => {
                    let (key, text) = match heap.get(key) {
                        Obj::Str(s) => (key, Cow::Borrowed(&**s)),
                        _ => {
                            let text = map_key_text(prog, heap, key)?;
                            (self.b.st.new_str(&*text), text)
                        }
                    };
                    self.set_member_key(key, text);
                    continue;
                }
                Some((_, member)) => member,
                None => {
                    let Some(parent) = self.parents.pop() else {
                        return match self.innermost.members {
                            PartialJsonMembers::Root(json) => Ok(json),
                            _ => Err(missing_root_error()),
                        };
                    };
                    let complete = std::mem::replace(&mut self.innermost, parent).members;
                    let json = self.finish_container(complete);
                    self.push_member(json)?;
                    continue;
                }
            };
            // Unwrap nested `Some` payloads iteratively.
            let json = 'convert: loop {
                self.b.meter.charge(1)?;
                self.b.reserve()?;
                let container = match v {
                    VmValue::Unit | VmValue::Uninit => break 'convert VmValue::Tag(ids.null),
                    VmValue::Bool(_) => break 'convert self.json_variant(ids.bool_, v),
                    VmValue::Int(i) | VmValue::Dur(i) => break 'convert self.json_variant(ids.int, VmValue::Int(i)),
                    VmValue::Float(_) => break 'convert self.json_variant(ids.float, v),
                    VmValue::Tag(id) if id == prog.ids.none || id == ids.null => break 'convert VmValue::Tag(ids.null),
                    VmValue::Tag(id) => {
                        let name = self.variant_name_str(id, runtime_variant(prog, id)?);
                        break 'convert self.json_variant(ids.str_, name);
                    }
                    VmValue::Pair(first, second) => self.partial_array(2, ContainerChildren::Pair(first, second))?,
                    VmValue::Obj(_) => match heap.get(v) {
                        Obj::Str(_) => break 'convert self.json_variant(ids.str_, v),
                        Obj::StrBuf(s) => break 'convert self.json_str_copy(s)?,
                        Obj::Bytes(b) => {
                            self.b.st.check_memory(self.b.pending.saturating_add(b.len() / 3 * 4 + 4))?;
                            self.b.meter.charge_slow_bytes(b.len())?;
                            let text = self.b.st.new_str(base64::encode(b, false));
                            break 'convert self.json_variant(ids.str_, text);
                        }
                        Obj::List(items) => self.partial_array(items.len(), ContainerChildren::Items(items.iter()))?,
                        Obj::Tuple(items) => self.partial_array(items.len(), ContainerChildren::Items(items.iter()))?,
                        Obj::Set(set) => self.partial_array(set.live, ContainerChildren::Set(set.entries.iter()))?,
                        Obj::Map(map) => PartialJson {
                            children: ContainerChildren::Map(map.entries.iter(), None),
                            members: PartialJsonMembers::Object(self.b.start_map(), None),
                        },
                        Obj::Struct(def, values) => {
                            let names = &prog.types[*def as usize].fields;
                            self.partial_fields_object(
                                FieldsOwner::Struct(*def),
                                names,
                                ContainerChildren::Fields(names.iter(), values.iter()),
                            )
                        }
                        Obj::Variant(id, _) if ids.contains(*id) => break 'convert v,
                        Obj::Variant(id, values) => {
                            if let Some(&payload) = values.first()
                                && *id == prog.ids.some
                            {
                                v = payload;
                                continue 'convert;
                            }
                            let variant = runtime_variant(prog, *id)?;
                            self.enter_variant_object(*id, variant, values.len(), ContainerChildren::from_payload(variant, values))?
                        }
                        Obj::Range(start, end, inclusive) => {
                            let range = PartialJsonMembers::Object(self.b.start_map(), None);
                            self.enter_container(PartialJson { children: ContainerChildren::empty(), members: range })?;
                            let bounds = [("start", ids.int, VmValue::Int(*start)), ("end", ids.int, VmValue::Int(*end))];
                            let inclusive = inclusive.then_some(("inclusive", ids.bool_, VmValue::Bool(true)));
                            for (name, id, payload) in bounds.into_iter().chain(inclusive) {
                                let key = self.b.st.new_str(name);
                                self.set_member_key(key, Cow::Borrowed(name));
                                let member = self.json_variant(id, payload);
                                self.push_member(member)?;
                            }
                            continue 'next_member;
                        }
                        o @ (Obj::Value(_) | Obj::Packed(..)) => {
                            break 'convert self.json_str_copy(&o.value().map(ValueRef::text).unwrap_or_default())?;
                        }
                        o @ (Obj::Closure(..)
                        | Obj::NativeFn(_)
                        | Obj::IterFn(_)
                        | Obj::Task(_)
                        | Obj::Channel(_)
                        | Obj::Native(_)
                        | Obj::TypeDesc(_)
                        | Obj::Cell(_)
                        | Obj::Free(_)) => return Err(without_trait(o.kind_name(), "encoded as JSON", "Encode")),
                    },
                    VmValue::VUnit(..)
                    | VmValue::VBool(..)
                    | VmValue::VInt(..)
                    | VmValue::VFloat(..)
                    | VmValue::VDur(..)
                    | VmValue::VTag(..)
                    | VmValue::VObj(..) => {
                        let (id, payload) = inline_variant_payload(v)?;
                        if ids.contains(id) {
                            break 'convert v;
                        }
                        if id == prog.ids.some {
                            v = payload;
                            continue 'convert;
                        }
                        let variant = runtime_variant(prog, id)?;
                        self.enter_variant_object(id, variant, 1, ContainerChildren::from_inline_payload(variant, payload))?
                    }
                };
                self.enter_container(container)?;
                continue 'next_member;
            };
            self.push_member(json)?;
        }
    }

    /// Builds the `Json` variant `id` with `payload`.
    fn json_variant(&mut self, id: u32, payload: VmValue) -> VmValue {
        self.b.st.new_variant(id, &[payload])
    }

    /// Allocates `text` as a `Json.Str` variant.
    fn json_str_copy(&mut self, text: &str) -> Result<VmValue, Fail> {
        self.b.meter.charge_bytes(text.len())?;
        let text = self.b.st.new_str(text);
        Ok(self.json_variant(self.ids.str_, text))
    }

    /// Returns the interned Sand `str` for the name of `variant`.
    fn variant_name_str(&mut self, id: u32, variant: &VariantRt) -> VmValue {
        *self.variant_names.entry(id).or_insert_with(|| self.b.st.new_str(variant.name.as_str()))
    }

    /// Pushes `self.innermost` onto `self.parents` and sets `container` as
    /// `self.innermost`.
    fn enter_container(&mut self, container: PartialJson<'a>) -> Result<(), Fail> {
        if self.parents.len() >= MAX_NESTING_DEPTH {
            return nesting_limit_error("convert to JSON");
        }
        self.parents.push(std::mem::replace(&mut self.innermost, container));
        Ok(())
    }

    /// Enters an outer object keyed by `variant.name` and returns the inner
    /// container for `payload`.
    fn enter_variant_object(
        &mut self,
        id: u32,
        variant: &'a VariantRt,
        len: usize,
        payload: ContainerChildren<'a>,
    ) -> Result<PartialJson<'a>, Fail> {
        let key = (self.variant_name_str(id, variant), Cow::Borrowed(variant.name.as_str()));
        let members = PartialJsonMembers::Object(self.b.start_map(), Some(key));
        self.enter_container(PartialJson { children: ContainerChildren::empty(), members })?;
        match &variant.field_names {
            Some(names) => Ok(self.partial_fields_object(FieldsOwner::Variant(id), names, payload)),
            None => self.partial_array(len, payload),
        }
    }

    /// Builds an open object container for the fields `names` of `owner`.
    fn partial_fields_object(&mut self, owner: FieldsOwner, names: &[String], children: ContainerChildren<'a>) -> PartialJson<'a> {
        let first_key = *self.first_field_keys.entry(owner).or_insert_with(|| {
            let first_key = self.field_keys.len();
            let key = |name: &String| FieldKey { name: self.b.st.new_str(name.as_str()), hash: str_hash(name), name_len: name.len() };
            self.field_keys.extend(names.iter().map(key));
            first_key
        });
        PartialJson { children, members: PartialJsonMembers::Fields(self.b.start_map(), first_key) }
    }

    /// Builds an open array container with capacity `len`.
    fn partial_array(&mut self, len: usize, children: ContainerChildren<'a>) -> Result<PartialJson<'a>, Fail> {
        Ok(PartialJson { children, members: PartialJsonMembers::Array(self.b.list_buffer(len)?) })
    }

    /// Converts completed `members` into a Sand `Json` value.
    fn finish_container(&mut self, members: PartialJsonMembers<'a>) -> VmValue {
        let (id, payload) = match members {
            PartialJsonMembers::Root(json) => return json,
            PartialJsonMembers::Array(items) => (self.ids.arr, self.b.list(items)),
            PartialJsonMembers::Object(map, _) | PartialJsonMembers::Fields(map, _) => (self.ids.obj, self.b.map(map)),
        };
        self.json_variant(id, payload)
    }

    /// Sets the pending key for the next member of the innermost object.
    fn set_member_key(&mut self, key: VmValue, text: Cow<'a, str>) {
        if let PartialJsonMembers::Object(_, next_key) = &mut self.innermost.members {
            *next_key = Some((key, text));
        }
    }

    /// Appends `json` as the next member of the innermost container.
    fn push_member(&mut self, json: VmValue) -> Result<(), Fail> {
        match &mut self.innermost.members {
            PartialJsonMembers::Root(root) => *root = json,
            PartialJsonMembers::Array(items) => items.push(json),
            PartialJsonMembers::Object(map, key) => {
                let Some((key, text)) = key.take() else { return internal("json.to_json: missing object member key") };
                let hash = self.b.hash(&text)?;
                self.b.put(map, hash, Some(text), key, json)?;
            }
            PartialJsonMembers::Fields(map, next_key) => {
                let Some(key) = self.field_keys.get(*next_key) else { return internal("json.to_json: missing field key") };
                *next_key += 1;
                self.b.meter.charge_bytes(key.name_len)?;
                self.b.append(map, key.hash, key.name, json)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_str_escapes_a_byte_at_each_position() {
        for len in 1..=17 {
            for at in 0..len {
                for byte in 0..=0x7fu8 {
                    let (before, after) = ("a".repeat(at), "é".repeat(len - at - 1));
                    let mut out = String::new();
                    write_str(&mut out, &format!("{before}{}{after}", char::from(byte)));
                    let escaped = match byte {
                        b'"' => "\\\"".to_string(),
                        b'\\' => "\\\\".to_string(),
                        b'\n' => "\\n".to_string(),
                        b'\r' => "\\r".to_string(),
                        b'\t' => "\\t".to_string(),
                        0x08 => "\\b".to_string(),
                        0x0c => "\\f".to_string(),
                        0..0x20 => format!("\\u{byte:04x}"),
                        _ => char::from(byte).to_string(),
                    };
                    assert_eq!(out, format!("\"{before}{escaped}{after}\""), "byte {byte:#x} at {at} of {len}");
                }
            }
        }
    }
}
