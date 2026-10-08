//! The `json` module: a JSON parser and printer, plus reflection-based
//! conversion between JSON and typed values (`encode`, `decode`, `to_json`).

use super::base64;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::types::ConstVal;
use crate::vm::bytecode::JsonIds;
use crate::vm::value::{Obj, TypeDesc, Value};
use crate::vm::{Ctl, NResult, Vm, panic};
use std::borrow::Cow;
use std::fmt::Write;

/// Maximum nesting of arrays and objects, both when parsing and printing.
const MAX_DEPTH: usize = 512;

pub fn register(r: &mut Registry) {
    r.source("json", ModKind::Std, include_str!("json.sand"));
    r.add_builtin("json.parse", |vm, a| {
        vm.reserve(vm.str(a[0]).len())?;
        let text = vm.str(a[0]).to_string();
        parse_text(vm, &text)
    });
    r.add_builtin("json.stringify", |vm, a| encode_native(vm, a[0], a[1]));
    r.add_builtin("json.encode", |vm, a| encode_native(vm, a[0], a[1]));
    r.add_builtin("json.decode", |vm, a| {
        let Obj::TypeDesc(desc) = vm.heap.get(a[0]) else { return panic("json.decode: missing type description") };
        let desc = desc.clone();
        vm.reserve(vm.str(a[1]).len())?;
        let text = vm.str(a[1]).to_string();
        let mut d = Decoder { p: Parser::new(&text), path: Vec::new(), stack: Vec::new() };
        let r = d.decode(vm, &desc).and_then(|v| d.p.finish().map(|()| v));
        into_result(vm, r)
    });
    r.add_builtin("json.to_json", |vm, a| {
        let ids = json_ids(vm)?;
        to_json(vm, &ids, a[0], 0)
    });
}

/// Parses JSON text into a `Result<json.Json>` value. The `json` module must
/// be part of the program (imported by the calling module's Sand source).
pub(crate) fn parse_text(vm: &mut Vm, text: &str) -> NResult {
    let ids = json_ids(vm)?;
    let mut p = Parser::new(text);
    let r = p.parse_json(vm, &ids).and_then(|v| p.finish().map(|()| v));
    into_result(vm, r)
}

fn json_ids(vm: &Vm) -> Result<JsonIds, Ctl> {
    match vm.prog.ids.json {
        Some(ids) => Ok(ids),
        None => panic("the json module is not loaded"),
    }
}

/// Failure while parsing or decoding.
enum Error {
    /// Invalid JSON or a mismatch with the target type: becomes `Err(Error { kind: "parse" })`.
    Parse(String),
    /// Aborts the native call (memory limit, panic).
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
        Err(Error::Parse(msg)) => Ok(vm.err_value("parse", &msg)),
        Err(Error::Ctl(c)) => Err(c),
    }
}

fn variant(vm: &mut Vm, id: u32, payload: Value) -> Value {
    vm.heap.alloc(Obj::Variant(id, Box::new([payload])))
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
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Parser<'a> {
        Parser { src, b: src.as_bytes(), pos: 0, depth: 0 }
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
        for &c in word {
            if !self.eat(c) {
                return Err(self.unexpected());
            }
        }
        Ok(())
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

    /// A string literal; the current byte is the opening quote.
    fn string(&mut self) -> PResult<String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&c) = self.b.get(self.pos) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.src[start..self.pos]);
            match self.peek() {
                None => return Err(self.unexpected()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
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
                            out.push(c);
                            continue;
                        }
                        _ => return Err(self.err_at(esc, "invalid escape")),
                    };
                    self.pos += 1;
                    out.push(c);
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
            Some(b'"') => self.string().map(drop),
            Some(b'-' | b'0'..=b'9') => self.number().map(drop),
            Some(b'[') => self.array(|p, _| p.skip()),
            Some(b'{') => self.object(|p, _| p.skip()),
            _ => Err(self.unexpected()),
        }
    }

    /// Parses a value into a `json.Json`.
    fn parse_json(&mut self, vm: &mut Vm, ids: &JsonIds) -> PResult<Value> {
        self.ws();
        vm.reserve(64)?;
        Ok(match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                Value::Tag(ids.null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                variant(vm, ids.bool_, Value::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                variant(vm, ids.bool_, Value::Bool(false))
            }
            Some(b'"') => {
                let s = self.string()?;
                let s = vm.new_str(s);
                variant(vm, ids.str_, s)
            }
            Some(b'-' | b'0'..=b'9') => {
                let n = self.number()?.float();
                variant(vm, ids.num, Value::Float(n))
            }
            Some(b'[') => {
                let mut items = Vec::new();
                self.array(|p, _| {
                    items.push(p.parse_json(vm, ids)?);
                    Ok(())
                })?;
                let list = vm.heap.new_list(items);
                variant(vm, ids.arr, list)
            }
            Some(b'{') => {
                let map = vm.heap.alloc(Obj::Map(Box::default()));
                self.object(|p, key| {
                    let v = p.parse_json(vm, ids)?;
                    let k = vm.new_str(key);
                    vm.map_insert(map, k, v).map_err(Ctl::Panic)?;
                    Ok(())
                })?;
                variant(vm, ids.obj, map)
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

    /// Parses `{"key": v, ...}`, calling `f` with each key before its value.
    fn object(&mut self, mut f: impl FnMut(&mut Self, String) -> PResult<()>) -> PResult<()> {
        self.parser().enter()?;
        self.parser().ws();
        if !self.parser().eat(b'}') {
            loop {
                let p = self.parser();
                p.ws();
                if p.peek() != Some(b'"') {
                    return Err(p.unexpected());
                }
                let key = p.string()?;
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

/// A variant in a `TypeDesc::Enum`: name, runtime id, payload types and
/// payload field names (for named payloads).
type VariantDesc = (String, u32, Vec<TypeDesc>, Option<Vec<String>>);

enum Seg {
    Key(String),
    Index(usize),
}

/// Decodes JSON text directly into values of a described type.
struct Decoder<'a, 'd> {
    p: Parser<'a>,
    /// Location of the value being decoded, for error messages.
    path: Vec<Seg>,
    /// Enclosing struct and enum types, to resolve recursive references.
    stack: Vec<&'d TypeDesc>,
}

impl<'a> Source<'a> for Decoder<'a, '_> {
    fn parser(&mut self) -> &mut Parser<'a> {
        &mut self.p
    }
}

fn is_ident(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

impl<'a, 'd> Decoder<'a, 'd> {
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

    fn string(&mut self, what: &str) -> PResult<String> {
        if self.p.peek() == Some(b'"') { self.p.string() } else { Err(self.mismatch(what)) }
    }

    fn expect_open(&self, open: u8, what: &str) -> PResult<()> {
        if self.p.peek() == Some(open) { Ok(()) } else { Err(self.mismatch(what)) }
    }

    fn with_seg<T>(&mut self, seg: Seg, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.path.push(seg);
        let r = f(self);
        self.path.pop();
        r
    }

    fn decode(&mut self, vm: &mut Vm, t: &'d TypeDesc) -> PResult<Value> {
        self.p.ws();
        vm.reserve(64)?;
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
                Ok(vm.new_str(s))
            }
            TypeDesc::Bytes => {
                let s = self.string("base64 str")?;
                let b = base64::decode(&s, false).map_err(|_| self.fail("invalid base64"))?;
                Ok(vm.heap.new_bytes(b))
            }
            TypeDesc::Option(inner) => {
                if self.p.peek() == Some(b'n') {
                    self.p.literal(b"null")?;
                    Ok(vm.none())
                } else {
                    let v = self.decode(vm, inner)?;
                    Ok(vm.some(v))
                }
            }
            TypeDesc::List(elem) => {
                self.expect_open(b'[', "array")?;
                let mut items = Vec::new();
                self.array(|d, i| {
                    items.push(d.with_seg(Seg::Index(i), |d| d.decode(vm, elem))?);
                    Ok(())
                })?;
                Ok(vm.heap.new_list(items))
            }
            TypeDesc::Set(elem) => {
                self.expect_open(b'[', "array")?;
                let set = vm.heap.alloc(Obj::Set(Box::default()));
                self.array(|d, i| {
                    let v = d.with_seg(Seg::Index(i), |d| d.decode(vm, elem))?;
                    vm.set_add(set, v).map_err(Ctl::Panic)?;
                    Ok(())
                })?;
                Ok(set)
            }
            TypeDesc::Tuple(elems) => {
                let what = format!("array of length {}", elems.len());
                self.expect_open(b'[', &what)?;
                let mut items = Vec::with_capacity(elems.len());
                self.array(|d, i| {
                    let Some(elem) = elems.get(i) else { return Err(d.fail(&format!("expected {what}"))) };
                    items.push(d.with_seg(Seg::Index(i), |d| d.decode(vm, elem))?);
                    Ok(())
                })?;
                if items.len() != elems.len() {
                    return Err(self.fail(&format!("expected {what}")));
                }
                Ok(vm.heap.new_tuple(items))
            }
            TypeDesc::Map(key_type, value_type) => {
                self.expect_open(b'{', "object")?;
                let map = vm.heap.alloc(Obj::Map(Box::default()));
                self.object(|d, key| {
                    let k = d.map_key(vm, key_type, &key)?;
                    let v = d.with_seg(Seg::Key(key), |d| d.decode(vm, value_type))?;
                    vm.map_insert(map, k, v).map_err(Ctl::Panic)?;
                    Ok(())
                })?;
                Ok(map)
            }
            TypeDesc::Struct { def, fields, .. } => {
                self.expect_open(b'{', "object")?;
                self.stack.push(t);
                let mut slots: Vec<Option<Value>> = vec![None; fields.len()];
                let r = self.object(|d, key| match fields.iter().position(|f| f.0 == key) {
                    Some(i) => {
                        slots[i] = Some(d.with_seg(Seg::Key(key), |d| d.decode(vm, &fields[i].1))?);
                        Ok(())
                    }
                    None => d.p.skip(),
                });
                self.stack.pop();
                r?;
                let mut values = Vec::with_capacity(fields.len());
                for ((field, ty, default), slot) in fields.iter().zip(slots) {
                    values.push(match (slot, default, ty) {
                        (Some(v), _, _) => v,
                        (None, Some(c), _) => const_value(vm, c),
                        (None, None, TypeDesc::Option(_)) => vm.none(),
                        _ => return Err(self.fail(&format!("missing field `{field}`"))),
                    });
                }
                Ok(vm.heap.alloc(Obj::Struct(*def, values.into_boxed_slice())))
            }
            TypeDesc::Enum { name, variants } => {
                self.stack.push(t);
                let r = self.decode_enum(vm, name, variants);
                self.stack.pop();
                r
            }
            TypeDesc::Json => {
                let ids = json_ids(vm)?;
                self.p.parse_json(vm, &ids)
            }
            TypeDesc::Recur(name) => {
                let found = self.stack.iter().rev().find(|d| match d {
                    TypeDesc::Struct { name: n, .. } | TypeDesc::Enum { name: n, .. } => n == name,
                    _ => false,
                });
                match found {
                    Some(&d) => self.decode(vm, d),
                    None => Err(Error::Ctl(Ctl::Panic(format!("json.decode: unresolved recursive type {name}")))),
                }
            }
            TypeDesc::Unsupported(name) => Err(Error::Ctl(Ctl::Panic(format!("json.decode: cannot decode values of type {name}")))),
        }
    }

    /// Converts an object key to a map key of type `t`.
    fn map_key(&self, vm: &mut Vm, t: &TypeDesc, key: &str) -> PResult<Value> {
        let bad = || self.fail(&format!("invalid map key `{key}`"));
        Ok(match t {
            TypeDesc::Str => vm.new_str(key),
            TypeDesc::Int => Value::Int(key.parse().map_err(|_| bad())?),
            TypeDesc::Duration => Value::Dur(key.parse().map_err(|_| bad())?),
            TypeDesc::Bool => match key {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(bad()),
            },
            TypeDesc::Enum { variants, .. } => match variants.iter().find(|v| v.0 == key && v.2.is_empty()) {
                Some(v) => Value::Tag(v.1),
                None => return Err(bad()),
            },
            _ => return Err(Error::Ctl(Ctl::Panic("json.decode: unsupported map key type".into()))),
        })
    }

    /// An enum: `"Variant"` for payload-less variants, otherwise
    /// `{"Variant": [payload...]}` or `{"Variant": {"field": ...}}`.
    fn decode_enum(&mut self, vm: &mut Vm, name: &str, variants: &'d [VariantDesc]) -> PResult<Value> {
        let find = |d: &Self, tag: &str| match variants.iter().find(|v| v.0 == tag) {
            Some(v) => Ok(v),
            None => Err(d.fail(&format!("unknown variant `{tag}` of {name}"))),
        };
        match self.p.peek() {
            Some(b'"') => {
                let tag = self.p.string()?;
                let v = find(self, &tag)?;
                if !v.2.is_empty() {
                    return Err(self.fail(&format!("variant `{tag}` of {name} needs a payload")));
                }
                Ok(Value::Tag(v.1))
            }
            Some(b'{') => {
                let mut result = None;
                self.object(|d, tag| {
                    if result.is_some() {
                        return Err(d.fail(&format!("expected a single variant of {name}")));
                    }
                    let v = find(d, &tag)?;
                    result = Some(d.with_seg(Seg::Key(tag), |d| d.decode_payload(vm, v))?);
                    Ok(())
                })?;
                result.ok_or_else(|| self.fail(&format!("expected a variant of {name}")))
            }
            _ => Err(self.mismatch(name)),
        }
    }

    fn decode_payload(&mut self, vm: &mut Vm, v: &'d VariantDesc) -> PResult<Value> {
        let (_, id, types, names) = v;
        self.p.ws();
        let mut slots: Vec<Option<Value>> = vec![None; types.len()];
        match (self.p.peek(), names) {
            (Some(b'{'), Some(names)) => {
                self.object(|d, key| match names.iter().position(|n| *n == key) {
                    Some(i) => {
                        slots[i] = Some(d.with_seg(Seg::Key(key), |d| d.decode(vm, &types[i]))?);
                        Ok(())
                    }
                    None => d.p.skip(),
                })?;
                for (i, slot) in slots.iter_mut().enumerate() {
                    if slot.is_none() {
                        if !matches!(types[i], TypeDesc::Option(_)) {
                            return Err(self.fail(&format!("missing field `{}`", names[i])));
                        }
                        *slot = Some(vm.none());
                    }
                }
            }
            (Some(b'['), _) => {
                let what = format!("array of length {}", types.len());
                let mut n = 0;
                self.array(|d, i| {
                    let Some(ty) = types.get(i) else { return Err(d.fail(&format!("expected {what}"))) };
                    slots[i] = Some(d.with_seg(Seg::Index(i), |d| d.decode(vm, ty))?);
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
        Ok(if fields.is_empty() { Value::Tag(*id) } else { vm.heap.alloc(Obj::Variant(*id, fields.into_boxed_slice())) })
    }
}

fn const_value(vm: &mut Vm, c: &ConstVal) -> Value {
    match c {
        ConstVal::Unit => Value::Unit,
        ConstVal::Bool(b) => Value::Bool(*b),
        ConstVal::Int(i) => Value::Int(*i),
        ConstVal::Float(f) => Value::Float(*f),
        ConstVal::Str(s) => vm.new_str(s.as_str()),
        ConstVal::Dur(d) => Value::Dur(*d),
        ConstVal::EmptyList => vm.heap.new_list(Vec::new()),
        ConstVal::EmptyMap => vm.heap.alloc(Obj::Map(Box::default())),
        ConstVal::None => vm.none(),
    }
}

// ---------------------------------------------------------------------------
// Encoding

/// The JSON shape of a value. Strings borrow from the heap where possible.
enum Node<'h> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Cow<'h, str>),
    Arr(Vec<Item<'h>>),
    Obj(Vec<(Cow<'h, str>, Item<'h>)>),
}

/// An element of an array or object: a value still to be converted, or a
/// synthetic node (enum payload wrappers, range elements).
enum Item<'h> {
    Val(Value),
    Node(Node<'h>),
}

impl Node<'_> {
    fn into_owned(self) -> Node<'static> {
        let item = |it: Item<'_>| match it {
            Item::Val(v) => Item::Val(v),
            Item::Node(n) => Item::Node(n.into_owned()),
        };
        match self {
            Node::Null => Node::Null,
            Node::Bool(b) => Node::Bool(b),
            Node::Int(i) => Node::Int(i),
            Node::Float(f) => Node::Float(f),
            Node::Str(s) => Node::Str(Cow::Owned(s.into_owned())),
            Node::Arr(items) => Node::Arr(items.into_iter().map(item).collect()),
            Node::Obj(entries) => Node::Obj(entries.into_iter().map(|(k, v)| (Cow::Owned(k.into_owned()), item(v))).collect()),
        }
    }
}

fn vals<'h>(vs: &[Value]) -> Vec<Item<'h>> {
    vs.iter().map(|v| Item::Val(*v)).collect()
}

/// Classifies a value by its JSON shape (spec §13.8).
fn classify<'h>(vm: &'h Vm, ids: &JsonIds, mut v: Value) -> Result<Node<'h>, Ctl> {
    // `Some(x)` encodes as `x`.
    while let Value::Obj(_) = v {
        if vm.heap.variant_id(v) != vm.prog.ids.some {
            break;
        }
        v = vm.heap.fields(v)[0];
    }
    let prog = &vm.prog;
    Ok(match v {
        Value::Unit | Value::Uninit => Node::Null,
        Value::Bool(b) => Node::Bool(b),
        Value::Int(i) | Value::Dur(i) => Node::Int(i),
        Value::Float(f) => Node::Float(f),
        Value::Tag(id) if id == prog.ids.none || id == ids.null => Node::Null,
        Value::Tag(id) => Node::Str(Cow::Borrowed(&prog.variants[id as usize].name)),
        Value::Obj(_) => match vm.heap.get(v) {
            Obj::Str(s) => Node::Str(Cow::Borrowed(s)),
            Obj::Bytes(b) => Node::Str(Cow::Owned(base64::encode(b, false))),
            Obj::List(items) => Node::Arr(vals(items)),
            Obj::Tuple(items) => Node::Arr(vals(items)),
            Obj::Set(s) => Node::Arr(s.entries.iter().flatten().map(|v| Item::Val(*v)).collect()),
            Obj::Range(a, b, inclusive) => {
                let end = if *inclusive { b.saturating_add(1) } else { *b };
                let n = end.saturating_sub(*a).max(0) as usize;
                vm.reserve(n.saturating_mul(32))?;
                Node::Arr((*a..end).map(|i| Item::Node(Node::Int(i))).collect())
            }
            Obj::Map(m) => {
                let mut entries = Vec::with_capacity(m.live);
                for (k, v) in m.entries.iter().flatten() {
                    let key = match vm.heap.get(*k) {
                        Obj::Str(s) => Cow::Borrowed(&**s),
                        _ => Cow::Owned(vm.display(*k)?),
                    };
                    entries.push((key, Item::Val(*v)));
                }
                Node::Obj(entries)
            }
            Obj::Struct(def, fields) => {
                let names = &prog.types[*def as usize].fields;
                Node::Obj(names.iter().zip(fields.iter()).map(|(n, v)| (Cow::Borrowed(n.as_str()), Item::Val(*v))).collect())
            }
            Obj::Variant(id, fields) => {
                let rt = &prog.variants[*id as usize];
                if rt.def == ids.def {
                    match fields[0] {
                        Value::Bool(b) => Node::Bool(b),
                        Value::Float(f) => Node::Float(f),
                        payload => match vm.heap.get(payload) {
                            Obj::Str(s) => Node::Str(Cow::Borrowed(s)),
                            Obj::List(items) => Node::Arr(vals(items)),
                            _ => return classify(vm, ids, payload),
                        },
                    }
                } else {
                    let payload = match &rt.field_names {
                        Some(names) => {
                            Node::Obj(names.iter().zip(fields.iter()).map(|(n, v)| (Cow::Borrowed(n.as_str()), Item::Val(*v))).collect())
                        }
                        None => Node::Arr(vals(fields)),
                    };
                    Node::Obj(vec![(Cow::Borrowed(rt.name.as_str()), Item::Node(payload))])
                }
            }
            Obj::Closure(..) | Obj::NativeFn(_) => return panic("cannot encode a function as JSON"),
            Obj::Task(_) => return panic("cannot encode a task as JSON"),
            Obj::Channel(_) => return panic("cannot encode a channel as JSON"),
            Obj::Native(n) => return panic(format!("cannot encode a {} as JSON", n.type_name())),
            Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_) => return panic("cannot encode this value as JSON"),
        },
    })
}

fn encode_native(vm: &mut Vm, value: Value, indent: Value) -> NResult {
    let ids = json_ids(vm)?;
    let mut e = Emitter { vm, ids, indent: indent.as_int().clamp(0, 1 << 16) as usize, out: String::new() };
    e.item(Item::Val(value), 0)?;
    let out = e.out;
    Ok(vm.new_str(out))
}

/// Writes JSON text, compact or pretty-printed.
struct Emitter<'h> {
    vm: &'h Vm,
    ids: JsonIds,
    indent: usize,
    out: String,
}

impl<'h> Emitter<'h> {
    fn newline(&mut self, level: usize) -> Result<(), Ctl> {
        if self.indent > 0 {
            let n = self.indent * level;
            self.vm.reserve(self.out.len() + n)?;
            self.out.push('\n');
            self.out.extend(std::iter::repeat_n(' ', n));
        }
        Ok(())
    }

    fn item(&mut self, it: Item<'h>, level: usize) -> Result<(), Ctl> {
        let node = match it {
            Item::Val(v) => classify(self.vm, &self.ids, v)?,
            Item::Node(n) => n,
        };
        self.vm.reserve(self.out.len())?;
        match node {
            Node::Null => self.out.push_str("null"),
            Node::Bool(b) => self.out.push_str(if b { "true" } else { "false" }),
            Node::Int(i) => {
                let _ = write!(self.out, "{i}");
            }
            Node::Float(f) => write_float(&mut self.out, f),
            Node::Str(s) => write_str(&mut self.out, &s),
            Node::Arr(items) => {
                if items.is_empty() {
                    self.out.push_str("[]");
                    return Ok(());
                }
                if level >= MAX_DEPTH {
                    return panic("value too deeply nested to encode as JSON");
                }
                self.out.push('[');
                for (i, it) in items.into_iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    self.newline(level + 1)?;
                    self.item(it, level + 1)?;
                }
                self.newline(level)?;
                self.out.push(']');
            }
            Node::Obj(entries) => {
                if entries.is_empty() {
                    self.out.push_str("{}");
                    return Ok(());
                }
                if level >= MAX_DEPTH {
                    return panic("value too deeply nested to encode as JSON");
                }
                self.out.push('{');
                for (i, (k, it)) in entries.into_iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    self.newline(level + 1)?;
                    write_str(&mut self.out, &k);
                    self.out.push_str(if self.indent > 0 { ": " } else { ":" });
                    self.item(it, level + 1)?;
                }
                self.newline(level)?;
                self.out.push('}');
            }
        }
        Ok(())
    }
}

/// Integral floats within ±2^53 print without a fraction; non-finite
/// numbers print as `null`.
fn write_float(out: &mut String, f: f64) {
    if !f.is_finite() {
        out.push_str("null");
    } else if f.fract() == 0.0 && f.abs() <= 9007199254740992.0 {
        let _ = write!(out, "{}", f as i64);
    } else {
        let _ = write!(out, "{f:?}");
    }
}

fn write_str(out: &mut String, s: &str) {
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

fn to_json(vm: &mut Vm, ids: &JsonIds, v: Value, depth: usize) -> NResult {
    if vm.prog.variants.get(vm.heap.variant_id(v) as usize).is_some_and(|rt| rt.def == ids.def) {
        return Ok(v);
    }
    let node = classify(vm, ids, v)?.into_owned();
    build(vm, ids, node, depth)
}

fn build(vm: &mut Vm, ids: &JsonIds, node: Node<'static>, depth: usize) -> NResult {
    vm.reserve(64)?;
    let item = |vm: &mut Vm, it: Item<'static>| match it {
        Item::Val(v) => to_json(vm, ids, v, depth + 1),
        Item::Node(n) => build(vm, ids, n, depth + 1),
    };
    Ok(match node {
        Node::Null => Value::Tag(ids.null),
        Node::Bool(b) => variant(vm, ids.bool_, Value::Bool(b)),
        Node::Int(i) => variant(vm, ids.num, Value::Float(i as f64)),
        Node::Float(f) => variant(vm, ids.num, Value::Float(f)),
        Node::Str(s) => {
            let s = vm.new_str(s.into_owned());
            variant(vm, ids.str_, s)
        }
        Node::Arr(items) => {
            if depth >= MAX_DEPTH {
                return panic("value too deeply nested to convert to JSON");
            }
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(item(vm, it)?);
            }
            let list = vm.heap.new_list(out);
            variant(vm, ids.arr, list)
        }
        Node::Obj(entries) => {
            if depth >= MAX_DEPTH {
                return panic("value too deeply nested to convert to JSON");
            }
            let map = vm.heap.alloc(Obj::Map(Box::default()));
            for (k, it) in entries {
                let v = item(vm, it)?;
                let k = vm.new_str(k.into_owned());
                vm.map_insert(map, k, v).map_err(Ctl::Panic)?;
            }
            variant(vm, ids.obj, map)
        }
    })
}
