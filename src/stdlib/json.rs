//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::ty::{Param, Reflect, VmVal};
use crate::native::{EnumType, NamedType, NativeModule, variant_index};
use crate::syntax::lexer::is_ident;
use crate::types::ConstVal;
use crate::types::traits;
use crate::vm::bytecode::{Program, RtIds};
use crate::vm::compare::{hash_key, str_hash};
use crate::vm::encode::{self, Container, EncodeSink, Leaf, MAX_DEPTH, Opened, Role, VariantRef};
use crate::vm::heap::{Heap, Staging};
use crate::vm::meter::{Meter, SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{
    FieldDefault, FieldDesc, FxHashMap, KeyIndex, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, TypeDesc, VariantDesc, VmValue, slot_fits,
};
use crate::vm::{Fail, Vm, fail, internal, without_trait};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;

const NODE_BYTES: usize = 64;

/// Dynamic JSON value.
///
/// Integers in the 64-bit signed `int` range without a fractional part,
/// exponent, or `-0` are represented as `Int`, and all other numbers are
/// represented as `Float`. `Json` equality is variant-exact, so
/// `Json.Int(1) != Json.Float(1.0)`; use `as_float()` or `as_int()` to read
/// numbers across both variants. Supports up to 512 levels of nesting.
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
    fn of(prog: &Program) -> Result<JsonIds, Fail> {
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

    fn has(&self, id: u32) -> bool {
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
        let ids = JsonIds::of(&vm.prog)?;
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
        let (heap, meter, indent) = (&vm.heap, &vm.meter, indent.clamp(0, 1 << 16) as usize);
        let mut out = JsonTextOutput { heap, meter, indent, text: String::new(), charged: 0, scratch: Scratch::new() };
        let root = OpenBracket { is_array: false, is_empty: true };
        write_json(&vm.prog, heap, meter, JsonIds::of(&vm.prog)?, &mut out, root, value.raw())?;
        Ok(VmVal::from_raw(vm.new_str(out.text)))
    });
    m.bind::<Decoded<()>>();
    m.function("decode_raw", |vm: &mut Vm, ty: Reflect<0>, text: VmVal<String>| -> Result<Decoded<Param<0>>, Fail> {
        let desc = ty.desc(&vm.heap)?.clone();
        let ids = vm.prog.ids;
        let meter = &vm.meter;
        let (r, fills) = vm.heap.staged(|heap, st| {
            let mut d = Decoder {
                p: Parser::new(text.str(heap)),
                b: Builder::new(heap, st, meter),
                ids,
                path: Vec::new(),
                stack: Vec::new(),
                slots: Vec::new(),
                indexes: FxHashMap::default(),
                fills: Vec::new(),
                thunks: Vec::new(),
            };
            let r = d.decode(&desc).and_then(|v| d.p.finish().map(|()| v));
            let charged = d.p.charge(meter);
            let r = charged.map_err(Error::Fail).and(r).map(|v| match d.thunks.is_empty() {
                true => Decoded::Done(Param::from_raw(v)),
                false => Decoded::Pending(Param::from_raw(v), VmVal::from_raw(d.b.list(std::mem::take(&mut d.thunks)))),
            });
            (r, d.fills)
        });
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
        let fail = |kind: &str, message| Ok(Decoded::Failed(super::core::Error { kind: kind.into(), message }));
        match r {
            Ok(d) => Ok(d),
            Err(Error::Parse(msg)) => fail("parse", msg),
            Err(Error::Invalid(msg)) => fail("invalid", msg),
            Err(Error::Fail(c)) => Err(c),
        }
    });
    m.function("to_json", |vm: &mut Vm, value: Param<0, traits::Encode>| -> Result<VmVal<Json>, Fail> {
        let json_ids = JsonIds::of(&vm.prog)?;
        let (prog, meter) = (&vm.prog, &vm.meter);
        let v = vm.heap.staged(|heap, st| {
            let mut out = JsonValueOutput::new(Builder::new(heap, st, meter), json_ids);
            match write_json(prog, heap, meter, json_ids, &mut out, PartialJson::Root(VmValue::Unit), value.raw())? {
                PartialJson::Root(v) => Ok(v),
                _ => internal("json.to_json: the walk did not return the root"),
            }
        })?;
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

    /// Returns an empty buffer for a list of `len` items, after a check of the
    /// buffer against the memory limit.
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
        let ids = JsonIds::of(&vm.prog)?;
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
            if stack.len() > MAX_DEPTH {
                return fail("limit", "value too deeply nested to convert");
            }
            node = loop {
                match stack.last_mut() {
                    None => return value.ok_or_else(empty_stack),
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
        let ids = JsonIds::of(&vm.prog)?;
        let meter = &vm.meter;
        vm.heap.staged(|heap, st| Builder::new(heap, st, meter).json(ids, self))
    }
}

fn empty_stack() -> Fail {
    Fail::internal("json: a conversion without a value")
}

/// A Sand `Json` array or object whose members wait for their Rust values.
enum ReadFrame<'h> {
    Arr(Vec<Json>, std::slice::Iter<'h, VmValue>),
    /// The members, the entries that remain, and the key of the member that
    /// the conversion makes now.
    Obj(indexmap::IndexMap<String, Json>, std::slice::Iter<'h, Option<(VmValue, VmValue)>>, String),
}

const OBJ_ENTRY_BYTES: usize = std::mem::size_of::<(u64, String, Json)>() + std::mem::size_of::<usize>();

/// Returns a copy of the Sand `str` `v`, and charges fuel and memory for the
/// copy.
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

/// A Rust `Json` array or object whose members wait for their Sand values.
enum WriteFrame {
    Arr(std::vec::IntoIter<Json>, Vec<VmValue>),
    /// The members that remain, the map, and the key and the key hash of the
    /// member that the conversion makes now.
    Obj(indexmap::map::IntoIter<String, Json>, MapBuilder, String, u64),
}

impl Builder<'_, '_, '_> {
    /// Returns the Sand `Json` value of `json`.
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
            if stack.len() > MAX_DEPTH {
                return fail("limit", "value too deeply nested to convert");
            }
            node = loop {
                match stack.last_mut() {
                    None => return value.ok_or_else(empty_stack),
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
        if self.depth >= MAX_DEPTH {
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

enum Seg<'a> {
    Key(Cow<'a, str>),
    Index(usize),
}

const INDEXED_FIELDS: usize = 8;

struct Decoder<'a, 'd, 's, 'h> {
    p: Parser<'a>,
    b: Builder<'a, 's, 'h>,
    ids: crate::vm::bytecode::RtIds,
    path: Vec<Seg<'a>>,
    stack: Vec<&'d TypeDesc>,
    slots: Vec<Option<VmValue>>,
    indexes: FxHashMap<u32, FxHashMap<&'d str, usize>>,
    fills: Vec<Fill>,
    thunks: Vec<VmValue>,
}

enum Fill {
    Set(VmValue, Vec<VmValue>),
    Map(VmValue, Vec<(VmValue, VmValue)>),
}

impl<'a> Source<'a> for Decoder<'a, '_, '_, '_> {
    fn parser(&mut self) -> &mut Parser<'a> {
        &mut self.p
    }
}

const LOOKUPS_PER_UNIT: usize = 16;

fn lookup<T>(meter: &Meter, items: &[T], pred: impl Fn(&T) -> bool) -> Result<Option<usize>, Fail> {
    meter.charge(1 + (items.len() / LOOKUPS_PER_UNIT) as u64)?;
    Ok(items.iter().position(pred))
}

const QUOTED_KEY: usize = 64;

impl<'a, 'd> Decoder<'a, 'd, '_, '_> {
    fn path_str(&self) -> String {
        let mut s = String::from("$");
        for seg in &self.path {
            match seg {
                Seg::Index(i) => {
                    let _ = write!(s, "[{i}]");
                }
                Seg::Key(k) if is_ident(k) => {
                    s.push('.');
                    s.push_str(super::truncate(k, QUOTED_KEY));
                }
                Seg::Key(k) => {
                    s.push('[');
                    write_str(&mut s, super::truncate(k, QUOTED_KEY));
                    s.push(']');
                }
            }
        }
        s
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

    fn fail(&self, msg: &str) -> Error {
        Error::Parse(format!("{}: {msg}", self.path_str()))
    }

    fn unsupported(&self, what: &str) -> Error {
        Error::Invalid(format!("{}: cannot decode {what}", self.path_str()))
    }

    fn mismatch(&self, what: &str) -> Error {
        match self.p.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => self.fail(&format!("expected {what}")),
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
            self.fail(if f.is_finite() && f.fract() == 0.0 { "integer out of range" } else { "expected int" })
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
        self.path.push(seg);
        let r = f(self);
        self.path.pop();
        r
    }

    fn decode(&mut self, t: &'d TypeDesc) -> PResult<VmValue> {
        self.p.ws();
        self.p.charge(self.b.meter)?;
        self.b.reserve()?;
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
                Ok(self.b.st.new_str(&*s))
            }
            TypeDesc::Bytes => {
                let s = self.string("base64 str")?;
                let b = base64::decode(&s, false).map_err(|_| self.fail("invalid base64"))?;
                Ok(self.b.st.new_bytes(b))
            }
            TypeDesc::Option(_) => {
                // Handle nested `Option` types iteratively because they do not increment `self.p.depth`.
                let (mut inner, mut layers) = (t, 0);
                while let TypeDesc::Option(e) = inner {
                    (inner, layers) = (e, layers + 1);
                }
                self.b.meter.charge(layers)?;
                if self.p.peek() == Some(b'n') {
                    self.p.literal(b"null")?;
                    return Ok(VmValue::Tag(self.ids.none));
                }
                let mut v = self.decode(inner)?;
                for _ in 0..layers {
                    self.b.reserve()?;
                    v = self.b.st.new_variant(self.ids.some, &[v]);
                }
                Ok(v)
            }
            TypeDesc::List(elem) => {
                self.expect_open(b'[', "array")?;
                let mut items = Vec::new();
                self.array(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(elem))?;
                    Ok(d.b.push(&mut items, v)?)
                })?;
                Ok(self.b.list(items))
            }
            TypeDesc::Set(elem, fill) => {
                self.expect_open(b'[', "array")?;
                let mut items = Vec::new();
                let thunks = self.thunks.len();
                self.array(|d, i| {
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
                let what = format!("array of length {}", elems.len());
                self.expect_open(b'[', &what)?;
                let mut items = Vec::with_capacity(elems.len());
                self.array(|d, i| {
                    let Some(elem) = elems.get(i) else { return Err(d.fail(&format!("expected {what}"))) };
                    items.push(d.with_seg(Seg::Index(i), |d| d.decode(elem))?);
                    Ok(())
                })?;
                if items.len() != elems.len() {
                    return Err(self.fail(&format!("expected {what}")));
                }
                Ok(self.b.st.new_tuple(items))
            }
            TypeDesc::Map(key_type, value_type) => {
                self.expect_open(b'{', "object")?;
                let mut m = self.b.start_map();
                self.object(true, |d, key| {
                    let (k, hash, text) = d.map_key(key_type, key)?;
                    let seg = Seg::Key(text.clone().unwrap_or_default());
                    let v = d.with_seg(seg, |d| d.decode(value_type))?;
                    Ok(d.b.put(&mut m, hash, text, k, v)?)
                })?;
                Ok(self.b.map(m))
            }
            TypeDesc::Struct { id, def, fields, .. } => self.decode_struct(t, *id, *def, fields),
            TypeDesc::Enum { variants, .. } if is_json(t) => {
                let ids = JsonIds::at(variants.first().map_or(0, |v| v.1));
                self.p.parse_json(&mut self.b, &ids)
            }
            TypeDesc::Enum { name, variants, .. } => {
                self.stack.push(t);
                let r = self.decode_enum(name, variants);
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
            TypeDesc::Value { name, parse } => {
                let s = self.string(name)?;
                self.b.meter.charge_bytes(s.len())?;
                match parse(&s) {
                    Ok(v) => Ok(self.b.st.alloc(v)),
                    Err(msg) => Err(self.fail(&msg)),
                }
            }
            TypeDesc::Range => self.decode_range(),
            TypeDesc::Unsupported(name) => Err(self.unsupported(&format!("values of type {name}"))),
            TypeDesc::Param(_) => Err(Error::Fail(Fail::internal("json.decode: uninstantiated type parameter"))),
        }
    }

    fn decode_struct(&mut self, t: &'d TypeDesc, id: u32, def: u32, fields: &'d [FieldDesc]) -> PResult<VmValue> {
        self.expect_open(b'{', "object")?;
        self.stack.push(t);
        let base = self.slots.len();
        self.slots.resize(base + fields.len(), None);
        let mut next = 0;
        let r = self.object(true, |d, key| match d.field_index(id, fields, &key, next)? {
            Some(i) => {
                next = i + 1;
                d.slots[base + i] = Some(d.with_seg(Seg::Key(key), |d| d.decode(&fields[i].1))?);
                Ok(())
            }
            None => d.p.skip(),
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

    fn map_key(&mut self, t: &TypeDesc, key: Cow<'a, str>) -> PResult<(VmValue, u64, Option<Cow<'a, str>>)> {
        let quoted = super::truncate(&key, QUOTED_KEY);
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
            TypeDesc::Enum { variants, .. } => match lookup(self.b.meter, variants, |v| v.0 == *key && v.2.is_empty())? {
                Some(i) => VmValue::Tag(variants[i].1),
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

    fn decode_enum(&mut self, name: &str, variants: &'d [VariantDesc]) -> PResult<VmValue> {
        let find = |d: &Self, tag: &str| match lookup(d.b.meter, variants, |v| v.0 == tag)? {
            Some(i) => Ok(&variants[i]),
            None => Err(d.fail(&format!("unknown variant `{}` of {name}", super::truncate(tag, QUOTED_KEY)))),
        };
        match self.p.peek() {
            Some(b'"') => {
                let tag = self.p.string(true)?;
                let v = find(self, &tag)?;
                if !v.2.is_empty() {
                    return Err(self.fail(&format!("variant `{tag}` of {name} needs a payload")));
                }
                Ok(VmValue::Tag(v.1))
            }
            Some(b'{') => {
                let mut result = None;
                self.object(true, |d, tag| {
                    if result.is_some() {
                        return Err(d.fail(&format!("expected a single variant of {name}")));
                    }
                    let v = find(d, &tag)?;
                    result = Some(d.with_seg(Seg::Key(tag), |d| d.decode_payload(v))?);
                    Ok(())
                })?;
                result.ok_or_else(|| self.fail(&format!("expected a variant of {name}")))
            }
            _ => Err(self.mismatch(name)),
        }
    }

    fn decode_payload(&mut self, v: &'d VariantDesc) -> PResult<VmValue> {
        let (_, id, types, names) = v;
        self.p.ws();
        let mut slots: Vec<Option<VmValue>> = vec![None; types.len()];
        match (self.p.peek(), names) {
            (Some(b'{'), Some(names)) => {
                self.object(true, |d, key| match lookup(d.b.meter, names, |n| *n == *key)? {
                    Some(i) => {
                        slots[i] = Some(d.with_seg(Seg::Key(key), |d| d.decode(&types[i]))?);
                        Ok(())
                    }
                    None => d.p.skip(),
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
            (Some(b'['), _) => {
                let what = format!("array of length {}", types.len());
                let mut n = 0;
                self.array(|d, i| {
                    let Some(ty) = types.get(i) else { return Err(d.fail(&format!("expected {what}"))) };
                    slots[i] = Some(d.with_seg(Seg::Index(i), |d| d.decode(ty))?);
                    n += 1;
                    Ok(())
                })?;
                if n != types.len() {
                    return Err(self.fail(&format!("expected {what}")));
                }
            }
            _ => return Err(self.mismatch(if names.is_some() { "object" } else { "array" })),
        }
        let fields: Vec<VmValue> = slots.into_iter().map(|s| s.unwrap_or(VmValue::Unit)).collect();
        Ok(self.b.st.new_variant(*id, &fields))
    }
}

const SPACES: &str = "                                                                ";

fn invalid_key<T>() -> Result<T, Fail> {
    fail("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type")
}

/// The text of a JSON string or of an object key.
struct JsonText<'h> {
    text: Cow<'h, str>,
    sand_str: SandStr,
}

/// The Sand `str` that an output can use for a [`JsonText`].
#[derive(Clone, Copy)]
enum SandStr {
    /// The `str` on the heap that has the text.
    OnHeap(VmValue),
    /// One `str` for each use of the name of the variant with this runtime
    /// ID.
    VariantName(u32),
    /// A new copy of the text.
    Copy,
}

impl<'h> JsonText<'h> {
    fn new(text: impl Into<Cow<'h, str>>) -> Self {
        JsonText { text: text.into(), sand_str: SandStr::Copy }
    }

    /// Returns the text of a string. `heap_str` is the string if it is an
    /// immutable `str` on the heap.
    fn for_str(text: &'h str, heap_str: Option<VmValue>) -> Self {
        JsonText { text: text.into(), sand_str: heap_str.map_or(SandStr::Copy, SandStr::OnHeap) }
    }

    fn variant_name(variant: VariantRef<'h>) -> Self {
        JsonText { text: variant.name.into(), sand_str: SandStr::VariantName(variant.id) }
    }

    /// Returns the text of `leaf` as a JSON object key, or `None` if `leaf`
    /// has no text form.
    #[inline(always)]
    fn for_key(leaf: Leaf<'h>, rt_ids: RtIds) -> Option<Self> {
        Some(match leaf {
            Leaf::Int(i) | Leaf::Duration(i) => JsonText::new(i.to_string()),
            Leaf::Bool(b) => JsonText::new(if b { "true" } else { "false" }),
            Leaf::Tag(variant) if variant.id != rt_ids.none => JsonText::new(variant.name),
            Leaf::Str { text, heap_str } => JsonText::for_str(text, heap_str),
            Leaf::Native(native) => JsonText::new(native.text()),
            _ => return None,
        })
    }
}

/// A JSON value without members.
enum Scalar<'h> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(JsonText<'h>),
    /// The Base64 text of a `bytes` value.
    Base64(String),
}

/// The key of a member of a JSON object.
enum JsonKey<'h> {
    Text(JsonText<'h>),
    /// The next of the field names that the object began with.
    Field(&'h str),
}

/// A JSON array or object that begins.
enum Begin<'h> {
    /// An array with this number of members.
    Array(usize),
    Object,
    /// An object with a [`JsonKey::Field`] for each of the field names
    /// `names` of `owner`, in order.
    Fields {
        owner: FieldsOwner,
        names: &'h [String],
    },
}

/// A struct type or a variant with field names, by its runtime ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum FieldsOwner {
    Struct(u32),
    Variant(u32),
}

/// The receiver of the JSON values that a [`JsonSink`] makes from Sand values.
///
/// The sink starts each member of an array or object with
/// [`JsonOutput::member`]. The value of the member follows: a scalar, a `Json`
/// value, or an array or object with its own members. An output charges fuel
/// only for the bytes that it writes or copies, because [`encode::walk`]
/// charges for each value.
///
/// Each method of an implementation must be `#[inline(always)]`, for the
/// reason that [`EncodeSink`] gives.
trait JsonOutput<'h> {
    /// The state of a JSON array or object that waits for its members.
    type Open;

    /// The message of the limit error for a value with more than
    /// [`MAX_DEPTH`] levels of JSON arrays and objects.
    const TOO_DEEP: &'static str;

    /// Fails with a memory limit error if the output has no room for a string
    /// of `len` bytes.
    fn reserve(&mut self, len: usize) -> Result<(), Fail>;

    /// Starts the next member of `parent`. `key` is `None` in an array.
    /// `depth` is the number of open arrays and objects.
    fn member(&mut self, parent: &mut Self::Open, key: Option<JsonKey<'h>>, depth: usize) -> Result<(), Fail>;

    /// Receives `scalar` as the value of the member of `parent` that started
    /// last.
    fn scalar(&mut self, parent: &mut Self::Open, scalar: Scalar<'h>) -> Result<(), Fail>;

    /// Receives the Sand `Json` value `json` as the value of the member of
    /// `parent` that started last, and returns true. An output that returns
    /// false receives the content of `json` as the value instead.
    fn embed(&mut self, parent: &mut Self::Open, json: VmValue) -> Result<bool, Fail>;

    fn begin(&mut self, begin: Begin<'h>) -> Result<Self::Open, Fail>;

    /// Receives the end of `open` as the value of the member of `parent` that
    /// started last. `depth` is the number of arrays and objects that stay
    /// open.
    fn end(&mut self, parent: &mut Self::Open, open: Self::Open, depth: usize) -> Result<(), Fail>;
}

/// The [`EncodeSink`] that gives each Sand value its JSON form and sends that
/// form to a [`JsonOutput`].
struct JsonSink<'h, 'o, O> {
    out: &'o mut O,
    meter: &'h Meter,
    json_ids: JsonIds,
    rt_ids: RtIds,
    /// The number of open JSON arrays and objects.
    depth: usize,
}

/// The JSON arrays and objects that are open for a Sand container.
struct JsonLevels<O> {
    /// The array or object that receives the children of the container.
    members: O,
    /// The object that has the variant name as its key and `members` as its
    /// value, if the container is a variant with a payload.
    wrapper: Option<O>,
}

/// Sends the JSON form of `value` to `out`, and returns `root` with the value
/// in it.
#[inline(always)]
fn write_json<'h, O: JsonOutput<'h>>(
    prog: &'h Program,
    heap: &'h Heap,
    meter: &'h Meter,
    json_ids: JsonIds,
    out: &mut O,
    root: O::Open,
    value: VmValue,
) -> Result<O::Open, Fail> {
    let mut sink = JsonSink { out, meter, json_ids, rt_ids: prog.ids, depth: 0 };
    Ok(encode::walk(prog, heap, meter, &mut sink, JsonLevels { members: root, wrapper: None }, value)?.members)
}

impl<'h, O: JsonOutput<'h>> JsonSink<'h, '_, O> {
    /// Starts a value with the role `role` in `parent`.
    #[inline(always)]
    fn begin_value(&mut self, parent: &mut O::Open, role: Role<'h>) -> Result<(), Fail> {
        match role {
            Role::Root | Role::Payload | Role::MapValue => Ok(()),
            Role::Item => self.out.member(parent, None, self.depth),
            Role::Field(name) => self.out.member(parent, Some(JsonKey::Field(name)), self.depth),
            Role::MapKey => invalid_key(),
        }
    }

    #[inline(always)]
    fn begin(&mut self, begin: Begin<'h>) -> Result<O::Open, Fail> {
        if self.depth >= MAX_DEPTH {
            return fail("limit", O::TOO_DEEP);
        }
        self.depth += 1;
        self.out.begin(begin)
    }

    #[inline(always)]
    fn end(&mut self, parent: &mut O::Open, open: O::Open) -> Result<(), Fail> {
        self.depth -= 1;
        self.out.end(parent, open, self.depth)
    }

    fn range(&mut self, parent: &mut O::Open, start: i64, end: i64, inclusive: bool) -> Result<(), Fail> {
        let mut object = self.begin(Begin::Object)?;
        let fields = [("start", Scalar::Int(start)), ("end", Scalar::Int(end)), ("inclusive", Scalar::Bool(true))];
        for (name, value) in fields.into_iter().take(if inclusive { 3 } else { 2 }) {
            self.out.member(&mut object, Some(JsonKey::Text(JsonText::new(name))), self.depth)?;
            self.out.scalar(&mut object, value)?;
        }
        self.end(parent, object)
    }
}

impl<'h, O: JsonOutput<'h>> EncodeSink<'h> for JsonSink<'h, '_, O> {
    type Open = JsonLevels<O::Open>;

    const TOO_DEEP: &'static str = O::TOO_DEEP;

    #[inline(always)]
    fn leaf(&mut self, parent: &mut Self::Open, role: Role<'h>, leaf: Leaf<'h>) -> Result<(), Fail> {
        let parent = &mut parent.members;
        if let Role::MapKey = role {
            let Some(key) = JsonText::for_key(leaf, self.rt_ids) else { return invalid_key() };
            return self.out.member(parent, Some(JsonKey::Text(key)), self.depth);
        }
        self.begin_value(parent, role)?;
        let scalar = match leaf {
            Leaf::Unit => Scalar::Null,
            Leaf::Bool(b) => Scalar::Bool(b),
            Leaf::Int(i) | Leaf::Duration(i) => Scalar::Int(i),
            Leaf::Float(f) => Scalar::Float(f),
            Leaf::Str { text, heap_str } => Scalar::Str(JsonText::for_str(text, heap_str)),
            Leaf::Bytes(bytes) => {
                self.out.reserve(bytes.len() / 3 * 4 + 4)?;
                self.meter.charge_slow_bytes(bytes.len())?;
                Scalar::Base64(base64::encode(bytes, false))
            }
            Leaf::Range { start, end, inclusive } => return self.range(parent, start, end, inclusive),
            Leaf::Tag(variant) if variant.id == self.rt_ids.none || variant.id == self.json_ids.null => Scalar::Null,
            Leaf::Tag(variant) => Scalar::Str(JsonText::variant_name(variant)),
            Leaf::Native(native) => Scalar::Str(JsonText::new(native.text())),
            Leaf::Opaque { kind_name, .. } => return Err(without_trait(kind_name, "encoded as JSON", "Encode")),
        };
        self.out.scalar(parent, scalar)
    }

    #[inline(always)]
    fn open(&mut self, parent: &mut Self::Open, role: Role<'h>, container: Container<'h>, len: usize) -> Result<Opened<Self::Open>, Fail> {
        let parent = &mut parent.members;
        let mut wrapper = None;
        if let Container::Variant(variant) = container {
            if variant.id == self.rt_ids.some || self.json_ids.has(variant.id) {
                self.begin_value(parent, role)?;
                let is_embedded = self.json_ids.has(variant.id) && self.out.embed(parent, variant.value)?;
                return Ok(if is_embedded { Opened::Closed } else { Opened::Unwrapped });
            }
            self.begin_value(parent, role)?;
            let mut object = self.begin(Begin::Object)?;
            self.out.member(&mut object, Some(JsonKey::Text(JsonText::variant_name(variant))), self.depth)?;
            wrapper = Some(object);
        } else {
            self.begin_value(parent, role)?;
        }
        let members = self.begin(match container {
            Container::Map => Begin::Object,
            Container::Struct { def, ty } => Begin::Fields { owner: FieldsOwner::Struct(def), names: &ty.fields },
            Container::Variant(VariantRef { id, field_names: Some(names), .. }) => Begin::Fields { owner: FieldsOwner::Variant(id), names },
            _ => Begin::Array(len),
        })?;
        Ok(Opened::Children(JsonLevels { members, wrapper }))
    }

    #[inline(always)]
    fn close(&mut self, parent: &mut Self::Open, open: Self::Open) -> Result<(), Fail> {
        let mut outermost = open.members;
        if let Some(mut wrapper) = open.wrapper {
            self.end(&mut wrapper, outermost)?;
            outermost = wrapper;
        }
        self.end(&mut parent.members, outermost)
    }
}

/// The [`JsonOutput`] of `json.encode`, which writes JSON text.
struct JsonTextOutput<'h> {
    heap: &'h Heap,
    meter: &'h Meter,
    indent: usize,
    text: String,
    /// The number of bytes of `text` that the output has charged fuel for.
    charged: usize,
    scratch: Scratch,
}

/// A JSON array or object in the text that waits for its members.
struct OpenBracket {
    is_array: bool,
    is_empty: bool,
}

impl JsonTextOutput<'_> {
    /// Starts a line for a member at `depth`, if the text has an indent.
    #[inline(always)]
    fn newline(&mut self, depth: usize) -> Result<(), Fail> {
        if self.indent > 0 { self.indented_line(depth) } else { Ok(()) }
    }

    fn indented_line(&mut self, depth: usize) -> Result<(), Fail> {
        let n = self.indent.saturating_mul(depth);
        self.scratch.check(self.heap, self.text.len().saturating_add(n))?;
        self.text.push('\n');
        let mut left = n;
        while left > 0 {
            let spaces = &SPACES[..left.min(SPACES.len())];
            self.text.push_str(spaces);
            left -= spaces.len();
        }
        Ok(())
    }

    #[inline(always)]
    fn string(&mut self, s: &str) -> Result<(), Fail> {
        if s.len() > 1024 {
            self.reserve(escaped_len(s))?;
        }
        write_str(&mut self.text, s);
        Ok(())
    }

    /// Charges fuel for the text that the output wrote before the JSON value
    /// that starts now, and checks the text against the memory limit.
    #[inline(always)]
    fn charge_written(&mut self) -> Result<(), Fail> {
        let written = self.text.len() - self.charged;
        self.charged = self.text.len();
        self.meter.charge_bytes(written)?;
        self.scratch.check(self.heap, self.text.capacity())
    }
}

impl<'h> JsonOutput<'h> for JsonTextOutput<'h> {
    type Open = OpenBracket;

    const TOO_DEEP: &'static str = "value too deeply nested to encode as JSON";

    #[inline(always)]
    fn reserve(&mut self, len: usize) -> Result<(), Fail> {
        self.heap.check_memory(self.text.len().saturating_add(len))
    }

    #[inline(always)]
    fn member(&mut self, parent: &mut OpenBracket, key: Option<JsonKey<'h>>, depth: usize) -> Result<(), Fail> {
        if !std::mem::replace(&mut parent.is_empty, false) {
            self.text.push(',');
        }
        self.newline(depth)?;
        match key {
            Some(JsonKey::Text(key)) => self.string(&key.text)?,
            Some(JsonKey::Field(name)) => self.string(name)?,
            None => return Ok(()),
        }
        self.text.push(':');
        if self.indent > 0 {
            self.text.push(' ');
        }
        Ok(())
    }

    #[inline(always)]
    fn scalar(&mut self, _: &mut OpenBracket, scalar: Scalar<'h>) -> Result<(), Fail> {
        self.charge_written()?;
        match scalar {
            Scalar::Bool(b) => self.text.push_str(if b { "true" } else { "false" }),
            Scalar::Int(i) => self.text.push_str(itoa::Buffer::new().format(i)),
            Scalar::Float(f) if f.is_finite() => _ = write!(self.text, "{f:?}"),
            Scalar::Null | Scalar::Float(_) => self.text.push_str("null"),
            Scalar::Str(string) => self.string(&string.text)?,
            Scalar::Base64(text) => self.text.extend(["\"", &text, "\""]),
        }
        Ok(())
    }

    #[inline(always)]
    fn embed(&mut self, _: &mut OpenBracket, _: VmValue) -> Result<bool, Fail> {
        Ok(false)
    }

    #[inline(always)]
    fn begin(&mut self, begin: Begin<'h>) -> Result<OpenBracket, Fail> {
        self.charge_written()?;
        let is_array = matches!(begin, Begin::Array(_));
        self.text.push(if is_array { '[' } else { '{' });
        Ok(OpenBracket { is_array, is_empty: true })
    }

    #[inline(always)]
    fn end(&mut self, _: &mut OpenBracket, open: OpenBracket, depth: usize) -> Result<(), Fail> {
        if !open.is_empty {
            self.newline(depth)?;
        }
        self.text.push_str(if open.is_array { "]" } else { "}" });
        Ok(())
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

/// Returns true if a JSON string must escape a byte of `bytes`.
#[inline]
fn has_escape(bytes: &[u8]) -> bool {
    const LOW_BITS: u64 = u64::from_ne_bytes([0x01; 8]);
    const HIGH_BITS: u64 = u64::from_ne_bytes([0x80; 8]);
    // `(x - LOW_BITS) & !x & HIGH_BITS` is not zero if a byte of `x` is zero.
    // With `LOW_BITS * n` in place of `LOW_BITS`, it is not zero if a byte of
    // `x` is less than `n`.
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

/// Writes the content of a JSON string for `s` with its escapes.
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

/// The [`JsonOutput`] of `json.to_json`, which builds a Sand `Json` value.
struct JsonValueOutput<'a, 's, 'h> {
    b: Builder<'a, 's, 'h>,
    json_ids: JsonIds,
    /// The keys of the fields of each struct type and variant that the output
    /// has received, in order. `None` follows the keys of each type.
    field_keys: Vec<Option<FieldKey>>,
    /// The index in `field_keys` of the first field of each struct type and
    /// variant that the output has received.
    first_field_keys: FxHashMap<FieldsOwner, usize>,
    /// The name string of each variant that the output has received, by
    /// runtime variant ID.
    variant_names: FxHashMap<u32, VmValue>,
    /// The key string and the key text of each object member that waits for
    /// its value.
    pending_keys: Vec<(VmValue, Cow<'a, str>)>,
}

/// The key of an object member for a field.
#[derive(Clone, Copy)]
struct FieldKey {
    /// The Sand `str` that has the field name.
    name: VmValue,
    hash: u64,
    name_len: usize,
}

/// A `Json.Arr` or `Json.Obj` value that waits for its members.
enum PartialJson {
    Root(VmValue),
    Array(Vec<VmValue>),
    Object(MapBuilder),
    /// An object with field names, and the index in
    /// [`JsonValueOutput::field_keys`] of the key of its first member.
    Fields(MapBuilder, usize),
}

impl<'a, 's, 'h> JsonValueOutput<'a, 's, 'h> {
    fn new(b: Builder<'a, 's, 'h>, json_ids: JsonIds) -> Self {
        let (field_keys, first_field_keys, variant_names, pending_keys) = Default::default();
        JsonValueOutput { b, json_ids, field_keys, first_field_keys, variant_names, pending_keys }
    }

    /// Returns the Sand `str` that has the text of `text`, or `None` if the
    /// output must copy the text to make one.
    #[inline(always)]
    fn shared_str(&mut self, text: &JsonText<'_>) -> Option<VmValue> {
        match text.sand_str {
            SandStr::OnHeap(heap_str) => Some(heap_str),
            SandStr::VariantName(id) => Some(*self.variant_names.entry(id).or_insert_with(|| self.b.st.new_str(&*text.text))),
            SandStr::Copy => None,
        }
    }

    /// Returns the key of the next member of an object with field names, or
    /// `None` if the object has a member for each field name. The walk
    /// reports the fields of a type in order, so the number of members that
    /// the object has gives the next field.
    #[inline(always)]
    fn next_field_key(&self, map: &MapBuilder, first_key: usize) -> Option<FieldKey> {
        self.field_keys.get(first_key + self.b.entries.len() - map.base).copied().flatten()
    }

    #[inline]
    fn put(&mut self, parent: &mut PartialJson, value: VmValue) -> Result<(), Fail> {
        match parent {
            PartialJson::Root(root) => *root = value,
            PartialJson::Array(items) => items.push(value),
            PartialJson::Object(map) => {
                let Some((key, text)) = self.pending_keys.pop() else { return internal("json.to_json: a member without a key") };
                let hash = self.b.hash(&text)?;
                self.b.put(map, hash, Some(text), key, value)?;
            }
            PartialJson::Fields(map, first_key) => {
                let Some(key) = self.next_field_key(map, *first_key) else {
                    return internal("json.to_json: more members than field names");
                };
                self.b.meter.charge_bytes(key.name_len)?;
                self.b.append(map, key.hash, key.name, value)?;
            }
        }
        Ok(())
    }
}

impl<'a> JsonOutput<'a> for JsonValueOutput<'a, '_, '_> {
    type Open = PartialJson;

    const TOO_DEEP: &'static str = "value too deeply nested to convert to JSON";

    #[inline(always)]
    fn reserve(&mut self, len: usize) -> Result<(), Fail> {
        self.b.st.check_memory(self.b.pending.saturating_add(len))
    }

    #[inline(always)]
    fn member(&mut self, _: &mut PartialJson, key: Option<JsonKey<'a>>, _: usize) -> Result<(), Fail> {
        if let Some(JsonKey::Text(key)) = key {
            let string = match self.shared_str(&key) {
                Some(string) => string,
                None => self.b.st.new_str(&*key.text),
            };
            self.pending_keys.push((string, key.text));
        }
        Ok(())
    }

    #[inline(always)]
    fn scalar(&mut self, parent: &mut PartialJson, scalar: Scalar<'a>) -> Result<(), Fail> {
        self.b.reserve()?;
        let json_ids = self.json_ids;
        let (id, payload) = match scalar {
            Scalar::Null => return self.put(parent, VmValue::Tag(json_ids.null)),
            Scalar::Bool(b) => (json_ids.bool_, VmValue::Bool(b)),
            Scalar::Int(i) => (json_ids.int, VmValue::Int(i)),
            Scalar::Float(f) => (json_ids.float, VmValue::Float(f)),
            Scalar::Str(text) => match self.shared_str(&text) {
                Some(string) => (json_ids.str_, string),
                None => {
                    self.b.meter.charge_bytes(text.text.len())?;
                    (json_ids.str_, self.b.st.new_str(&*text.text))
                }
            },
            Scalar::Base64(text) => (json_ids.str_, self.b.st.new_str(text)),
        };
        let value = self.b.st.new_variant(id, &[payload]);
        self.put(parent, value)
    }

    #[inline(always)]
    fn embed(&mut self, parent: &mut PartialJson, json: VmValue) -> Result<bool, Fail> {
        self.b.reserve()?;
        self.put(parent, json)?;
        Ok(true)
    }

    #[inline(always)]
    fn begin(&mut self, begin: Begin<'a>) -> Result<PartialJson, Fail> {
        self.b.reserve()?;
        let (owner, names) = match begin {
            Begin::Array(len) => return Ok(PartialJson::Array(self.b.list_buffer(len)?)),
            Begin::Object => return Ok(PartialJson::Object(self.b.start_map())),
            Begin::Fields { owner, names } => (owner, names),
        };
        let first_key = *self.first_field_keys.entry(owner).or_insert_with(|| {
            let first_key = self.field_keys.len();
            let key = |name: &String| Some(FieldKey { name: self.b.st.new_str(name.as_str()), hash: str_hash(name), name_len: name.len() });
            self.field_keys.extend(names.iter().map(key).chain([None]));
            first_key
        });
        Ok(PartialJson::Fields(self.b.start_map(), first_key))
    }

    #[inline(always)]
    fn end(&mut self, parent: &mut PartialJson, open: PartialJson, _: usize) -> Result<(), Fail> {
        let (id, payload) = match open {
            PartialJson::Root(_) => return internal("json.to_json: the root has no end"),
            PartialJson::Array(items) => (self.json_ids.arr, self.b.list(items)),
            PartialJson::Object(map) => (self.json_ids.obj, self.b.map(map)),
            PartialJson::Fields(map, first_key) => {
                if self.next_field_key(&map, first_key).is_some() {
                    return internal("json.to_json: fewer members than field names");
                }
                (self.json_ids.obj, self.b.map(map))
            }
        };
        let value = self.b.st.new_variant(id, &[payload]);
        self.put(parent, value)
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
