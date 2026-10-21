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
use crate::engine::Registry;
use crate::types::ConstVal;
use crate::vm::bytecode::{JsonIds, Program};
use crate::vm::compare::{hash_key, str_hash};
use crate::vm::heap::{Heap, Staging};
use crate::vm::meter::{BYTES_PER_UNIT, Meter, SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{FieldDesc, FxHashMap, KeyTable, MAP_ENTRY_SIZE, MapObj, Obj, TypeDesc, Value, ValueRef, VariantDesc, slot_fits};
use crate::vm::{Ctl, NResult, Vm, fail, internal};
use hashbrown::HashTable;
use std::borrow::Cow;
use std::fmt::Write;

/// Maximum nesting of arrays and objects, both when parsing and printing.
const MAX_DEPTH: usize = 512;

/// Approximate heap cost of one value built (a string, list, map or struct
/// header), checked against the memory limit before each.
const NODE: usize = 64;

pub fn register(r: &mut Registry) {
    r.source("json", ModKind::Std, include_str!("json.sand"));
    r.add_builtin("json.parse", |vm, a| {
        let ids = json_ids(vm)?;
        let (text, meter) = (a[0], &vm.meter);
        let r = vm.heap.staged(|heap, st| {
            let mut b = Build::new(heap, st, meter);
            let mut p = Parser::new(heap.str(text));
            let r = p.parse_json(&mut b, &ids).and_then(|v| p.finish().map(|()| v));
            p.charge(meter)?;
            r
        });
        into_result(vm, r)
    });
    r.add_builtin("json.encode", |vm, a| {
        let ids = json_ids(vm)?;
        let indent = a[1].as_int().clamp(0, 1 << 16) as usize;
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
        e.value(a[0], 0)?;
        let out = e.out;
        Ok(vm.new_str(out))
    });
    r.add_builtin("json.decode", |vm, a| {
        let Obj::TypeDesc(desc) = vm.heap.get(a[0]) else { return internal("json.decode: missing type description") };
        let desc = desc.clone();
        let ids = vm.prog.ids;
        let (text, meter) = (a[1], &vm.meter);
        let (r, sets) = vm.heap.staged(|heap, st| {
            let mut d = Decoder {
                p: Parser::new(heap.str(text)),
                b: Build::new(heap, st, meter),
                ids,
                path: Vec::new(),
                stack: Vec::new(),
                slots: Vec::new(),
                indexes: FxHashMap::default(),
                sets: Vec::new(),
            };
            let r = d.decode(&desc).and_then(|v| d.p.finish().map(|()| v));
            let charged = d.p.charge(meter);
            (charged.map_err(Error::Ctl).and(r), d.sets)
        });
        // Sets are filled once their elements are on the heap (to hash them).
        if r.is_ok() {
            for (set, items) in sets {
                for x in items {
                    vm.set_add(set, x)?;
                }
            }
        }
        into_result(vm, r)
    });
    r.add_builtin("json.to_json", |vm, a| {
        let ids = json_ids(vm)?;
        let (prog, meter) = (&vm.prog, &vm.meter);
        vm.heap.staged(|heap, st| {
            let mut c =
                Converter { prog, heap, b: Build::new(heap, st, meter), ids, fields: FxHashMap::default(), names: FxHashMap::default() };
            c.convert(a[0], 0)
        })
    });
}

fn json_ids(vm: &Vm) -> Result<JsonIds, Ctl> {
    match vm.prog.ids.json {
        Some(ids) => Ok(ids),
        None => internal("the json module is not loaded"),
    }
}

/// Failure while parsing or decoding.
enum Error {
    /// Invalid JSON or a mismatch with the target type: becomes `Err(Error { kind: "parse" })`.
    Parse(String),
    /// Aborts the native call (memory limit, error).
    Ctl(Ctl),
}

impl From<Ctl> for Error {
    fn from(c: Ctl) -> Self {
        Error::Ctl(c)
    }
}

type PResult<T> = Result<T, Error>;

fn into_result(vm: &mut Vm, r: PResult<Value>) -> NResult {
    match r {
        Ok(v) => Ok(vm.ok(v)),
        Err(Error::Parse(msg)) => Err(Ctl::native_err("parse", msg)),
        Err(Error::Ctl(c)) => Err(c),
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
        self.st.reserve(NODE + self.pending)
    }

    /// Appends `v` to a list being assembled, checking its buffer's growth
    /// against the memory limit first: elements that are inline values
    /// (numbers, booleans, `null`) stage no object, so the buffer would
    /// otherwise grow unchecked until the list is complete.
    fn push(&mut self, items: &mut Vec<Value>, v: Value) -> Result<(), Ctl> {
        if items.len() == items.capacity() {
            let more = items.capacity().max(4);
            self.st.reserve((NODE + self.pending).saturating_add(more.saturating_mul(2 * std::mem::size_of::<Value>())))?;
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

/// Decodes JSON text directly into values of a described type.
struct Decoder<'a, 'd, 's, 'h> {
    p: Parser<'a>,
    b: Build<'a, 's, 'h>,
    ids: crate::vm::bytecode::RtIds,
    /// Location of the value being decoded, for error messages.
    path: Vec<Seg<'a>>,
    /// Enclosing struct and enum types, to resolve recursive references.
    stack: Vec<&'d TypeDesc>,
    /// The fields of the structs being decoded.
    slots: Vec<Option<Value>>,
    /// Field indexes of large structs, by struct instance id.
    indexes: FxHashMap<u32, FxHashMap<&'d str, usize>>,
    /// Staged sets and their elements, added once these are on the heap.
    sets: Vec<(Value, Vec<Value>)>,
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

fn is_ident(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

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
                    s.push_str(k);
                }
                Seg::Key(k) => {
                    s.push('[');
                    write_str(&mut s, k);
                    s.push(']');
                }
            }
        }
        s
    }

    fn fail(&self, msg: &str) -> Error {
        Error::Parse(format!("{}: {msg}", self.path_str()))
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
            TypeDesc::Bool => match self.p.peek() {
                Some(b't') => self.p.literal(b"true").map(|()| Value::Bool(true)),
                Some(b'f') => self.p.literal(b"false").map(|()| Value::Bool(false)),
                _ => Err(self.mismatch("bool")),
            },
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
            TypeDesc::Option(inner) => {
                if self.p.peek() == Some(b'n') {
                    self.p.literal(b"null")?;
                    Ok(Value::Tag(self.ids.none))
                } else {
                    let v = self.decode(inner)?;
                    Ok(self.b.st.new_variant(self.ids.some, &[v]))
                }
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
            TypeDesc::Set(elem) => {
                self.expect_open(b'[', "array")?;
                let mut items = Vec::new();
                self.array(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(elem))?;
                    Ok(d.b.push(&mut items, v)?)
                })?;
                // The elements' buffer stays pending until the set is filled.
                let set = self.b.st.alloc(Obj::Set(Box::default()));
                self.sets.push((set, items));
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
            TypeDesc::Struct { id, def, fields, .. } => {
                self.expect_open(b'{', "object")?;
                self.stack.push(t);
                let base = self.slots.len();
                self.slots.resize(base + fields.len(), None);
                let mut next = 0;
                let r = self.object(true, |d, key| match d.field_index(*id, fields, &key, next)? {
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
                for (i, (field, ty, default)) in fields.iter().enumerate() {
                    values.push(match (self.slots[base + i], default, &**ty) {
                        (Some(v), _, _) => v,
                        (None, Some(c), _) => const_value(self.b.st, self.ids.none, c),
                        (None, None, TypeDesc::Option(_)) => Value::Tag(self.ids.none),
                        _ => {
                            self.slots.truncate(base);
                            return Err(self.fail(&format!("missing field `{field}`")));
                        }
                    });
                }
                self.slots.truncate(base);
                Ok(self.b.st.alloc(Obj::Struct(*def, values.into_boxed_slice())))
            }
            TypeDesc::Enum { name, variants, .. } => {
                self.stack.push(t);
                let r = self.decode_enum(name, variants);
                self.stack.pop();
                r
            }
            TypeDesc::Json => {
                let Some(ids) = self.ids.json else { return Err(Error::Ctl(Ctl::internal("the json module is not loaded"))) };
                self.p.parse_json(&mut self.b, &ids)
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
            TypeDesc::Unsupported(name) => {
                Err(Error::Ctl(Ctl::fail("invalid", format!("json.decode: cannot decode values of type {name}"))))
            }
        }
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
            self.b.st.reserve(self.b.pending.saturating_add(fields.len().saturating_mul(32)))?;
            self.b.pending += fields.len() * 32;
            let index = fields.iter().enumerate().rev().map(|(i, f)| (f.0.as_str(), i)).collect();
            self.indexes.insert(id, index);
        }
        Ok(self.indexes[&id].get(key).copied())
    }

    /// Converts an object key to a map key of type `t`, with its text if it
    /// is a string.
    fn map_key(&mut self, t: &TypeDesc, key: Cow<'a, str>) -> PResult<(Value, u64, Option<Cow<'a, str>>)> {
        let bad = |d: &Self| d.fail(&format!("invalid map key `{key}`"));
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
                let v = parse(&key).map_err(|msg| self.fail(&format!("invalid map key `{key}`: {msg}")))?;
                let Some(x) = v.value() else { return Err(Error::Ctl(Ctl::internal("json.decode: not a value"))) };
                // Staged object keys are deduplicated by their canonical text.
                let (hash, text) = (x.value_hash(), x.text());
                return Ok((self.b.st.alloc(v), hash, Some(Cow::Owned(text))));
            }
            _ => return Err(Error::Ctl(Ctl::fail("invalid", "json.decode: unsupported map key type"))),
        };
        Ok((k, hash_key(self.b.heap, self.b.meter, k)?, None))
    }

    /// An enum: `"Variant"` for payload-less variants, otherwise
    /// `{"Variant": [payload...]}` or `{"Variant": {"field": ...}}`.
    fn decode_enum(&mut self, name: &str, variants: &'d [VariantDesc]) -> PResult<Value> {
        let find = |d: &Self, tag: &str| match lookup(d.b.meter, variants, |v| v.0 == tag)? {
            Some(i) => Ok(&variants[i]),
            None => Err(d.fail(&format!("unknown variant `{tag}` of {name}"))),
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

fn const_value(st: &mut Staging, none: u32, c: &ConstVal) -> Value {
    match c {
        ConstVal::Unit => Value::Unit,
        ConstVal::Bool(b) => Value::Bool(*b),
        ConstVal::Int(i) => Value::Int(*i),
        ConstVal::Float(f) => Value::Float(*f),
        ConstVal::Str(s) => st.new_str(s.as_str()),
        ConstVal::Dur(d) => Value::Dur(*d),
        ConstVal::EmptyList => st.new_list(Vec::new()),
        ConstVal::EmptyMap => st.alloc(Obj::Map(Box::default())),
        ConstVal::None => Value::Tag(none),
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
        if len == 0 {
            self.out.push_str("[]");
            return Ok(());
        }
        if level >= MAX_DEPTH {
            return fail("limit", "value too deeply nested to encode as JSON");
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
        if len == 0 {
            self.out.push_str("{}");
            return Ok(());
        }
        if level >= MAX_DEPTH {
            return fail("limit", "value too deeply nested to encode as JSON");
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
                    let (n, ints) = range_ints(*a, *b, *inclusive);
                    self.array(n, ints.map(Value::Int), level)?
                }
                Obj::Map(m) => self.object(m.live, m.entries.iter().flatten(), level, |e, &(k, v)| {
                    match heap.get(k) {
                        Obj::Str(s) => e.string(s)?,
                        _ => {
                            let key = crate::vm::display::to_display_metered(prog, heap, e.meter, k)?;
                            e.string(&key)?
                        }
                    }
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
                Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) => return fail("invalid", "cannot encode a function as JSON"),
                Obj::Task(_) => return fail("invalid", "cannot encode a task as JSON"),
                Obj::Channel(_) => return fail("invalid", "cannot encode a channel as JSON"),
                Obj::Native(n) => return fail("invalid", format!("cannot encode a {} as JSON", n.type_name())),
                o @ (Obj::Value(_) | Obj::Packed(..)) => self.string(&o.value().map(ValueRef::text).unwrap_or_default())?,
                Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_) => return fail("invalid", "cannot encode this value as JSON"),
            },
            _ => {
                let (id, payload) = v.as_inline_variant().unwrap_or((u32::MAX, Value::Unit));
                self.variant(id, &[payload], level)?
            }
        }
        Ok(())
    }

    /// An enum variant with a payload: `Json` values as their payload's
    /// JSON, other variants as `{"Name": payload}`.
    fn variant(&mut self, id: u32, fields: &[Value], level: usize) -> Result<(), Ctl> {
        let Some(rt) = self.prog.variants.get(id as usize) else { return fail("invalid", "cannot encode this value as JSON") };
        if rt.def == self.ids.def {
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

/// Floats print with a fraction or an exponent (`2.0`, `1e20`), so that they
/// parse back as floats; non-finite numbers print as `null`.
fn write_float(out: &mut String, f: f64) {
    if f.is_finite() {
        let _ = write!(out, "{f:?}");
    } else {
        out.push_str("null");
    }
}

/// `Json.Int(n)` or `Json.Float(n)` for a number `n`.
fn json_number(st: &mut Staging, ids: &JsonIds, n: Value) -> Value {
    let id = if matches!(n, Value::Int(_)) { ids.int } else { ids.float };
    st.new_variant(id, &[n])
}

/// The number and the integers of the range `a..b` (`a..=b` if `inclusive`).
fn range_ints(a: i64, b: i64, inclusive: bool) -> (usize, impl Iterator<Item = i64>) {
    let last = if inclusive { Some(b) } else { b.checked_sub(1) };
    let ints = last.filter(|&last| a <= last).map(|last| a..=last).into_iter().flatten();
    let n = last.map_or(0, |last| (last as i128 - a as i128 + 1).max(0));
    (usize::try_from(n).unwrap_or(usize::MAX), ints)
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
            match prog.variants.get(heap.variant_id(v) as usize) {
                // `Json` values are shared.
                Some(rt) if rt.def == ids.def => return Ok(v),
                // `Some(x)` converts as `x`.
                _ if heap.variant_id(v) == prog.ids.some => {
                    self.b.meter.charge(1)?;
                    v = heap.field(v, 0).unwrap_or(Value::Unit);
                }
                _ => break,
            }
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
                    self.b.st.reserve(self.b.pending.saturating_add(b.len() / 3 * 4 + 4))?;
                    self.b.meter.charge_slow_bytes(b.len())?;
                    let s = self.b.st.new_str(base64::encode(b, false));
                    self.b.st.new_variant(ids.str_, &[s])
                }
                Obj::List(items) => self.array(items.iter().copied(), depth)?,
                Obj::Tuple(items) => self.array(items.iter().copied(), depth)?,
                Obj::Set(s) => self.array(s.entries.iter().flatten().copied(), depth)?,
                Obj::Range(a, b, inclusive) => {
                    let (n, ints) = range_ints(*a, *b, *inclusive);
                    self.b.st.reserve(self.b.pending.saturating_add(n.saturating_mul(std::mem::size_of::<Value>())))?;
                    self.b.meter.charge(n as u64)?;
                    let items = ints.map(|i| json_number(self.b.st, &ids, Value::Int(i))).collect();
                    let list = self.b.st.new_list(items);
                    self.b.st.new_variant(ids.arr, &[list])
                }
                Obj::Map(m) => {
                    self.check_depth(depth)?;
                    let mut out = self.b.start_map();
                    for &(k, x) in m.entries.iter().flatten() {
                        let x = self.convert(x, depth + 1)?;
                        let (k, text) = match heap.get(k) {
                            Obj::Str(s) => (k, Cow::Borrowed(&**s)),
                            _ => {
                                let key = crate::vm::display::to_display_metered(prog, heap, self.b.meter, k)?;
                                (self.b.st.new_str(key.as_str()), Cow::Owned(key))
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
                Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) => return fail("invalid", "cannot encode a function as JSON"),
                Obj::Task(_) => return fail("invalid", "cannot encode a task as JSON"),
                Obj::Channel(_) => return fail("invalid", "cannot encode a channel as JSON"),
                Obj::Native(n) => return fail("invalid", format!("cannot encode a {} as JSON", n.type_name())),
                o @ (Obj::Value(_) | Obj::Packed(..)) => {
                    let s = o.value().map(ValueRef::text).unwrap_or_default();
                    self.b.meter.charge_bytes(s.len())?;
                    let s = self.b.st.new_str(s);
                    self.b.st.new_variant(ids.str_, &[s])
                }
                Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_) => return fail("invalid", "cannot encode this value as JSON"),
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
        let Some(rt) = prog.variants.get(id as usize) else { return fail("invalid", "cannot encode this value as JSON") };
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
