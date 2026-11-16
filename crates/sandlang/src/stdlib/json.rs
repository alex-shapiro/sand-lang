//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::func::Later;
use crate::native::ty::{Param, TraitDict, VmVal};
use crate::native::{NamedType, NativeModule, variant_index};
use crate::syntax::lexer::is_ident;
use crate::types::traits;
use crate::value::{StructDesc, VariantDesc};
use crate::vm::bytecode::Program;
use crate::vm::codec::MAX_NESTING_DEPTH;
use crate::vm::codec::{self, DecodeShared, EncodeShared};
use crate::vm::compare::str_hash;
use crate::vm::heap::Staging;
use crate::vm::meter::{Meter, SLOW_BYTES_PER_UNIT};
use crate::vm::value::{KeyIndex, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, VmValue, slot_fits};
use crate::vm::{Fail, Vm, fail, internal};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;

const NODE_BYTES: usize = 64;

/// Dynamic JSON value.
///
/// Integers in the 64-bit signed `int` range without a fractional part,
/// exponent, or `-0` are represented as `Int`, and all other numbers are
/// represented as `Float`. `Json` equality is variant-exact, so
/// `Json.Int(1) != Json.Float(1.0)`; use `as_float()` or `as_int()` to read
/// numbers across both variants. Supports up to 512 levels of nesting.
#[derive(Clone, Debug, PartialEq, crate::SandEnum)]
#[sand(module = "json", manual_conversion, encode = "manual", decode = "manual")]
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

#[derive(Clone, Copy)]
struct JsonIds {
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
        JsonIds { null, bool_, int, float, str_, arr, obj }
    }
}

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("json", ModKind::Std, include_str!("json.sand"));
    m.bind::<Json>();
    m.function("parse", |vm: &mut Vm, text: VmVal<String>| -> Result<VmVal<Json>, HostError> {
        let mut source = JsonSource::new(vm, text.raw())?;
        let v = decode_json(vm, &mut source).and_then(|v| source.finish().map(|()| v));
        vm.charge(1 + (source.work() / SLOW_BYTES_PER_UNIT) as u64)?;
        Ok(VmVal::from_raw(v.map_err(HostError::into_ctl_error)?))
    });
    m.function("encode", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, value: Param<0>, indent: i64| {
        let indent = indent.clamp(0, 1 << 16) as usize;
        let shared = EncodeShared::new(JsonSink::new(TextOut::new(indent)));
        codec::encode::<_, _, HostError>(vm, shared, value.raw(), dict.raw(), |vm, sink| {
            Ok(VmVal::<String>::from_raw(vm.new_str(std::mem::take(&mut sink.out.out))))
        })
    });
    m.function(
        "decode",
        |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, text: VmVal<String>| -> Result<Later<Param<0>, HostError>, HostError> {
            let source = JsonSource::new(vm, text.raw())?;
            codec::decode::<_, _, HostError>(vm, DecodeShared::new(source), dict.raw(), |_, v, source| {
                source.finish()?;
                Ok(Param::from_raw(v))
            })
        },
    );
    m
}

const MAX_SHARED_KEY_LEN: usize = 64;
const MAX_SHARED_KEYS: usize = 4096;
const SCANNED_KEYS: usize = 16;

struct Builder<'a, 's, 'h> {
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
    fn new(st: &'s mut Staging<'h>, meter: &'a Meter) -> Self {
        Builder { st, meter, pending: 0, keys: HashTable::new(), entries: Vec::new() }
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
        vm.heap.staged(|_, st| Builder::new(st, meter).json(ids, self))
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

impl crate::native::codec::Encode for Json {
    /// Writes the data of `value`: `Null` as `()`, `Arr` as a list, `Obj` as a
    /// map with `str` keys, and each scalar as itself.
    fn encode(vm: &mut Vm, value: VmVal<Json>, sink: &mut dyn EncodeSink) -> CodecResult {
        encode_json(vm, value.raw(), sink)
    }
}

impl crate::native::codec::Decode for Json {
    /// Reads `Json` from the data of any input.
    fn decode(vm: &mut Vm, source: &mut dyn DecodeSource) -> CodecResult<VmVal<Json>> {
        decode_json(vm, source).map(VmVal::from_raw)
    }
}

fn encode_json(vm: &mut Vm, value: VmValue, sink: &mut dyn EncodeSink) -> CodecResult {
    enum Open<'h> {
        Arr(std::slice::Iter<'h, VmValue>),
        Obj(std::slice::Iter<'h, Option<(VmValue, VmValue)>>),
    }
    let ids = JsonIds::new(&vm.prog)?;
    let (heap, meter) = (&vm.heap, &vm.meter);
    let mut stack: Vec<Open> = Vec::new();
    let mut node = value;
    loop {
        meter.charge(1)?;
        let id = heap.variant_id(node);
        match heap.field(node, 0) {
            _ if id == ids.null => sink.unit()?,
            Some(VmValue::Bool(b)) if id == ids.bool_ => sink.bool(b)?,
            Some(VmValue::Int(i)) if id == ids.int => sink.int(i)?,
            Some(VmValue::Float(f)) if id == ids.float => sink.float(f)?,
            Some(s) if id == ids.str_ => {
                let s = heap.str(s);
                meter.charge_bytes(s.len())?;
                sink.str(s)?;
            }
            Some(items) if id == ids.arr => {
                let Obj::List(items) = heap.get(items) else { return Err(mismatch::<Json>().into()) };
                sink.begin_list(items.len())?;
                stack.push(Open::Arr(items.iter()));
            }
            Some(map) if id == ids.obj => {
                let Obj::Map(map) = heap.get(map) else { return Err(mismatch::<Json>().into()) };
                sink.begin_map(map.live)?;
                stack.push(Open::Obj(map.entries.iter()));
            }
            _ => return Err(mismatch::<Json>().into()),
        }
        if stack.len() > MAX_NESTING_DEPTH {
            return Err(Fail::runtime_error("limit", "value too deeply nested to encode or decode").into());
        }
        node = loop {
            let Some(top) = stack.last_mut() else { return Ok(()) };
            let next = match top {
                Open::Arr(items) => items.next().copied(),
                Open::Obj(entries) => match entries.find_map(|e| *e) {
                    Some((k, v)) => {
                        let k = heap.str(k);
                        meter.charge_bytes(k.len())?;
                        sink.str(k)?;
                        Some(v)
                    }
                    None => None,
                },
            };
            match next {
                Some(v) => break v,
                None => {
                    stack.pop();
                    sink.end()?;
                }
            }
        };
    }
}

fn decode_json(vm: &mut Vm, source: &mut dyn DecodeSource) -> CodecResult<VmValue> {
    enum Open {
        Arr(Vec<VmValue>),
        /// The map and the key text and hash of the value to read next.
        Obj(MapBuilder, String, u64),
    }
    let ids = JsonIds::new(&vm.prog)?;
    let meter = &vm.meter;
    let mut stack: Vec<Open> = Vec::new();
    vm.heap.staged(|_, st| {
        let mut b = Builder::new(st, meter);
        loop {
            b.reserve()?;
            meter.charge(1 + (source.work() / SLOW_BYTES_PER_UNIT) as u64)?;
            let mut value = match source.kind()? {
                InputKind::Null => {
                    source.unit()?;
                    Some(VmValue::Tag(ids.null))
                }
                InputKind::Bool => Some(b.st.new_variant(ids.bool_, &[VmValue::Bool(source.bool()?)])),
                InputKind::Int => Some(b.st.new_variant(ids.int, &[VmValue::Int(source.int()?)])),
                InputKind::Float => Some(b.st.new_variant(ids.float, &[VmValue::Float(source.float()?)])),
                InputKind::Str => {
                    let s = source.str()?;
                    b.st.check_memory((NODE_BYTES + b.pending).saturating_add(s.len()))?;
                    meter.charge_bytes(s.len())?;
                    let s = b.st.new_str(&*s);
                    Some(b.st.new_variant(ids.str_, &[s]))
                }
                InputKind::List => {
                    source.begin_list()?;
                    stack.push(Open::Arr(Vec::new()));
                    None
                }
                InputKind::Map => {
                    source.begin_map()?;
                    stack.push(Open::Obj(b.start_map(), String::new(), 0));
                    None
                }
                InputKind::Duration | InputKind::Bytes | InputKind::Variant => {
                    return Err(HostError::new("parse", format!("{}: expected JSON data", source.path())));
                }
            };
            if stack.len() > MAX_NESTING_DEPTH {
                return Err(Fail::runtime_error("limit", "value too deeply nested to encode or decode").into());
            }
            loop {
                let next = match stack.last_mut() {
                    None => return value.ok_or_else(|| missing_root_error().into()),
                    Some(Open::Arr(items)) => {
                        if let Some(v) = value.take() {
                            b.push(items, v)?;
                        }
                        source.next()?
                    }
                    Some(Open::Obj(m, key, hash)) => {
                        if let Some(v) = value.take() {
                            let text = Cow::Owned(std::mem::take(key));
                            let k = b.key(&text, *hash);
                            b.put(m, *hash, Some(text), k, v)?;
                        }
                        match source.next()? {
                            true => {
                                let text = source.str()?;
                                *hash = b.hash(&text)?;
                                *key = text.into_owned();
                                true
                            }
                            false => false,
                        }
                    }
                };
                if next {
                    break;
                }
                source.end()?;
                value = Some(match stack.pop() {
                    Some(Open::Arr(items)) => {
                        let list = b.list(items);
                        b.st.new_variant(ids.arr, &[list])
                    }
                    Some(Open::Obj(m, ..)) => {
                        let map = b.map(m);
                        b.st.new_variant(ids.obj, &[map])
                    }
                    None => return Err(missing_root_error().into()),
                });
            }
        }
    })
}

fn invalid_key<T>() -> CodecResult<T> {
    Err(HostError::new("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type"))
}

/// An open container of a [`JsonSink`].
enum Frame {
    /// A JSON array.
    Array,
    /// A map, whose even parts are keys.
    Map { key_next: bool },
    /// The fields of a struct.
    Struct { desc: Arc<StructDesc>, next: usize },
    /// The named payload of a variant, inside the object that holds the
    /// variant name.
    Named { desc: Arc<VariantDesc>, next: usize },
    /// The positional payload of a variant, inside the object that holds
    /// the variant name.
    Positional,
    /// `Some`, or a variant without a payload, which writes no bracket.
    Bare,
}

/// Applies the JSON rules to Sand values and writes them as JSON text.
///
/// `Some(x)` is `x`, and `None` and `()` are `null`. `bytes` is a Base64
/// string, `duration` is a count of nanoseconds, and a native value is its
/// text. A map key is the text of a string, an int, a duration, a bool, a
/// variant without a payload, or a native value. A variant without a payload
/// is its name, and another variant is an object of one member, from the
/// variant name to an array or an object of the payload.
struct JsonSink {
    out: TextOut,
    frames: Vec<Frame>,
}

impl JsonSink {
    fn new(out: TextOut) -> JsonSink {
        JsonSink { out, frames: Vec::new() }
    }

    /// Prepares the next value. Returns `true` if the value is a map key.
    fn slot(&mut self) -> CodecResult<bool> {
        match self.frames.last_mut() {
            Some(Frame::Map { key_next }) => {
                let is_key = *key_next;
                *key_next = !is_key;
                Ok(is_key)
            }
            Some(Frame::Struct { desc, next }) => {
                let name = desc.fields.get(*next).map_or("", String::as_str);
                *next += 1;
                self.out.key(name)?;
                Ok(false)
            }
            Some(Frame::Named { desc, next }) => {
                let name = desc.fields.as_ref().and_then(|f| f.get(*next)).map_or("", String::as_str);
                *next += 1;
                self.out.key(name)?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Writes a scalar: as a key if it is a map key, or else as `value`.
    fn scalar(&mut self, key: impl FnOnce() -> Option<String>, value: impl FnOnce(&mut TextOut) -> CodecResult) -> CodecResult {
        match self.slot()? {
            true => match key() {
                Some(k) => self.out.key(&k),
                None => invalid_key(),
            },
            false => value(&mut self.out),
        }
    }

    fn begin(&mut self, frame: Frame) -> CodecResult {
        if self.slot()? {
            return invalid_key();
        }
        self.frames.push(frame);
        Ok(())
    }
}

impl codec::WalkSink for JsonSink {
    fn set_max_allocated(&mut self, bytes: usize) {
        self.out.max_allocated = bytes;
    }
}

impl EncodeSink for JsonSink {
    fn unit(&mut self) -> CodecResult {
        self.scalar(|| None, |o| o.null())
    }

    fn bool(&mut self, b: bool) -> CodecResult {
        self.scalar(|| Some(b.to_string()), |o| o.bool(b))
    }

    fn int(&mut self, i: i64) -> CodecResult {
        self.scalar(|| Some(i.to_string()), |o| o.int(i))
    }

    fn float(&mut self, f: f64) -> CodecResult {
        if !f.is_finite() {
            return Err(HostError::new("invalid", format!("JSON cannot represent the float {f}")));
        }
        self.scalar(|| None, |o| o.float(f))
    }

    fn duration(&mut self, nanos: i64) -> CodecResult {
        self.int(nanos)
    }

    fn str(&mut self, s: &str) -> CodecResult {
        match self.slot()? {
            true => self.out.key(s),
            false => self.out.str(s),
        }
    }

    fn bytes(&mut self, b: &[u8]) -> CodecResult {
        if self.slot()? {
            return invalid_key();
        }
        self.out.str(&base64::encode(b, false))
    }

    fn range(&mut self, start: i64, end: i64, inclusive: bool) -> CodecResult {
        if self.slot()? {
            return invalid_key();
        }
        self.out.begin_object(2 + usize::from(inclusive))?;
        for (name, bound) in [("start", start), ("end", end)] {
            self.out.key(name)?;
            self.out.int(bound)?;
        }
        if inclusive {
            self.out.key("inclusive")?;
            self.out.bool(true)?;
        }
        self.out.end()
    }

    fn native(&mut self, _: &str, text: &str) -> CodecResult {
        match self.slot()? {
            true => self.out.key(text),
            false => self.out.str(text),
        }
    }

    fn none(&mut self) -> CodecResult {
        self.unit()
    }

    fn begin_some(&mut self) -> CodecResult {
        if self.slot()? {
            return invalid_key();
        }
        self.frames.push(Frame::Bare);
        Ok(())
    }

    fn begin_list(&mut self, len: usize) -> CodecResult {
        self.begin(Frame::Array)?;
        self.out.begin_array(len)
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        self.begin_list(len)
    }

    fn begin_set(&mut self, len: usize) -> CodecResult {
        self.begin_list(len)
    }

    fn begin_map(&mut self, len: usize) -> CodecResult {
        self.begin(Frame::Map { key_next: true })?;
        self.out.begin_object(len)
    }

    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult {
        self.begin(Frame::Struct { desc: desc.clone(), next: 0 })?;
        self.out.begin_object(desc.fields.len())
    }

    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult {
        if len == 0 {
            let r = self.str(&desc.name);
            self.frames.push(Frame::Bare);
            return r;
        }
        let frame = match desc.fields {
            Some(_) => Frame::Named { desc: desc.clone(), next: 0 },
            None => Frame::Positional,
        };
        self.begin(frame)?;
        self.out.begin_object(1)?;
        self.out.key(&desc.name)?;
        match desc.fields {
            Some(_) => self.out.begin_object(len),
            None => self.out.begin_array(len),
        }
    }

    fn end(&mut self) -> CodecResult {
        match self.frames.pop() {
            Some(Frame::Bare) | None => {}
            Some(Frame::Named { .. } | Frame::Positional) => {
                self.out.end()?;
                self.out.end()?;
            }
            Some(_) => self.out.end()?,
        }
        Ok(())
    }

    fn allocated(&self) -> usize {
        self.out.allocated()
    }
}

const SPACES: &str = "                                                                ";

/// Writes JSON text.
struct TextOut {
    out: String,
    /// The most bytes that `out` can hold within the memory limit.
    max_allocated: usize,
    /// Spaces per level, or 0 for compact text.
    indent: usize,
    /// For each open array or object: whether it is an array, and whether it
    /// has no member yet.
    open: Vec<(bool, bool)>,
    /// Whether a key precedes the next value.
    after_key: bool,
}

/// The most bytes that a scalar other than a string writes.
const SCALAR_BYTES: usize = 32;

impl TextOut {
    fn new(indent: usize) -> TextOut {
        TextOut { out: String::new(), max_allocated: usize::MAX, indent, open: Vec::new(), after_key: false }
    }

    /// Fails with the memory limit if `out` cannot grow by `bytes`.
    #[inline(always)]
    fn reserve(&self, bytes: usize) -> CodecResult {
        let need = self.out.len().saturating_add(bytes);
        if need > self.out.capacity() && need.max(2 * self.out.capacity()) > self.max_allocated {
            return crate::native::codec::memory_limit();
        }
        Ok(())
    }

    /// Writes the separator before the next member of the innermost
    /// container, and reserves `bytes` for the member.
    #[inline(always)]
    fn separate(&mut self, bytes: usize) -> CodecResult {
        if std::mem::replace(&mut self.after_key, false) {
            return self.reserve(bytes);
        }
        if let Some((_, empty)) = self.open.last_mut() {
            if !std::mem::replace(empty, false) {
                self.out.push(',');
            }
            self.start_line()?;
        }
        self.reserve(bytes)
    }

    /// Writes a newline and indentation if `self.indent` is positive.
    #[inline(always)]
    fn start_line(&mut self) -> CodecResult {
        if self.indent == 0 {
            return Ok(());
        }
        let mut left = self.indent.saturating_mul(self.open.len());
        self.reserve(left.saturating_add(2))?;
        self.out.push('\n');
        while left > 0 {
            let spaces = &SPACES[..left.min(SPACES.len())];
            self.out.push_str(spaces);
            left -= spaces.len();
        }
        Ok(())
    }

    fn begin(&mut self, is_array: bool) -> CodecResult {
        if self.open.len() >= MAX_NESTING_DEPTH {
            return Err(Fail::runtime_error("limit", "value too deeply nested to encode as JSON").into());
        }
        self.separate(1)?;
        self.out.push(if is_array { '[' } else { '{' });
        self.open.push((is_array, true));
        Ok(())
    }

    fn null(&mut self) -> CodecResult {
        self.separate(SCALAR_BYTES)?;
        self.out.push_str("null");
        Ok(())
    }

    fn bool(&mut self, b: bool) -> CodecResult {
        self.separate(SCALAR_BYTES)?;
        self.out.push_str(if b { "true" } else { "false" });
        Ok(())
    }

    fn int(&mut self, i: i64) -> CodecResult {
        self.separate(SCALAR_BYTES)?;
        self.out.push_str(itoa::Buffer::new().format(i));
        Ok(())
    }

    fn float(&mut self, f: f64) -> CodecResult {
        self.separate(SCALAR_BYTES)?;
        let _ = write!(self.out, "{f:?}");
        Ok(())
    }

    fn str(&mut self, s: &str) -> CodecResult {
        self.separate(quoted_len(s))?;
        write_str(&mut self.out, s);
        Ok(())
    }

    fn begin_array(&mut self, _: usize) -> CodecResult {
        self.begin(true)
    }

    fn begin_object(&mut self, _: usize) -> CodecResult {
        self.begin(false)
    }

    fn key(&mut self, k: &str) -> CodecResult {
        self.separate(quoted_len(k) + 2)?;
        write_str(&mut self.out, k);
        self.out.push(':');
        if self.indent > 0 {
            self.out.push(' ');
        }
        self.after_key = true;
        Ok(())
    }

    fn end(&mut self) -> CodecResult {
        let Some((is_array, empty)) = self.open.pop() else { return Ok(()) };
        if !empty {
            self.start_line()?;
        }
        self.reserve(1)?;
        self.out.push(if is_array { ']' } else { '}' });
        Ok(())
    }

    fn allocated(&self) -> usize {
        self.out.capacity()
    }
}

/// The longest key that a path quotes in full.
const QUOTED_KEY: usize = 64;

/// A segment of the path of a [`JsonSource`].
enum Segment {
    None,
    Index(usize),
    /// A key, as a byte range of the input text.
    Span(usize, usize),
    /// A key with escapes.
    Owned(String),
}

/// An open container of a [`JsonSource`].
struct Level {
    kind: LevelKind,
    /// The path segment of the current member.
    segment: Segment,
    /// The number of members read.
    count: usize,
    /// Whether the input of the container has ended.
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LevelKind {
    /// An array, with the length of a tuple.
    Array(Option<usize>),
    Object,
    Map,
    Some,
    /// A variant: written as a string, or as an object of one member.
    Variant {
        object: bool,
    },
}

/// Reads JSON text for the decode walker.
pub(crate) struct JsonSource {
    text: Box<str>,
    pos: usize,
    /// The position at the last call of [`DecodeSource::work`].
    charged: usize,
    levels: Vec<Level>,
    /// The map key that the walker reads next, as a byte range or as owned
    /// text.
    key: Option<Segment>,
    /// The text of the last string with escapes.
    scratch: String,
    /// The names of the variant and of its enum from the last
    /// `begin_variant`, for messages.
    variant: String,
    enum_name: String,
}

/// The result of a string scan: a byte range of the input, or the scratch
/// text.
enum Scanned {
    Span(usize, usize),
    Scratch,
}

impl JsonSource {
    /// Copies `text`, a Sand `str`, into a new source.
    fn new(vm: &Vm, text: VmValue) -> Result<JsonSource, Fail> {
        let text = vm.heap.str(text);
        vm.check_memory(text.len())?;
        let text: Box<str> = text.into();
        Ok(JsonSource {
            text,
            pos: 0,
            charged: 0,
            levels: Vec::new(),
            key: None,
            scratch: String::new(),
            variant: String::new(),
            enum_name: String::new(),
        })
    }

    /// Fails if text other than whitespace follows the value.
    fn finish(&mut self) -> CodecResult {
        self.ws();
        if self.pos < self.text.len() { Err(self.unexpected()) } else { Ok(()) }
    }

    fn input(&self) -> &[u8] {
        self.text.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.input().get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.input().get(self.pos) {
            self.pos += 1;
        }
    }

    fn err_at(&self, pos: usize, what: &str) -> HostError<Fail> {
        let pos = pos.min(self.text.len());
        let before = &self.input()[..pos];
        let line = 1 + before.iter().filter(|&&c| c == b'\n').count();
        let line_start = before.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        let col = 1 + self.text.get(line_start..pos).map_or(pos - line_start, |s| s.chars().count());
        HostError::new("parse", format!("{what} at line {line}, column {col}"))
    }

    fn unexpected(&self) -> HostError<Fail> {
        match self.text.get(self.pos..).and_then(|s| s.chars().next()) {
            None => self.err_at(self.pos, "unexpected end of input"),
            Some(c) => self.err_at(self.pos, &format!("unexpected character `{}`", c.escape_debug())),
        }
    }

    fn fail(&self, msg: &str) -> HostError<Fail> {
        HostError::new("parse", format!("{}: {msg}", self.path()))
    }

    /// Returns an error for a value that is not `what`.
    fn mismatch(&self, what: &str) -> HostError<Fail> {
        match self.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => self.fail(&format!("expected {what}")),
            _ => self.unexpected(),
        }
    }

    /// Skips whitespace and returns the next byte. Fails at a map key, which
    /// only a scalar read can take.
    fn start(&mut self, what: &str) -> CodecResult<Option<u8>> {
        if let Some(key) = self.key.take() {
            let text = self.segment_text(&key).to_string();
            return Err(self.fail(&format!("expected {what}, found the map key `{}`", super::truncate(&text, QUOTED_KEY))));
        }
        self.ws();
        Ok(self.peek())
    }

    fn literal(&mut self, word: &[u8]) -> CodecResult {
        if self.input()[self.pos..].starts_with(word) {
            self.pos += word.len();
            return Ok(());
        }
        while self.eat(word[0]) {}
        Err(self.unexpected())
    }

    fn digits(&mut self) {
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
    }

    /// Scans a number. Returns its byte range and whether it has integer
    /// syntax.
    fn number(&mut self) -> CodecResult<(usize, usize, bool)> {
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
        Ok((start, self.pos, is_int))
    }

    fn hex4(&mut self, esc: usize) -> CodecResult<u32> {
        let digits = self.text.get(self.pos..self.pos + 4).filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()));
        let Some(h) = digits else { return Err(self.err_at(esc, "invalid unicode escape")) };
        let code = u32::from_str_radix(h, 16).unwrap_or(0);
        self.pos += 4;
        Ok(code)
    }

    /// Scans a string at the opening quote. Keeps the text of a string with
    /// escapes in `scratch` if `keep` is set.
    fn string(&mut self, keep: bool) -> CodecResult<Scanned> {
        self.pos += 1;
        let first = self.pos;
        let mut escaped = false;
        loop {
            let start = self.pos;
            while let Some(&c) = self.input().get(self.pos) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            match self.peek() {
                None => return Err(self.unexpected()),
                Some(b'"') => {
                    if escaped && keep {
                        let run = &self.text[start..self.pos];
                        self.scratch.push_str(run);
                    }
                    self.pos += 1;
                    return Ok(if escaped { Scanned::Scratch } else { Scanned::Span(first, self.pos - 1) });
                }
                Some(b'\\') => {
                    if keep {
                        if !escaped {
                            self.scratch.clear();
                        }
                        let run = &self.text[start..self.pos];
                        self.scratch.push_str(run);
                    }
                    escaped = true;
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
                                self.scratch.push(c);
                            }
                            continue;
                        }
                        _ => return Err(self.err_at(esc, "invalid escape")),
                    };
                    self.pos += 1;
                    if keep {
                        self.scratch.push(c);
                    }
                }
                Some(_) => return Err(self.err_at(self.pos, "control character in string")),
            }
        }
    }

    fn scanned(&self, s: &Scanned) -> &str {
        match s {
            Scanned::Span(a, b) => &self.text[*a..*b],
            Scanned::Scratch => &self.scratch,
        }
    }

    fn segment_text<'a>(&'a self, s: &'a Segment) -> &'a str {
        match s {
            Segment::Span(a, b) => &self.text[*a..*b],
            Segment::Owned(t) => t,
            Segment::None | Segment::Index(_) => "",
        }
    }

    /// Scans a string and returns it as a path segment.
    fn key_segment(&mut self) -> CodecResult<Segment> {
        Ok(match self.string(true)? {
            Scanned::Span(a, b) => Segment::Span(a, b),
            Scanned::Scratch => Segment::Owned(self.scratch.clone()),
        })
    }

    /// Reads a string value, or the text of the map key to read.
    fn text_value(&mut self, what: &str) -> CodecResult<Cow<'_, str>> {
        if let Some(key) = self.key.take() {
            return Ok(match key {
                Segment::Span(a, b) => Cow::Borrowed(&self.text[a..b]),
                Segment::Owned(t) => Cow::Owned(t),
                _ => Cow::Borrowed(""),
            });
        }
        self.ws();
        if self.peek() != Some(b'"') {
            return Err(self.mismatch(what));
        }
        let s = self.string(true)?;
        Ok(Cow::Borrowed(self.scanned(&s)))
    }

    /// Parses the map key to read with `parse`, or reads a number.
    fn number_value<T>(
        &mut self,
        what: &str,
        key: impl Fn(&str) -> Option<T>,
        number: impl Fn(&Self, &str, bool) -> CodecResult<T>,
    ) -> CodecResult<T> {
        if let Some(k) = self.key.take() {
            let text = self.segment_text(&k);
            return key(text).ok_or_else(|| self.fail(&format!("invalid map key `{}`", super::truncate(text, QUOTED_KEY))));
        }
        self.ws();
        if !matches!(self.peek(), Some(b'-' | b'0'..=b'9')) {
            return Err(self.mismatch(what));
        }
        let (a, b, is_int) = self.number()?;
        number(self, &self.text[a..b], is_int)
    }

    fn open(&mut self, kind: LevelKind) -> CodecResult {
        if self.levels.len() >= MAX_NESTING_DEPTH {
            return Err(self.err_at(self.pos, "nesting too deep"));
        }
        self.levels.push(Level { kind, segment: Segment::None, count: 0, closed: false });
        Ok(())
    }

    fn top(&mut self) -> CodecResult<&mut Level> {
        self.levels.last_mut().ok_or_else(|| HostError::from(Fail::internal("json: no open container")))
    }

    /// Reads the separator before the next member of the innermost array or
    /// object. Returns `false` and closes the container at its end.
    fn next_member(&mut self, close: u8) -> CodecResult<bool> {
        self.ws();
        let first = self.top()?.count == 0;
        if self.eat(close) {
            self.top()?.closed = true;
            return Ok(false);
        }
        if !first && !self.eat(b',') {
            return Err(self.unexpected());
        }
        self.ws();
        let top = self.top()?;
        top.count += 1;
        Ok(true)
    }

    /// Reads the key of the next member of the innermost object and the `:`
    /// after it. Returns `None` and closes the object at its end.
    fn next_key(&mut self) -> CodecResult<Option<Segment>> {
        if !self.next_member(b'}')? {
            return Ok(None);
        }
        if self.peek() != Some(b'"') {
            return Err(self.unexpected());
        }
        let key = self.key_segment()?;
        self.ws();
        if !self.eat(b':') {
            return Err(self.unexpected());
        }
        Ok(Some(key))
    }

    /// Begins the payload of a variant in object form.
    fn begin_payload_value(&mut self, form: PayloadForm) -> CodecResult {
        match form {
            PayloadForm::Named => {
                if self.start("object")? != Some(b'{') {
                    return Err(self.mismatch("object"));
                }
                self.pos += 1;
                self.open(LevelKind::Object)
            }
            PayloadForm::Unit => self.begin_tuple(0),
            PayloadForm::Positional(n) => self.begin_tuple(n),
        }
    }

    fn short_name(type_name: &str) -> &str {
        type_name.rsplit('.').next().unwrap_or(type_name)
    }
}

impl DecodeSource for JsonSource {
    fn kind(&mut self) -> CodecResult<InputKind> {
        if self.key.is_some() {
            return Ok(InputKind::Str);
        }
        self.ws();
        Ok(match self.peek() {
            Some(b'n') => InputKind::Null,
            Some(b't' | b'f') => InputKind::Bool,
            Some(b'"') => InputKind::Str,
            Some(b'[') => InputKind::List,
            Some(b'{') => InputKind::Map,
            Some(b'-' | b'0'..=b'9') => {
                let at = self.pos;
                let (a, b, is_int) = self.number()?;
                let text = &self.text[a..b];
                let int = is_int && text.parse::<i64>().is_ok() && !(text.starts_with('-') && text.parse::<i64>() == Ok(0));
                self.pos = at;
                if int { InputKind::Int } else { InputKind::Float }
            }
            _ => return Err(self.unexpected()),
        })
    }

    fn unit(&mut self) -> CodecResult {
        match self.start("null")? {
            Some(b'n') => self.literal(b"null"),
            _ => Err(self.mismatch("null")),
        }
    }

    fn bool(&mut self) -> CodecResult<bool> {
        if let Some(k) = self.key.take() {
            return match self.segment_text(&k) {
                "true" => Ok(true),
                "false" => Ok(false),
                t => Err(self.fail(&format!("invalid map key `{}`", super::truncate(t, QUOTED_KEY)))),
            };
        }
        self.ws();
        match self.peek() {
            Some(b't') => self.literal(b"true").map(|()| true),
            Some(b'f') => self.literal(b"false").map(|()| false),
            _ => Err(self.mismatch("bool")),
        }
    }

    fn int(&mut self) -> CodecResult<i64> {
        self.number_value(
            "int",
            |k| k.parse().ok(),
            |s, text, is_int| {
                if is_int && let Ok(i) = text.parse() {
                    return Ok(i);
                }
                let f: f64 = text.parse().unwrap_or(f64::NAN);
                if f.fract() == 0.0 && (-9223372036854775808.0..9223372036854775808.0).contains(&f) {
                    return Ok(f as i64);
                }
                Err(s.fail(if f.is_finite() && f.fract() == 0.0 { "integer out of range" } else { "expected int" }))
            },
        )
    }

    fn float(&mut self) -> CodecResult<f64> {
        self.number_value("float", |_| None, |_, text, _| Ok(text.parse().unwrap_or(f64::NAN)))
    }

    fn duration(&mut self) -> CodecResult<i64> {
        self.int()
    }

    fn str(&mut self) -> CodecResult<Cow<'_, str>> {
        self.text_value("str")
    }

    fn bytes(&mut self) -> CodecResult<Cow<'_, [u8]>> {
        let decoded = {
            let text = self.text_value("base64 str")?;
            base64::decode(&text, false)
        };
        decoded.map(Cow::Owned).map_err(|_| self.fail("invalid base64"))
    }

    fn range(&mut self) -> CodecResult<(i64, i64, bool)> {
        if self.start("object")? != Some(b'{') {
            return Err(self.mismatch("object"));
        }
        self.pos += 1;
        self.open(LevelKind::Object)?;
        let (mut start, mut end, mut inclusive) = (None, None, false);
        while let Some(key) = self.next_key()? {
            let name = self.segment_text(&key).to_string();
            self.top()?.segment = key;
            match &*name {
                "start" => start = Some(self.int()?),
                "end" => end = Some(self.int()?),
                "inclusive" => inclusive = self.bool()?,
                _ => self.skip()?,
            }
        }
        self.top()?.segment = Segment::None;
        let r = match (start, end) {
            (Some(start), Some(end)) => Ok((start, end, inclusive)),
            (None, _) => Err(self.fail("missing field `start`")),
            (_, None) => Err(self.fail("missing field `end`")),
        };
        self.levels.pop();
        r
    }

    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        let name = Self::short_name(type_name).to_string();
        self.text_value(&name)
    }

    fn option(&mut self) -> CodecResult<bool> {
        if self.key.is_none() {
            self.ws();
            if self.peek() == Some(b'n') {
                self.literal(b"null")?;
                return Ok(false);
            }
        }
        self.open(LevelKind::Some)?;
        Ok(true)
    }

    fn begin_list(&mut self) -> CodecResult {
        if self.start("array")? != Some(b'[') {
            return Err(self.mismatch("array"));
        }
        self.open(LevelKind::Array(None))?;
        self.pos += 1;
        Ok(())
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        let what = format!("array of length {len}");
        if self.start(&what)? != Some(b'[') {
            return Err(self.mismatch(&what));
        }
        self.open(LevelKind::Array(Some(len)))?;
        self.pos += 1;
        Ok(())
    }

    fn begin_map(&mut self) -> CodecResult {
        if self.start("object")? != Some(b'{') {
            return Err(self.mismatch("object"));
        }
        self.open(LevelKind::Map)?;
        self.pos += 1;
        Ok(())
    }

    fn next(&mut self) -> CodecResult<bool> {
        match self.top()?.kind {
            LevelKind::Map => match self.next_key()? {
                Some(key) => {
                    let copy = match &key {
                        Segment::Span(a, b) => Segment::Span(*a, *b),
                        Segment::Owned(t) => Segment::Owned(t.clone()),
                        _ => Segment::None,
                    };
                    self.top()?.segment = key;
                    self.key = Some(copy);
                    Ok(true)
                }
                None => Ok(false),
            },
            LevelKind::Array(len) => {
                let count = self.top()?.count;
                if !self.next_member(b']')? {
                    return Ok(false);
                }
                if len.is_some_and(|n| count >= n) {
                    return Err(self.fail(&format!("expected array of length {}", len.unwrap_or(0))));
                }
                self.top()?.segment = Segment::Index(count);
                Ok(true)
            }
            _ => Err(Fail::internal("json: `next` outside an array or a map").into()),
        }
    }

    fn begin_struct(&mut self, desc: &StructDesc) -> CodecResult {
        let _ = desc;
        if self.start("object")? != Some(b'{') {
            return Err(self.mismatch("object"));
        }
        self.open(LevelKind::Object)?;
        self.pos += 1;
        Ok(())
    }

    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>> {
        match self.next_key()? {
            Some(key) => {
                self.top()?.segment = key;
                let top = self.levels.last().expect("an open object");
                Ok(Some(Cow::Borrowed(self.segment_text(&top.segment))))
            }
            None => {
                self.top()?.segment = Segment::None;
                Ok(None)
            }
        }
    }

    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        let name = Self::short_name(type_name).to_string();
        self.enum_name.clone_from(&name);
        if let Some(key) = self.key.take() {
            self.open(LevelKind::Variant { object: false })?;
            self.variant = self.segment_text(&key).to_string();
            return Ok(Cow::Borrowed(&self.variant));
        }
        self.ws();
        match self.peek() {
            Some(b'"') => {
                let s = self.string(true)?;
                self.variant = self.scanned(&s).to_string();
                self.open(LevelKind::Variant { object: false })?;
            }
            Some(b'{') => {
                self.pos += 1;
                self.open(LevelKind::Variant { object: true })?;
                match self.next_key()? {
                    Some(key) => self.variant = self.segment_text(&key).to_string(),
                    None => return Err(self.fail(&format!("expected a variant of {name}"))),
                }
            }
            _ => return Err(self.mismatch(&name)),
        }
        Ok(Cow::Borrowed(&self.variant))
    }

    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult {
        let LevelKind::Variant { object } = self.top()?.kind else {
            return Err(Fail::internal("json: a payload outside a variant").into());
        };
        match (object, form) {
            (false, PayloadForm::Unit) => Ok(()),
            (false, _) => {
                let msg = format!("variant `{}` of {} needs a payload", self.variant, self.enum_name);
                Err(self.fail(&msg))
            }
            (true, form) => {
                self.top()?.segment = Segment::Owned(self.variant.clone());
                self.begin_payload_value(form)
            }
        }
    }

    fn skip(&mut self) -> CodecResult {
        if self.key.take().is_some() {
            return Ok(());
        }
        let depth = self.levels.len();
        loop {
            self.ws();
            match self.peek() {
                Some(b'n') => self.literal(b"null")?,
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'"') => {
                    self.string(false)?;
                }
                Some(b'-' | b'0'..=b'9') => drop(self.number()?),
                Some(b'[') => {
                    self.pos += 1;
                    self.open(LevelKind::Array(None))?;
                }
                Some(b'{') => {
                    self.pos += 1;
                    self.open(LevelKind::Object)?;
                }
                _ => return Err(self.unexpected()),
            }
            loop {
                if self.levels.len() == depth {
                    return Ok(());
                }
                let more = match self.top()?.kind {
                    LevelKind::Object => self.next_key()?.is_some(),
                    _ => self.next_member(b']')?,
                };
                if more {
                    break;
                }
                self.levels.pop();
            }
        }
    }

    fn end(&mut self) -> CodecResult {
        let Some(level) = self.levels.pop() else { return Ok(()) };
        if let LevelKind::Array(Some(n)) = level.kind {
            self.ws();
            if level.count != n || (!level.closed && !self.eat(b']')) {
                self.levels.push(level);
                return Err(self.fail(&format!("expected array of length {n}")));
            }
        }
        if let Some(Level { kind: LevelKind::Variant { object: true }, .. }) = self.levels.last()
            && !matches!(level.kind, LevelKind::Variant { .. })
        {
            // The payload ended. Close the object that holds the variant name.
            self.ws();
            if !self.eat(b'}') {
                let msg = format!("expected a single variant of {}", self.enum_name);
                return Err(if self.peek() == Some(b',') { self.fail(&msg) } else { self.unexpected() });
            }
            self.levels.pop();
        }
        Ok(())
    }

    fn path(&self) -> String {
        let mut s = String::from("$");
        for level in &self.levels {
            match &level.segment {
                Segment::None => {}
                Segment::Index(i) => {
                    let _ = write!(s, "[{i}]");
                }
                seg => {
                    let k = super::truncate(self.segment_text(seg), QUOTED_KEY);
                    if is_ident(k) {
                        s.push('.');
                        s.push_str(k);
                    } else {
                        s.push('[');
                        write_str(&mut s, k);
                        s.push(']');
                    }
                }
            }
        }
        s
    }

    fn allocated(&self) -> usize {
        self.text.len() + self.scratch.capacity() + self.levels.capacity() * std::mem::size_of::<Level>()
    }

    fn work(&mut self) -> usize {
        let consumed = self.pos.saturating_sub(self.charged);
        self.charged = self.pos;
        consumed
    }
}

#[inline(always)]
/// Returns a bound on the bytes that [`write_str`] writes for `s`. The bound
/// of a short string skips the scan for escapes.
fn quoted_len(s: &str) -> usize {
    let escaped = if s.len() <= 4096 || has_escape(s.as_bytes()) { 6 * s.len() } else { s.len() };
    escaped + 2
}

fn write_str(out: &mut String, s: &str) {
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
