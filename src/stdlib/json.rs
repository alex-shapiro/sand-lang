//! The `json` module: a JSON parser and printer, plus reflection-based
//! conversion between JSON and typed values (`encode`, `decode`, `to_json`).
//!
//! Parsing and conversion build their results with `Heap::staged`, reading
//! the input text (or the value converted) where it lives on the heap:
//! strings are copied once, from the input into their heap objects, and
//! object keys that repeat within a document share one string. Encoding
//! walks the value on the heap and writes the text directly.

use super::base64;
use crate::check::ModKind;
use crate::native::traits;
use crate::native::{
    Binding, Constant, Conversion, EnumType, EnumVariant, FromValue, HostError, IntoValue, NamedType, NativeModule, Param, Reflect,
    SandType, Type, Val, VariantField, mismatch, variant_const, variant_index,
};
use crate::syntax::lexer::is_ident;
use crate::types::ConstVal;
use crate::vm::bytecode::Program;
use crate::vm::compare::{hash_key, str_hash};
use crate::vm::heap::{Heap, Staging};
use crate::vm::meter::{BYTES_PER_UNIT, Meter, SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{
    FieldDefault, FieldDesc, FxHashMap, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, TypeDesc, Value, ValueRef, VariantDesc, slot_fits,
};
use crate::vm::{Ctl, NResult, Vm, fail, internal, without_trait};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;

/// Maximum nesting of arrays and objects, both when parsing and printing.
const MAX_DEPTH: usize = 512;

/// Approximate heap cost of one value built (a string, list, map or struct
/// header), checked against the memory limit before each.
const NODE: usize = 64;

/// A dynamic JSON value. Plain integers that fit in a 64-bit signed `int` parse
/// as `Int`; all other numbers parse as `Float`. Object entries preserve
/// insertion order.
///
/// Conversion between Sand and Rust accepts values with up to 512 nested
/// arrays and objects, the same limit as `json.parse`, and uses a constant
/// amount of stack. A deeper value stops the script with a `limit` error.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    /// JSON `null`.
    Null,
    /// JSON boolean (`true` or `false`).
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

/// Runtime variant IDs for `Json` in a compiled program.
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
    fn of(prog: &Program) -> Result<JsonIds, Ctl> {
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
    m.vm_function("parse", |vm: &mut Vm, text: Val<String>| -> Result<Val<Json>, HostError> {
        let ids = JsonIds::of(&vm.prog)?;
        let meter = &vm.meter;
        let r = vm.heap.staged(|heap, st| {
            let mut b = Build::new(heap, st, meter);
            let mut p = Parser::new(text.str(heap));
            let r = p.parse_json(&mut b, &ids).and_then(|v| p.finish().map(|()| v));
            p.charge(meter)?;
            r
        });
        into_result(r)
    });
    m.vm_function("encode", |vm: &mut Vm, value: Param<0, traits::Encode>, indent: i64| -> Result<Val<String>, Ctl> {
        let ids = JsonIds::of(&vm.prog)?;
        let indent = indent.clamp(0, 1 << 16) as usize;
        let mut e = Encoder {
            prog: &vm.prog,
            heap: &vm.heap,
            meter: &vm.meter,
            ids,
            indent,
            out: String::new(),
            charged: 0,
            scratch: Scratch::new(),
        };
        e.value(value.raw(), 0)?;
        let out = e.out;
        Ok(Val::from_raw(vm.new_str(out)))
    });
    m.bind::<Decoded<()>>();
    m.vm_function("decode_raw", |vm: &mut Vm, ty: Reflect<0>, text: Val<String>| -> Result<Decoded<Param<0>>, Ctl> {
        let desc = ty.desc(&vm.heap)?.clone();
        let ids = vm.prog.ids;
        let meter = &vm.meter;
        let (r, fills) = vm.heap.staged(|heap, st| {
            let mut d = Decoder {
                p: Parser::new(text.str(heap)),
                b: Build::new(heap, st, meter),
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
            let r = charged.map_err(Error::Ctl).and(r).map(|v| match d.thunks.is_empty() {
                true => Decoded::Done(Param::from_raw(v)),
                false => Decoded::Pending(Param::from_raw(v), Val::from_raw(d.b.list(std::mem::take(&mut d.thunks)))),
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
        let fail = |kind: &str, message| Ok(Decoded::Failed(super::Error { kind: kind.into(), message }));
        match r {
            Ok(d) => Ok(d),
            Err(Error::Parse(msg)) => fail("parse", msg),
            Err(Error::Invalid(msg)) => fail("invalid", msg),
            Err(Error::Ctl(c)) => Err(c),
        }
    });
    m.vm_function("to_json", |vm: &mut Vm, value: Param<0, traits::Encode>| -> Result<Val<Json>, Ctl> {
        let ids = JsonIds::of(&vm.prog)?;
        let (prog, meter) = (&vm.prog, &vm.meter);
        let v = vm.heap.staged(|heap, st| {
            let mut c =
                Converter { prog, heap, b: Build::new(heap, st, meter), ids, fields: FxHashMap::default(), names: FxHashMap::default() };
            c.convert(value.raw(), 0)
        })?;
        Ok(Val::from_raw(v))
    });
    m
}

/// Intermediate result returned by `decode_raw` and finalized by `decode`.
#[derive(crate::SandEnum)]
#[sand(module = "json", private)]
enum Decoded<T> {
    /// Fully decoded value.
    Done(T),
    /// Decoded value accompanied by deferred thunks that evaluate non-constant
    /// field defaults and populate sets containing those structs.
    Pending(T, Val<Vec<fn()>>),
    /// Parse or type error encountered during decoding.
    Failed(super::Error),
}

/// Failure while parsing or decoding.
enum Error {
    Parse(String),
    Invalid(String),
    Ctl(Ctl),
}

impl From<Ctl> for Error {
    fn from(c: Ctl) -> Self {
        Error::Ctl(c)
    }
}

type PResult<T> = Result<T, Error>;

fn into_result<T>(r: PResult<Value>) -> Result<Val<T>, HostError> {
    match r {
        Ok(v) => Ok(Val::from_raw(v)),
        Err(Error::Parse(msg)) => Err(HostError::new("parse", msg)),
        Err(Error::Invalid(msg)) => Err(HostError::new("invalid", msg)),
        Err(Error::Ctl(c)) => Err(c.into()),
    }
}

// ---------------------------------------------------------------------------
// Building values

/// Object keys longer than this are not shared between occurrences.
const MAX_SHARED_KEY: usize = 64;
/// Distinct keys shared per call, at most.
const MAX_SHARED_KEYS: usize = 4096;
/// Maps being assembled look their keys up by a scan up to this size, and
/// through a hash index beyond.
const SCANNED_KEYS: usize = 16;

/// Builds staged values: checks memory, shares repeated object keys, and
/// assembles maps outside the heap (keys are hashed by their text, as staged
/// strings cannot be read back from the heap yet).
struct Build<'a, 's, 'h> {
    heap: &'a Heap,
    st: &'s mut Staging<'h>,
    meter: &'a Meter,
    /// Bytes held by the lists and maps being assembled, and by `keys`.
    pending: usize,
    /// The strings of object keys seen so far, with their texts and hashes.
    keys: HashTable<(u64, Cow<'a, str>, Value)>,
    /// The entries of the maps being assembled (each map's entries follow
    /// those of the maps enclosing it).
    entries: Vec<Entry<'a>>,
}

/// An entry of a map being assembled.
struct Entry<'a> {
    hash: u64,
    /// The key's text, if it is a string (other keys are inline values:
    /// numbers, booleans, payload-less variants).
    text: Option<Cow<'a, str>>,
    key: Value,
    value: Value,
}

/// A map being assembled by [`Build`]: its entries are `entries[base..]`.
struct MapBuild {
    base: usize,
    /// The slots of the entries by hash, once there are many.
    index: Option<KeyTable>,
}

impl<'a, 's, 'h> Build<'a, 's, 'h> {
    fn new(heap: &'a Heap, st: &'s mut Staging<'h>, meter: &'a Meter) -> Self {
        Build { heap, st, meter, pending: 0, keys: HashTable::new(), entries: Vec::new() }
    }

    /// Checks that a value's heap objects fit besides those being assembled.
    fn reserve(&self) -> Result<(), Ctl> {
        self.st.check_memory(NODE + self.pending)
    }

    /// Appends `v` to a list being assembled, checking its buffer's growth
    /// against the memory limit first: elements that are inline values
    /// (numbers, booleans, `null`) stage no object, so the buffer would
    /// otherwise grow unchecked until the list is complete.
    fn push(&mut self, items: &mut Vec<Value>, v: Value) -> Result<(), Ctl> {
        if items.len() == items.capacity() {
            let more = items.capacity().max(4);
            self.st.check_memory((NODE + self.pending).saturating_add(more.saturating_mul(2 * std::mem::size_of::<Value>())))?;
            let before = items.capacity();
            items.reserve_exact(more);
            self.pending += (items.capacity() - before) * std::mem::size_of::<Value>();
        }
        items.push(v);
        Ok(())
    }

    /// Stages a list assembled with [`Build::push`].
    fn list(&mut self, items: Vec<Value>) -> Value {
        self.pending = self.pending.saturating_sub(items.capacity() * std::mem::size_of::<Value>());
        self.st.new_list(items)
    }

    /// The hash of a string key with text `text`, charging for it.
    fn hash(&self, text: &str) -> Result<u64, Ctl> {
        self.meter.charge_bytes(text.len())?;
        Ok(str_hash(text))
    }

    /// The string of object key `key` (whose hash is `hash`): shared with
    /// earlier occurrences of the same key (strings are immutable).
    // A `Cow`, to keep a borrowed key borrowed in the table.
    #[allow(clippy::ptr_arg)]
    fn key(&mut self, key: &Cow<'a, str>, hash: u64) -> Value {
        if let Some(&(_, _, v)) = self.keys.find(hash, |e| e.1 == *key) {
            return v;
        }
        let v = self.st.new_str(&**key);
        if key.len() <= MAX_SHARED_KEY && self.keys.len() < MAX_SHARED_KEYS {
            self.pending += 2 * NODE + key.len();
            self.keys.insert_unique(hash, (hash, key.clone(), v), |e| e.0);
        }
        v
    }

    fn start_map(&self) -> MapBuild {
        MapBuild { base: self.entries.len(), index: None }
    }

    /// Adds the entry `key: value` to `m`, replacing the entry of an equal
    /// key. `text` is the key's text if it is a string, and `hash` its hash.
    fn put(&mut self, m: &mut MapBuild, hash: u64, text: Option<Cow<'a, str>>, key: Value, value: Value) -> Result<(), Ctl> {
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
                let mut index = KeyTable::default();
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

    /// Stages a map assembled with [`Build::put`].
    fn map(&mut self, m: MapBuild) -> Value {
        let n = self.entries.len() - m.base;
        let mut map = MapObj { entries: Vec::with_capacity(n), live: n, ..MapObj::default() };
        map.index.reserve(n);
        for (slot, e) in self.entries.drain(m.base..).enumerate() {
            map.index.insert(e.key, e.hash, slot);
            map.entries.push(Some((e.key, e.value)));
        }
        let index = m.index.map_or(0, |ix| ix.bytes());
        self.pending = self.pending.saturating_sub(n * ENTRY_PENDING + index);
        self.st.alloc(Obj::Map(Box::new(map)))
    }
}

/// Bytes counted per entry of a map being assembled.
const ENTRY_PENDING: usize = std::mem::size_of::<Entry>() + MAP_ENTRY_SIZE;

// ---------------------------------------------------------------------------
// Conversion between Sand and Rust `Json` values

impl NamedType for Json {
    const MODULE: &'static str = "json";
    const NAME: &'static str = "Json";
    const DOC: &'static str = "A dynamic JSON value. Plain integers that fit in a 64-bit signed `int` parse\n\
                               as `Int`; all other numbers parse as `Float`. Object entries preserve\n\
                               insertion order.";
    const PUBLIC: bool = true;
    fn binding() -> Binding<Json> {
        Binding::of_enum()
    }
}

impl SandType for Json {
    fn sand_type() -> Type {
        Type::bound::<Json>(Vec::new())
    }
}

impl EnumType for Json {
    const VARIANTS: &'static [EnumVariant] = &[
        EnumVariant { name: "Null", doc: "JSON `null`.", fields: &[] },
        EnumVariant { name: "Bool", doc: "JSON boolean (`true` or `false`).", fields: &[VariantField { name: None, ty: bool::sand_type }] },
        EnumVariant { name: "Int", doc: "Exact 64-bit signed integer.", fields: &[VariantField { name: None, ty: i64::sand_type }] },
        EnumVariant { name: "Float", doc: "IEEE-754 floating-point number.", fields: &[VariantField { name: None, ty: f64::sand_type }] },
        EnumVariant { name: "Str", doc: "UTF-8 string.", fields: &[VariantField { name: None, ty: String::sand_type }] },
        EnumVariant { name: "Arr", doc: "JSON array.", fields: &[VariantField { name: None, ty: Vec::<Json>::sand_type }] },
        EnumVariant {
            name: "Obj",
            doc: "Insertion-ordered JSON object.",
            fields: &[VariantField { name: None, ty: indexmap::IndexMap::<String, Json>::sand_type }],
        },
    ];

    fn index(&self) -> usize {
        match self {
            Json::Null => 0,
            Json::Bool(_) => 1,
            Json::Int(_) => 2,
            Json::Float(_) => 3,
            Json::Str(_) => 4,
            Json::Arr(_) => 5,
            Json::Obj(_) => 6,
        }
    }
}

impl FromValue for Json {
    type Item<'a> = Json;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Json, Ctl> {
        walk(&mut Reader { vm, ids: JsonIds::of(&vm.prog)?, conv }, v)
    }
}

impl IntoValue for Json {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        let ids = JsonIds::of(&vm.prog)?;
        let meter = &vm.meter;
        vm.heap.staged(|heap, st| walk(&mut Writer { b: Build::new(heap, st, meter), ids }, self))
    }

    fn to_const(&self) -> Option<Constant> {
        let payload = match self {
            Json::Null => None,
            Json::Bool(b) => Some(b.to_const()),
            Json::Int(i) => Some(i.to_const()),
            Json::Float(f) => Some(f.to_const()),
            Json::Str(s) => Some(s.to_const()),
            Json::Arr(items) => Some(items.to_const()),
            Json::Obj(fields) => Some(fields.to_const()),
        };
        variant_const::<Json>(self.index(), payload)
    }
}

/// A depth-first conversion of a tree of arrays and objects. [`walk`] keeps
/// the open containers in a `Vec`, so the stack use does not depend on the
/// depth of the tree.
trait Walk {
    type Node;
    /// A container whose children are being converted.
    type Frame;
    type Out;

    /// Converts a leaf, or opens a container.
    fn open(&mut self, node: Self::Node) -> Result<Step<Self::Frame, Self::Out>, Ctl>;
    /// The next child of `f`, or `None` after the last child.
    fn next(&mut self, f: &mut Self::Frame) -> Result<Option<Self::Node>, Ctl>;
    /// Adds the converted child that [`Walk::next`] returned last to `f`.
    fn put(&mut self, f: &mut Self::Frame, child: Self::Out) -> Result<(), Ctl>;
    fn close(&mut self, f: Self::Frame) -> Self::Out;
}

enum Step<F, O> {
    Open(F),
    Leaf(O),
}

/// Converts `root`, failing if it has more than [`MAX_DEPTH`] nested containers.
fn walk<W: Walk>(w: &mut W, root: W::Node) -> Result<W::Out, Ctl> {
    let mut top = match w.open(root)? {
        Step::Open(f) => f,
        Step::Leaf(out) => return Ok(out),
    };
    let mut parents = Vec::new();
    loop {
        match w.next(&mut top)? {
            Some(child) => match w.open(child)? {
                Step::Leaf(out) => w.put(&mut top, out)?,
                Step::Open(_) if parents.len() + 1 >= MAX_DEPTH => return fail("limit", "value too deeply nested to convert"),
                Step::Open(f) => parents.push(std::mem::replace(&mut top, f)),
            },
            None => {
                let out = w.close(top);
                match parents.pop() {
                    Some(parent) => {
                        top = parent;
                        w.put(&mut top, out)?;
                    }
                    None => return Ok(out),
                }
            }
        }
    }
}

/// Reads `json.Json` values on the heap into Rust [`Json`] values.
struct Reader<'h, 'c> {
    vm: &'h Vm,
    ids: JsonIds,
    conv: &'c mut Conversion,
}

enum ReadFrame<'h> {
    Arr(Vec<Json>, std::slice::Iter<'h, Value>),
    /// An object, the remaining entries, and the key of the entry being read.
    Obj(indexmap::IndexMap<String, Json>, std::slice::Iter<'h, Option<(Value, Value)>>, String),
}

/// Rust memory held per object entry: the entry and its slot in the hash index.
const OBJ_ENTRY: usize = std::mem::size_of::<(u64, String, Json)>() + std::mem::size_of::<usize>();

impl<'h> Reader<'h, '_> {
    fn string(&mut self, v: Value) -> Result<String, Ctl> {
        let s = match self.vm.heap.get(v) {
            Obj::Str(s) => &**s,
            Obj::StrBuf(s) => s.as_str(),
            _ => return Err(mismatch::<Json>()),
        };
        self.conv.alloc(self.vm, s.len())?;
        self.vm.charge_bytes(s.len())?;
        Ok(s.to_string())
    }
}

impl<'h> Walk for Reader<'h, '_> {
    type Node = Value;
    type Frame = ReadFrame<'h>;
    type Out = Json;

    fn open(&mut self, v: Value) -> Result<Step<ReadFrame<'h>, Json>, Ctl> {
        let (vm, ids) = (self.vm, self.ids);
        vm.charge(1)?;
        let id = vm.heap.variant_id(v);
        if id == ids.null {
            return Ok(Step::Leaf(Json::Null));
        }
        let payload = vm.heap.field(v, 0).ok_or_else(mismatch::<Json>)?;
        Ok(Step::Leaf(match (id, payload) {
            (_, Value::Bool(b)) if id == ids.bool_ => Json::Bool(b),
            (_, Value::Int(i)) if id == ids.int => Json::Int(i),
            (_, Value::Float(f)) if id == ids.float => Json::Float(f),
            _ if id == ids.str_ => Json::Str(self.string(payload)?),
            _ if id == ids.arr => {
                let Obj::List(items) = vm.heap.get(payload) else { return Err(mismatch::<Json>()) };
                self.conv.alloc(vm, items.len().saturating_mul(std::mem::size_of::<Json>()))?;
                vm.charge(items.len() as u64)?;
                return Ok(Step::Open(ReadFrame::Arr(Vec::with_capacity(items.len()), items.iter())));
            }
            _ if id == ids.obj => {
                let Obj::Map(m) = vm.heap.get(payload) else { return Err(mismatch::<Json>()) };
                self.conv.alloc(vm, m.live.saturating_mul(OBJ_ENTRY))?;
                vm.charge(m.live as u64)?;
                let fields = indexmap::IndexMap::with_capacity(m.live);
                return Ok(Step::Open(ReadFrame::Obj(fields, m.entries.iter(), String::new())));
            }
            _ => return Err(mismatch::<Json>()),
        }))
    }

    fn next(&mut self, f: &mut ReadFrame<'h>) -> Result<Option<Value>, Ctl> {
        Ok(match f {
            ReadFrame::Arr(_, items) => items.next().copied(),
            ReadFrame::Obj(_, entries, key) => match entries.find_map(|e| *e) {
                Some((k, v)) => {
                    *key = self.string(k)?;
                    Some(v)
                }
                None => None,
            },
        })
    }

    fn put(&mut self, f: &mut ReadFrame<'h>, child: Json) -> Result<(), Ctl> {
        match f {
            ReadFrame::Arr(items, _) => items.push(child),
            ReadFrame::Obj(fields, _, key) => {
                fields.insert(std::mem::take(key), child);
            }
        }
        Ok(())
    }

    fn close(&mut self, f: ReadFrame<'h>) -> Json {
        match f {
            ReadFrame::Arr(items, _) => Json::Arr(items),
            ReadFrame::Obj(fields, _, _) => Json::Obj(fields),
        }
    }
}

/// Stages Rust [`Json`] values as `json.Json` values.
struct Writer<'a, 's, 'h> {
    b: Build<'a, 's, 'h>,
    ids: JsonIds,
}

enum WriteFrame {
    Arr(std::vec::IntoIter<Json>, Vec<Value>),
    /// An object, the remaining entries, and the key and hash of the entry
    /// being written.
    Obj(indexmap::map::IntoIter<String, Json>, MapBuild, String, u64),
}

impl Walk for Writer<'_, '_, '_> {
    type Node = Json;
    type Frame = WriteFrame;
    type Out = Value;

    fn open(&mut self, j: Json) -> Result<Step<WriteFrame, Value>, Ctl> {
        let (b, ids) = (&mut self.b, self.ids);
        b.reserve()?;
        b.meter.charge(1)?;
        Ok(Step::Leaf(match j {
            Json::Null => Value::Tag(ids.null),
            Json::Bool(x) => b.st.new_variant(ids.bool_, &[Value::Bool(x)]),
            Json::Int(i) => b.st.new_variant(ids.int, &[Value::Int(i)]),
            Json::Float(f) => b.st.new_variant(ids.float, &[Value::Float(f)]),
            Json::Str(s) => {
                b.st.check_memory((NODE + b.pending).saturating_add(s.len()))?;
                b.meter.charge_bytes(s.len())?;
                let s = b.st.new_str(s);
                b.st.new_variant(ids.str_, &[s])
            }
            Json::Arr(items) => return Ok(Step::Open(WriteFrame::Arr(items.into_iter(), Vec::new()))),
            Json::Obj(fields) => return Ok(Step::Open(WriteFrame::Obj(fields.into_iter(), b.start_map(), String::new(), 0))),
        }))
    }

    fn next(&mut self, f: &mut WriteFrame) -> Result<Option<Json>, Ctl> {
        Ok(match f {
            WriteFrame::Arr(items, _) => items.next(),
            WriteFrame::Obj(entries, _, key, hash) => match entries.next() {
                Some((k, v)) => {
                    *hash = self.b.hash(&k)?;
                    *key = k;
                    Some(v)
                }
                None => None,
            },
        })
    }

    fn put(&mut self, f: &mut WriteFrame, child: Value) -> Result<(), Ctl> {
        match f {
            WriteFrame::Arr(_, out) => self.b.push(out, child),
            WriteFrame::Obj(_, m, key, hash) => {
                let text = Cow::Owned(std::mem::take(key));
                let k = self.b.key(&text, *hash);
                self.b.put(m, *hash, Some(text), k, child)
            }
        }
    }

    fn close(&mut self, f: WriteFrame) -> Value {
        let ids = self.ids;
        match f {
            WriteFrame::Arr(_, out) => {
                let list = self.b.list(out);
                self.b.st.new_variant(ids.arr, &[list])
            }
            WriteFrame::Obj(_, m, _, _) => {
                let map = self.b.map(m);
                self.b.st.new_variant(ids.obj, &[map])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing

/// A JSON number, kept as text so integers convert exactly.
struct Number<'a> {
    text: &'a str,
    is_int: bool,
}

impl Number<'_> {
    fn float(&self) -> f64 {
        self.text.parse().unwrap_or(f64::NAN)
    }

    /// The number as a `json.Json` payload: an `int` if it is written without
    /// a fraction or exponent and fits (`-0` stays a float, keeping its sign).
    fn value(&self) -> Value {
        match self.text.parse::<i64>() {
            Ok(i) if self.is_int && !(i == 0 && self.text.starts_with('-')) => Value::Int(i),
            _ => Value::Float(self.float()),
        }
    }

    /// The value if it is integral and fits in an `int`.
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
    /// Input consumed up to here has been charged to the meter.
    charged: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Parser<'a> {
        Parser { src, b: src.as_bytes(), pos: 0, depth: 0, charged: 0 }
    }

    /// Charges one unit per value plus the input consumed since the last
    /// charge, so that parsing huge documents costs fuel and can be stopped.
    fn charge(&mut self, meter: &Meter) -> Result<(), Ctl> {
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

    /// An error for whatever is at the current position.
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

    /// Requires the end of input (after whitespace).
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

    /// A string literal; the current byte is the opening quote. Borrowed
    /// from the input unless it has escapes; with `keep` false the text is
    /// only validated (the result is then meaningless).
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

    /// Skips a value without building it (still validating its syntax).
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

    /// Parses a value into a `json.Json`. Every `Json` value is a tag or an
    /// inline variant, so only its payload's objects are staged.
    fn parse_json(&mut self, b: &mut Build<'a, '_, '_>, ids: &JsonIds) -> PResult<Value> {
        self.ws();
        self.charge(b.meter)?;
        b.reserve()?;
        Ok(match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                Value::Tag(ids.null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                b.st.new_variant(ids.bool_, &[Value::Bool(true)])
            }
            Some(b'f') => {
                self.literal(b"false")?;
                b.st.new_variant(ids.bool_, &[Value::Bool(false)])
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

/// Iteration over arrays and objects, shared by the untyped parser and the
/// typed decoder (whose callbacks need the decoder's state).
trait Source<'a>: Sized {
    fn parser(&mut self) -> &mut Parser<'a>;

    /// Parses `[v, ...]`, calling `f` with the parser before each element.
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

    /// Parses `{"key": v, ...}`, calling `f` with each key (meaningless
    /// unless `keys`) before its value.
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

// ---------------------------------------------------------------------------
// Typed decoding

enum Seg<'a> {
    Key(Cow<'a, str>),
    Index(usize),
}

/// Structs with more fields than this find them through a hash index.
const INDEXED_FIELDS: usize = 8;

/// Decodes JSON text into values described by a [`TypeDesc`].
struct Decoder<'a, 'd, 's, 'h> {
    p: Parser<'a>,
    b: Build<'a, 's, 'h>,
    ids: crate::vm::bytecode::RtIds,
    /// Current JSON path for diagnostic messages.
    path: Vec<Seg<'a>>,
    /// Active enclosing struct and enum descriptors used to resolve recursive references.
    stack: Vec<&'d TypeDesc>,
    /// Decoded field values for in-progress structs.
    slots: Vec<Option<Value>>,
    /// Field name lookup tables for large structs, keyed by struct instance ID.
    indexes: FxHashMap<u32, FxHashMap<&'d str, usize>>,
    /// Staged sets and maps to populate after their elements are committed to the heap.
    fills: Vec<Fill>,
    /// Deferred thunks that evaluate computed field defaults and populate sets (see [`Decoded::Pending`]).
    thunks: Vec<Value>,
}

/// Deferred elements or entries to insert into a staged set or map after heap allocation.
enum Fill {
    Set(Value, Vec<Value>),
    Map(Value, Vec<(Value, Value)>),
}

impl<'a> Source<'a> for Decoder<'a, '_, '_, '_> {
    fn parser(&mut self) -> &mut Parser<'a> {
        &mut self.p
    }
}

/// Names compared per fuel unit when looking up a key among the fields of a
/// struct or the variants of an enum.
const LOOKUPS_PER_UNIT: usize = 16;

/// The position of the first of `items` that `is` accepts, charging for the
/// comparisons: a type may have thousands of fields, and a document may
/// repeat a key that is not among them any number of times.
fn lookup<T>(meter: &Meter, items: &[T], is: impl Fn(&T) -> bool) -> Result<Option<usize>, Ctl> {
    meter.charge(1 + (items.len() / LOOKUPS_PER_UNIT) as u64)?;
    Ok(items.iter().position(is))
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

    /// The error for a value of the wrong kind: a syntax error if there is no
    /// value at all, otherwise `expected <what>`.
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

    fn decode(&mut self, t: &'d TypeDesc) -> PResult<Value> {
        self.p.ws();
        self.p.charge(self.b.meter)?;
        self.b.reserve()?;
        match t {
            TypeDesc::Int => Ok(Value::Int(self.int()?)),
            TypeDesc::Duration => Ok(Value::Dur(self.int()?)),
            TypeDesc::Float => Ok(Value::Float(self.number("float")?.float())),
            TypeDesc::Bool => Ok(Value::Bool(self.bool()?)),
            TypeDesc::Unit => match self.p.peek() {
                Some(b'n') => self.p.literal(b"null").map(|()| Value::Unit),
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
                // A loop, not recursion: the layers of `T??` read no JSON
                // nesting, so the nesting limit does not bound them.
                let (mut inner, mut layers) = (t, 0);
                while let TypeDesc::Option(e) = inner {
                    (inner, layers) = (e, layers + 1);
                }
                self.b.meter.charge(layers)?;
                if self.p.peek() == Some(b'n') {
                    self.p.literal(b"null")?;
                    return Ok(Value::Tag(self.ids.none));
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
                    // Defer set insertion until element field defaults have been evaluated.
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
                    None => Err(Error::Ctl(Ctl::internal("json.decode: unresolved recursive type"))),
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
            TypeDesc::Param(_) => Err(Error::Ctl(Ctl::internal("json.decode: a type parameter without a description"))),
        }
    }

    fn decode_struct(&mut self, t: &'d TypeDesc, id: u32, def: u32, fields: &'d [FieldDesc]) -> PResult<Value> {
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
                    Value::Unit
                }
                (None, FieldDefault::None, TypeDesc::Option(_)) => Value::Tag(self.ids.none),
                (None, FieldDefault::None, _) => {
                    self.slots.truncate(base);
                    return Err(self.fail(&format!("missing field `{field}`")));
                }
            });
        }
        self.slots.truncate(base);
        let s = self.b.st.alloc(Obj::Struct(def, values.into_boxed_slice()));
        for (fill, mask) in computed {
            self.thunk(fill, Box::new([s, Value::Int(mask)]))?;
        }
        Ok(s)
    }

    fn thunk(&mut self, proto: u32, captures: Box<[Value]>) -> PResult<()> {
        self.b.reserve()?;
        let c = self.b.st.alloc(Obj::Closure(proto, captures));
        Ok(self.b.push(&mut self.thunks, c)?)
    }

    fn constant(&mut self, c: &ConstVal) -> PResult<Value> {
        self.b.meter.charge(1)?;
        self.b.reserve()?;
        let all = |d: &mut Self, xs: &[ConstVal]| xs.iter().map(|x| d.constant(x)).collect::<PResult<Vec<_>>>();
        Ok(match c {
            ConstVal::Unit => Value::Unit,
            ConstVal::Bool(b) => Value::Bool(*b),
            ConstVal::Int(i) => Value::Int(*i),
            ConstVal::Float(f) => Value::Float(*f),
            ConstVal::Dur(d) => Value::Dur(*d),
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

    fn decode_range(&mut self) -> PResult<Value> {
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

    /// The field of struct `id` named `key`: `guess` (the field after the
    /// last one found, as documents usually list fields in order) first,
    /// then by an index (large structs) or a scan.
    fn field_index(&mut self, id: u32, fields: &'d [FieldDesc], key: &str, guess: usize) -> Result<Option<usize>, Ctl> {
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

    /// Converts an object key to a map key of type `t`, with its text if it
    /// is a string.
    fn map_key(&mut self, t: &TypeDesc, key: Cow<'a, str>) -> PResult<(Value, u64, Option<Cow<'a, str>>)> {
        let quoted = super::truncate(&key, QUOTED_KEY);
        let bad = |d: &Self| d.fail(&format!("invalid map key `{quoted}`"));
        let k = match t {
            TypeDesc::Str => {
                let hash = self.b.hash(&key)?;
                let k = self.b.key(&key, hash);
                return Ok((k, hash, Some(key)));
            }
            TypeDesc::Int => Value::Int(key.parse().map_err(|_| bad(self))?),
            TypeDesc::Duration => Value::Dur(key.parse().map_err(|_| bad(self))?),
            TypeDesc::Bool => match &*key {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(bad(self)),
            },
            TypeDesc::Enum { variants, .. } => match lookup(self.b.meter, variants, |v| v.0 == *key && v.2.is_empty())? {
                Some(i) => Value::Tag(variants[i].1),
                None => return Err(bad(self)),
            },
            TypeDesc::Value { parse, .. } => {
                self.b.meter.charge_bytes(key.len())?;
                let v = parse(&key).map_err(|msg| self.fail(&format!("invalid map key `{quoted}`: {msg}")))?;
                let Some(x) = v.value() else { return Err(Error::Ctl(Ctl::internal("json.decode: not a value"))) };
                // Staged object keys are deduplicated by their canonical text.
                let (hash, text) = (x.value_hash(), x.text());
                return Ok((self.b.st.alloc(v), hash, Some(Cow::Owned(text))));
            }
            _ => return Err(self.unsupported(&format!("map keys of type {}", self.type_name(t)))),
        };
        Ok((k, hash_key(self.b.heap, self.b.meter, k)?, None))
    }

    /// An enum: `"Variant"` for payload-less variants, otherwise
    /// `{"Variant": [payload...]}` or `{"Variant": {"field": ...}}`.
    fn decode_enum(&mut self, name: &str, variants: &'d [VariantDesc]) -> PResult<Value> {
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
                Ok(Value::Tag(v.1))
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

    fn decode_payload(&mut self, v: &'d VariantDesc) -> PResult<Value> {
        let (_, id, types, names) = v;
        self.p.ws();
        let mut slots: Vec<Option<Value>> = vec![None; types.len()];
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
                        *slot = Some(Value::Tag(self.ids.none));
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
        let fields: Vec<Value> = slots.into_iter().map(|s| s.unwrap_or(Value::Unit)).collect();
        Ok(self.b.st.new_variant(*id, &fields))
    }
}

// ---------------------------------------------------------------------------
// Encoding

/// Writes the JSON text of values on the heap (spec §13.7), compact or
/// pretty-printed.
struct Encoder<'v> {
    prog: &'v Program,
    heap: &'v Heap,
    meter: &'v Meter,
    ids: JsonIds,
    indent: usize,
    out: String,
    /// Output up to here has been charged to the meter.
    charged: usize,
    scratch: Scratch,
}

impl Encoder<'_> {
    /// Charges one unit per value plus the output written since the last
    /// charge, and checks the output buffer against the memory limit.
    fn step(&mut self) -> Result<(), Ctl> {
        let written = self.out.len() - self.charged;
        self.charged = self.out.len();
        self.meter.charge(1 + (written / BYTES_PER_UNIT) as u64)?;
        self.scratch.check(self.heap, self.out.capacity())
    }

    fn newline(&mut self, level: usize) -> Result<(), Ctl> {
        if self.indent > 0 {
            let n = self.indent.saturating_mul(level);
            self.scratch.check(self.heap, self.out.len().saturating_add(n))?;
            self.out.push('\n');
            self.out.extend(std::iter::repeat_n(' ', n));
        }
        Ok(())
    }

    fn string(&mut self, s: &str) -> Result<(), Ctl> {
        if s.len() > 1024 && self.heap.would_exceed(self.out.len().saturating_add(escaped_len(s))) {
            return Err(Ctl::limit(crate::LimitKind::Memory));
        }
        write_str(&mut self.out, s);
        Ok(())
    }

    /// Writes `[items...]` (`len` items).
    fn array(&mut self, len: usize, items: impl Iterator<Item = Value>, level: usize) -> Result<(), Ctl> {
        if level >= MAX_DEPTH {
            return fail("limit", "value too deeply nested to encode as JSON");
        }
        if len == 0 {
            self.out.push_str("[]");
            return Ok(());
        }
        self.out.push('[');
        for (i, x) in items.enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            self.newline(level + 1)?;
            self.value(x, level + 1)?;
        }
        self.newline(level)?;
        self.out.push(']');
        Ok(())
    }

    /// Writes `{"key": value, ...}` (`len` entries); `entry` writes the key
    /// of an entry and returns its value.
    fn object<T>(
        &mut self,
        len: usize,
        entries: impl Iterator<Item = T>,
        level: usize,
        mut entry: impl FnMut(&mut Self, T) -> Result<Value, Ctl>,
    ) -> Result<(), Ctl> {
        if level >= MAX_DEPTH {
            return fail("limit", "value too deeply nested to encode as JSON");
        }
        if len == 0 {
            self.out.push_str("{}");
            return Ok(());
        }
        self.out.push('{');
        for (i, e) in entries.enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            self.newline(level + 1)?;
            let v = entry(self, e)?;
            self.out.push_str(if self.indent > 0 { ": " } else { ":" });
            self.value(v, level + 1)?;
        }
        self.newline(level)?;
        self.out.push('}');
        Ok(())
    }

    fn value(&mut self, mut v: Value, level: usize) -> Result<(), Ctl> {
        self.step()?;
        let (prog, heap) = (self.prog, self.heap);
        // `Some(x)` encodes as `x`.
        while heap.variant_id(v) == prog.ids.some {
            self.meter.charge(1)?;
            v = heap.field(v, 0).unwrap_or(Value::Unit);
        }
        match v {
            Value::Unit | Value::Uninit => self.out.push_str("null"),
            Value::Bool(b) => self.out.push_str(if b { "true" } else { "false" }),
            Value::Int(i) | Value::Dur(i) => {
                let _ = write!(self.out, "{i}");
            }
            Value::Float(f) => write_float(&mut self.out, f),
            Value::Pair(a, b) => self.array(2, [Value::Int(a as i64), Value::Int(b)].into_iter(), level)?,
            Value::Tag(id) if id == prog.ids.none || id == self.ids.null => self.out.push_str("null"),
            Value::Tag(id) => self.string(&prog.variants[id as usize].name)?,
            Value::Obj(_) => match heap.get(v) {
                Obj::Str(s) => self.string(s)?,
                Obj::StrBuf(s) => self.string(s)?,
                Obj::Bytes(b) => {
                    if heap.would_exceed(self.out.len().saturating_add(b.len() / 3 * 4 + 4)) {
                        return Err(Ctl::limit(crate::LimitKind::Memory));
                    }
                    self.meter.charge_slow_bytes(b.len())?;
                    self.out.push('"');
                    self.out.push_str(&base64::encode(b, false));
                    self.out.push('"');
                }
                Obj::List(items) => self.array(items.len(), items.iter().copied(), level)?,
                Obj::Tuple(items) => self.array(items.len(), items.iter().copied(), level)?,
                Obj::Set(s) => self.array(s.live, s.entries.iter().flatten().copied(), level)?,
                Obj::Range(a, b, inclusive) => {
                    let fields = [("start", Value::Int(*a)), ("end", Value::Int(*b)), ("inclusive", Value::Bool(true))];
                    let n = if *inclusive { 3 } else { 2 };
                    self.object(n, fields[..n].iter(), level, |e, &(k, v)| {
                        e.string(k)?;
                        Ok(v)
                    })?
                }
                Obj::Map(m) => self.object(m.live, m.entries.iter().flatten(), level, |e, &(k, v)| {
                    e.string(&format_map_key(prog, heap, k)?)?;
                    Ok(v)
                })?,
                Obj::Struct(def, fields) => {
                    let names = &prog.types[*def as usize].fields;
                    self.object(fields.len().min(names.len()), names.iter().zip(fields.iter()), level, |e, (n, &v)| {
                        e.string(n)?;
                        Ok(v)
                    })?
                }
                Obj::Variant(id, fields) => self.variant(*id, fields, level)?,
                o @ (Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) | Obj::Task(_) | Obj::Channel(_) | Obj::Native(_)) => {
                    return Err(without_trait(o.kind_name(), "encoded as JSON", "Encode"));
                }
                o @ (Obj::Value(_) | Obj::Packed(..)) => self.string(&o.value().map(ValueRef::text).unwrap_or_default())?,
                o @ (Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_)) => {
                    return Err(without_trait(o.kind_name(), "encoded as JSON", "Encode"));
                }
            },
            _ => {
                let (id, payload) = v.as_inline_variant().unwrap_or((u32::MAX, Value::Unit));
                self.variant(id, &[payload], level)?
            }
        }
        Ok(())
    }

    fn variant(&mut self, id: u32, fields: &[Value], level: usize) -> Result<(), Ctl> {
        let Some(rt) = self.prog.variants.get(id as usize) else { return internal("json.encode: a variant id without a variant") };
        if self.ids.has(id) {
            return self.value(fields.first().copied().unwrap_or(Value::Unit), level);
        }
        if level >= MAX_DEPTH {
            return fail("limit", "value too deeply nested to encode as JSON");
        }
        self.out.push('{');
        self.newline(level + 1)?;
        self.string(&rt.name)?;
        self.out.push_str(if self.indent > 0 { ": " } else { ":" });
        self.step()?;
        match &rt.field_names {
            Some(names) => self.object(fields.len().min(names.len()), names.iter().zip(fields.iter()), level + 1, |e, (n, &v)| {
                e.string(n)?;
                Ok(v)
            })?,
            None => self.array(fields.len(), fields.iter().copied(), level + 1)?,
        }
        self.newline(level)?;
        self.out.push('}');
        Ok(())
    }
}

/// Formats finite floats with a fraction or exponent (`2.0`, `1e20`) and non-finite floats as `null`.
fn write_float(out: &mut String, f: f64) {
    if f.is_finite() {
        let _ = write!(out, "{f:?}");
    } else {
        out.push_str("null");
    }
}

fn json_number(st: &mut Staging, ids: &JsonIds, n: Value) -> Value {
    let id = if matches!(n, Value::Int(_)) { ids.int } else { ids.float };
    st.new_variant(id, &[n])
}

/// Formats a map key `k` as a JSON object key string, or fails with kind `"invalid"` if `k` cannot be represented as a JSON key.
fn format_map_key<'v>(prog: &'v Program, heap: &'v Heap, k: Value) -> Result<Cow<'v, str>, Ctl> {
    let bad = || fail("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type");
    Ok(match k {
        Value::Int(i) | Value::Dur(i) => Cow::Owned(i.to_string()),
        Value::Bool(b) => Cow::Borrowed(if b { "true" } else { "false" }),
        Value::Tag(id) if id != prog.ids.none => Cow::Borrowed(prog.variants[id as usize].name.as_str()),
        Value::Obj(_) => match heap.get(k) {
            Obj::Str(s) => Cow::Borrowed(&**s),
            o @ (Obj::Value(_) | Obj::Packed(..)) => Cow::Owned(o.value().map(ValueRef::text).unwrap_or_default()),
            _ => return bad(),
        },
        _ => return bad(),
    })
}

/// The length of `s` as written by [`write_str`].
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
    out.push('"');
}

// ---------------------------------------------------------------------------
// Conversion to `Json` values

/// Converts values on the heap to `json.Json` values (staged). Strings are
/// shared, not copied: `Json.Str(s)` holds the converted string itself.
struct Converter<'a, 's, 'h> {
    prog: &'a Program,
    heap: &'a Heap,
    b: Build<'a, 's, 'h>,
    ids: JsonIds,
    /// The strings of the field names of each struct converted, by definition.
    fields: FxHashMap<u32, Vec<Value>>,
    /// The strings of the names of payload-less variants converted.
    names: FxHashMap<u32, Value>,
}

impl<'a> Converter<'a, '_, '_> {
    fn convert(&mut self, mut v: Value, depth: usize) -> NResult {
        let (prog, heap, ids) = (self.prog, self.heap, self.ids);
        self.b.reserve()?;
        self.b.meter.charge(1)?;
        loop {
            let id = heap.variant_id(v);
            // `Json` values are shared.
            if ids.has(id) {
                return Ok(v);
            }
            // `Some(x)` converts as `x`.
            if id != prog.ids.some {
                break;
            }
            self.b.meter.charge(1)?;
            v = heap.field(v, 0).unwrap_or(Value::Unit);
        }
        Ok(match v {
            Value::Unit | Value::Uninit => Value::Tag(ids.null),
            Value::Bool(b) => self.b.st.new_variant(ids.bool_, &[Value::Bool(b)]),
            Value::Int(i) | Value::Dur(i) => json_number(self.b.st, &ids, Value::Int(i)),
            Value::Float(_) => json_number(self.b.st, &ids, v),
            Value::Pair(a, b) => self.array([Value::Int(a as i64), Value::Int(b)].into_iter(), depth)?,
            Value::Tag(id) if id == prog.ids.none => Value::Tag(ids.null),
            Value::Tag(id) => {
                let name = self.variant_name(id);
                self.b.st.new_variant(ids.str_, &[name])
            }
            Value::Obj(_) => match heap.get(v) {
                Obj::Str(_) => self.b.st.new_variant(ids.str_, &[v]),
                Obj::StrBuf(s) => {
                    self.b.meter.charge_bytes(s.len())?;
                    let s = self.b.st.new_str(s.as_str());
                    self.b.st.new_variant(ids.str_, &[s])
                }
                Obj::Bytes(b) => {
                    self.b.st.check_memory(self.b.pending.saturating_add(b.len() / 3 * 4 + 4))?;
                    self.b.meter.charge_slow_bytes(b.len())?;
                    let s = self.b.st.new_str(base64::encode(b, false));
                    self.b.st.new_variant(ids.str_, &[s])
                }
                Obj::List(items) => self.array(items.iter().copied(), depth)?,
                Obj::Tuple(items) => self.array(items.iter().copied(), depth)?,
                Obj::Set(s) => self.array(s.entries.iter().flatten().copied(), depth)?,
                Obj::Range(a, b, inclusive) => {
                    self.check_depth(depth)?;
                    let mut fields = vec![
                        ("start", json_number(self.b.st, &ids, Value::Int(*a))),
                        ("end", json_number(self.b.st, &ids, Value::Int(*b))),
                    ];
                    if *inclusive {
                        fields.push(("inclusive", self.b.st.new_variant(ids.bool_, &[Value::Bool(true)])));
                    }
                    let mut out = self.b.start_map();
                    for (name, x) in fields {
                        let k = self.b.st.new_str(name);
                        self.put(&mut out, k, Cow::Borrowed(name), x)?;
                    }
                    let map = self.b.map(out);
                    self.b.st.new_variant(ids.obj, &[map])
                }
                Obj::Map(m) => {
                    self.check_depth(depth)?;
                    let mut out = self.b.start_map();
                    for &(k, x) in m.entries.iter().flatten() {
                        let x = self.convert(x, depth + 1)?;
                        let (k, text) = match heap.get(k) {
                            Obj::Str(s) => (k, Cow::Borrowed(&**s)),
                            _ => {
                                let key = format_map_key(prog, heap, k)?;
                                (self.b.st.new_str(&*key), key)
                            }
                        };
                        self.put(&mut out, k, text, x)?;
                    }
                    let map = self.b.map(out);
                    self.b.st.new_variant(ids.obj, &[map])
                }
                Obj::Struct(def, values) => {
                    self.check_depth(depth)?;
                    let names = &prog.types[*def as usize].fields;
                    if !self.fields.contains_key(def) {
                        let strs = names.iter().map(|n| self.b.st.new_str(n.as_str())).collect();
                        self.fields.insert(*def, strs);
                    }
                    let mut out = self.b.start_map();
                    for (i, (name, &x)) in names.iter().zip(values.iter()).enumerate() {
                        let x = self.convert(x, depth + 1)?;
                        let k = self.fields[def][i];
                        self.put(&mut out, k, Cow::Borrowed(name.as_str()), x)?;
                    }
                    let map = self.b.map(out);
                    self.b.st.new_variant(ids.obj, &[map])
                }
                Obj::Variant(id, fields) => self.variant(*id, fields, depth)?,
                o @ (Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) | Obj::Task(_) | Obj::Channel(_) | Obj::Native(_)) => {
                    return Err(without_trait(o.kind_name(), "encoded as JSON", "Encode"));
                }
                o @ (Obj::Value(_) | Obj::Packed(..)) => {
                    let s = o.value().map(ValueRef::text).unwrap_or_default();
                    self.b.meter.charge_bytes(s.len())?;
                    let s = self.b.st.new_str(s);
                    self.b.st.new_variant(ids.str_, &[s])
                }
                o @ (Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_)) => {
                    return Err(without_trait(o.kind_name(), "encoded as JSON", "Encode"));
                }
            },
            _ => {
                let (id, payload) = v.as_inline_variant().unwrap_or((u32::MAX, Value::Unit));
                self.variant(id, &[payload], depth)?
            }
        })
    }

    /// Adds the entry `key: value` to `m`, where `key` is a string with text `text`.
    fn put(&mut self, m: &mut MapBuild, key: Value, text: Cow<'a, str>, value: Value) -> Result<(), Ctl> {
        let hash = self.b.hash(&text)?;
        self.b.put(m, hash, Some(text), key, value)
    }

    fn check_depth(&self, depth: usize) -> Result<(), Ctl> {
        if depth >= MAX_DEPTH { fail("limit", "value too deeply nested to convert to JSON") } else { Ok(()) }
    }

    /// The string of the name of variant `id` (one per call).
    fn variant_name(&mut self, id: u32) -> Value {
        let prog = self.prog;
        *self.names.entry(id).or_insert_with(|| self.b.st.new_str(prog.variants[id as usize].name.as_str()))
    }

    fn array(&mut self, items: impl Iterator<Item = Value>, depth: usize) -> NResult {
        self.check_depth(depth)?;
        let mut out = Vec::new();
        for x in items {
            let x = self.convert(x, depth + 1)?;
            self.b.push(&mut out, x)?;
        }
        let list = self.b.list(out);
        Ok(self.b.st.new_variant(self.ids.arr, &[list]))
    }

    /// A variant with a payload: `{"Name": payload}`.
    fn variant(&mut self, id: u32, fields: &[Value], depth: usize) -> NResult {
        let (prog, ids) = (self.prog, self.ids);
        let Some(rt) = prog.variants.get(id as usize) else { return internal("json.encode: a variant id without a variant") };
        self.check_depth(depth)?;
        let payload = match &rt.field_names {
            Some(names) => {
                let mut out = self.b.start_map();
                for (name, &x) in names.iter().zip(fields) {
                    let x = self.convert(x, depth + 2)?;
                    let k = self.b.st.new_str(name.as_str());
                    self.put(&mut out, k, Cow::Borrowed(name.as_str()), x)?;
                }
                let map = self.b.map(out);
                self.b.st.new_variant(ids.obj, &[map])
            }
            None => self.array(fields.iter().copied(), depth + 1)?,
        };
        let name = self.variant_name(id);
        let mut out = self.b.start_map();
        self.put(&mut out, name, Cow::Borrowed(rt.name.as_str()), payload)?;
        let map = self.b.map(out);
        Ok(self.b.st.new_variant(ids.obj, &[map]))
    }
}
