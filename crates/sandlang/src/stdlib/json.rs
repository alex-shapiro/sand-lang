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
use crate::vm::bytecode::{Program, ShapeKind};
use crate::vm::codec::{self, Shared};
use crate::vm::codec::{MAX_NESTING_DEPTH, NODE_BYTES};
use crate::vm::compare::str_hash;
use crate::vm::heap::Staging;
use crate::vm::iomem::{IoMemory, Reservation};
use crate::vm::meter::{Meter, SLOW_BYTES_PER_UNIT};
use crate::vm::value::{KeyIndex, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, VmValue, slot_fits};
use crate::vm::{Fail, Vm, fail, internal};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;

/// Dynamic JSON value.
///
/// Signed 64-bit integers without a fractional part, exponent, or `-0` are
/// represented as `Int`. All other numbers are represented as `Float`. Two
/// `Json` values are equal only if they have the same variant and equal
/// payloads. In particular, `Json.Int(1) != Json.Float(1.0)`. Call `as_float()`
/// or `as_int()` to read a number from either numeric variant. `Json` values
/// can be nested up to 512 levels.
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
        let shared = Shared::new(JsonSink::new(JsonTextWriter::new(indent, &vm.heap.io_memory)));
        codec::encode::<_, _, HostError>(vm, shared, value.raw(), dict.raw(), |vm, sink| {
            let (text, reservation) = sink.out.take_text();
            drop(reservation);
            Ok(VmVal::<String>::from_raw(vm.new_str(text)))
        })
    });
    m.function("to_json", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, value: Param<0>| {
        if let Some(json) = direct_json(vm, dict.raw(), value.raw())? {
            return Ok(Later::ready(Ok(VmVal::<Json>::from_raw(json))));
        }
        let shared = Shared::new(JsonSink::new(JsonTextWriter::new(0, &vm.heap.io_memory)));
        codec::encode::<_, _, Fail>(vm, shared, value.raw(), dict.raw(), |vm, sink| {
            let (text, reservation) = sink.out.take_text();
            let mut source = JsonSource::owned(text.into_boxed_str(), reservation);
            let json = decode_json(vm, &mut source)?;
            vm.charge(1 + (source.work() / SLOW_BYTES_PER_UNIT) as u64)?;
            Ok(VmVal::<Json>::from_raw(json))
        })
    });
    m.function(
        "decode",
        |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, text: VmVal<String>| -> Result<Later<Param<0>, HostError>, HostError> {
            let source = JsonSource::new(vm, text.raw())?;
            codec::decode::<_, _, HostError>(vm, Shared::new(source), dict.raw(), |_, v, source| {
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
    /// Writes the underlying JSON value of `value` to `sink`.
    fn encode(vm: &mut Vm, value: VmVal<Json>, sink: &mut dyn EncodeSink) -> CodecResult {
        encode_json(vm, value.raw(), sink)
    }
}

impl crate::native::codec::Decode for Json {
    /// Reads a JSON value from `source`.
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
            return codec::too_deep();
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
        /// Map builder, pending key text, and key hash for the next entry.
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
                InputKind::Duration
                | InputKind::Bytes
                | InputKind::Tuple
                | InputKind::Struct
                | InputKind::Range
                | InputKind::Native
                | InputKind::Variant => {
                    return Err(HostError::new("parse", format!("{}: expected JSON data", source.path())));
                }
            };
            if stack.len() > MAX_NESTING_DEPTH {
                return codec::too_deep();
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

/// Converts `value` to `Json` without a codec walk if `dict` is the built-in
/// `Encode` dictionary of `Json` or of a JSON scalar.
///
/// This fast path handles common `json!` interpolations directly. Returns
/// `None` for composite values, custom `Encode` implementations, and non-finite
/// floats.
fn direct_json(vm: &mut Vm, dict: VmValue, value: VmValue) -> Result<Option<VmValue>, Fail> {
    let ids = JsonIds::new(&vm.prog)?;
    let kind = codec::structural_kind(vm, dict)?;
    // Variant IDs are unique across a program. A manual-codec value with a
    // `Json` variant ID is therefore already a `Json` value.
    if let Some(ShapeKind::ManualEncode(_)) = kind
        && [ids.null, ids.bool_, ids.int, ids.float, ids.str_, ids.arr, ids.obj].contains(&vm.heap.variant_id(value))
    {
        return Ok(all_floats_finite(vm, &ids, value)?.then_some(value));
    }
    let id = match (kind, value) {
        (Some(ShapeKind::Unit), _) => return Ok(Some(VmValue::Tag(ids.null))),
        (Some(ShapeKind::Bool), VmValue::Bool(_)) => ids.bool_,
        (Some(ShapeKind::Int), VmValue::Int(_)) => ids.int,
        (Some(ShapeKind::Float), VmValue::Float(f)) if f.is_finite() => ids.float,
        (Some(ShapeKind::Str), _) => ids.str_,
        _ => return Ok(None),
    };
    vm.charge(1)?;
    Ok(Some(vm.heap.new_variant(id, &[value])))
}

/// Returns `true` if every float in `json` is finite.
///
/// Charges fuel for each visited `Json` node.
fn all_floats_finite(vm: &Vm, ids: &JsonIds, json: VmValue) -> Result<bool, Fail> {
    let mut stack = vec![json];
    while let Some(v) = stack.pop() {
        vm.charge(1)?;
        let id = vm.heap.variant_id(v);
        let payload = vm.heap.payload_if(&v, id);
        if id == ids.float {
            if !matches!(payload, VmValue::Float(f) if f.is_finite()) {
                return Ok(false);
            }
        } else if id == ids.arr {
            stack.extend_from_slice(vm.heap.list(payload));
        } else if id == ids.obj
            && let Obj::Map(m) = vm.heap.get(payload)
        {
            stack.extend(m.entries.iter().flatten().map(|&(_, v)| v));
        }
    }
    Ok(true)
}

fn invalid_key_error<T>() -> CodecResult<T> {
    Err(HostError::new("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type"))
}

/// Open container frame in [`JsonSink`].
enum SinkFrame {
    Array,
    Map {
        key_next: bool,
    },
    Struct {
        desc: Arc<StructDesc>,
        next: usize,
    },
    /// Struct-variant payload inside the outer `{"Variant": ...}` object.
    NamedVariant {
        desc: Arc<VariantDesc>,
        next: usize,
    },
    /// Tuple-variant payload inside the outer `{"Variant": ...}` object.
    PositionalVariant,
    /// `Some` or a unit variant written inline without enclosing brackets.
    Inline,
}

/// [`EncodeSink`] that serializes Sand values as JSON text.
struct JsonSink {
    out: JsonTextWriter,
    frames: Vec<SinkFrame>,
}

impl JsonSink {
    fn new(out: JsonTextWriter) -> JsonSink {
        JsonSink { out, frames: Vec::new() }
    }

    /// Emits any pending struct or variant field key before the next value.
    ///
    /// Returns `true` if the next value is a map key.
    fn prepare_next_value(&mut self) -> CodecResult<bool> {
        match self.frames.last_mut() {
            Some(SinkFrame::Map { key_next }) => {
                let is_key = *key_next;
                *key_next = !is_key;
                Ok(is_key)
            }
            Some(SinkFrame::Struct { desc, next }) => {
                let name = desc.fields.get(*next).map_or("", String::as_str);
                *next += 1;
                self.out.key(name)?;
                Ok(false)
            }
            Some(SinkFrame::NamedVariant { desc, next }) => {
                let name = desc.fields.as_ref().and_then(|f| f.get(*next)).map_or("", String::as_str);
                *next += 1;
                self.out.key(name)?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Writes a scalar either as a map key via `format_key` or as a JSON value
    /// via `write_value`.
    fn write_scalar(
        &mut self,
        format_key: impl FnOnce() -> Option<String>,
        write_value: impl FnOnce(&mut JsonTextWriter) -> CodecResult,
    ) -> CodecResult {
        match self.prepare_next_value()? {
            true => match format_key() {
                Some(k) => self.out.key(&k),
                None => invalid_key_error(),
            },
            false => write_value(&mut self.out),
        }
    }

    fn begin(&mut self, frame: SinkFrame) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.frames.push(frame);
        Ok(())
    }
}

impl EncodeSink for JsonSink {
    fn unit(&mut self) -> CodecResult {
        self.write_scalar(|| None, |o| o.null())
    }

    fn bool(&mut self, b: bool) -> CodecResult {
        self.write_scalar(|| Some(b.to_string()), |o| o.bool(b))
    }

    fn int(&mut self, i: i64) -> CodecResult {
        self.write_scalar(|| Some(i.to_string()), |o| o.int(i))
    }

    fn float(&mut self, f: f64) -> CodecResult {
        if !f.is_finite() {
            return Err(HostError::new("invalid", format!("JSON cannot represent the float {f}")));
        }
        self.write_scalar(|| None, |o| o.float(f))
    }

    fn duration(&mut self, nanos: i64) -> CodecResult {
        self.int(nanos)
    }

    fn str(&mut self, s: &str) -> CodecResult {
        match self.prepare_next_value()? {
            true => self.out.key(s),
            false => self.out.str(s),
        }
    }

    fn bytes(&mut self, b: &[u8]) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.out.base64(b)
    }

    fn range(&mut self, start: i64, end: i64, inclusive: bool) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.out.begin(false)?;
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
        match self.prepare_next_value()? {
            true => self.out.key(text),
            false => self.out.str(text),
        }
    }

    fn none(&mut self) -> CodecResult {
        self.unit()
    }

    fn begin_some(&mut self) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.frames.push(SinkFrame::Inline);
        Ok(())
    }

    fn begin_list(&mut self, _: usize) -> CodecResult {
        self.begin(SinkFrame::Array)?;
        self.out.begin(true)
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        self.begin_list(len)
    }

    fn begin_set(&mut self, len: usize) -> CodecResult {
        self.begin_list(len)
    }

    fn begin_map(&mut self, _: usize) -> CodecResult {
        self.begin(SinkFrame::Map { key_next: true })?;
        self.out.begin(false)
    }

    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult {
        self.begin(SinkFrame::Struct { desc: desc.clone(), next: 0 })?;
        self.out.begin(false)
    }

    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult {
        if len == 0 {
            let r = self.str(&desc.name);
            self.frames.push(SinkFrame::Inline);
            return r;
        }
        let frame = match desc.fields {
            Some(_) => SinkFrame::NamedVariant { desc: desc.clone(), next: 0 },
            None => SinkFrame::PositionalVariant,
        };
        self.begin(frame)?;
        self.out.begin(false)?;
        self.out.key(&desc.name)?;
        match desc.fields {
            Some(_) => self.out.begin(false),
            None => self.out.begin(true),
        }
    }

    fn end(&mut self) -> CodecResult {
        match self.frames.pop() {
            Some(SinkFrame::Inline) | None => {}
            Some(SinkFrame::NamedVariant { .. } | SinkFrame::PositionalVariant) => {
                self.out.end()?;
                self.out.end()?;
            }
            Some(_) => self.out.end()?,
        }
        Ok(())
    }
}

const SPACES: &str = "                                                                ";

struct OpenJsonContainer {
    is_array: bool,
    is_empty: bool,
}

/// Formats tokens, separators, and indentation into a memory-tracked JSON buffer.
struct JsonTextWriter {
    out: String,
    /// Tracks the memory capacity of `out` against `max_memory`.
    reservation: Reservation,
    /// Spaces per indentation level, or `0` for compact output.
    indent: usize,
    open_containers: Vec<OpenJsonContainer>,
    /// `true` right after an object key and `:` have been written.
    after_key: bool,
}

/// Maximum byte length of any encoded non-string JSON scalar.
const MAX_SCALAR_BYTES: usize = 32;

impl JsonTextWriter {
    fn new(indent: usize, memory: &Arc<IoMemory>) -> JsonTextWriter {
        JsonTextWriter { out: String::new(), reservation: Reservation::new(memory), indent, open_containers: Vec::new(), after_key: false }
    }

    /// Takes the finished JSON string and its memory reservation.
    fn take_text(&mut self) -> (String, Reservation) {
        let sibling = self.reservation.sibling();
        (std::mem::take(&mut self.out), std::mem::replace(&mut self.reservation, sibling))
    }

    /// Ensures `out` has capacity for `bytes` additional bytes within `max_memory`.
    #[inline(always)]
    fn reserve(&mut self, bytes: usize) -> CodecResult {
        let need = self.out.len().saturating_add(bytes);
        if need <= self.out.capacity() {
            return Ok(());
        }
        self.grow(need)
    }

    /// Grows `out` to at least `need` bytes using the largest growth step that
    /// fits within `max_memory`.
    ///
    /// Reserves `target + cap` bytes because both the old and new buffers are
    /// allocated during reallocation.
    #[cold]
    fn grow(&mut self, need: usize) -> CodecResult {
        let cap = self.out.capacity();
        for target in [cap.saturating_mul(2), cap + cap / 2, cap + cap / 4, cap + cap / 8] {
            let target = target.max(need);
            if self.reservation.ensure(target.saturating_add(cap)).is_ok() {
                self.out.reserve_exact(target - self.out.len());
                return Ok(());
            }
        }
        crate::native::codec::memory_limit()
    }

    /// Writes any comma and indentation needed before the next token.
    ///
    /// Also reserves `bytes` of output capacity for that token.
    #[inline(always)]
    fn separate(&mut self, bytes: usize) -> CodecResult {
        if std::mem::replace(&mut self.after_key, false) {
            return self.reserve(bytes);
        }
        if let Some(container) = self.open_containers.last_mut() {
            if !std::mem::replace(&mut container.is_empty, false) {
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
        let mut left = self.indent.saturating_mul(self.open_containers.len());
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
        if self.open_containers.len() >= MAX_NESTING_DEPTH {
            return codec::too_deep();
        }
        self.separate(1)?;
        self.out.push(if is_array { '[' } else { '{' });
        self.open_containers.push(OpenJsonContainer { is_array, is_empty: true });
        Ok(())
    }

    fn null(&mut self) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        self.out.push_str("null");
        Ok(())
    }

    fn bool(&mut self, b: bool) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        self.out.push_str(if b { "true" } else { "false" });
        Ok(())
    }

    fn int(&mut self, i: i64) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        self.out.push_str(itoa::Buffer::new().format(i));
        Ok(())
    }

    fn float(&mut self, f: f64) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        let _ = write!(self.out, "{f:?}");
        Ok(())
    }

    fn str(&mut self, s: &str) -> CodecResult {
        self.separate(quoted_len(s))?;
        write_str(&mut self.out, s);
        Ok(())
    }

    /// Writes `b` as a quoted Base64 string.
    fn base64(&mut self, b: &[u8]) -> CodecResult {
        self.separate(b.len().div_ceil(3) * 4 + 2)?;
        self.out.push('"');
        base64::encode_into(&mut self.out, b, false);
        self.out.push('"');
        Ok(())
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
        let Some(container) = self.open_containers.pop() else { return Ok(()) };
        if !container.is_empty {
            self.start_line()?;
        }
        self.reserve(1)?;
        self.out.push(if container.is_array { ']' } else { '}' });
        Ok(())
    }
}

/// Maximum byte length of an object key shown untruncated in a JSON path.
const MAX_PATH_KEY_LEN: usize = 64;

/// Path component of an open container in [`JsonSource`].
enum PathSegment {
    None,
    Index(usize),
    /// Unescaped key slice `start..end` in `JsonSource::text`.
    Span(usize, usize),
    /// Unescaped key text decoded from a JSON string with escapes.
    Owned(String),
}

/// Open container frame in [`JsonSource`].
struct SourceFrame {
    kind: SourceFrameKind,
    /// Path segment of the current element or entry.
    segment: PathSegment,
    /// Number of elements or entries read from this frame.
    count: usize,
    /// `true` once the closing `]` or `}` has been consumed.
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceFrameKind {
    /// JSON array, optionally constrained to an exact tuple length.
    Array(Option<usize>),
    Object,
    Map,
    Some,
    /// Enum variant encoded either as a string or as a single-entry object.
    Variant {
        is_object: bool,
    },
}

/// [`DecodeSource`] that parses JSON text for the decode walker.
pub(crate) struct JsonSource {
    text: Box<str>,
    /// Holds the memory reservation for `text` until decoding finishes.
    _text_reservation: Reservation,
    /// Tracks peak memory used by `scratch` and Base64 decoding.
    scratch_reservation: Reservation,
    pos: usize,
    /// Byte offset in `text` already reported by [`DecodeSource::work`].
    charged: usize,
    frames: Vec<SourceFrame>,
    /// Number of `SourceFrameKind::Some` frames in `frames`.
    ///
    /// `Some` frames omit JSON brackets and are excluded from the nesting limit.
    some_depth: usize,
    /// Map key buffered for the next scalar decode step.
    pending_map_key: Option<PathSegment>,
    /// Unescaped contents of the most recently scanned escaped string.
    scratch: String,
    /// Variant name from the active `begin_variant` call for error messages.
    variant_name: String,
    /// Enum name from the active `begin_variant` call for error messages.
    enum_name: String,
}

/// Location of a scanned JSON string's unescaped text.
enum ScannedStr {
    /// Unescaped slice `start..end` in `JsonSource::text`.
    Span(usize, usize),
    /// Unescaped string written into `JsonSource::scratch`.
    Scratch,
}

impl JsonSource {
    /// Copies the Sand string `text` into a memory-tracked [`JsonSource`].
    fn new(vm: &Vm, text: VmValue) -> Result<JsonSource, Fail> {
        let text = vm.heap.str(text);
        let mut reservation = Reservation::new(&vm.heap.io_memory);
        if reservation.ensure(text.len()).is_err() {
            return Err(Fail::limit(crate::engine::LimitKind::Memory));
        }
        Ok(JsonSource::owned(text.into(), reservation))
    }

    /// Creates a [`JsonSource`] over `text` backed by `reservation`.
    fn owned(text: Box<str>, reservation: Reservation) -> JsonSource {
        JsonSource {
            text,
            scratch_reservation: reservation.sibling(),
            _text_reservation: reservation,
            pos: 0,
            charged: 0,
            frames: Vec::new(),
            some_depth: 0,
            pending_map_key: None,
            scratch: String::new(),
            variant_name: String::new(),
            enum_name: String::new(),
        }
    }

    /// Fails if `self` has trailing non-whitespace bytes.
    fn finish(&mut self) -> CodecResult {
        self.skip_whitespace();
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

    fn skip_whitespace(&mut self) {
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

    /// Returns a parse error stating that `what` was expected at the current token.
    fn mismatch(&self, what: &str) -> HostError<Fail> {
        match self.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => self.fail(&format!("expected {what}")),
            _ => self.unexpected(),
        }
    }

    /// Skips whitespace and peeks at the next input byte.
    ///
    /// Fails if a map key is pending because only scalar decoders can read map keys.
    fn start(&mut self, what: &str) -> CodecResult<Option<u8>> {
        if let Some(key) = self.pending_map_key.take() {
            let text = self.segment_text(&key).to_string();
            return Err(self.fail(&format!("expected {what}, found the map key `{}`", super::truncate(&text, MAX_PATH_KEY_LEN))));
        }
        self.skip_whitespace();
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

    /// Scans a JSON number and returns `(start, end, is_int)`.
    ///
    /// `is_int` is `false` if the token has a fraction or exponent.
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

    /// Scans a quoted JSON string starting at `"`.
    ///
    /// Writes unescaped text into `self.scratch` if the string contains escapes
    /// and `keep` is `true`.
    fn string(&mut self, keep: bool) -> CodecResult<ScannedStr> {
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
                        if self.scratch_reservation.ensure(self.scratch.capacity()).is_err() {
                            return crate::native::codec::memory_limit();
                        }
                    }
                    self.pos += 1;
                    return Ok(if escaped { ScannedStr::Scratch } else { ScannedStr::Span(first, self.pos - 1) });
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

    fn scanned_str(&self, s: &ScannedStr) -> &str {
        match s {
            ScannedStr::Span(a, b) => &self.text[*a..*b],
            ScannedStr::Scratch => &self.scratch,
        }
    }

    fn segment_text<'a>(&'a self, s: &'a PathSegment) -> &'a str {
        match s {
            PathSegment::Span(a, b) => &self.text[*a..*b],
            PathSegment::Owned(t) => t,
            PathSegment::None | PathSegment::Index(_) => "",
        }
    }

    /// Scans a string and returns it as a path segment.
    fn key_segment(&mut self) -> CodecResult<PathSegment> {
        Ok(match self.string(true)? {
            ScannedStr::Span(a, b) => PathSegment::Span(a, b),
            ScannedStr::Scratch => PathSegment::Owned(self.scratch.clone()),
        })
    }

    /// Consumes a pending map key or scans a quoted JSON string.
    fn text_value(&mut self, what: &str) -> CodecResult<Cow<'_, str>> {
        if let Some(key) = self.pending_map_key.take() {
            return Ok(match key {
                PathSegment::Span(a, b) => Cow::Borrowed(&self.text[a..b]),
                PathSegment::Owned(t) => Cow::Owned(t),
                _ => Cow::Borrowed(""),
            });
        }
        self.skip_whitespace();
        if self.peek() != Some(b'"') {
            return Err(self.mismatch(what));
        }
        let s = self.string(true)?;
        Ok(Cow::Borrowed(self.scanned_str(&s)))
    }

    /// Parses a pending map key with `parse_key` or a JSON number token with
    /// `parse_number`.
    fn number_value<T>(
        &mut self,
        what: &str,
        parse_key: impl Fn(&str) -> Option<T>,
        parse_number: impl Fn(&Self, &str, bool) -> CodecResult<T>,
    ) -> CodecResult<T> {
        if let Some(k) = self.pending_map_key.take() {
            let text = self.segment_text(&k);
            return parse_key(text).ok_or_else(|| self.fail(&format!("invalid map key `{}`", super::truncate(text, MAX_PATH_KEY_LEN))));
        }
        self.skip_whitespace();
        if !matches!(self.peek(), Some(b'-' | b'0'..=b'9')) {
            return Err(self.mismatch(what));
        }
        let (a, b, is_int) = self.number()?;
        parse_number(self, &self.text[a..b], is_int)
    }

    fn open(&mut self, kind: SourceFrameKind) -> CodecResult {
        if kind == SourceFrameKind::Some {
            self.some_depth += 1;
        } else if self.frames.len() - self.some_depth >= MAX_NESTING_DEPTH {
            return Err(self.err_at(self.pos, "nesting too deep"));
        }
        self.frames.push(SourceFrame { kind, segment: PathSegment::None, count: 0, closed: false });
        Ok(())
    }

    fn top(&mut self) -> CodecResult<&mut SourceFrame> {
        self.frames.last_mut().ok_or_else(|| HostError::from(Fail::internal("json: no open container")))
    }

    /// Advances to the next member of the innermost container.
    ///
    /// Consumes `close` and returns `false` at the closing delimiter.
    fn next_member(&mut self, close: u8) -> CodecResult<bool> {
        self.skip_whitespace();
        let first = self.top()?.count == 0;
        if self.eat(close) {
            self.top()?.closed = true;
            return Ok(false);
        }
        if !first && !self.eat(b',') {
            return Err(self.unexpected());
        }
        self.skip_whitespace();
        let top = self.top()?;
        top.count += 1;
        Ok(true)
    }

    /// Reads the next object key and its trailing `:`.
    ///
    /// Consumes `}` and returns `None` at the end of the object.
    fn next_key(&mut self) -> CodecResult<Option<PathSegment>> {
        if !self.next_member(b'}')? {
            return Ok(None);
        }
        if self.peek() != Some(b'"') {
            return Err(self.unexpected());
        }
        let key = self.key_segment()?;
        self.skip_whitespace();
        if !self.eat(b':') {
            return Err(self.unexpected());
        }
        Ok(Some(key))
    }

    /// Opens the container for a variant payload inside `{"Variant": ...}`.
    fn begin_payload_value(&mut self, form: PayloadForm) -> CodecResult {
        match form {
            PayloadForm::Named => {
                if self.start("object")? != Some(b'{') {
                    return Err(self.mismatch("object"));
                }
                self.pos += 1;
                self.open(SourceFrameKind::Object)
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
        if self.pending_map_key.is_some() {
            return Ok(InputKind::Str);
        }
        self.skip_whitespace();
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
        if let Some(k) = self.pending_map_key.take() {
            return match self.segment_text(&k) {
                "true" => Ok(true),
                "false" => Ok(false),
                t => Err(self.fail(&format!("invalid map key `{}`", super::truncate(t, MAX_PATH_KEY_LEN)))),
            };
        }
        self.skip_whitespace();
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
        let mut decoded_reservation = self.scratch_reservation.sibling();
        let decoded = {
            let text = self.text_value("base64 str")?;
            if decoded_reservation.ensure(text.len() / 4 * 3 + 3).is_err() {
                return crate::native::codec::memory_limit();
            }
            base64::decode(&text, false)
        };
        let need = self.scratch.capacity() + decoded_reservation.bytes();
        drop(decoded_reservation);
        if self.scratch_reservation.ensure(need).is_err() {
            return crate::native::codec::memory_limit();
        }
        decoded.map(Cow::Owned).map_err(|_| self.fail("invalid base64"))
    }

    fn range(&mut self) -> CodecResult<(i64, i64, bool)> {
        if self.start("object")? != Some(b'{') {
            return Err(self.mismatch("object"));
        }
        self.pos += 1;
        self.open(SourceFrameKind::Object)?;
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
        self.top()?.segment = PathSegment::None;
        let r = match (start, end) {
            (Some(start), Some(end)) => Ok((start, end, inclusive)),
            (None, _) => Err(self.fail("missing field `start`")),
            (_, None) => Err(self.fail("missing field `end`")),
        };
        self.frames.pop();
        r
    }

    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        let name = Self::short_name(type_name).to_string();
        self.text_value(&name)
    }

    fn option(&mut self) -> CodecResult<bool> {
        if self.pending_map_key.is_none() {
            self.skip_whitespace();
            if self.peek() == Some(b'n') {
                self.literal(b"null")?;
                return Ok(false);
            }
        }
        self.open(SourceFrameKind::Some)?;
        Ok(true)
    }

    fn begin_list(&mut self) -> CodecResult {
        if self.start("array")? != Some(b'[') {
            return Err(self.mismatch("array"));
        }
        self.open(SourceFrameKind::Array(None))?;
        self.pos += 1;
        Ok(())
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        let what = format!("array of length {len}");
        if self.start(&what)? != Some(b'[') {
            return Err(self.mismatch(&what));
        }
        self.open(SourceFrameKind::Array(Some(len)))?;
        self.pos += 1;
        Ok(())
    }

    fn begin_map(&mut self) -> CodecResult {
        if self.start("object")? != Some(b'{') {
            return Err(self.mismatch("object"));
        }
        self.open(SourceFrameKind::Map)?;
        self.pos += 1;
        Ok(())
    }

    fn next(&mut self) -> CodecResult<bool> {
        match self.top()?.kind {
            SourceFrameKind::Map => match self.next_key()? {
                Some(key) => {
                    let copy = match &key {
                        PathSegment::Span(a, b) => PathSegment::Span(*a, *b),
                        PathSegment::Owned(t) => PathSegment::Owned(t.clone()),
                        _ => PathSegment::None,
                    };
                    self.top()?.segment = key;
                    self.pending_map_key = Some(copy);
                    Ok(true)
                }
                None => Ok(false),
            },
            SourceFrameKind::Array(len) => {
                let count = self.top()?.count;
                if !self.next_member(b']')? {
                    return Ok(false);
                }
                if len.is_some_and(|n| count >= n) {
                    return Err(self.fail(&format!("expected array of length {}", len.unwrap_or(0))));
                }
                self.top()?.segment = PathSegment::Index(count);
                Ok(true)
            }
            _ => Err(Fail::internal("json: `next` outside an array or a map").into()),
        }
    }

    fn begin_struct(&mut self, _: &StructDesc) -> CodecResult {
        if self.start("object")? != Some(b'{') {
            return Err(self.mismatch("object"));
        }
        self.open(SourceFrameKind::Object)?;
        self.pos += 1;
        Ok(())
    }

    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>> {
        match self.next_key()? {
            Some(key) => {
                self.top()?.segment = key;
                let top = self.frames.last().expect("an open object");
                Ok(Some(Cow::Borrowed(self.segment_text(&top.segment))))
            }
            None => {
                self.top()?.segment = PathSegment::None;
                Ok(None)
            }
        }
    }

    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        let name = Self::short_name(type_name).to_string();
        self.enum_name.clone_from(&name);
        if let Some(key) = self.pending_map_key.take() {
            self.open(SourceFrameKind::Variant { is_object: false })?;
            self.variant_name = self.segment_text(&key).to_string();
            return Ok(Cow::Borrowed(&self.variant_name));
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => {
                let s = self.string(true)?;
                self.variant_name = self.scanned_str(&s).to_string();
                self.open(SourceFrameKind::Variant { is_object: false })?;
            }
            Some(b'{') => {
                self.pos += 1;
                self.open(SourceFrameKind::Variant { is_object: true })?;
                match self.next_key()? {
                    Some(key) => self.variant_name = self.segment_text(&key).to_string(),
                    None => return Err(self.fail(&format!("expected a variant of {name}"))),
                }
            }
            _ => return Err(self.mismatch(&name)),
        }
        Ok(Cow::Borrowed(&self.variant_name))
    }

    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult {
        let SourceFrameKind::Variant { is_object } = self.top()?.kind else {
            return Err(Fail::internal("json: a payload outside a variant").into());
        };
        match (is_object, form) {
            (false, PayloadForm::Unit) => Ok(()),
            (false, _) => {
                let msg = format!("variant `{}` of {} needs a payload", self.variant_name, self.enum_name);
                Err(self.fail(&msg))
            }
            (true, form) => {
                self.top()?.segment = PathSegment::Owned(self.variant_name.clone());
                self.begin_payload_value(form)
            }
        }
    }

    fn skip(&mut self) -> CodecResult {
        if self.pending_map_key.take().is_some() {
            return Ok(());
        }
        let depth = self.frames.len();
        loop {
            self.skip_whitespace();
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
                    self.open(SourceFrameKind::Array(None))?;
                }
                Some(b'{') => {
                    self.pos += 1;
                    self.open(SourceFrameKind::Object)?;
                }
                _ => return Err(self.unexpected()),
            }
            loop {
                if self.frames.len() == depth {
                    return Ok(());
                }
                let more = match self.top()?.kind {
                    SourceFrameKind::Object => self.next_key()?.is_some(),
                    _ => self.next_member(b']')?,
                };
                if more {
                    break;
                }
                self.frames.pop();
            }
        }
    }

    fn end(&mut self) -> CodecResult {
        let Some(frame) = self.frames.pop() else { return Ok(()) };
        if frame.kind == SourceFrameKind::Some {
            self.some_depth -= 1;
        }
        if let SourceFrameKind::Array(Some(n)) = frame.kind {
            self.skip_whitespace();
            if frame.count != n || (!frame.closed && !self.eat(b']')) {
                self.frames.push(frame);
                return Err(self.fail(&format!("expected array of length {n}")));
            }
        }
        if let Some(SourceFrame { kind: SourceFrameKind::Variant { is_object: true }, .. }) = self.frames.last()
            && !matches!(frame.kind, SourceFrameKind::Variant { .. })
        {
            // Consume the outer `}` of `{"Variant": payload}`.
            self.skip_whitespace();
            if !self.eat(b'}') {
                let msg = format!("expected a single variant of {}", self.enum_name);
                return Err(if self.peek() == Some(b',') { self.fail(&msg) } else { self.unexpected() });
            }
            self.frames.pop();
        }
        Ok(())
    }

    fn path(&self) -> String {
        let mut s = String::from("$");
        for frame in &self.frames {
            match &frame.segment {
                PathSegment::None => {}
                PathSegment::Index(i) => {
                    let _ = write!(s, "[{i}]");
                }
                seg => {
                    let k = super::truncate(self.segment_text(seg), MAX_PATH_KEY_LEN);
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

    fn work(&mut self) -> usize {
        let consumed = self.pos.saturating_sub(self.charged);
        self.charged = self.pos;
        consumed
    }
}

/// Returns an upper bound on the byte length that [`write_str`] writes for `s`.
///
/// Assumes worst-case escaping for strings up to 4096 bytes to avoid scanning twice.
#[inline(always)]
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
