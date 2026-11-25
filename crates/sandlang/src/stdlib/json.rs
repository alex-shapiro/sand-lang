//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::context::CallContext;
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::ty::{Param, TraitDict, VmVal};
use crate::native::{NamedType, NativeModule, variant_index};
use crate::stdlib::codec::{self, Decoder, Encoder};
use crate::stdlib::codec::{MAX_NESTING_DEPTH, NODE_BYTES};
use crate::syntax::lexer::is_ident;
use crate::types::traits;
use crate::value::{StructDesc, VariantDesc};
use crate::vm::bytecode::{Program, ShapeKind};
use crate::vm::compare::str_hash;
use crate::vm::heap::Staging;
use crate::vm::iomem::{IoMemory, Reservation};
use crate::vm::meter::{BYTES_PER_FUEL_UNIT, SLOW_BYTES_PER_FUEL_UNIT, VmMeter, WorkerMeter};
use crate::vm::value::{KeyIndex, MAP_ENTRY_SIZE, MapObj, Obj, VmValue, slot_fits};
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
#[sand(module = "json", manual_conversion)]
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
    let mut module = NativeModule::std("json", ModKind::Std, include_str!("json.sand"));
    module.bind::<Json>();
    module.function("parse", |context: &mut CallContext, text: VmVal<String>| -> CodecResult<VmVal<Json>> {
        let mut source = context.step(|vm| JsonSource::new(vm, text.raw()))?;
        let json = decode_json(context, &mut source)?;
        source.finish()?;
        context.vm().charge(1 + (source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
        Ok(VmVal::from_raw(json))
    });
    module.function("Json.encode", |context: &mut CallContext, value: VmVal<Json>, encoder: VmVal<Encoder>| {
        Encoder::write(context, encoder, |context, sink| encode_json(context.vm(), value.raw(), sink))
    });
    module.function("Json.decode", |context: &mut CallContext, decoder: VmVal<Decoder>| {
        Decoder::read(context, decoder, |context, source| decode_json(context, source).map(VmVal::<Json>::from_raw))
    });
    module.function(
        "encode",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>, indent: i64| -> CodecResult<VmVal<String>> {
            let indent = indent.clamp(0, 1 << 16) as usize;
            let sink = JsonSink::new(JsonTextWriter::new(indent, &context.vm().heap.io_memory));
            codec::encode(context, sink, value.raw(), dict.raw(), |context, mut sink| {
                let (text, reservation) = sink.writer.take_text();
                drop(reservation);
                context.step(|vm| vm.check_memory(NODE_BYTES.saturating_add(text.len())))?;
                Ok(VmVal::<String>::from_raw(context.vm_mut().new_str(text)))
            })
        },
    );
    module.function(
        "to_json",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>| -> Result<VmVal<Json>, Fail> {
            let dict_root = context.root(dict.raw());
            let val_root = context.root(value.raw());
            if let Some(json) = context.step(|vm| direct_json(vm, dict.raw(), value.raw()))? {
                return Ok(VmVal::<Json>::from_raw(json));
            }
            let sink = JsonSink::new(JsonValueWriter::new(&context.vm().heap.io_memory));
            codec::encode(context, sink, context.value(val_root), context.value(dict_root), |context, mut sink| {
                let (json, _reservation) = sink.writer.take_json().ok_or_else(|| Fail::internal("to_json: no value"))?;
                let vm_val = context.step(|vm| json_to_vm_value(vm, &json))?;
                Ok(VmVal::<Json>::from_raw(vm_val))
            })
            .map_err(codec::walk_error_to_fail)
        },
    );
    module.function(
        "decode",
        |context: &mut CallContext, dict: TraitDict<0, traits::Decode>, text: VmVal<String>| -> CodecResult<Param<0>> {
            let dict_root = context.root(dict.raw());
            let source = context.step(|vm| JsonSource::new(vm, text.raw()))?;
            codec::decode(context, source, context.value(dict_root), |context, value, mut source| {
                source.finish()?;
                context.vm().charge((source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
                Ok(Param::from_raw(value))
            })
        },
    );
    module
}

const MAX_SHARED_KEY_LEN: usize = 64;
const MAX_SHARED_KEYS: usize = 4096;

struct Builder<'a, 's, 'h> {
    st: &'s mut Staging<'h>,
    meter: &'a VmMeter,
    pending: usize,
    /// Shared key strings with their hashes and text spans.
    keys: HashTable<(u64, u32, u32, VmValue)>,
    entries: Vec<Entry>,
    /// Text of interned short keys in `keys`.
    key_text: String,
}

struct Entry {
    hash: u64,
    key: VmValue,
    value: VmValue,
}

impl<'a, 's, 'h> Builder<'a, 's, 'h> {
    fn new(st: &'s mut Staging<'h>, meter: &'a VmMeter) -> Self {
        Builder { st, meter, pending: 0, keys: HashTable::new(), entries: Vec::new(), key_text: String::new() }
    }

    #[inline]
    fn reserve(&self) -> Result<(), Fail> {
        self.st.check_memory(NODE_BYTES + self.pending)
    }

    /// Appends `v` to `items` after checking the memory limit.
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

    /// Returns the key string for `key`, sharing short keys across the value.
    fn key(&mut self, key: &str, hash: u64) -> VmValue {
        let key_text = &self.key_text;
        if let Some(&(_, _, _, v)) = self.keys.find(hash, |e| &key_text[e.1 as usize..(e.1 + e.2) as usize] == key) {
            return v;
        }
        let v = self.st.new_str(key);
        if key.len() <= MAX_SHARED_KEY_LEN && self.keys.len() < MAX_SHARED_KEYS {
            let start = self.key_text.len() as u32;
            self.key_text.push_str(key);
            self.pending += 2 * NODE_BYTES + key.len();
            self.keys.insert_unique(hash, (hash, start, key.len() as u32, v), |e| e.0);
        }
        v
    }

    /// Appends a unique key and value to the map starting at `base`.
    fn append(&mut self, base: usize, hash: u64, key: VmValue, value: VmValue) -> Result<(), Fail> {
        if !slot_fits(self.entries.len() - base) {
            return fail("limit", "map too large");
        }
        self.pending += ENTRY_PENDING;
        self.entries.push(Entry { hash, key, value });
        Ok(())
    }

    fn map(&mut self, base: usize) -> VmValue {
        let n = self.entries.len() - base;
        let index = KeyIndex::for_keys(n, self.entries[base..].iter().map(|e| e.key));
        let mut map = MapObj { entries: Vec::with_capacity(n), live: n, index, ..MapObj::default() };
        for (slot, e) in self.entries.drain(base..).enumerate() {
            map.index.insert(e.key, e.hash, slot);
            map.entries.push(Some((e.key, e.value)));
        }
        self.pending = self.pending.saturating_sub(n * ENTRY_PENDING);
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
        json_to_vm_value(vm, &self)
    }
}

fn json_to_vm_value(vm: &mut Vm, json: &Json) -> Result<VmValue, Fail> {
    let ids = JsonIds::new(&vm.prog)?;
    let meter = &vm.meter;
    vm.heap.staged(|_, st| Builder::new(st, meter).json(ids, json))
}

fn missing_root_error() -> Fail {
    Fail::internal("json: missing root value in conversion")
}

/// Open Sand `Json` array or object during conversion to Rust [`Json`].
enum ReadFrame<'h> {
    Arr(Vec<Json>, std::slice::Iter<'h, VmValue>),
    /// Converted members, remaining entries, and the key of the current member.
    Obj(indexmap::IndexMap<String, Json>, std::slice::Iter<'h, Option<(VmValue, VmValue)>>, String),
}

const OBJ_ENTRY_BYTES: usize = std::mem::size_of::<(u64, String, Json)>() + std::mem::size_of::<usize>();

/// Copies the Sand `str` `v` into a Rust `String`.
///
/// Charges fuel and conversion memory for the string bytes.
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

/// Open Rust [`Json`] array or object during conversion to Sand `Json`.
enum WriteFrame<'j> {
    Arr(std::slice::Iter<'j, Json>, Vec<VmValue>),
    /// Remaining members, map start offset, and the key and hash of the current
    /// member.
    Obj(indexmap::map::Iter<'j, String, Json>, usize, &'j str, u64),
}

impl Builder<'_, '_, '_> {
    /// Converts `json` into a Sand `Json` value.
    fn json(&mut self, ids: JsonIds, json: &Json) -> Result<VmValue, Fail> {
        let mut stack = Vec::new();
        let mut node = json;
        loop {
            self.reserve()?;
            self.meter.charge(1)?;
            let mut value = match node {
                Json::Null => Some(VmValue::Tag(ids.null)),
                Json::Bool(b) => Some(self.st.new_variant(ids.bool_, &[VmValue::Bool(*b)])),
                Json::Int(i) => Some(self.st.new_variant(ids.int, &[VmValue::Int(*i)])),
                Json::Float(f) => Some(self.st.new_variant(ids.float, &[VmValue::Float(*f)])),
                Json::Str(s) => {
                    self.st.check_memory((NODE_BYTES + self.pending).saturating_add(s.len()))?;
                    self.meter.charge_bytes(s.len())?;
                    let s = self.st.new_str(s.as_str());
                    Some(self.st.new_variant(ids.str_, &[s]))
                }
                Json::Arr(items) => {
                    stack.push(WriteFrame::Arr(items.iter(), Vec::new()));
                    None
                }
                Json::Obj(members) => {
                    stack.push(WriteFrame::Obj(members.iter(), self.entries.len(), "", 0));
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
                    Some(WriteFrame::Obj(members, base, key, hash)) => {
                        if let Some(value) = value.take() {
                            let key = self.key(key, *hash);
                            self.append(*base, *hash, key, value)?;
                        }
                        if let Some((next_key, member)) = members.next() {
                            *hash = self.hash(next_key)?;
                            *key = next_key.as_str();
                            break member;
                        }
                    }
                }
                value = stack.pop().map(|frame| match frame {
                    WriteFrame::Arr(_, values) => {
                        let list = self.list(values);
                        self.st.new_variant(ids.arr, &[list])
                    }
                    WriteFrame::Obj(_, base, ..) => {
                        let map = self.map(base);
                        self.st.new_variant(ids.obj, &[map])
                    }
                });
            };
        }
    }
}

fn encode_json(vm: &Vm, value: VmValue, sink: &mut dyn EncodeSink) -> CodecResult {
    enum EncodeJsonFrame<'h> {
        Arr(std::slice::Iter<'h, VmValue>),
        Obj(std::slice::Iter<'h, Option<(VmValue, VmValue)>>),
    }
    let ids = JsonIds::new(&vm.prog)?;
    let (heap, meter) = (&vm.heap, &vm.meter);
    let mut stack: Vec<EncodeJsonFrame> = Vec::new();
    let mut node = value;
    loop {
        meter.charge(1)?;
        let id = heap.variant_id(node);
        match heap.field(node, 0) {
            _ if id == ids.null => sink.unit()?,
            Some(VmValue::Bool(flag)) if id == ids.bool_ => sink.bool(flag)?,
            Some(VmValue::Int(num)) if id == ids.int => sink.int(num)?,
            Some(VmValue::Float(num)) if id == ids.float => sink.float(num)?,
            Some(str_val) if id == ids.str_ => {
                let text = heap.str(str_val);
                meter.charge_bytes(text.len())?;
                sink.str(text)?;
            }
            Some(items) if id == ids.arr => {
                let Obj::List(items) = heap.get(items) else { return Err(mismatch::<Json>().into()) };
                sink.begin_list(items.len())?;
                stack.push(EncodeJsonFrame::Arr(items.iter()));
            }
            Some(map) if id == ids.obj => {
                let Obj::Map(map) = heap.get(map) else { return Err(mismatch::<Json>().into()) };
                sink.begin_map(map.live)?;
                stack.push(EncodeJsonFrame::Obj(map.entries.iter()));
            }
            _ => return Err(mismatch::<Json>().into()),
        }
        if stack.len() > MAX_NESTING_DEPTH {
            return codec::nesting_limit_error();
        }
        node = loop {
            let Some(top) = stack.last_mut() else { return Ok(()) };
            let next = match top {
                EncodeJsonFrame::Arr(items) => items.next().copied(),
                EncodeJsonFrame::Obj(entries) => match entries.find_map(|entry| *entry) {
                    Some((key, entry_val)) => {
                        let key_text = heap.str(key);
                        meter.charge_bytes(key_text.len())?;
                        sink.str(key_text)?;
                        Some(entry_val)
                    }
                    None => None,
                },
            };
            match next {
                Some(next_node) => break next_node,
                None => {
                    stack.pop();
                    sink.end()?;
                }
            }
        };
    }
}

/// Decodes a Sand `Json` value from `source`, rooting open containers so
/// garbage collection can run before any allocation hits `max_memory`.
fn decode_json(context: &mut CallContext, source: &mut dyn DecodeSource) -> CodecResult<VmValue> {
    enum Frame {
        /// Root index of the open `Obj::List`.
        Arr(usize),
        /// Root indices `(map, key)` of the open `Obj::Map` and its pending key.
        Obj(usize, usize),
    }
    let ids = JsonIds::new(&context.vm().prog)?;
    let mark = context.root_mark();
    let scratch = context.push_roots(1, VmValue::Unit)?;
    let mut stack: Vec<Frame> = Vec::new();
    let result = (|| -> CodecResult<VmValue> {
        loop {
            let consumed = source.take_consumed_bytes();
            context.vm().charge(1 + (consumed / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
            let mut value = match source.kind()? {
                InputKind::Null => {
                    source.unit()?;
                    Some(VmValue::Tag(ids.null))
                }
                InputKind::Bool => {
                    let b = source.bool()?;
                    Some(context.step(|vm| {
                        vm.check_memory(NODE_BYTES)?;
                        Ok::<_, Fail>(vm.heap.new_variant(ids.bool_, &[VmValue::Bool(b)]))
                    })?)
                }
                InputKind::Int => {
                    let n = source.int()?;
                    Some(context.step(|vm| {
                        vm.check_memory(NODE_BYTES)?;
                        Ok::<_, Fail>(vm.heap.new_variant(ids.int, &[VmValue::Int(n)]))
                    })?)
                }
                InputKind::Float => {
                    let n = source.float()?;
                    Some(context.step(|vm| {
                        vm.check_memory(NODE_BYTES)?;
                        Ok::<_, Fail>(vm.heap.new_variant(ids.float, &[VmValue::Float(n)]))
                    })?)
                }
                InputKind::Str => {
                    let text = source.str()?;
                    context.vm().charge_bytes(text.len())?;
                    let val = context.step(|vm| {
                        vm.check_memory(NODE_BYTES.saturating_mul(2).saturating_add(text.len()))?;
                        let s = vm.new_str(&*text);
                        Ok::<_, Fail>(vm.heap.new_variant(ids.str_, &[s]))
                    })?;
                    Some(val)
                }
                InputKind::List => {
                    source.begin_list()?;
                    if stack.len() >= MAX_NESTING_DEPTH {
                        return codec::nesting_limit_error();
                    }
                    let base = context.push_roots(1, VmValue::Unit)?;
                    let list = context.step(|vm| {
                        vm.check_memory(NODE_BYTES)?;
                        Ok::<_, Fail>(vm.heap.new_list(Vec::new()))
                    })?;
                    context.set_root(base, list);
                    stack.push(Frame::Arr(base));
                    None
                }
                InputKind::Map => {
                    source.begin_map()?;
                    if stack.len() >= MAX_NESTING_DEPTH {
                        return codec::nesting_limit_error();
                    }
                    let base = context.push_roots(2, VmValue::Unit)?;
                    let map = context.step(|vm| {
                        vm.check_memory(NODE_BYTES)?;
                        Ok::<_, Fail>(vm.heap.alloc(Obj::Map(Box::<MapObj>::default())))
                    })?;
                    context.set_root(base, map);
                    stack.push(Frame::Obj(base, base + 1));
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
            loop {
                let next = match stack.last() {
                    None => return value.ok_or_else(|| missing_root_error().into()),
                    Some(&Frame::Arr(list_root)) => {
                        if let Some(item) = value.take() {
                            context.set_root(scratch, item);
                            let list = context.root_at(list_root);
                            codec::check_list_growth(context, list)?;
                            let item = context.root_at(scratch);
                            context.vm_mut().heap.list_push(list, item).map_err(Fail::limit)?;
                        }
                        source.next()?
                    }
                    Some(&Frame::Obj(map_root, key_root)) => {
                        if let Some(entry_val) = value.take() {
                            context.set_root(scratch, entry_val);
                            let map = context.root_at(map_root);
                            let key = context.root_at(key_root);
                            context.step(|vm| codec::check_set_or_map_growth(vm, map, key))?;
                            let entry_val = context.root_at(scratch);
                            context.vm_mut().map_insert(map, key, entry_val)?;
                        }
                        match source.next()? {
                            true => {
                                let key_text = source.str()?;
                                context.vm().charge_bytes(key_text.len())?;
                                let key_val = context.step(|vm| {
                                    vm.check_memory(NODE_BYTES.saturating_add(key_text.len()))?;
                                    Ok::<_, Fail>(vm.new_str(&*key_text))
                                })?;
                                context.set_root(key_root, key_val);
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
                    Some(Frame::Arr(list_root)) => {
                        let list = context.root_at(list_root);
                        let wrapped = context.step(|vm| {
                            vm.check_memory(NODE_BYTES)?;
                            Ok::<_, Fail>(vm.heap.new_variant(ids.arr, &[list]))
                        })?;
                        context.truncate_roots(list_root);
                        wrapped
                    }
                    Some(Frame::Obj(map_root, _)) => {
                        let map = context.root_at(map_root);
                        let wrapped = context.step(|vm| {
                            vm.check_memory(NODE_BYTES)?;
                            Ok::<_, Fail>(vm.heap.new_variant(ids.obj, &[map]))
                        })?;
                        context.truncate_roots(map_root);
                        wrapped
                    }
                    None => return Err(missing_root_error().into()),
                });
            }
        }
    })();
    context.truncate_roots(mark);
    result
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
    // Variant IDs are unique across a program, so a value with a `Json` variant
    // ID is a `Json` value.
    if [ids.null, ids.bool_, ids.int, ids.float, ids.str_, ids.arr, ids.obj].contains(&vm.heap.variant_id(value)) {
        return Ok(all_floats_finite(vm, &ids, value)?.then_some(value));
    }
    let id = match (kind, value) {
        (Some(ShapeKind::Unit), _) => return Ok(Some(VmValue::Tag(ids.null))),
        (Some(ShapeKind::Bool), VmValue::Bool(_)) => ids.bool_,
        (Some(ShapeKind::Int), VmValue::Int(_)) => ids.int,
        (Some(ShapeKind::Float), VmValue::Float(num)) if num.is_finite() => ids.float,
        (Some(ShapeKind::Str), _) => ids.str_,
        _ => return Ok(None),
    };
    vm.check_memory(NODE_BYTES)?;
    vm.charge(1)?;
    Ok(Some(vm.heap.new_variant(id, &[value])))
}

/// Returns `true` if every float in `json` is finite.
///
/// Charges fuel for each visited `Json` node.
fn all_floats_finite(vm: &Vm, ids: &JsonIds, json: VmValue) -> Result<bool, Fail> {
    let mut stack = vec![(json, 0usize)];
    while let Some((node, depth)) = stack.pop() {
        vm.charge(1)?;
        let id = vm.heap.variant_id(node);
        let payload = vm.heap.variant_payload_if(&node, id);
        if id == ids.float {
            if !matches!(payload, VmValue::Float(num) if num.is_finite()) {
                return Ok(false);
            }
        } else if id == ids.arr {
            if depth >= MAX_NESTING_DEPTH {
                return Ok(false);
            }
            stack.extend(vm.heap.list(payload).iter().map(|&item| (item, depth + 1)));
        } else if id == ids.obj
            && let Obj::Map(map) = vm.heap.get(payload)
        {
            if depth >= MAX_NESTING_DEPTH {
                return Ok(false);
            }
            stack.extend(map.entries.iter().flatten().map(|&(_, entry_val)| (entry_val, depth + 1)));
        }
    }
    Ok(true)
}

fn invalid_key_error<T>() -> CodecResult<T> {
    Err(HostError::new("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type"))
}

/// Target backend driven by [`JsonSink`] for JSON text or [`Json`] trees.
trait JsonWriter {
    fn begin(&mut self, is_array: bool) -> CodecResult;
    fn null(&mut self) -> CodecResult;
    fn bool(&mut self, value: bool) -> CodecResult;
    fn int(&mut self, value: i64) -> CodecResult;
    fn float(&mut self, value: f64) -> CodecResult;
    fn str(&mut self, text: &str) -> CodecResult;
    fn base64(&mut self, bytes: &[u8]) -> CodecResult;
    fn key(&mut self, key: &str) -> CodecResult;
    fn end(&mut self) -> CodecResult;
}

/// Kind of open container frame in [`JsonSink`].
enum SinkFrameKind {
    Array,
    Map {
        key_next: bool,
    },
    Struct {
        desc: Arc<StructDesc>,
        next_field: usize,
    },
    /// Struct-variant payload inside the outer `{"Variant": ...}` object.
    NamedVariant {
        desc: Arc<VariantDesc>,
        next_field: usize,
    },
    /// Tuple-variant payload inside the outer `{"Variant": ...}` object.
    PositionalVariant,
}

/// Open container frame in [`JsonSink`].
struct SinkFrame {
    kind: SinkFrameKind,
    /// Open `Some` or unit-variant inline wrappers inside this container.
    inline: usize,
}

/// [`EncodeSink`] that serializes Sand values through a [`JsonWriter`].
struct JsonSink<W> {
    writer: W,
    frames: Vec<SinkFrame>,
    /// Open `Some` or unit-variant inline wrappers outside any container.
    root_inline: usize,
}

impl<W: JsonWriter> JsonSink<W> {
    fn new(writer: W) -> JsonSink<W> {
        JsonSink { writer, frames: Vec::new(), root_inline: 0 }
    }

    fn inline_slot(&mut self) -> &mut usize {
        match self.frames.last_mut() {
            Some(frame) => &mut frame.inline,
            None => &mut self.root_inline,
        }
    }

    /// Emits any pending struct or variant field key before the next value.
    ///
    /// Returns `true` if the next value is a map key.
    fn prepare_next_value(&mut self) -> CodecResult<bool> {
        if *self.inline_slot() > 0 {
            return Ok(false);
        }
        let Some(frame) = self.frames.last_mut() else { return Ok(false) };
        match &mut frame.kind {
            SinkFrameKind::Map { key_next } => {
                let is_key = *key_next;
                *key_next = !is_key;
                Ok(is_key)
            }
            SinkFrameKind::Struct { desc, next_field } => {
                let name = desc.fields.get(*next_field).map_or("", String::as_str);
                *next_field += 1;
                self.writer.key(name)?;
                Ok(false)
            }
            SinkFrameKind::NamedVariant { desc, next_field } => {
                let name = desc.fields.as_ref().and_then(|fields| fields.get(*next_field)).map_or("", String::as_str);
                *next_field += 1;
                self.writer.key(name)?;
                Ok(false)
            }
            SinkFrameKind::Array | SinkFrameKind::PositionalVariant => Ok(false),
        }
    }

    /// Writes a scalar either as a map key via `format_key` or as a JSON value
    /// via `write_value`.
    fn write_scalar(
        &mut self,
        format_key: impl FnOnce() -> Option<String>,
        write_value: impl FnOnce(&mut W) -> CodecResult,
    ) -> CodecResult {
        match self.prepare_next_value()? {
            true => match format_key() {
                Some(key) => self.writer.key(&key),
                None => invalid_key_error(),
            },
            false => write_value(&mut self.writer),
        }
    }

    fn begin(&mut self, kind: SinkFrameKind) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.frames.push(SinkFrame { kind, inline: 0 });
        Ok(())
    }
}

impl<W: JsonWriter + Send> EncodeSink for JsonSink<W> {
    fn unit(&mut self) -> CodecResult {
        self.write_scalar(|| None, |writer| writer.null())
    }

    fn bool(&mut self, value: bool) -> CodecResult {
        self.write_scalar(|| Some(value.to_string()), |writer| writer.bool(value))
    }

    fn int(&mut self, value: i64) -> CodecResult {
        self.write_scalar(|| Some(value.to_string()), |writer| writer.int(value))
    }

    fn float(&mut self, value: f64) -> CodecResult {
        if !value.is_finite() {
            return Err(HostError::new("invalid", format!("JSON cannot represent the float {value}")));
        }
        self.write_scalar(|| None, |writer| writer.float(value))
    }

    fn duration(&mut self, nanos: i64) -> CodecResult {
        self.int(nanos)
    }

    fn str(&mut self, text: &str) -> CodecResult {
        match self.prepare_next_value()? {
            true => self.writer.key(text),
            false => self.writer.str(text),
        }
    }

    fn bytes(&mut self, bytes: &[u8]) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.writer.base64(bytes)
    }

    fn range(&mut self, start: i64, end: i64, inclusive: bool) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        self.writer.begin(false)?;
        for (name, bound) in [("start", start), ("end", end)] {
            self.writer.key(name)?;
            self.writer.int(bound)?;
        }
        if inclusive {
            self.writer.key("inclusive")?;
            self.writer.bool(true)?;
        }
        self.writer.end()
    }

    fn native(&mut self, _: &str, text: &str) -> CodecResult {
        match self.prepare_next_value()? {
            true => self.writer.key(text),
            false => self.writer.str(text),
        }
    }

    fn none(&mut self) -> CodecResult {
        self.unit()
    }

    fn begin_some(&mut self) -> CodecResult {
        if self.prepare_next_value()? {
            return invalid_key_error();
        }
        *self.inline_slot() += 1;
        Ok(())
    }

    fn begin_list(&mut self, _: usize) -> CodecResult {
        self.begin(SinkFrameKind::Array)?;
        self.writer.begin(true)
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        self.begin_list(len)
    }

    fn begin_set(&mut self, len: usize) -> CodecResult {
        self.begin_list(len)
    }

    fn begin_map(&mut self, _: usize) -> CodecResult {
        self.begin(SinkFrameKind::Map { key_next: true })?;
        self.writer.begin(false)
    }

    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult {
        self.begin(SinkFrameKind::Struct { desc: desc.clone(), next_field: 0 })?;
        self.writer.begin(false)
    }

    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult {
        if len == 0 {
            let result = self.str(&desc.name);
            *self.inline_slot() += 1;
            return result;
        }
        let kind = match desc.fields {
            Some(_) => SinkFrameKind::NamedVariant { desc: desc.clone(), next_field: 0 },
            None => SinkFrameKind::PositionalVariant,
        };
        self.begin(kind)?;
        self.writer.begin(false)?;
        self.writer.key(&desc.name)?;
        match desc.fields {
            Some(_) => self.writer.begin(false),
            None => self.writer.begin(true),
        }
    }

    fn end(&mut self) -> CodecResult {
        let inline = self.inline_slot();
        if *inline > 0 {
            *inline -= 1;
            return Ok(());
        }
        match self.frames.pop().map(|f| f.kind) {
            None => {}
            Some(SinkFrameKind::NamedVariant { .. } | SinkFrameKind::PositionalVariant) => {
                self.writer.end()?;
                self.writer.end()?;
            }
            Some(_) => self.writer.end()?,
        }
        Ok(())
    }
}

/// Open container in [`JsonValueWriter`].
enum OpenJsonValue {
    Arr(Vec<Json>),
    Obj(indexmap::IndexMap<String, Json>, Option<String>),
}

/// [`JsonWriter`] that assembles a memory-tracked Rust [`Json`] tree directly.
struct JsonValueWriter {
    root: Option<Json>,
    open_containers: Vec<OpenJsonValue>,
    allocated: usize,
    reservation: Reservation,
}

impl JsonValueWriter {
    fn new(memory: &Arc<IoMemory>) -> JsonValueWriter {
        JsonValueWriter { root: None, open_containers: Vec::new(), allocated: 0, reservation: Reservation::new(memory) }
    }

    fn take_json(&mut self) -> Option<(Json, Reservation)> {
        let root = self.root.take()?;
        let sibling = self.reservation.sibling();
        Some((root, std::mem::replace(&mut self.reservation, sibling)))
    }

    fn grow(&mut self, bytes: usize) -> CodecResult {
        self.allocated = self.allocated.saturating_add(bytes);
        if self.allocated <= self.reservation.bytes() {
            return Ok(());
        }
        let ahead = self.allocated.saturating_mul(2).max(1 << 10);
        match self.reservation.ensure(ahead).or_else(|_| self.reservation.ensure(self.allocated)) {
            Ok(()) => Ok(()),
            Err(_) => crate::native::codec::memory_limit_error(),
        }
    }

    fn push(&mut self, value: Json) -> CodecResult {
        self.grow(std::mem::size_of::<Json>())?;
        match self.open_containers.last_mut() {
            None => self.root = Some(value),
            Some(OpenJsonValue::Arr(items)) => items.push(value),
            Some(OpenJsonValue::Obj(members, pending_key)) => {
                let key = pending_key.take().unwrap_or_default();
                members.insert(key, value);
            }
        }
        Ok(())
    }
}

impl JsonWriter for JsonValueWriter {
    fn begin(&mut self, is_array: bool) -> CodecResult {
        if self.open_containers.len() >= MAX_NESTING_DEPTH {
            return codec::nesting_limit_error();
        }
        self.grow(std::mem::size_of::<OpenJsonValue>())?;
        self.open_containers.push(if is_array {
            OpenJsonValue::Arr(Vec::new())
        } else {
            OpenJsonValue::Obj(indexmap::IndexMap::new(), None)
        });
        Ok(())
    }

    fn null(&mut self) -> CodecResult {
        self.push(Json::Null)
    }

    fn bool(&mut self, value: bool) -> CodecResult {
        self.push(Json::Bool(value))
    }

    fn int(&mut self, value: i64) -> CodecResult {
        self.push(Json::Int(value))
    }

    fn float(&mut self, value: f64) -> CodecResult {
        self.push(Json::Float(value))
    }

    fn str(&mut self, text: &str) -> CodecResult {
        self.grow(text.len())?;
        self.push(Json::Str(text.to_string()))
    }

    fn base64(&mut self, bytes: &[u8]) -> CodecResult {
        let len = bytes.len().div_ceil(3) * 4;
        self.grow(len)?;
        let mut encoded = String::with_capacity(len);
        base64::encode_into(&mut encoded, bytes, false);
        self.push(Json::Str(encoded))
    }

    fn key(&mut self, key: &str) -> CodecResult {
        self.grow(OBJ_ENTRY_BYTES.saturating_add(key.len()))?;
        if let Some(OpenJsonValue::Obj(_, pending_key)) = self.open_containers.last_mut() {
            *pending_key = Some(key.to_string());
        }
        Ok(())
    }

    fn end(&mut self) -> CodecResult {
        let Some(frame) = self.open_containers.pop() else { return Ok(()) };
        let value = match frame {
            OpenJsonValue::Arr(items) => Json::Arr(items),
            OpenJsonValue::Obj(members, _) => Json::Obj(members),
        };
        self.push(value)
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
    /// Marks the position between an object key's `:` and its value.
    after_key: bool,
}

/// Maximum byte length of any encoded non-string JSON scalar.
const MAX_SCALAR_BYTES: usize = 32;

/// Smallest capacity of the output text. A smaller first buffer reallocates
/// several times for a small value.
const MIN_TEXT_CAPACITY: usize = 128;

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

    /// Expands `out` to at least `need` bytes within `max_memory`.
    ///
    /// Tries progressively smaller growth steps before failing. Reallocation
    /// holds both the old and new buffers in memory at once. The reservation
    /// therefore checks `target + capacity` bytes.
    #[cold]
    fn grow(&mut self, need: usize) -> CodecResult {
        let capacity = self.out.capacity();
        for target in [capacity.saturating_mul(2), capacity + capacity / 2, capacity + capacity / 4, capacity + capacity / 8] {
            let target = target.max(need).max(MIN_TEXT_CAPACITY);
            if self.reservation.ensure(target.saturating_add(capacity)).is_ok() {
                self.out.reserve_exact(target - self.out.len());
                return Ok(());
            }
        }
        crate::native::codec::memory_limit_error()
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
}

impl JsonWriter for JsonTextWriter {
    fn begin(&mut self, is_array: bool) -> CodecResult {
        if self.open_containers.len() >= MAX_NESTING_DEPTH {
            return codec::nesting_limit_error();
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

    fn bool(&mut self, value: bool) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        self.out.push_str(if value { "true" } else { "false" });
        Ok(())
    }

    fn int(&mut self, value: i64) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        self.out.push_str(itoa::Buffer::new().format(value));
        Ok(())
    }

    fn float(&mut self, value: f64) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        let _ = write!(self.out, "{value:?}");
        Ok(())
    }

    fn str(&mut self, text: &str) -> CodecResult {
        self.separate(quoted_len(text))?;
        write_str(&mut self.out, text);
        Ok(())
    }

    /// Writes `bytes` as a quoted Base64 string.
    fn base64(&mut self, bytes: &[u8]) -> CodecResult {
        self.separate(bytes.len().div_ceil(3) * 4 + 2)?;
        self.out.push('"');
        base64::encode_into(&mut self.out, bytes, false);
        self.out.push('"');
        Ok(())
    }

    fn key(&mut self, key: &str) -> CodecResult {
        self.separate(quoted_len(key) + 2)?;
        write_str(&mut self.out, key);
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
    /// Open `Option.Some` wrappers inside this container.
    inline_some: usize,
}

enum SourceFrameKind {
    /// JSON array. A `Some(len)` value enforces an exact tuple length.
    Array(Option<usize>),
    Object,
    Map,
    /// Enum variant. `is_object` is `true` for `{"Variant": payload}` and `false` for `"Variant"`.
    Variant {
        is_object: bool,
        enum_name: String,
        variant_name: String,
    },
}

/// [`DecodeSource`] that parses JSON text for the decode walker.
pub(crate) struct JsonSource {
    text: Box<str>,
    /// Holds the memory reservation for `text` until decoding finishes.
    _text_reservation: Reservation,
    /// Tracks peak memory used by `scratch` and Base64 decoding.
    scratch_reservation: Reservation,
    /// Enforces execution limits and interrupts during internal `skip` and
    /// `range` loops.
    meter: WorkerMeter,
    pos: usize,
    /// Byte offset in `text` already reported by [`DecodeSource::take_consumed_bytes`].
    charged: usize,
    frames: Vec<SourceFrame>,
    /// Open `Option.Some` wrappers outside any container.
    root_inline_some: usize,
    /// Map key buffered for the next scalar decode step.
    pending_map_key: Option<PathSegment>,
    /// Unescaped contents of the most recently scanned escaped string.
    scratch: String,
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
        Ok(JsonSource {
            text: text.into(),
            scratch_reservation: reservation.sibling(),
            _text_reservation: reservation,
            meter: vm.meter.worker_meter(),
            pos: 0,
            charged: 0,
            frames: Vec::new(),
            root_inline_some: 0,
            pending_map_key: None,
            scratch: String::new(),
        })
    }

    fn inline_some_slot(&mut self) -> &mut usize {
        match self.frames.last_mut() {
            Some(frame) => &mut frame.inline_some,
            None => &mut self.root_inline_some,
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

    fn eat(&mut self, byte: u8) -> bool {
        let hit = self.peek() == Some(byte);
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

    fn error_at(&self, pos: usize, what: &str) -> HostError<Fail> {
        let pos = pos.min(self.text.len());
        let before = &self.input()[..pos];
        let line = 1 + before.iter().filter(|&&byte| byte == b'\n').count();
        let line_start = before.iter().rposition(|&byte| byte == b'\n').map_or(0, |idx| idx + 1);
        let col = 1 + self.text.get(line_start..pos).map_or(pos - line_start, |slice| slice.chars().count());
        HostError::new("parse", format!("{what} at line {line}, column {col}"))
    }

    fn unexpected(&self) -> HostError<Fail> {
        match self.text.get(self.pos..).and_then(|rest| rest.chars().next()) {
            None => self.error_at(self.pos, "unexpected end of input"),
            Some(ch) => self.error_at(self.pos, &format!("unexpected character `{}`", ch.escape_debug())),
        }
    }

    fn fail(&self, msg: &str) -> HostError<Fail> {
        HostError::new("parse", format!("{}: {msg}", self.path()))
    }

    /// Returns a parse error if the current token does not match `expected`.
    fn mismatch(&self, expected: &str) -> HostError<Fail> {
        match self.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => self.fail(&format!("expected {expected}")),
            _ => self.unexpected(),
        }
    }

    /// Skips whitespace before peeking at the next input byte.
    ///
    /// Fails if a map key is pending. Only scalar decoders can read map keys.
    fn start(&mut self, expected: &str) -> CodecResult<Option<u8>> {
        if let Some(key) = self.pending_map_key.take() {
            let text = self.segment_text(&key).to_string();
            return Err(self.fail(&format!("expected {expected}, found the map key `{}`", super::truncate(&text, MAX_PATH_KEY_LEN))));
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

    /// Scans a JSON number token into `(start, end, is_int)`.
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

    fn hex4(&mut self, escape_pos: usize) -> CodecResult<u32> {
        let digits = self.text.get(self.pos..self.pos + 4).filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let Some(hex) = digits else { return Err(self.error_at(escape_pos, "invalid unicode escape")) };
        let code = u32::from_str_radix(hex, 16).unwrap_or(0);
        self.pos += 4;
        Ok(code)
    }

    /// Scans a quoted JSON string from the opening `"` at `self.pos`.
    ///
    /// Returns a span in `self.text` if the string is unescaped. Writes decoded
    /// text into `self.scratch` if the string has escapes and `keep_text` is
    /// `true`.
    fn scan_str(&mut self, keep_text: bool) -> CodecResult<ScannedStr> {
        self.pos += 1;
        let first = self.pos;
        let mut escaped = false;
        loop {
            let start = self.pos;
            while let Some(&byte) = self.input().get(self.pos) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            match self.peek() {
                None => return Err(self.unexpected()),
                Some(b'"') => {
                    if escaped && keep_text {
                        let run = &self.text[start..self.pos];
                        self.scratch.push_str(run);
                        if self.scratch_reservation.ensure(self.scratch.capacity()).is_err() {
                            return crate::native::codec::memory_limit_error();
                        }
                    }
                    self.pos += 1;
                    return Ok(if escaped { ScannedStr::Scratch } else { ScannedStr::Span(first, self.pos - 1) });
                }
                Some(b'\\') => {
                    if keep_text {
                        if !escaped {
                            self.scratch.clear();
                        }
                        let run = &self.text[start..self.pos];
                        self.scratch.push_str(run);
                    }
                    escaped = true;
                    let escape_pos = self.pos;
                    self.pos += 1;
                    let ch = match self.peek() {
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
                            let hi = self.hex4(escape_pos)?;
                            let code = if (0xd800..0xdc00).contains(&hi) {
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return Err(self.error_at(escape_pos, "invalid unicode escape"));
                                }
                                let lo = self.hex4(escape_pos)?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.error_at(escape_pos, "invalid unicode escape"));
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            let Some(ch) = char::from_u32(code) else { return Err(self.error_at(escape_pos, "invalid unicode escape")) };
                            if keep_text {
                                self.scratch.push(ch);
                            }
                            continue;
                        }
                        _ => return Err(self.error_at(escape_pos, "invalid escape")),
                    };
                    self.pos += 1;
                    if keep_text {
                        self.scratch.push(ch);
                    }
                }
                Some(_) => return Err(self.error_at(self.pos, "control character in string")),
            }
        }
    }

    fn scanned_str(&self, scanned: &ScannedStr) -> &str {
        match scanned {
            ScannedStr::Span(start, end) => &self.text[*start..*end],
            ScannedStr::Scratch => &self.scratch,
        }
    }

    fn segment_text<'a>(&'a self, segment: &'a PathSegment) -> &'a str {
        match segment {
            PathSegment::Span(start, end) => &self.text[*start..*end],
            PathSegment::Owned(text) => text,
            PathSegment::None | PathSegment::Index(_) => "",
        }
    }

    /// Scans a quoted JSON string as a [`PathSegment`].
    fn key_segment(&mut self) -> CodecResult<PathSegment> {
        Ok(match self.scan_str(true)? {
            ScannedStr::Span(start, end) => PathSegment::Span(start, end),
            ScannedStr::Scratch => PathSegment::Owned(self.scratch.clone()),
        })
    }

    /// Consumes a pending map key or scans a quoted JSON string.
    fn text_value(&mut self, expected: &str) -> CodecResult<Cow<'_, str>> {
        if let Some(key) = self.pending_map_key.take() {
            return Ok(match key {
                PathSegment::Span(start, end) => Cow::Borrowed(&self.text[start..end]),
                PathSegment::Owned(text) => Cow::Owned(text),
                _ => Cow::Borrowed(""),
            });
        }
        self.skip_whitespace();
        if self.peek() != Some(b'"') {
            return Err(self.mismatch(expected));
        }
        let scanned = self.scan_str(true)?;
        Ok(Cow::Borrowed(self.scanned_str(&scanned)))
    }

    /// Parses a pending map key with `parse_key` or a JSON number token with
    /// `parse_number`.
    fn number_value<T>(
        &mut self,
        expected: &str,
        parse_key: impl Fn(&str) -> Option<T>,
        parse_number: impl Fn(&Self, &str, bool) -> CodecResult<T>,
    ) -> CodecResult<T> {
        if let Some(key) = self.pending_map_key.take() {
            let text = self.segment_text(&key);
            return parse_key(text).ok_or_else(|| self.fail(&format!("invalid map key `{}`", super::truncate(text, MAX_PATH_KEY_LEN))));
        }
        self.skip_whitespace();
        if !matches!(self.peek(), Some(b'-' | b'0'..=b'9')) {
            return Err(self.mismatch(expected));
        }
        let (start, end, is_int) = self.number()?;
        parse_number(self, &self.text[start..end], is_int)
    }

    fn open(&mut self, kind: SourceFrameKind) -> CodecResult {
        if self.frames.len() >= MAX_NESTING_DEPTH {
            return Err(self.error_at(self.pos, "nesting too deep"));
        }
        self.frames.push(SourceFrame { kind, segment: PathSegment::None, count: 0, closed: false, inline_some: 0 });
        Ok(())
    }

    fn top(&mut self) -> CodecResult<&mut SourceFrame> {
        self.frames.last_mut().ok_or_else(|| HostError::from(Fail::internal("json: no open container")))
    }

    /// Advances to the next member of the innermost container.
    ///
    /// Returns `false` after consuming `close` at the end of the container.
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

    /// Reads the next object key with its trailing `:`.
    ///
    /// Returns `None` after consuming `}` at the end of the object.
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
            PayloadForm::Positional(len) => self.begin_tuple(len),
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
                let saved_pos = self.pos;
                let (start, end, is_int) = self.number()?;
                let text = &self.text[start..end];
                let fits_i64 = is_int && text.parse::<i64>().is_ok() && !(text.starts_with('-') && text.parse::<i64>() == Ok(0));
                self.pos = saved_pos;
                if fits_i64 { InputKind::Int } else { InputKind::Float }
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
        if let Some(key) = self.pending_map_key.take() {
            return match self.segment_text(&key) {
                "true" => Ok(true),
                "false" => Ok(false),
                text => Err(self.fail(&format!("invalid map key `{}`", super::truncate(text, MAX_PATH_KEY_LEN)))),
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
            |key| key.parse().ok(),
            |source, text, is_int| {
                if is_int && let Ok(num) = text.parse() {
                    return Ok(num);
                }
                let float_val: f64 = text.parse().unwrap_or(f64::NAN);
                if float_val.fract() == 0.0 && (-9223372036854775808.0..9223372036854775808.0).contains(&float_val) {
                    return Ok(float_val as i64);
                }
                Err(source.fail(if float_val.is_finite() && float_val.fract() == 0.0 { "integer out of range" } else { "expected int" }))
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
                return crate::native::codec::memory_limit_error();
            }
            base64::decode(&text, false)
        };
        let need = self.scratch.capacity() + decoded_reservation.bytes();
        drop(decoded_reservation);
        if self.scratch_reservation.ensure(need).is_err() {
            return crate::native::codec::memory_limit_error();
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
            self.meter.charge_fail(1)?;
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
        let result = match (start, end) {
            (Some(start), Some(end)) => Ok((start, end, inclusive)),
            (None, _) => Err(self.fail("missing field `start`")),
            (_, None) => Err(self.fail("missing field `end`")),
        };
        self.frames.pop();
        result
    }

    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        let name = Self::short_name(type_name).to_string();
        self.text_value(&name)
    }

    fn option(&mut self) -> CodecResult<bool> {
        if self.start("value")? == Some(b'n') {
            self.literal(b"null")?;
            return Ok(false);
        }
        *self.inline_some_slot() += 1;
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
        let expected = format!("array of length {len}");
        if self.start(&expected)? != Some(b'[') {
            return Err(self.mismatch(&expected));
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
                        PathSegment::Span(start, end) => PathSegment::Span(*start, *end),
                        PathSegment::Owned(text) => PathSegment::Owned(text.clone()),
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
                if len.is_some_and(|expected_len| count >= expected_len) {
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
        let enum_name = Self::short_name(type_name).to_string();
        let (is_object, variant_name) = if let Some(key) = self.pending_map_key.take() {
            (false, self.segment_text(&key).to_string())
        } else {
            self.skip_whitespace();
            match self.peek() {
                Some(b'"') => {
                    let scanned = self.scan_str(true)?;
                    (false, self.scanned_str(&scanned).to_string())
                }
                Some(b'{') => {
                    self.pos += 1;
                    self.open(SourceFrameKind::Variant { is_object: true, enum_name: enum_name.clone(), variant_name: String::new() })?;
                    let variant_name = match self.next_key()? {
                        Some(key) => self.segment_text(&key).to_string(),
                        None => return Err(self.fail(&format!("expected a variant of {enum_name}"))),
                    };
                    if let SourceFrameKind::Variant { variant_name: slot, .. } = &mut self.top()?.kind {
                        slot.clone_from(&variant_name);
                    }
                    return Ok(Cow::Owned(variant_name));
                }
                _ => return Err(self.mismatch(&enum_name)),
            }
        };
        self.open(SourceFrameKind::Variant { is_object, enum_name, variant_name: variant_name.clone() })?;
        Ok(Cow::Owned(variant_name))
    }

    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult {
        let (is_object, enum_name, variant_name) = match &self.top()?.kind {
            SourceFrameKind::Variant { is_object, enum_name, variant_name } => (*is_object, enum_name.clone(), variant_name.clone()),
            _ => return Err(Fail::internal("json: a payload outside a variant").into()),
        };
        match (is_object, form) {
            (false, PayloadForm::Unit) => Ok(()),
            (false, _) => {
                let msg = format!("variant `{variant_name}` of {enum_name} needs a payload");
                Err(self.fail(&msg))
            }
            (true, form) => {
                self.top()?.segment = PathSegment::Owned(variant_name);
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
            let before = self.pos;
            self.skip_whitespace();
            match self.peek() {
                Some(b'n') => self.literal(b"null")?,
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'"') => {
                    self.scan_str(false)?;
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
            let bytes = self.pos.saturating_sub(before);
            self.meter.charge_fail(1 + (bytes / BYTES_PER_FUEL_UNIT) as u64)?;
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
        let inline = self.inline_some_slot();
        if *inline > 0 {
            *inline -= 1;
            return Ok(());
        }
        let Some(frame) = self.frames.pop() else { return Ok(()) };
        if let SourceFrameKind::Array(Some(expected_len)) = frame.kind {
            self.skip_whitespace();
            if frame.count != expected_len || (!frame.closed && !self.eat(b']')) {
                self.frames.push(frame);
                return Err(self.fail(&format!("expected array of length {expected_len}")));
            }
        }
        if let Some(SourceFrame { kind: SourceFrameKind::Variant { is_object: true, enum_name, .. }, .. }) = self.frames.last()
            && !matches!(frame.kind, SourceFrameKind::Variant { .. })
        {
            let enum_name = enum_name.clone();
            // Consume the outer `}` of `{"Variant": payload}`.
            self.skip_whitespace();
            if !self.eat(b'}') {
                let msg = format!("expected a single variant of {enum_name}");
                return Err(if self.peek() == Some(b',') { self.fail(&msg) } else { self.unexpected() });
            }
            self.frames.pop();
        }
        Ok(())
    }

    fn path(&self) -> String {
        let mut out = String::from("$");
        for frame in &self.frames {
            match &frame.segment {
                PathSegment::None => {}
                PathSegment::Index(idx) => {
                    let _ = write!(out, "[{idx}]");
                }
                segment => {
                    let key = super::truncate(self.segment_text(segment), MAX_PATH_KEY_LEN);
                    if is_ident(key) {
                        out.push('.');
                        out.push_str(key);
                    } else {
                        out.push('[');
                        write_str(&mut out, key);
                        out.push(']');
                    }
                }
            }
        }
        out
    }

    fn take_consumed_bytes(&mut self) -> usize {
        let consumed = self.pos.saturating_sub(self.charged);
        self.charged = self.pos;
        consumed
    }
}

/// Returns an upper bound on the byte length that [`write_str`] writes for `text`.
///
/// Assumes worst-case escaping for strings up to 4096 bytes to avoid scanning twice.
#[inline(always)]
fn quoted_len(text: &str) -> usize {
    let escaped = if text.len() <= 4096 || has_escape(text.as_bytes()) { 6 * text.len() } else { text.len() };
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

/// Writes `text` with JSON string escapes into `out`.
#[inline(never)]
fn write_escaped(out: &mut String, text: &str) {
    let mut start = 0;
    for (idx, byte) in text.bytes().enumerate() {
        let escaped = match byte {
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
        out.push_str(&text[start..idx]);
        if escaped.is_empty() {
            let _ = write!(out, "\\u{byte:04x}");
        } else {
            out.push_str(escaped);
        }
        start = idx + 1;
    }
    out.push_str(&text[start..]);
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
