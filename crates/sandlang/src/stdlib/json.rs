//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::native::context::CallContext;
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::ty::{Param, TraitDict, VmVal};
use crate::native::{NamedType, NativeModule, variant_index};
use crate::stdlib::codec::{self, CodecResult, Container, Decoder, EncodeSink, Encoder, InputKind, MAX_NESTING_DEPTH, NODE_BYTES, Scalar};
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
            let sink = JsonSink::text(indent.clamp(0, 1 << 16) as usize, context.vm());
            codec::encode(context, sink, value.raw(), dict.raw(), |context, mut sink| {
                let (text, reservation, fuel) = sink.out.take_text();
                drop(reservation);
                context.vm().charge(fuel)?;
                context.step(|vm| vm.check_memory(NODE_BYTES.saturating_add(text.len())))?;
                Ok(VmVal::<String>::from_raw(context.vm_mut().new_str(text)))
            })
        },
    );
    module.function(
        "to_json",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>| -> CodecResult<VmVal<Json>> {
            let sink = JsonSink::tokens(context.vm());
            codec::encode(context, sink, value.raw(), dict.raw(), |context, sink| {
                let ids = JsonIds::new(&context.vm().prog)?;
                let json = context.step(|vm| {
                    let meter = &vm.meter;
                    vm.heap.staged(|_, st| Builder::new(st, meter).build_tokens(ids, &sink.out.tokens, &sink.out.text))
                })?;
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

trait JsonOut: Send + 'static {
    fn null(&mut self) -> CodecResult;
    fn bool_(&mut self, v: bool, is_key: bool) -> CodecResult;
    fn int(&mut self, v: i64, is_key: bool) -> CodecResult;
    fn float(&mut self, v: f64) -> CodecResult;
    fn str_(&mut self, s: &str, is_key: bool) -> CodecResult;
    fn bytes(&mut self, b: &[u8]) -> CodecResult;
    fn open_bracket(&mut self, is_array: bool) -> CodecResult;
    fn close_bracket(&mut self) -> CodecResult;
}

/// [`EncodeSink`] that writes JSON to a [`JsonOut`] backend.
struct JsonSink<O> {
    out: O,
    frames: Vec<SinkFrame>,
}

impl JsonSink<TextOut> {
    fn text(indent: usize, vm: &Vm) -> Self {
        let (reservation, meter) = (Reservation::new(&vm.heap.io_memory), vm.meter.worker_meter());
        JsonSink {
            out: TextOut { out: String::new(), reservation, meter, indent, brackets: Vec::new(), after_key: false },
            frames: Vec::new(),
        }
    }
}

impl JsonSink<TokenOut> {
    fn tokens(vm: &Vm) -> Self {
        JsonSink {
            out: TokenOut { tokens: Vec::new(), text: String::new(), depth: 0, reservation: Reservation::new(&vm.heap.io_memory) },
            frames: Vec::new(),
        }
    }
}

impl<O: JsonOut> JsonSink<O> {
    #[inline(always)]
    fn next_is_key(&mut self) -> bool {
        if let Some(SinkFrame::Map(k)) = self.frames.last_mut() { std::mem::replace(k, !*k) } else { false }
    }
    fn begin(&mut self, frame: SinkFrame, is_array: bool) -> CodecResult {
        if self.next_is_key() {
            return invalid_key_error();
        }
        self.frames.push(frame);
        self.out.open_bracket(is_array)
    }
}

struct TextOut {
    out: String,
    reservation: Reservation,
    meter: WorkerMeter,
    indent: usize,
    brackets: Vec<(bool, bool)>,
    after_key: bool,
}

impl TextOut {
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
}

impl JsonOut for TextOut {
    fn null(&mut self) -> CodecResult {
        self.write_atom("null", false)
    }
    fn bool_(&mut self, v: bool, is_key: bool) -> CodecResult {
        self.write_atom(if v { "true" } else { "false" }, is_key)
    }
    fn int(&mut self, v: i64, is_key: bool) -> CodecResult {
        self.write_atom(itoa::Buffer::new().format(v), is_key)
    }
    fn float(&mut self, v: f64) -> CodecResult {
        self.separate(MAX_SCALAR_BYTES)?;
        let _ = write!(self.out, "{v:?}");
        Ok(())
    }
    fn str_(&mut self, s: &str, is_key: bool) -> CodecResult {
        if is_key { self.write_key(s) } else { self.write_quoted(s, 0) }
    }
    fn bytes(&mut self, b: &[u8]) -> CodecResult {
        self.separate(b.len().div_ceil(3) * 4 + 2)?;
        self.out.push('"');
        base64::encode_into(&mut self.out, b, false);
        self.out.push('"');
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
}

enum JsonToken {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(u32, u32),
    Arr,
    Obj,
    End,
}

struct TokenOut {
    tokens: Vec<JsonToken>,
    text: String,
    depth: usize,
    reservation: Reservation,
}

impl TokenOut {
    fn reserve(&mut self, extra_tokens: usize, extra_bytes: usize) -> CodecResult {
        let bytes = self
            .tokens
            .len()
            .saturating_add(extra_tokens)
            .saturating_mul(std::mem::size_of::<JsonToken>())
            .saturating_add(self.text.len().saturating_add(extra_bytes));
        if bytes > self.reservation.bytes()
            && self.reservation.ensure(bytes.saturating_mul(2).max(256)).or_else(|_| self.reservation.ensure(bytes)).is_err()
        {
            return codec::memory_limit_error();
        }
        Ok(())
    }
    fn push(&mut self, tok: JsonToken) -> CodecResult {
        self.reserve(1, 0)?;
        self.tokens.push(tok);
        Ok(())
    }
}

impl JsonOut for TokenOut {
    fn null(&mut self) -> CodecResult {
        self.push(JsonToken::Null)
    }
    fn bool_(&mut self, v: bool, is_key: bool) -> CodecResult {
        if is_key { self.str_(if v { "true" } else { "false" }, true) } else { self.push(JsonToken::Bool(v)) }
    }
    fn int(&mut self, v: i64, is_key: bool) -> CodecResult {
        if is_key { self.str_(itoa::Buffer::new().format(v), true) } else { self.push(JsonToken::Int(v)) }
    }
    fn float(&mut self, v: f64) -> CodecResult {
        self.push(JsonToken::Float(v))
    }
    fn str_(&mut self, s: &str, _is_key: bool) -> CodecResult {
        self.reserve(1, s.len())?;
        let start = u32::try_from(self.text.len()).map_err(|_| Fail::limit(crate::engine::LimitKind::Memory))?;
        let len = u32::try_from(s.len()).map_err(|_| Fail::limit(crate::engine::LimitKind::Memory))?;
        self.text.push_str(s);
        self.tokens.push(JsonToken::Str(start, len));
        Ok(())
    }
    fn bytes(&mut self, b: &[u8]) -> CodecResult {
        self.reserve(1, b.len().div_ceil(3) * 4)?;
        let start = u32::try_from(self.text.len()).map_err(|_| Fail::limit(crate::engine::LimitKind::Memory))?;
        base64::encode_into(&mut self.text, b, false);
        let len = (self.text.len() - start as usize) as u32;
        self.tokens.push(JsonToken::Str(start, len));
        Ok(())
    }
    fn open_bracket(&mut self, is_array: bool) -> CodecResult {
        if self.depth >= MAX_NESTING_DEPTH {
            return codec::nesting_limit_error();
        }
        self.depth += 1;
        self.push(if is_array { JsonToken::Arr } else { JsonToken::Obj })
    }
    fn close_bracket(&mut self) -> CodecResult {
        self.depth = self.depth.saturating_sub(1);
        self.push(JsonToken::End)
    }
}

impl<O: JsonOut> EncodeSink for JsonSink<O> {
    fn scalar(&mut self, value: Scalar<'_>) -> CodecResult {
        let is_key = self.next_is_key();
        match value {
            Scalar::Unit | Scalar::None if !is_key => self.out.null(),
            Scalar::Bool(v) => self.out.bool_(v, is_key),
            Scalar::Int(v) | Scalar::Duration(v) => self.out.int(v, is_key),
            Scalar::Float(v) if !v.is_finite() => Err(HostError::new("invalid", format!("JSON cannot represent the float {v}"))),
            Scalar::Float(v) if !is_key => self.out.float(v),
            Scalar::Str(s) | Scalar::Native(_, s) => self.out.str_(s, is_key),
            Scalar::Variant(d) => self.out.str_(&d.name, is_key),
            Scalar::Bytes(b) if !is_key => self.out.bytes(b),
            Scalar::Range(s, e, inc) if !is_key => {
                self.out.open_bracket(false)?;
                for (name, b) in [("start", s), ("end", e)] {
                    self.out.str_(name, true)?;
                    self.out.int(b, false)?;
                }
                if inc {
                    self.out.str_("inclusive", true)?;
                    self.out.bool_(true, false)?;
                }
                self.out.close_bracket()
            }
            _ => invalid_key_error(),
        }
    }

    fn field(&mut self, name: &str) -> CodecResult {
        self.out.str_(name, true)
    }

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
                self.out.str_(&d.name, true)?;
                self.out.open_bracket(d.fields.is_none())
            }
        }
    }

    fn end(&mut self) -> CodecResult {
        match self.frames.pop() {
            Some(SinkFrame::Inline) | None => Ok(()),
            Some(SinkFrame::Variant) => {
                self.out.close_bracket()?;
                self.out.close_bracket()
            }
            Some(_) => self.out.close_bracket(),
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

fn encode_json(vm: &Vm, value: VmValue, sink: &mut dyn EncodeSink) -> CodecResult {
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
    if let Some(json_sink) = (sink as &mut dyn std::any::Any).downcast_mut::<JsonSink<TextOut>>() {
        walk(vm, ids, value, json_sink, 0)
    } else {
        walk(vm, ids, value, sink, 0)
    }
}

impl Builder<'_, '_, '_> {
    fn build_tokens(&mut self, ids: JsonIds, tokens: &[JsonToken], text: &str) -> Result<VmValue, Fail> {
        enum Frame {
            Arr(Vec<VmValue>),
            Obj(usize, u64, VmValue),
        }
        let mut stack = Vec::new();
        let mut idx = 0usize;
        loop {
            self.reserve()?;
            self.meter.charge(1)?;
            let tok = tokens.get(idx).ok_or_else(missing_root_error)?;
            idx += 1;
            let mut value = match *tok {
                JsonToken::Null => Some(VmValue::Tag(ids.null)),
                JsonToken::Bool(v) => Some(self.st.new_variant(ids.bool_, &[VmValue::Bool(v)])),
                JsonToken::Int(v) => Some(self.st.new_variant(ids.int, &[VmValue::Int(v)])),
                JsonToken::Float(v) => Some(self.st.new_variant(ids.float, &[VmValue::Float(v)])),
                JsonToken::Str(start, len) => {
                    let s = &text[start as usize..(start + len) as usize];
                    self.st.check_memory((NODE_BYTES + self.pending).saturating_add(s.len()))?;
                    self.meter.charge_bytes(s.len())?;
                    let sv = self.st.new_str(s);
                    Some(self.st.new_variant(ids.str_, &[sv]))
                }
                JsonToken::Arr => {
                    stack.push(Frame::Arr(Vec::new()));
                    None
                }
                JsonToken::Obj => {
                    stack.push(Frame::Obj(self.entries.len(), 0, VmValue::Unit));
                    None
                }
                JsonToken::End => return Err(missing_root_error()),
            };
            loop {
                match stack.last_mut() {
                    None => return value.ok_or_else(missing_root_error),
                    Some(Frame::Arr(items)) => {
                        if let Some(elem) = value.take() {
                            self.push(items, elem)?;
                        }
                        if !matches!(tokens.get(idx), None | Some(JsonToken::End)) {
                            break;
                        }
                    }
                    Some(Frame::Obj(base, hash, key)) => {
                        if let Some(elem) = value.take() {
                            self.append(*base, *hash, *key, elem)?;
                        }
                        if let Some(&JsonToken::Str(start, len)) = tokens.get(idx) {
                            idx += 1;
                            let key_text = &text[start as usize..(start + len) as usize];
                            let h = self.hash(key_text)?;
                            *key = self.key(key_text, h)?;
                            *hash = h;
                            break;
                        }
                    }
                }
                idx += 1;
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
