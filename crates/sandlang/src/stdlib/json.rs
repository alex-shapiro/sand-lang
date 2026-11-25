//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::native::context::CallContext;
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::ty::{Param, TraitDict, VmVal};
use crate::native::{NamedType, NativeModule, variant_index};
use crate::stdlib::codec::{
    self, CodecResult, Container, Decoder, DynEncodeSink, EncodeSink, Encoder, InputKind, MAX_NESTING_DEPTH, NODE_BYTES, Scalar,
};
use crate::syntax::lexer::is_ident;
use crate::types::traits;
use crate::vm::bytecode::Program;
use crate::vm::compare::str_hash;
use crate::vm::heap::Staging;
use crate::vm::iomem::Reservation;
use crate::vm::meter::{BYTES_PER_FUEL_UNIT, SLOW_BYTES_PER_FUEL_UNIT, VmMeter, WorkerMeter};
use crate::vm::value::{FxHashMap, KeyIndex, MAP_ENTRY_SIZE, MapObj, Obj, VmValue, slot_fits};
use crate::vm::{Fail, Vm, fail, internal};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;

/// Dynamic JSON value.
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

impl crate::value::ToValue for Json {
    fn to_value(&self) -> crate::value::Value {
        match self {
            Json::Null => crate::value::Value::Unit,
            Json::Bool(b) => crate::value::Value::Bool(*b),
            Json::Int(n) => crate::value::Value::Int(*n),
            Json::Float(n) => crate::value::Value::Float(*n),
            Json::Str(s) => crate::value::Value::Str(s.clone()),
            Json::Arr(items) => crate::value::Value::List(items.iter().map(Self::to_value).collect()),
            Json::Obj(fields) => {
                crate::value::Value::Map(fields.iter().map(|(k, v)| (crate::value::Value::Str(k.clone()), v.to_value())).collect())
            }
        }
    }
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
        let json = decode_json(context, &mut source).and_then(|value| source.finish().map(|()| value));
        context.vm().charge(1 + (source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
        json.map(VmVal::from_raw)
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
            let finish = |context: &mut CallContext, mut sink: JsonSink| -> CodecResult<VmVal<String>> {
                let (text, reservation, fuel) = sink.take_text();
                drop(reservation);
                context.vm().charge(fuel)?;
                context.step(|vm| vm.check_memory(NODE_BYTES.saturating_add(text.len())))?;
                Ok(VmVal::<String>::from_raw(context.vm_mut().new_str(text)))
            };
            if codec::dict_is_pure(context.vm(), dict.raw()) {
                let value_root = context.root(value.raw());
                if context.vm().heap.limit.is_some_and(|limit| context.vm().heap.object_bytes >= limit / 2) {
                    context.collect_garbage();
                }
                let vm = context.vm();
                let mut sink = JsonSink::new(indent, vm);
                codec::encode_pure(&vm.prog, &vm.heap, &vm.meter, context.value(value_root), &mut sink)?;
                return finish(context, sink);
            }
            let sink = JsonSink::new(indent, context.vm());
            codec::encode(context, sink, value.raw(), dict.raw(), finish)
        },
    );
    module.function(
        "to_json",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>| -> CodecResult<VmVal<Json>> {
            if codec::dict_is_pure(context.vm(), dict.raw()) {
                let value_root = context.root(value.raw());
                let json = pure_to_json(context, value_root)?;
                return Ok(VmVal::<Json>::from_raw(json));
            }
            let sink = JsonSink::new(0, context.vm());
            codec::encode(context, sink, value.raw(), dict.raw(), |context, mut sink| {
                let (text, reservation, fuel) = sink.take_text();
                context.vm().charge(fuel)?;
                let mut source = JsonSource::owned(text, reservation, context.vm().meter.worker_meter());
                let json = decode_json(context, &mut source)?;
                context.vm().charge(1 + (source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
                Ok(VmVal::<Json>::from_raw(json))
            })
        },
    );
    module.function(
        "decode",
        |context: &mut CallContext, dict: TraitDict<0, traits::Decode>, text: VmVal<String>| -> CodecResult<Param<0>> {
            let dict_root = context.root(dict.raw());
            let source = context.step(|vm| JsonSource::new(vm, text.raw()))?;
            codec::decode(context, source, context.value(dict_root), |context, value, mut source| {
                let result = source.finish();
                context.vm().charge((source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
                result?;
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
    keys: HashTable<(u64, u32, u32, VmValue)>,
    long_keys: FxHashMap<u32, Box<str>>,
    entries: Vec<Entry>,
    key_text: String,
}

struct Entry {
    hash: u64,
    key: VmValue,
    value: VmValue,
}

impl<'a, 's, 'h> Builder<'a, 's, 'h> {
    fn new(st: &'s mut Staging<'h>, meter: &'a VmMeter) -> Self {
        Builder {
            st,
            meter,
            pending: 0,
            keys: HashTable::new(),
            long_keys: FxHashMap::default(),
            entries: Vec::new(),
            key_text: String::new(),
        }
    }

    #[inline]
    fn reserve(&self) -> Result<(), Fail> {
        self.st.check_memory(NODE_BYTES + self.pending)
    }

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

    fn key(&mut self, key: &str, hash: u64) -> Result<VmValue, Fail> {
        let key_text = &self.key_text;
        if let Some(&(_, _, _, v)) = self.keys.find(hash, |e| &key_text[e.1 as usize..(e.1 + e.2) as usize] == key) {
            return Ok(v);
        }
        self.st.check_memory((NODE_BYTES + self.pending).saturating_add(key.len()))?;
        let v = self.st.new_str(key);
        if key.len() <= MAX_SHARED_KEY_LEN && self.keys.len() < MAX_SHARED_KEYS {
            let start = self.key_text.len() as u32;
            self.key_text.push_str(key);
            self.pending += 2 * NODE_BYTES + key.len();
            self.keys.insert_unique(hash, (hash, start, key.len() as u32, v), |e| e.0);
        } else if let VmValue::Obj(handle) = v {
            self.pending += NODE_BYTES + key.len();
            self.long_keys.insert(handle, key.into());
        }
        Ok(v)
    }

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
        let mut map = MapObj { entries: Vec::with_capacity(n), live: 0, index, ..MapObj::default() };
        for e in self.entries.drain(base..) {
            let dup = map.index.find(e.key, e.hash, |slot| {
                let k = map.entries[slot].expect("live").0;
                k == e.key
                    || (!self.long_keys.is_empty()
                        && matches!((k, e.key), (VmValue::Obj(a), VmValue::Obj(b)) if self.long_keys.get(&a).is_some_and(|x| self.long_keys.get(&b) == Some(x))))
            });
            if let Some(slot) = dup {
                map.entries[slot] = Some((e.key, e.value));
            } else {
                let slot = map.entries.len();
                map.index.insert(e.key, e.hash, slot);
                map.entries.push(Some((e.key, e.value)));
                map.live += 1;
            }
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
        let ids = JsonIds::new(&vm.prog)?;
        let meter = &vm.meter;
        vm.heap.staged(|_, st| Builder::new(st, meter).json(ids, self))
    }
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
enum WriteFrame {
    Arr(std::vec::IntoIter<Json>, Vec<VmValue>),
    /// Remaining members, map start offset, and the key and hash of the current
    /// member.
    Obj(indexmap::map::IntoIter<String, Json>, usize, String, u64),
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
                    stack.push(WriteFrame::Obj(members.into_iter(), self.entries.len(), String::new(), 0));
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
                            let key = self.key(key, *hash)?;
                            self.append(*base, *hash, key, value)?;
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
                    WriteFrame::Obj(_, base, ..) => {
                        let map = self.map(base);
                        self.st.new_variant(ids.obj, &[map])
                    }
                });
            };
        }
    }
}

const MAX_PATH_KEY_LEN: usize = 64;

#[derive(Clone)]
enum PathSegment {
    None,
    Index(usize),
    Span(usize, usize),
    Owned(String),
}

struct SourceFrame {
    kind: SourceFrameKind,
    segment: PathSegment,
    count: usize,
}

enum SourceFrameKind {
    Array(Option<usize>),
    Object,
    Map,
    Some,
    Variant(bool, PathSegment),
}

/// Streaming JSON parser that feeds the decode walker.
pub(crate) struct JsonSource {
    s: String,
    _text_reservation: Reservation,
    scratch_reservation: Reservation,
    meter: WorkerMeter,
    pos: usize,
    charged: usize,
    frames: Vec<SourceFrame>,
    some_depth: usize,
    pending_map_key: Option<PathSegment>,
    scratch: String,
}

impl JsonSource {
    fn new(vm: &Vm, text: VmValue) -> Result<JsonSource, Fail> {
        let s = vm.heap.str(text);
        let mut reservation = Reservation::new(&vm.heap.io_memory);
        if reservation.ensure(s.len()).is_err() {
            return Err(Fail::limit(crate::engine::LimitKind::Memory));
        }
        Ok(JsonSource::owned(s.to_owned(), reservation, vm.meter.worker_meter()))
    }

    fn owned(s: String, reservation: Reservation, meter: WorkerMeter) -> JsonSource {
        JsonSource {
            s,
            scratch_reservation: reservation.sibling(),
            _text_reservation: reservation,
            meter,
            pos: 0,
            charged: 0,
            frames: Vec::new(),
            some_depth: 0,
            pending_map_key: None,
            scratch: String::new(),
        }
    }

    fn err_at(&self, pos: usize, what: &str) -> HostError<Fail> {
        let pos = pos.min(self.s.len());
        let before = &self.s.as_bytes()[..pos];
        let line = 1 + before.iter().filter(|&&c| c == b'\n').count();
        let line_start = before.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        let col = 1 + self.s.get(line_start..pos).map_or(pos - line_start, |s| s.chars().count());
        HostError::new("parse", format!("{what} at line {line}, column {col}"))
    }

    fn unexpected(&self) -> HostError<Fail> {
        match self.s.get(self.pos..).and_then(|s| s.chars().next()) {
            None => self.err_at(self.pos, "unexpected end of input"),
            Some(c) => self.err_at(self.pos, &format!("unexpected character `{}`", c.escape_debug())),
        }
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.as_bytes().get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn finish(&mut self) -> CodecResult {
        self.ws();
        if self.pos < self.s.len() { Err(self.unexpected()) } else { Ok(()) }
    }

    fn literal(&mut self, word: &[u8]) -> CodecResult {
        if self.s.as_bytes()[self.pos..].starts_with(word) {
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
        let digits = self.s.get(self.pos..self.pos + 4).filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()));
        let Some(h) = digits else { return Err(self.err_at(esc, "invalid unicode escape")) };
        self.pos += 4;
        Ok(u32::from_str_radix(h, 16).unwrap_or(0))
    }

    fn reserve_scratch(&mut self, extra: usize) -> CodecResult {
        let need = self.scratch.len().saturating_add(extra);
        if need > self.scratch.capacity() {
            let cap = need.max(self.scratch.capacity().saturating_mul(2)).max(64);
            if self.scratch_reservation.ensure(cap).is_err() && self.scratch_reservation.ensure(need).is_err() {
                return codec::memory_limit_error();
            }
            self.scratch.reserve(extra);
        }
        Ok(())
    }

    fn string(&mut self, keep: bool) -> CodecResult<Option<(usize, usize)>> {
        self.pos += 1;
        let first = self.pos;
        let mut escaped = false;
        loop {
            let start = self.pos;
            while let Some(&c) = self.s.as_bytes().get(self.pos) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            match self.peek() {
                None => return Err(self.unexpected()),
                Some(b'"') => {
                    if escaped && keep {
                        let end = self.pos;
                        self.reserve_scratch(end - start)?;
                        self.scratch.push_str(&self.s[start..end]);
                    }
                    self.pos += 1;
                    return Ok((!escaped).then_some((first, self.pos - 1)));
                }
                Some(b'\\') => {
                    if keep {
                        if !escaped {
                            self.scratch.clear();
                        }
                        let end = self.pos;
                        self.reserve_scratch((end - start).saturating_add(4))?;
                        self.scratch.push_str(&self.s[start..end]);
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
                                self.reserve_scratch(4)?;
                                self.scratch.push(c);
                            }
                            continue;
                        }
                        _ => return Err(self.err_at(esc, "invalid escape")),
                    };
                    self.pos += 1;
                    if keep {
                        self.reserve_scratch(4)?;
                        self.scratch.push(c);
                    }
                }
                Some(_) => return Err(self.err_at(self.pos, "control character in string")),
            }
        }
    }

    fn fail(&self, message: &str) -> HostError<Fail> {
        HostError::new("parse", format!("{}: {message}", self.path()))
    }

    fn mismatch(&self, expected: &str) -> HostError<Fail> {
        match self.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => self.fail(&format!("expected {expected}")),
            _ => self.unexpected(),
        }
    }

    fn start(&mut self, expected: &str) -> CodecResult<Option<u8>> {
        if let Some(key) = self.pending_map_key.take() {
            let shown = super::truncate(self.segment_str(&key), MAX_PATH_KEY_LEN).to_string();
            return Err(self.fail(&format!("expected {expected}, found the map key `{shown}`")));
        }
        self.ws();
        Ok(self.peek())
    }

    fn str_of(&self, span: Option<(usize, usize)>) -> &str {
        span.map_or(&self.scratch, |(start, end)| &self.s[start..end])
    }

    fn segment_str<'a>(&'a self, segment: &'a PathSegment) -> &'a str {
        match segment {
            PathSegment::Span(start, end) => &self.s[*start..*end],
            PathSegment::Owned(text) => text,
            _ => "",
        }
    }

    pub(crate) fn read_str(&mut self, expected: &str) -> CodecResult<Cow<'_, str>> {
        if let Some(key) = self.pending_map_key.take() {
            return Ok(match key {
                PathSegment::Span(start, end) => Cow::Borrowed(&self.s[start..end]),
                PathSegment::Owned(text) => Cow::Owned(text),
                _ => Cow::Borrowed(""),
            });
        }
        self.ws();
        if self.peek() != Some(b'"') {
            return Err(self.mismatch(expected));
        }
        let span = self.string(true)?;
        Ok(Cow::Borrowed(self.str_of(span)))
    }

    fn open(&mut self, kind: SourceFrameKind) -> CodecResult {
        if matches!(kind, SourceFrameKind::Some) {
            self.some_depth += 1;
        } else if self.frames.len() - self.some_depth >= MAX_NESTING_DEPTH {
            return Err(self.err_at(self.pos, "nesting too deep"));
        }
        self.frames.push(SourceFrame { kind, segment: PathSegment::None, count: 0 });
        Ok(())
    }

    fn top(&mut self) -> CodecResult<&mut SourceFrame> {
        self.frames.last_mut().ok_or_else(|| Fail::internal("json: no open container").into())
    }

    fn next_member(&mut self, close: u8) -> CodecResult<bool> {
        self.ws();
        let first = self.top()?.count == 0;
        if self.eat(close) {
            return Ok(false);
        }
        if !first && !self.eat(b',') {
            return Err(self.unexpected());
        }
        self.ws();
        self.top()?.count += 1;
        Ok(true)
    }

    fn next_key(&mut self) -> CodecResult<Option<PathSegment>> {
        if !self.next_member(b'}')? {
            return Ok(None);
        }
        if self.peek() != Some(b'"') {
            return Err(self.unexpected());
        }
        let segment = self.string(true)?.map_or_else(|| PathSegment::Owned(self.scratch.clone()), |(a, b)| PathSegment::Span(a, b));
        self.ws();
        if !self.eat(b':') {
            return Err(self.unexpected());
        }
        Ok(Some(segment))
    }

    fn open_delim(&mut self, byte: u8, expected: &str, kind: SourceFrameKind) -> CodecResult {
        if self.start(expected)? != Some(byte) {
            return Err(self.mismatch(expected));
        }
        self.open(kind)?;
        self.pos += 1;
        Ok(())
    }

    pub(crate) fn read_null(&mut self) -> CodecResult {
        if self.start("null")? != Some(b'n') {
            return Err(self.mismatch("null"));
        }
        self.literal(b"null")
    }

    pub(crate) fn read_bool(&mut self) -> CodecResult<bool> {
        if let Some(key) = self.pending_map_key.take() {
            return match self.segment_str(&key) {
                "true" => Ok(true),
                "false" => Ok(false),
                text => Err(self.fail(&format!("invalid map key `{}`", super::truncate(text, MAX_PATH_KEY_LEN)))),
            };
        }
        self.ws();
        match self.peek() {
            Some(b't') => self.literal(b"true").map(|()| true),
            Some(b'f') => self.literal(b"false").map(|()| false),
            _ => Err(self.mismatch("bool")),
        }
    }

    pub(crate) fn read_int(&mut self) -> CodecResult<i64> {
        if let Some(key) = self.pending_map_key.take() {
            let text = self.segment_str(&key);
            return text.parse().map_err(|_| self.fail(&format!("invalid map key `{}`", super::truncate(text, MAX_PATH_KEY_LEN))));
        }
        self.ws();
        if !matches!(self.peek(), Some(b'-' | b'0'..=b'9')) {
            return Err(self.mismatch("int"));
        }
        let (start, end, is_int) = self.number()?;
        if !is_int {
            return Err(self.fail("expected int"));
        }
        self.s[start..end].parse().map_err(|_| self.fail("integer out of range"))
    }

    pub(crate) fn read_float(&mut self) -> CodecResult<f64> {
        if !matches!(self.start("float")?, Some(b'-' | b'0'..=b'9')) {
            return Err(self.mismatch("float"));
        }
        let (start, end, _) = self.number()?;
        Ok(self.s[start..end].parse().unwrap_or(f64::NAN))
    }

    pub(crate) fn read_bytes(&mut self) -> CodecResult<Vec<u8>> {
        if self.start("base64 str")? != Some(b'"') {
            return Err(self.mismatch("base64 str"));
        }
        let mut reservation = self.scratch_reservation.sibling();
        let span = self.string(true)?;
        let text = self.str_of(span);
        if reservation.ensure(text.len() / 4 * 3 + 3).is_err() {
            return codec::memory_limit_error();
        }
        base64::decode(text, false).map_err(|_| self.fail("invalid base64"))
    }

    pub(crate) fn read_range(&mut self) -> CodecResult<(i64, i64, bool)> {
        self.open_delim(b'{', "object", SourceFrameKind::Object)?;
        let (mut start, mut end, mut inclusive) = (None, None, false);
        while let Some(key) = self.next_key()? {
            self.meter.charge_fail(1)?;
            let name = self.segment_str(&key).to_string();
            self.top()?.segment = key;
            match &*name {
                "start" => start = Some(self.read_int()?),
                "end" => end = Some(self.read_int()?),
                "inclusive" => inclusive = self.read_bool()?,
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

    pub(crate) fn kind(&mut self) -> CodecResult<InputKind> {
        if self.pending_map_key.is_some() {
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
                let saved = self.pos;
                let scanned = self.number();
                self.pos = saved;
                let (start, end, is_int) = scanned?;
                let text = &self.s[start..end];
                if is_int && text.parse::<i64>().is_ok() && !(text.starts_with('-') && text.parse::<i64>() == Ok(0)) {
                    InputKind::Int
                } else {
                    InputKind::Float
                }
            }
            _ => return Err(self.unexpected()),
        })
    }

    pub(crate) fn option(&mut self) -> CodecResult<bool> {
        if self.start("value")? == Some(b'n') {
            self.literal(b"null").map(|()| false)
        } else {
            self.open(SourceFrameKind::Some).map(|()| true)
        }
    }

    pub(crate) fn open_seq(&mut self, len: Option<usize>) -> CodecResult {
        match len {
            None => self.open_delim(b'[', "array", SourceFrameKind::Array(None)),
            Some(len) => self.open_delim(b'[', &format!("array of length {len}"), SourceFrameKind::Array(Some(len))),
        }
    }

    pub(crate) fn open_map(&mut self, is_object: bool) -> CodecResult {
        self.open_delim(b'{', "object", if is_object { SourceFrameKind::Object } else { SourceFrameKind::Map })
    }

    pub(crate) fn next(&mut self) -> CodecResult<bool> {
        match self.top()?.kind {
            SourceFrameKind::Map => {
                let Some(key) = self.next_key()? else { return Ok(false) };
                self.pending_map_key = Some(match &key {
                    PathSegment::Span(start, end) => PathSegment::Span(*start, *end),
                    PathSegment::Owned(text) => PathSegment::Owned(text.clone()),
                    _ => PathSegment::None,
                });
                self.top()?.segment = key;
                Ok(true)
            }
            SourceFrameKind::Array(expected_len) => {
                let count = self.top()?.count;
                if !self.next_member(b']')? {
                    return Ok(false);
                }
                if let Some(len) = expected_len
                    && count >= len
                {
                    return Err(self.fail(&format!("expected array of length {len}")));
                }
                self.top()?.segment = PathSegment::Index(count);
                Ok(true)
            }
            _ => Err(Fail::internal("json: `next` outside an array or a map").into()),
        }
    }

    pub(crate) fn next_field(&mut self) -> CodecResult<Option<&str>> {
        let segment = self.next_key()?;
        self.top()?.segment = segment.unwrap_or(PathSegment::None);
        match &self.frames.last().expect("open").segment {
            PathSegment::None => Ok(None),
            segment => Ok(Some(self.segment_str(segment))),
        }
    }

    pub(crate) fn begin_variant(&mut self, enum_name: &str) -> CodecResult<&str> {
        let (is_object, segment) = if let Some(key) = self.pending_map_key.take() {
            (false, key)
        } else {
            self.ws();
            match self.peek() {
                Some(b'"') => {
                    let seg = self.string(true)?.map_or_else(|| PathSegment::Owned(self.scratch.clone()), |(a, b)| PathSegment::Span(a, b));
                    (false, seg)
                }
                Some(b'{') => {
                    self.pos += 1;
                    self.open(SourceFrameKind::Variant(true, PathSegment::None))?;
                    let Some(key) = self.next_key()? else { return Err(self.fail(&format!("expected a variant of {enum_name}"))) };
                    if let SourceFrameKind::Variant(_, slot) = &mut self.top()?.kind {
                        *slot = key;
                    }
                    let SourceFrameKind::Variant(_, seg) = &self.frames.last().expect("open").kind else { unreachable!() };
                    return Ok(self.segment_str(seg));
                }
                _ => return Err(self.mismatch(enum_name)),
            }
        };
        self.open(SourceFrameKind::Variant(is_object, segment))?;
        let SourceFrameKind::Variant(_, seg) = &self.frames.last().expect("open").kind else { unreachable!() };
        Ok(self.segment_str(seg))
    }

    pub(crate) fn begin_payload(&mut self, enum_name: &str, arity: usize, named: bool) -> CodecResult {
        let frame = self.top()?;
        let SourceFrameKind::Variant(is_object, slot) = &mut frame.kind else {
            return Err(Fail::internal("json: a payload outside a variant").into());
        };
        let is_object = *is_object;
        let segment = std::mem::replace(slot, PathSegment::None);
        if !is_object {
            return if arity == 0 {
                Ok(())
            } else {
                let variant_name = self.segment_str(&segment).to_string();
                Err(self.fail(&format!("variant `{variant_name}` of {enum_name} needs a payload")))
            };
        }
        frame.segment = segment;
        if named { self.open_map(true) } else { self.open_seq(Some(arity)) }
    }

    pub(crate) fn skip(&mut self) -> CodecResult {
        if self.pending_map_key.take().is_some() {
            return Ok(());
        }
        let depth = self.frames.len();
        loop {
            let before = self.pos;
            self.ws();
            match self.peek() {
                Some(b'n') => self.literal(b"null")?,
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'"') => drop(self.string(false)?),
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
            self.meter.charge_fail(1 + (self.pos.saturating_sub(before) / BYTES_PER_FUEL_UNIT) as u64)?;
            while self.frames.len() > depth {
                let more = match self.top()?.kind {
                    SourceFrameKind::Object => self.next_key()?.is_some(),
                    _ => self.next_member(b']')?,
                };
                if more {
                    break;
                }
                self.frames.pop();
            }
            if self.frames.len() == depth {
                return Ok(());
            }
        }
    }

    pub(crate) fn end(&mut self, enum_name: Option<&str>) -> CodecResult {
        let Some(frame) = self.frames.pop() else { return Ok(()) };
        if matches!(frame.kind, SourceFrameKind::Some) {
            self.some_depth -= 1;
            return Ok(());
        }
        if let SourceFrameKind::Array(Some(len)) = frame.kind {
            self.ws();
            if frame.count != len || !self.eat(b']') {
                self.frames.push(frame);
                return Err(self.fail(&format!("expected array of length {len}")));
            }
        }
        if let Some(SourceFrame { kind: SourceFrameKind::Variant(true, _), .. }) = self.frames.last()
            && !matches!(frame.kind, SourceFrameKind::Variant(..))
        {
            self.ws();
            if !self.eat(b'}') {
                return Err(if self.peek() == Some(b',') {
                    let name = enum_name.unwrap_or("enum");
                    self.fail(&format!("expected a single variant of {name}"))
                } else {
                    self.unexpected()
                });
            }
            self.frames.pop();
        }
        Ok(())
    }

    pub(crate) fn path(&self) -> String {
        let mut out = String::from("$");
        for frame in &self.frames {
            match &frame.segment {
                PathSegment::None => {}
                PathSegment::Index(index) => _ = write!(out, "[{index}]"),
                segment => {
                    let key = super::truncate(self.segment_str(segment), MAX_PATH_KEY_LEN);
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

    pub(crate) fn take_consumed_bytes(&mut self) -> usize {
        std::mem::replace(&mut self.charged, self.pos).abs_diff(self.pos)
    }
}

fn invalid_key_error<T>() -> CodecResult<T> {
    Err(HostError::new("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type"))
}

const SPACES: &str = "                                                                ";
const MAX_SCALAR_BYTES: usize = 32;
const MIN_TEXT_CAPACITY: usize = 128;

#[derive(Clone, Copy)]
enum SinkFrame {
    Plain,
    Map(bool),
    Variant,
    Inline,
}

/// [`EncodeSink`] that writes JSON text directly into a memory-tracked buffer.
struct JsonSink {
    out: String,
    reservation: Reservation,
    meter: WorkerMeter,
    indent: usize,
    brackets: Vec<(bool, bool)>,
    after_key: bool,
    frames: Vec<SinkFrame>,
}

impl JsonSink {
    fn new(indent: usize, vm: &Vm) -> JsonSink {
        let (reservation, meter) = (Reservation::new(&vm.heap.io_memory), vm.meter.worker_meter());
        JsonSink { out: String::new(), reservation, meter, indent, brackets: Vec::new(), after_key: false, frames: Vec::new() }
    }
    fn take_text(&mut self) -> (String, Reservation, u64) {
        let sibling = self.reservation.sibling();
        (std::mem::take(&mut self.out), std::mem::replace(&mut self.reservation, sibling), self.meter.fuel_used())
    }
    #[inline(always)]
    fn reserve(&mut self, bytes: usize) -> CodecResult {
        let need = self.out.len().saturating_add(bytes);
        if need <= self.out.capacity() { Ok(()) } else { self.grow(need) }
    }
    #[cold]
    fn grow(&mut self, need: usize) -> CodecResult {
        let cap = self.out.capacity();
        for t in [cap.saturating_mul(2), cap + cap / 2, cap + cap / 4, cap + cap / 8] {
            let t = t.max(need).max(MIN_TEXT_CAPACITY);
            if self.reservation.ensure(t.saturating_add(cap)).is_ok() {
                self.out.reserve_exact(t - self.out.len());
                return Ok(());
            }
        }
        codec::memory_limit_error()
    }
    #[inline(always)]
    fn separate(&mut self, bytes: usize) -> CodecResult {
        if std::mem::replace(&mut self.after_key, false) {
            return self.reserve(bytes);
        }
        if let Some((_, empty)) = self.brackets.last_mut() {
            if !std::mem::replace(empty, false) {
                self.reserve(bytes.saturating_add(1))?;
                self.out.push(',');
            }
            self.start_line()?;
        }
        self.reserve(bytes)
    }
    #[inline(always)]
    fn start_line(&mut self) -> CodecResult {
        if self.indent == 0 {
            return Ok(());
        }
        let mut left = self.indent.saturating_mul(self.brackets.len());
        self.reserve(left.saturating_add(2))?;
        if left >= BYTES_PER_FUEL_UNIT {
            self.meter.charge_fail((left / BYTES_PER_FUEL_UNIT) as u64)?;
        }
        self.out.push('\n');
        while left > 0 {
            let sp = &SPACES[..left.min(SPACES.len())];
            self.out.push_str(sp);
            left -= sp.len();
        }
        Ok(())
    }
    fn open_bracket(&mut self, is_array: bool) -> CodecResult {
        if self.brackets.len() >= MAX_NESTING_DEPTH {
            return codec::nesting_limit_error();
        }
        self.separate(1)?;
        self.out.push(if is_array { '[' } else { '{' });
        self.brackets.push((is_array, true));
        Ok(())
    }
    fn close_bracket(&mut self) -> CodecResult {
        let Some((is_array, is_empty)) = self.brackets.pop() else { return Ok(()) };
        if !is_empty {
            self.start_line()?;
        }
        self.reserve(1)?;
        self.out.push(if is_array { ']' } else { '}' });
        Ok(())
    }
    #[inline(always)]
    fn write_quoted(&mut self, text: &str, extra_reserve: usize) -> CodecResult {
        self.separate(quoted_len(text).saturating_add(extra_reserve))?;
        let before = self.out.len();
        write_str(&mut self.out, text);
        let extra = self.out.len().saturating_sub(before).saturating_sub(text.len() + 2);
        if extra >= BYTES_PER_FUEL_UNIT {
            self.meter.charge_fail((extra / BYTES_PER_FUEL_UNIT) as u64)?;
        }
        Ok(())
    }
    #[inline(always)]
    fn write_key(&mut self, key: &str) -> CodecResult {
        self.write_quoted(key, 2)?;
        self.out.push_str(if self.indent > 0 { ": " } else { ":" });
        self.after_key = true;
        Ok(())
    }
    #[inline(always)]
    fn write_atom(&mut self, s: &str, is_key: bool) -> CodecResult {
        if is_key {
            return self.write_key(s);
        }
        self.separate(MAX_SCALAR_BYTES)?;
        self.out.push_str(s);
        Ok(())
    }
    #[inline(always)]
    fn write_float(&mut self, v: f64) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        let _ = write!(self.out, "{v:?}");
        Ok(())
    }
    #[inline(always)]
    fn write_bytes(&mut self, b: &[u8]) -> CodecResult {
        self.separate(b.len().div_ceil(3) * 4 + 2)?;
        self.out.push('"');
        base64::encode_into(&mut self.out, b, false);
        self.out.push('"');
        Ok(())
    }
    #[inline(always)]
    fn next_is_key(&mut self) -> bool {
        if let Some(SinkFrame::Map(k)) = self.frames.last_mut() { std::mem::replace(k, !*k) } else { false }
    }
    fn begin(&mut self, frame: SinkFrame, is_array: bool) -> CodecResult {
        if self.next_is_key() {
            return invalid_key_error();
        }
        self.frames.push(frame);
        self.open_bracket(is_array)
    }
}

impl EncodeSink for JsonSink {
    #[inline(always)]
    fn scalar(&mut self, value: Scalar<'_>) -> CodecResult {
        let is_key = self.next_is_key();
        match value {
            Scalar::Unit | Scalar::None if !is_key => self.write_atom("null", false),
            Scalar::Bool(v) => self.write_atom(if v { "true" } else { "false" }, is_key),
            Scalar::Int(v) | Scalar::Duration(v) => self.write_atom(itoa::Buffer::new().format(v), is_key),
            Scalar::Float(v) if !v.is_finite() => Err(HostError::new("invalid", format!("JSON cannot represent the float {v}"))),
            Scalar::Float(v) if !is_key => self.write_float(v),
            Scalar::Str(s) | Scalar::Native(_, s) if is_key => self.write_key(s),
            Scalar::Str(s) | Scalar::Native(_, s) => self.write_quoted(s, 0),
            Scalar::Variant(d) if is_key => self.write_key(&d.name),
            Scalar::Variant(d) => self.write_quoted(&d.name, 0),
            Scalar::Bytes(b) if !is_key => self.write_bytes(b),
            Scalar::Range(s, e, inc) if !is_key => {
                self.open_bracket(false)?;
                for (name, b) in [("start", s), ("end", e)] {
                    self.write_key(name)?;
                    self.write_atom(itoa::Buffer::new().format(b), false)?;
                }
                if inc {
                    self.write_key("inclusive")?;
                    self.write_atom("true", false)?;
                }
                self.close_bracket()
            }
            _ => invalid_key_error(),
        }
    }

    #[inline(always)]
    fn field(&mut self, name: &str) -> CodecResult {
        self.write_key(name)
    }

    #[inline(always)]
    fn begin(&mut self, kind: Container<'_>) -> CodecResult {
        match kind {
            Container::Some => {
                if self.next_is_key() {
                    return invalid_key_error();
                }
                self.frames.push(SinkFrame::Inline);
                Ok(())
            }
            Container::List(_) | Container::Tuple(_) | Container::Set(_) => self.begin(SinkFrame::Plain, true),
            Container::Map(_) => self.begin(SinkFrame::Map(true), false),
            Container::Struct(_) => self.begin(SinkFrame::Plain, false),
            Container::Variant(d, _) => {
                self.begin(SinkFrame::Variant, false)?;
                self.write_key(&d.name)?;
                self.open_bracket(d.fields.is_none())
            }
        }
    }

    #[inline(always)]
    fn end(&mut self) -> CodecResult {
        match self.frames.pop() {
            Some(SinkFrame::Inline) | None => Ok(()),
            Some(SinkFrame::Variant) => {
                self.close_bracket()?;
                self.close_bracket()
            }
            Some(_) => self.close_bracket(),
        }
    }
}

#[inline(always)]
fn quoted_len(text: &str) -> usize {
    let escaped = if text.len() <= 4096 || has_escape(text.as_bytes()) { text.len().saturating_mul(6) } else { text.len() };
    escaped.saturating_add(2)
}

#[inline(always)]
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

fn encode_json(vm: &Vm, value: VmValue, sink: &mut dyn DynEncodeSink) -> CodecResult {
    fn walk<S: EncodeSink + ?Sized>(vm: &Vm, ids: JsonIds, node: VmValue, sink: &mut S, depth: usize) -> CodecResult {
        vm.meter.charge(1)?;
        let id = vm.heap.variant_id(node);
        match vm.heap.field(node, 0) {
            _ if id == ids.null => sink.scalar(Scalar::Unit),
            Some(VmValue::Bool(b)) if id == ids.bool_ => sink.scalar(Scalar::Bool(b)),
            Some(VmValue::Int(n)) if id == ids.int => sink.scalar(Scalar::Int(n)),
            Some(VmValue::Float(n)) if id == ids.float => sink.scalar(Scalar::Float(n)),
            Some(s) if id == ids.str_ => {
                let t = vm.heap.str(s);
                vm.meter.charge_bytes(t.len())?;
                sink.scalar(Scalar::Str(t))
            }
            Some(p) if id == ids.arr => {
                if depth >= MAX_NESTING_DEPTH {
                    return codec::nesting_limit_error();
                }
                let Obj::List(items) = vm.heap.get(p) else { return Err(mismatch::<Json>().into()) };
                sink.begin(Container::List(items.len()))?;
                for &item in items {
                    walk(vm, ids, item, sink, depth + 1)?;
                }
                sink.end()
            }
            Some(p) if id == ids.obj => {
                if depth >= MAX_NESTING_DEPTH {
                    return codec::nesting_limit_error();
                }
                let Obj::Map(map) = vm.heap.get(p) else { return Err(mismatch::<Json>().into()) };
                sink.begin(Container::Map(map.live))?;
                for &(k, v) in map.entries.iter().flatten() {
                    let t = vm.heap.str(k);
                    vm.meter.charge_bytes(t.len())?;
                    sink.scalar(Scalar::Str(t))?;
                    walk(vm, ids, v, sink, depth + 1)?;
                }
                sink.end()
            }
            _ => Err(mismatch::<Json>().into()),
        }
    }
    let ids = JsonIds::new(&vm.prog)?;
    if let Some(json_sink) = (sink as &mut dyn std::any::Any).downcast_mut::<JsonSink>() {
        walk(vm, ids, value, json_sink, 0)
    } else {
        walk(vm, ids, value, sink, 0)
    }
}

enum JsonBuildFrame {
    Arr(Vec<VmValue>, bool),
    Obj { base: usize, expect_key: bool, is_map: bool, hash: u64, key: VmValue, close_outer: bool },
    Inline,
}

/// [`EncodeSink`] that builds a staged Sand `Json` value via [`Builder`].
struct JsonBuilderSink<'a, 's, 'h> {
    builder: Builder<'a, 's, 'h>,
    ids: JsonIds,
    root: Option<VmValue>,
    frames: Vec<JsonBuildFrame>,
    depth: usize,
}

impl<'a, 's, 'h> JsonBuilderSink<'a, 's, 'h> {
    fn new(st: &'s mut Staging<'h>, meter: &'a VmMeter, ids: JsonIds) -> Self {
        JsonBuilderSink { builder: Builder::new(st, meter), ids, root: None, frames: Vec::new(), depth: 0 }
    }
    #[inline(always)]
    fn next_is_key(&mut self) -> bool {
        if let Some(JsonBuildFrame::Obj { expect_key, is_map: true, .. }) = self.frames.last_mut() {
            std::mem::replace(expect_key, !*expect_key)
        } else {
            false
        }
    }
    #[inline(always)]
    fn set_key(&mut self, key: &str) -> CodecResult {
        let h = str_hash(key);
        let k = self.builder.key(key, h)?;
        if let Some(JsonBuildFrame::Obj { hash, key: slot, .. }) = self.frames.last_mut() {
            *hash = h;
            *slot = k;
        }
        Ok(())
    }
    #[inline(always)]
    fn place(&mut self, val: VmValue) -> CodecResult {
        for frame in self.frames.iter_mut().rev() {
            match frame {
                JsonBuildFrame::Inline => continue,
                JsonBuildFrame::Arr(items, _) => {
                    self.builder.push(items, val)?;
                    return Ok(());
                }
                JsonBuildFrame::Obj { base, hash, key, .. } => {
                    self.builder.append(*base, *hash, *key, val)?;
                    return Ok(());
                }
            }
        }
        self.root = Some(val);
        Ok(())
    }
    fn open(&mut self, frame: JsonBuildFrame) -> CodecResult {
        if self.depth >= MAX_NESTING_DEPTH {
            return codec::nesting_limit_error();
        }
        self.depth += 1;
        self.frames.push(frame);
        Ok(())
    }
    fn open_obj(&mut self, is_map: bool, close_outer: bool) -> CodecResult {
        self.open(JsonBuildFrame::Obj {
            base: self.builder.entries.len(),
            expect_key: is_map,
            is_map,
            hash: 0,
            key: VmValue::Unit,
            close_outer,
        })
    }
    fn finish_frame(&mut self) -> CodecResult {
        let Some(frame) = self.frames.pop() else { return Ok(()) };
        let (val, close_outer) = match frame {
            JsonBuildFrame::Inline => return Ok(()),
            JsonBuildFrame::Arr(items, close_outer) => {
                self.depth -= 1;
                let list = self.builder.list(items);
                (self.builder.st.new_variant(self.ids.arr, &[list]), close_outer)
            }
            JsonBuildFrame::Obj { base, close_outer, .. } => {
                self.depth -= 1;
                let map = self.builder.map(base);
                (self.builder.st.new_variant(self.ids.obj, &[map]), close_outer)
            }
        };
        self.place(val)?;
        if close_outer { self.finish_frame() } else { Ok(()) }
    }
}

impl EncodeSink for JsonBuilderSink<'_, '_, '_> {
    #[inline(always)]
    fn scalar(&mut self, value: Scalar<'_>) -> CodecResult {
        self.builder.reserve()?;
        let is_key = self.next_is_key();
        let val = match value {
            Scalar::Unit | Scalar::None if !is_key => VmValue::Tag(self.ids.null),
            Scalar::Bool(b) if is_key => return self.set_key(if b { "true" } else { "false" }),
            Scalar::Bool(b) => self.builder.st.new_variant(self.ids.bool_, &[VmValue::Bool(b)]),
            Scalar::Int(n) | Scalar::Duration(n) if is_key => return self.set_key(itoa::Buffer::new().format(n)),
            Scalar::Int(n) | Scalar::Duration(n) => self.builder.st.new_variant(self.ids.int, &[VmValue::Int(n)]),
            Scalar::Float(f) if !f.is_finite() => return Err(HostError::new("invalid", format!("JSON cannot represent the float {f}"))),
            Scalar::Float(f) if !is_key => self.builder.st.new_variant(self.ids.float, &[VmValue::Float(f)]),
            Scalar::Str(s) | Scalar::Native(_, s) if is_key => return self.set_key(s),
            Scalar::Str(s) | Scalar::Native(_, s) => {
                self.builder.st.check_memory((NODE_BYTES + self.builder.pending).saturating_add(s.len()))?;
                let sv = self.builder.st.new_str(s);
                self.builder.st.new_variant(self.ids.str_, &[sv])
            }
            Scalar::Variant(d) if is_key => return self.set_key(&d.name),
            Scalar::Variant(d) => {
                let sv = self.builder.key(&d.name, str_hash(&d.name))?;
                self.builder.st.new_variant(self.ids.str_, &[sv])
            }
            Scalar::Bytes(b) if !is_key => {
                let encoded = base64::encode(b, false);
                self.builder.st.check_memory((NODE_BYTES + self.builder.pending).saturating_add(encoded.len()))?;
                let sv = self.builder.st.new_str(encoded);
                self.builder.st.new_variant(self.ids.str_, &[sv])
            }
            Scalar::Range(s, e, inc) if !is_key => {
                self.open_obj(false, false)?;
                for (name, bound) in [("start", s), ("end", e)] {
                    self.set_key(name)?;
                    let v = self.builder.st.new_variant(self.ids.int, &[VmValue::Int(bound)]);
                    self.place(v)?;
                }
                if inc {
                    self.set_key("inclusive")?;
                    let v = self.builder.st.new_variant(self.ids.bool_, &[VmValue::Bool(true)]);
                    self.place(v)?;
                }
                return self.finish_frame();
            }
            _ => return invalid_key_error(),
        };
        self.place(val)
    }

    #[inline(always)]
    fn vm_str(&mut self, value: VmValue, text: &str) -> CodecResult {
        self.builder.reserve()?;
        if self.next_is_key() {
            return self.set_key(text);
        }
        let val = self.builder.st.new_variant(self.ids.str_, &[value]);
        self.place(val)
    }

    #[inline(always)]
    fn field(&mut self, name: &str) -> CodecResult {
        self.set_key(name)
    }

    #[inline(always)]
    fn begin(&mut self, kind: Container<'_>) -> CodecResult {
        if self.next_is_key() {
            return invalid_key_error();
        }
        match kind {
            Container::Some => {
                self.frames.push(JsonBuildFrame::Inline);
                Ok(())
            }
            Container::List(_) | Container::Tuple(_) | Container::Set(_) => self.open(JsonBuildFrame::Arr(Vec::new(), false)),
            Container::Map(_) => self.open_obj(true, false),
            Container::Struct(_) => self.open_obj(false, false),
            Container::Variant(d, _) => {
                self.open_obj(false, false)?;
                self.set_key(&d.name)?;
                if d.fields.is_none() { self.open(JsonBuildFrame::Arr(Vec::new(), true)) } else { self.open_obj(false, true) }
            }
        }
    }

    #[inline(always)]
    fn end(&mut self) -> CodecResult {
        self.finish_frame()
    }
}

impl Builder<'_, '_, '_> {
    fn parse_json(&mut self, ids: JsonIds, source: &mut JsonSource) -> CodecResult<VmValue> {
        enum Frame {
            Arr(Vec<VmValue>),
            Obj(usize, u64, VmValue),
        }
        let base_depth = source.frames.len() - source.some_depth;
        let mut stack = Vec::new();
        loop {
            self.reserve()?;
            self.meter.charge(1 + (source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
            let mut value = if source.pending_map_key.is_some() {
                let text = source.read_str("str")?;
                self.st.check_memory((NODE_BYTES + self.pending).saturating_add(text.len()))?;
                self.meter.charge_bytes(text.len())?;
                let sv = self.st.new_str(&*text);
                Some(self.st.new_variant(ids.str_, &[sv]))
            } else {
                source.ws();
                match source.peek() {
                    Some(b'n') => {
                        source.literal(b"null")?;
                        Some(VmValue::Tag(ids.null))
                    }
                    Some(b't') => {
                        source.literal(b"true")?;
                        Some(self.st.new_variant(ids.bool_, &[VmValue::Bool(true)]))
                    }
                    Some(b'f') => {
                        source.literal(b"false")?;
                        Some(self.st.new_variant(ids.bool_, &[VmValue::Bool(false)]))
                    }
                    Some(b'"') => {
                        let span = source.string(true)?;
                        let text = source.str_of(span);
                        self.st.check_memory((NODE_BYTES + self.pending).saturating_add(text.len()))?;
                        self.meter.charge_bytes(text.len())?;
                        let sv = self.st.new_str(text);
                        Some(self.st.new_variant(ids.str_, &[sv]))
                    }
                    Some(b'-' | b'0'..=b'9') => {
                        let (start, end, is_int) = source.number()?;
                        let text = &source.s[start..end];
                        let is_i64 = is_int && text.parse::<i64>().is_ok_and(|n| !(text.starts_with('-') && n == 0));
                        Some(if is_i64 {
                            self.st.new_variant(ids.int, &[VmValue::Int(text.parse().unwrap_or(0))])
                        } else {
                            self.st.new_variant(ids.float, &[VmValue::Float(text.parse().unwrap_or(f64::NAN))])
                        })
                    }
                    Some(b'[' | b'{') => {
                        if base_depth + stack.len() >= MAX_NESTING_DEPTH {
                            return Err(source.err_at(source.pos, "nesting too deep"));
                        }
                        let is_arr = source.peek() == Some(b'[');
                        source.pos += 1;
                        stack.push(if is_arr { Frame::Arr(Vec::new()) } else { Frame::Obj(self.entries.len(), 0, VmValue::Unit) });
                        None
                    }
                    _ => return Err(source.unexpected()),
                }
            };
            loop {
                match stack.last_mut() {
                    None => return Ok(value.ok_or_else(missing_root_error)?),
                    Some(Frame::Arr(items)) => {
                        let first = items.is_empty() && value.is_none();
                        if let Some(elem) = value.take() {
                            self.push(items, elem)?;
                        }
                        source.ws();
                        if !source.eat(b']') {
                            if !first && !source.eat(b',') {
                                return Err(source.unexpected());
                            }
                            break;
                        }
                    }
                    Some(Frame::Obj(base, hash, key)) => {
                        let first = self.entries.len() == *base && value.is_none();
                        if let Some(elem) = value.take() {
                            self.append(*base, *hash, *key, elem)?;
                        }
                        source.ws();
                        if !source.eat(b'}') {
                            if !first {
                                if !source.eat(b',') {
                                    return Err(source.unexpected());
                                }
                                source.ws();
                            }
                            if source.peek() != Some(b'"') {
                                return Err(source.unexpected());
                            }
                            let span = source.string(true)?;
                            let key_text = source.str_of(span);
                            let h = self.hash(key_text)?;
                            *key = self.key(key_text, h)?;
                            *hash = h;
                            source.ws();
                            if !source.eat(b':') {
                                return Err(source.unexpected());
                            }
                            break;
                        }
                    }
                }
                value = Some(match stack.pop().expect("open") {
                    Frame::Arr(items) => {
                        let list = self.list(items);
                        self.st.new_variant(ids.arr, &[list])
                    }
                    Frame::Obj(base, ..) => {
                        let map = self.map(base);
                        self.st.new_variant(ids.obj, &[map])
                    }
                });
            }
        }
    }
}

fn pure_to_json(context: &mut CallContext, value_root: crate::native::context::RootedValue) -> CodecResult<VmValue> {
    let ids = JsonIds::new(&context.vm().prog)?;
    let root = context.value(value_root);
    context.step(|vm| {
        let (prog, meter) = (&vm.prog, &vm.meter);
        let result: CodecResult<VmValue> = vm.heap.staged(|heap, st| {
            let mut sink = JsonBuilderSink::new(st, meter, ids);
            codec::encode_pure(prog, heap, meter, root, &mut sink)?;
            sink.root.ok_or_else(|| missing_root_error().into())
        });
        match result {
            Ok(value) => Ok(Ok(value)),
            Err(error) => match error.into_abort() {
                Ok(fail) => Err(fail),
                Err(sand_err) => Ok(Err(sand_err)),
            },
        }
    })?
}

fn decode_json(context: &mut CallContext, source: &mut JsonSource) -> CodecResult<VmValue> {
    let ids = JsonIds::new(&context.vm().prog)?;
    let saved = (source.pos, source.charged, source.frames.len(), source.some_depth, source.pending_map_key.clone());
    context.step(|vm| {
        (source.pos, source.charged, source.some_depth, source.pending_map_key) = (saved.0, saved.1, saved.3, saved.4.clone());
        source.frames.truncate(saved.2);
        let meter = &vm.meter;
        match vm.heap.staged(|_, st| Builder::new(st, meter).parse_json(ids, source)) {
            Ok(value) => Ok(Ok(value)),
            Err(error) => match error.into_abort() {
                Ok(fail) => Err(fail),
                Err(sand_err) => Ok(Err(sand_err)),
            },
        }
    })?
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
