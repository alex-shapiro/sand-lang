//! JSON encoding and decoding.

use super::base64;
use crate::check::ModKind;
use crate::codec::MAX_NESTING_DEPTH;
use crate::codec::{
    DecodeDict, Decoder, EncodeDict, EncodeFormat, EncodeStep, Encoder, fast_schema, read_json, read_whole, with_encoder, with_input,
    write_whole,
};
use crate::native::convert::{Conversion, FromVmValue, HostError, IntoVmValue, mismatch};
use crate::native::ty::{Param, VmVal};
use crate::native::{EnumType, NamedType, NativeModule, variant_index};
use crate::native::{VariantDescCell, encode_variant};
use crate::value::{Decode, DecodeError, Encode, Value, ValueReader};
use crate::vm::bytecode::Program;
use crate::vm::compare::str_hash;
use crate::vm::heap::Staging;
use crate::vm::meter::{Meter, SLOW_BYTES_PER_UNIT};
use crate::vm::value::{KeyIndex, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, VmValue, slot_fits};
use crate::vm::{Fail, Vm, fail, internal};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;

mod input;
mod text;
mod tree;

use text::{TextDecoder, TextEncoder, TreeEncoder};
use tree::TreeDecoder;

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

/// The host descriptors of the variants of `Json`.
static VARIANT_DESCS: [VariantDescCell; Json::VARIANTS.len()] = [const { VariantDescCell::new() }; Json::VARIANTS.len()];

/// Returns the host value of the `Json` variant with index `index` and
/// `payload`.
fn json_variant(index: usize, payload: Option<Value>) -> Value {
    encode_variant::<Json>(&VARIANT_DESCS, index, payload)
}

impl Encode for Json {
    fn encode(&self) -> Value {
        let payload = match self {
            Json::Null => None,
            Json::Bool(b) => Some(b.encode()),
            Json::Int(i) => Some(i.encode()),
            Json::Float(f) => Some(f.encode()),
            Json::Str(s) => Some(s.encode()),
            Json::Arr(items) => Some(items.encode()),
            Json::Obj(members) => Some(members.encode()),
        };
        json_variant(self.index(), payload)
    }
}

/// Accepts a `Json` variant and any `Value` in JSON form.
impl Decode for Json {
    fn decode(r: &mut ValueReader<'_>) -> Result<Json, DecodeError> {
        let scalar = match r.node().map_err(|e| r.fail(e))? {
            Value::Unit => Json::Null,
            &Value::Bool(b) => Json::Bool(b),
            &Value::Int(i) => Json::Int(i),
            &Value::Float(f) => Json::Float(f),
            Value::Str(s) => Json::Str(s.clone()),
            Value::List(_) => return Vec::decode(r).map(Json::Arr),
            Value::Map(_) => return indexmap::IndexMap::decode(r).map(Json::Obj),
            _ => return crate::value::decode_scalar(r, |n, _| Err(n.mismatch("Json"))),
        };
        r.skip();
        Ok(scalar)
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
    let mut m = NativeModule::std("json", ModKind::Std, include_str!("json.sand"));
    m.bind::<Json>();
    m.bind::<Encoding<()>>();
    m.bind::<Decoding<()>>();
    m.function("parse", |vm: &mut Vm, text: VmVal<String>| -> Result<VmVal<Json>, HostError> {
        let ids = JsonIds::new(&vm.prog)?;
        let meter = &vm.meter;
        let r = vm.heap.staged(|heap, st| {
            let mut b = Builder::new(st, meter);
            let mut p = Parser::new(text.str(heap));
            let r = p.parse_json(&mut b, &ids).and_then(|v| p.finish().map(|()| v));
            p.charge(meter)?;
            r
        });
        into_result(r)
    });
    m.function("encode_text", |vm: &mut Vm, encode: EncodeDict, value: Param<0>, indent: i64| -> Result<Encoding<VmVal<String>>, Fail> {
        let mut f = TextEncoder::new(indent.clamp(0, 1 << 16) as usize);
        let Some(schema) = fast_schema(vm, encode) else { return vm.new_object(Encoder::new(Box::new(f))).map(Encoding::Driven) };
        let r = f.write_fast(vm, value.raw(), schema);
        let (text, counted) = f.take_text();
        vm.heap.account(counted, 0);
        r?;
        Ok(Encoding::Done(VmVal::from_raw(vm.new_str(text))))
    });
    m.function("encoded_text", |vm: &mut Vm, e: VmVal<Encoder>| -> Result<VmVal<String>, Fail> {
        let (text, counted) =
            with_encoder(vm, e, EncodeStep::Finish, |f, _| match (f as &mut dyn std::any::Any).downcast_mut::<TextEncoder>() {
                Some(text) => Ok(text.take_text()),
                None => internal("json.encode: not a JSON text encoder"),
            })?;
        vm.heap.account(counted, 0);
        Ok(VmVal::from_raw(vm.new_str(text)))
    });
    m.function("encode_json", |vm: &mut Vm, encode: EncodeDict, value: Param<0>| -> Result<Encoding<VmVal<Json>>, Fail> {
        let mut f = TreeEncoder::new();
        let Some(schema) = fast_schema(vm, encode) else { return vm.new_object(Encoder::new(Box::new(f))).map(Encoding::Driven) };
        let r = f.write_fast(vm, value.raw(), schema).and_then(|()| f.encoded());
        f.release(vm);
        Ok(Encoding::Done(VmVal::from_raw(r?)))
    });
    m.function("encoded_json", |vm: &mut Vm, e: VmVal<Encoder>| -> Result<VmVal<Json>, Fail> {
        let root = with_encoder(vm, e, EncodeStep::Finish, |f, _| match (f as &mut dyn std::any::Any).downcast_mut::<TreeEncoder>() {
            Some(tree) => tree.encoded(),
            None => internal("json.to_json: not a Json encoder"),
        });
        Ok(VmVal::from_raw(root?))
    });
    m.function("decode_text", |vm: &mut Vm, decode: DecodeDict, text: VmVal<String>| -> Result<Decoding<Param<0>>, Fail> {
        let mut f = TextDecoder::new();
        let Some(schema) = fast_schema(vm, decode) else {
            let d = vm.new_object(Decoder::new(Box::new(f)))?;
            d.push_value(vm, text.raw())?;
            return Ok(Decoding::Driven(d));
        };
        Ok(Decoding::of(with_input(vm, Some(text.raw()), &mut f, |f, vm| read_whole(vm, f, schema))??))
    });
    m.function("decode_json", |vm: &mut Vm, decode: DecodeDict, json: VmVal<Json>| -> Result<Decoding<Param<0>>, Fail> {
        let Some(schema) = fast_schema(vm, decode) else {
            let d = vm.new_object(Decoder::new(Box::new(TreeDecoder::new(vm, None)?)))?;
            d.push_value(vm, json.raw())?;
            return Ok(Decoding::Driven(d));
        };
        let mut f = TreeDecoder::new(vm, Some(json.raw()))?;
        let r = read_whole(vm, &mut f, schema);
        f.release_values(vm);
        Ok(Decoding::of(r?))
    });
    m.function("Json.encode", |vm: &mut Vm, json: VmVal<Json>, e: VmVal<Encoder>| write_whole(vm, e, |f, vm| f.json(vm, json.raw())));
    m.function("Json.decode", |vm: &mut Vm, d: VmVal<Decoder>| -> Result<VmVal<Json>, HostError> { read_json(vm, d).map(VmVal::from_raw) });
    m
}

/// The start of an encode: the encoded value of a type with a fast path, or
/// the `Encoder` that the Sand implementations of the type write to.
#[derive(crate::SandEnum)]
#[sand(module = "json", private, no_value)]
enum Encoding<T> {
    Done(T),
    Driven(VmVal<Encoder>),
}

/// The start of a decode: the result for a type with a fast path, or the
/// `Decoder` that the Sand implementations of the type read from.
#[derive(crate::SandEnum)]
#[sand(module = "json", private, no_value)]
enum Decoding<T> {
    Done(T),
    Failed(super::core::Error),
    Driven(VmVal<Decoder>),
}

impl Decoding<Param<0>> {
    fn of(r: Result<VmValue, super::core::Error>) -> Decoding<Param<0>> {
        match r {
            Ok(v) => Decoding::Done(Param::from_raw(v)),
            Err(e) => Decoding::Failed(e),
        }
    }
}

/// Converts the Sand `Json` value `v` into a Rust [`Json`].
pub(crate) fn json_from_vm(vm: &Vm, v: VmValue) -> Result<Json, Fail> {
    Json::from_vm_value(vm, v, &mut Conversion::new())
}

/// Copies the Sand `Json` value `root` to the host value that
/// [`Json::encode`] gives. `count` counts the bytes of each part of the copy
/// against the memory limit before the part exists.
pub(crate) fn json_host_value(
    vm: &mut Vm,
    root: VmValue,
    count: &mut dyn FnMut(&mut Vm, usize) -> Result<(), Fail>,
) -> Result<Value, Fail> {
    /// An open array or object, the index of its next element or entry,
    /// its finished parts, and the key of the entry being copied.
    enum Open {
        Arr(VmValue, usize, Vec<Value>),
        Obj(VmValue, usize, Vec<(Value, Value)>, Option<Value>),
    }
    let ids = JsonIds::new(&vm.prog)?;
    let index = |id: u32| (id - ids.null) as usize;
    let mut stack: Vec<Open> = Vec::new();
    let mut node = root;
    loop {
        vm.charge(1)?;
        let id = vm.heap.variant_id(node);
        count(vm, 2 * size_of::<Value>())?;
        let mut done = match vm.heap.field(node, 0) {
            _ if id == ids.null => Some(json_variant(index(id), None)),
            Some(VmValue::Bool(b)) if id == ids.bool_ => Some(json_variant(index(id), Some(Value::Bool(b)))),
            Some(VmValue::Int(i)) if id == ids.int => Some(json_variant(index(id), Some(Value::Int(i)))),
            Some(VmValue::Float(f)) if id == ids.float => Some(json_variant(index(id), Some(Value::Float(f)))),
            Some(p) if id == ids.str_ => {
                let len = vm.heap.str(p).len();
                vm.charge_bytes(len)?;
                count(vm, len)?;
                Some(json_variant(index(id), Some(Value::Str(vm.heap.str(p).to_string()))))
            }
            Some(p) if id == ids.arr => {
                let len = vm.heap.list(p).len();
                count(vm, len * size_of::<Value>())?;
                stack.push(Open::Arr(p, 0, Vec::with_capacity(len)));
                None
            }
            Some(p) if id == ids.obj => {
                let Obj::Map(m) = vm.heap.get(p) else { return internal("json: not an object") };
                let len = m.live;
                count(vm, len * size_of::<(Value, Value)>())?;
                stack.push(Open::Obj(p, 0, Vec::with_capacity(len), None));
                None
            }
            _ => return internal("json: not a Json value"),
        };
        if stack.len() > MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to copy");
        }
        node = loop {
            match stack.last_mut() {
                None => return done.ok_or_else(missing_root_error),
                Some(Open::Arr(list, i, items)) => {
                    items.extend(done.take());
                    if let Some(&x) = vm.heap.list(*list).get(*i) {
                        *i += 1;
                        break x;
                    }
                }
                Some(Open::Obj(map, i, entries, key)) => {
                    if let (Some(k), Some(v)) = (key.take(), done.take()) {
                        entries.push((k, v));
                    }
                    let Obj::Map(m) = vm.heap.get(*map) else { return internal("json: not an object") };
                    let next = m.entries.get(*i..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((k, (*e)?))));
                    if let Some((k, (key_str, x))) = next {
                        *i += k + 1;
                        let text = vm.heap.str(key_str).to_string();
                        vm.charge_bytes(text.len())?;
                        count(vm, text.len())?;
                        *key = Some(Value::Str(text));
                        break x;
                    }
                }
            }
            done = Some(match stack.pop() {
                Some(Open::Arr(.., items)) => json_variant(index(ids.arr), Some(Value::List(items))),
                Some(Open::Obj(_, _, entries, _)) => json_variant(index(ids.obj), Some(Value::Map(entries))),
                None => return internal("json: no open value"),
            });
        };
    }
}

/// Converts `json` into a Sand `Json` value.
pub(crate) fn json_into_vm(vm: &mut Vm, json: Json) -> Result<VmValue, Fail> {
    json.into_vm_value(vm)
}

enum Error {
    /// Invalid JSON syntax, with its location.
    Parse(String),
    /// Valid JSON that does not match the type that a decoder reads, such as
    /// "expected int".
    Mismatch(String),
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
        Err(Error::Parse(msg) | Error::Mismatch(msg)) => Err(HostError::new("parse", msg)),
        Err(Error::Fail(c)) => Err(c.into()),
    }
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

    /// Adds an entry to the map `m`. It is not inlined, so that the stack
    /// frame of each nesting level of [`Parser::parse_json`] stays small.
    #[inline(never)]
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
            _ => return self.parse_scalar(b, ids),
        })
    }

    /// Parses a JSON value that is not an array or an object. It is a
    /// separate function so that the stack frame of each nesting level of
    /// [`Parser::parse_json`] stays small.
    #[inline(never)]
    fn parse_scalar(&mut self, b: &mut Builder<'a, '_, '_>, ids: &JsonIds) -> PResult<VmValue> {
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

const SPACES: &str = "                                                                ";

fn json_number(st: &mut Staging, ids: &JsonIds, n: VmValue) -> VmValue {
    let id = if matches!(n, VmValue::Int(_)) { ids.int } else { ids.float };
    st.new_variant(id, &[n])
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
