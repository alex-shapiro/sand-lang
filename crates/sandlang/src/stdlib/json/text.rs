//! The JSON text format of `Encoder` and `Decoder`.

use super::{Builder, Error as ParseError, Json, JsonIds, Parser, SPACES, base64, json_from_vm, write_str};
use crate::codec::MAX_NESTING_DEPTH;
use crate::codec::value_format::write_json_form;
use crate::codec::{Budget, DecodeFail, DecodeFormat, DecodeResult, EncodeFormat, FormatMark, SeqKind};
use crate::value::Value;
use crate::vm::meter::SLOW_BYTES_PER_UNIT;
use crate::vm::value::{Obj, VmValue};
use crate::vm::{Fail, Vm, fail, internal};
use std::any::Any;
use std::borrow::Cow;
use std::fmt::Write;

fn invalid_key_error<T>() -> Result<T, Fail> {
    fail("invalid", "a JSON object key must be a string, int, duration, bool, payload-less variant, or value type")
}

fn nesting_limit_error<T>() -> Result<T, Fail> {
    fail("limit", "value too deeply nested to encode as JSON")
}

/// An open JSON array or object of [`TextEncoder`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bracket {
    Array,
    /// An object whose member keys come from `field` calls.
    Fields,
    /// A map, and `true` while the next value is a key.
    Map(bool),
    /// The outer object of a variant with a payload; its inner bracket follows.
    VariantWrapper,
    /// `Some`, which writes only its payload.
    Some,
}

#[derive(Clone, Copy)]
struct Open {
    bracket: Bracket,
    is_empty: bool,
    /// The struct definition, or the variant, whose field names `field` writes.
    names: Option<(u32, bool)>,
}

/// Writes JSON text.
pub(crate) struct TextEncoder {
    indent: usize,
    out: String,
    /// Bytes of `out` already charged to the meter.
    charged: usize,
    open: Vec<Open>,
    /// The number of open arrays and objects.
    depth: usize,
    budget: Budget,
}

impl TextEncoder {
    pub(crate) fn new(indent: usize) -> TextEncoder {
        TextEncoder { indent, out: String::new(), charged: 0, open: Vec::new(), depth: 0, budget: Budget::default() }
    }

    /// Returns the text written.
    pub(crate) fn take_text(&mut self) -> String {
        std::mem::take(&mut self.out)
    }

    /// Counts `extra` more bytes of output, and the indentation of a new line,
    /// against the memory limit.
    fn room(&mut self, vm: &mut Vm, extra: usize) -> Result<(), Fail> {
        let line = if self.indent > 0 { 1 + self.indent * (self.depth + 1) } else { 0 };
        self.budget.grow(vm, self.out.len().saturating_add(extra).saturating_add(line + 2))
    }

    fn charge_written(&mut self, vm: &Vm) -> Result<(), Fail> {
        let written = self.out.len() - self.charged;
        self.charged = self.out.len();
        vm.charge_bytes(written)
    }

    fn in_key(&self) -> bool {
        matches!(self.open.last(), Some(Open { bracket: Bracket::Map(true), .. }))
    }

    /// Writes a scalar that `text` holds: as a value, or as a map key.
    fn scalar(&mut self, vm: &mut Vm, text: &str, quote: bool) -> Result<(), Fail> {
        vm.charge(1)?;
        self.room(vm, text.len() * if quote { 6 } else { 1 } + 4)?;
        if self.in_key() {
            self.begin_member(Some(text));
        } else {
            self.begin_member(None);
            if quote {
                write_str(&mut self.out, text);
            } else {
                self.out.push_str(text);
            }
        }
        self.charge_written(vm)
    }

    /// Writes a value that cannot be a map key.
    fn value_only(&mut self, vm: &mut Vm, extra: usize) -> Result<(), Fail> {
        vm.charge(1)?;
        if self.in_key() {
            return invalid_key_error();
        }
        self.room(vm, extra)?;
        self.begin_member(None);
        Ok(())
    }

    /// Writes the separator and the key of the next member of the innermost
    /// array or object, and updates a map's key state.
    fn begin_member(&mut self, key: Option<&str>) {
        let Some(top) = self.open.last_mut() else { return };
        match top.bracket {
            Bracket::Some | Bracket::VariantWrapper => return,
            Bracket::Map(false) => {
                top.bracket = Bracket::Map(true);
                return;
            }
            Bracket::Map(true) => top.bracket = Bracket::Map(false),
            Bracket::Fields if key.is_none() => return,
            _ => {}
        }
        if !std::mem::replace(&mut top.is_empty, false) {
            self.out.push(',');
        }
        self.start_line();
        let Some(key) = key else { return };
        write_str(&mut self.out, key);
        self.out.push(':');
        if self.indent > 0 {
            self.out.push(' ');
        }
    }

    fn start_line(&mut self) {
        if self.indent == 0 {
            return;
        }
        self.out.push('\n');
        let mut left = self.indent * self.depth;
        while left > 0 {
            let spaces = &SPACES[..left.min(SPACES.len())];
            self.out.push_str(spaces);
            left -= spaces.len();
        }
    }

    fn open_bracket(&mut self, bracket: Bracket, names: Option<(u32, bool)>) -> Result<(), Fail> {
        let ch = match bracket {
            Bracket::Array => Some('['),
            Bracket::Fields | Bracket::Map(_) | Bracket::VariantWrapper => Some('{'),
            Bracket::Some => None,
        };
        if ch.is_some() {
            if self.depth >= MAX_NESTING_DEPTH {
                return nesting_limit_error();
            }
            self.depth += 1;
        }
        self.out.extend(ch);
        self.open.push(Open { bracket, is_empty: true, names });
        Ok(())
    }

    fn close_bracket(&mut self) -> Result<(), Fail> {
        let Some(top) = self.open.pop() else { return internal("json.encode: no open bracket") };
        let ch = match top.bracket {
            Bracket::Array => ']',
            Bracket::Some => return Ok(()),
            _ => '}',
        };
        self.depth -= 1;
        if !top.is_empty {
            self.start_line();
        }
        self.out.push(ch);
        if top.bracket != Bracket::VariantWrapper
            && let Some(Open { bracket: Bracket::VariantWrapper, .. }) = self.open.last()
        {
            return self.close_bracket();
        }
        Ok(())
    }
}

impl EncodeFormat for TextEncoder {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.value_only(vm, 4)?;
        self.out.push_str("null");
        self.charge_written(vm)
    }

    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail> {
        self.scalar(vm, if b { "true" } else { "false" }, false)
    }

    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail> {
        self.scalar(vm, itoa::Buffer::new().format(i), false)
    }

    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail> {
        self.value_only(vm, 32)?;
        if f.is_finite() {
            _ = write!(self.out, "{f:?}");
        } else {
            self.out.push_str("null");
        }
        self.charge_written(vm)
    }

    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail> {
        self.int(vm, nanos)
    }

    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail> {
        self.scalar(vm, s, true)
    }

    fn bytes(&mut self, vm: &mut Vm, b: &[u8]) -> Result<(), Fail> {
        self.value_only(vm, b.len() / 3 * 4 + 6)?;
        vm.charge_slow_bytes(b.len())?;
        self.out.extend(["\"", &base64::encode(b, false), "\""]);
        self.charge_written(vm)
    }

    fn native(&mut self, vm: &mut Vm, _: &str, text: &str) -> Result<(), Fail> {
        self.scalar(vm, text, true)
    }

    fn range(&mut self, vm: &mut Vm, start: i64, end: i64, inclusive: bool) -> Result<(), Fail> {
        self.value_only(vm, 64 + 4 * self.indent * (self.depth + 1))?;
        self.open_bracket(Bracket::Fields, None)?;
        for (name, bound) in [("start", start), ("end", end)] {
            self.begin_member(Some(name));
            self.out.push_str(itoa::Buffer::new().format(bound));
        }
        if inclusive {
            self.begin_member(Some("inclusive"));
            self.out.push_str("true");
        }
        self.close_bracket()?;
        self.charge_written(vm)
    }

    fn begin_seq(&mut self, vm: &mut Vm, _: SeqKind, _: usize) -> Result<(), Fail> {
        self.value_only(vm, 1)?;
        self.open_bracket(Bracket::Array, None)
    }

    fn begin_map(&mut self, vm: &mut Vm, _: usize) -> Result<(), Fail> {
        self.value_only(vm, 1)?;
        self.open_bracket(Bracket::Map(true), None)
    }

    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        self.value_only(vm, 1)?;
        self.open_bracket(Bracket::Fields, Some((def, false)))
    }

    fn field(&mut self, vm: &mut Vm, index: usize) -> Result<(), Fail> {
        let Some(Open { names: Some((id, variant)), .. }) = self.open.last() else { return internal("json.encode: no open struct") };
        let (id, variant) = (*id, *variant);
        let names = if variant {
            vm.prog.variants.get(id as usize).and_then(|v| v.field_names.as_deref())
        } else {
            vm.prog.types.get(id as usize).map(|t| &t.fields[..])
        };
        let Some(name) = names.and_then(|n| n.get(index)) else { return internal("json.encode: unknown field") };
        let need = name.len() * 6 + 4;
        let name = name.clone();
        self.room(vm, need)?;
        self.begin_member(Some(&name));
        self.charge_written(vm)
    }

    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        if variant == vm.prog.ids.none {
            return self.none(vm);
        }
        let Some(v) = vm.prog.variants.get(variant as usize) else { return internal("json.encode: unknown variant") };
        let name = v.name.clone();
        self.scalar(vm, &name, true)
    }

    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let Some(v) = vm.prog.variants.get(variant as usize) else { return internal("json.encode: unknown variant") };
        let (name, named) = (v.name.clone(), v.field_names.is_some());
        self.value_only(vm, name.len() * 6 + 8 + 2 * self.indent * (self.depth + 2))?;
        self.open_bracket(Bracket::VariantWrapper, None)?;
        // The wrapper has one member: the variant name.
        if let Some(top) = self.open.last_mut() {
            top.is_empty = false;
        }
        self.start_line();
        write_str(&mut self.out, &name);
        self.out.push(':');
        if self.indent > 0 {
            self.out.push(' ');
        }
        if named { self.open_bracket(Bracket::Fields, Some((variant, true))) } else { self.open_bracket(Bracket::Array, None) }
    }

    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.room(vm, 1)?;
        self.begin_member(None);
        self.open_bracket(Bracket::Some, None)
    }

    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit(vm)
    }

    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.room(vm, 2)?;
        self.close_bracket()?;
        self.charge_written(vm)
    }

    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail> {
        emit_json(vm, json, self)
    }

    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        write_json_form(self, vm, value)
    }

    fn mark(&self) -> FormatMark {
        FormatMark(Box::new((self.out.len(), self.charged, self.open.len(), self.open.last().copied(), self.depth)))
    }

    fn rollback(&mut self, mark: FormatMark) {
        let Ok(m) = mark.0.downcast::<(usize, usize, usize, Option<Open>, usize)>() else { return };
        let (out, charged, open, top, depth) = *m;
        self.out.truncate(out);
        self.open.truncate(open);
        if let (Some(top), Some(last)) = (top, self.open.last_mut()) {
            *last = top;
        }
        (self.charged, self.depth) = (charged, depth);
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Writes the Sand `Json` value `root` to `f` with the format's operations.
fn emit_json(vm: &mut Vm, root: VmValue, f: &mut dyn EncodeFormat) -> Result<(), Fail> {
    enum Step {
        Value(VmValue),
        Key(VmValue),
        End,
    }
    let ids = JsonIds::new(&vm.prog)?;
    let mut todo = vec![Step::Value(root)];
    while let Some(step) = todo.pop() {
        let v = match step {
            Step::End => {
                f.end(vm)?;
                continue;
            }
            Step::Key(k) => {
                let k = vm.heap.str(k).to_string();
                f.str(vm, &k)?;
                continue;
            }
            Step::Value(v) => v,
        };
        let id = vm.heap.variant_id(v);
        let payload = vm.heap.field(v, 0);
        match payload {
            _ if id == ids.null => f.unit(vm)?,
            Some(VmValue::Bool(b)) if id == ids.bool_ => f.bool(vm, b)?,
            Some(VmValue::Int(i)) if id == ids.int => f.int(vm, i)?,
            Some(VmValue::Float(x)) if id == ids.float => f.float(vm, x)?,
            Some(p) if id == ids.str_ => {
                let s = vm.heap.str(p).to_string();
                f.str(vm, &s)?;
            }
            Some(p) if id == ids.arr => {
                let Obj::List(items) = vm.heap.get(p) else { return internal("json: not an array") };
                let items = items.clone();
                f.begin_seq(vm, SeqKind::List, items.len())?;
                todo.push(Step::End);
                todo.extend(items.into_iter().rev().map(Step::Value));
            }
            Some(p) if id == ids.obj => {
                let Obj::Map(m) = vm.heap.get(p) else { return internal("json: not an object") };
                let entries: Vec<(VmValue, VmValue)> = m.entries.iter().flatten().copied().collect();
                f.begin_map(vm, entries.len())?;
                todo.push(Step::End);
                for (k, v) in entries.into_iter().rev() {
                    todo.push(Step::Value(v));
                    todo.push(Step::Key(k));
                }
            }
            _ => return internal("json: not a Json value"),
        }
    }
    Ok(())
}

/// Returns the JSON form of `json` as a host value.
pub(crate) fn json_value(json: &Json) -> Value {
    match json {
        Json::Null => Value::Unit,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Str(s.clone()),
        Json::Arr(items) => Value::List(items.iter().map(json_value).collect()),
        Json::Obj(members) => Value::Map(members.iter().map(|(k, v)| (Value::Str(k.clone()), json_value(v))).collect()),
    }
}

/// An open JSON array or object of [`TextDecoder`].
#[derive(Clone, Copy)]
struct Container {
    is_first: bool,
}

/// Reads JSON text.
pub(crate) struct TextDecoder {
    text: Box<str>,
    pos: usize,
    depth: usize,
    charged: usize,
    open: Vec<Container>,
    /// The key of the map entry that the next operations read.
    key: Option<String>,
    budget: Budget,
}

impl TextDecoder {
    pub(crate) fn new(vm: &mut Vm, text: &str) -> Result<TextDecoder, Fail> {
        let mut budget = Budget::default();
        budget.grow(vm, text.len())?;
        Ok(TextDecoder { text: text.into(), pos: 0, depth: 0, charged: 0, open: Vec::new(), key: None, budget })
    }

    /// Runs `f` on a parser at the current position. On an error, the
    /// position does not move, so a run of the native function again starts
    /// from the same input.
    fn parse<R>(&mut self, vm: &mut Vm, f: impl FnOnce(&mut Parser<'_>, &mut Vm) -> Result<R, ParseError>) -> DecodeResult<R> {
        let mut p = Parser { src: &self.text, b: self.text.as_bytes(), pos: self.pos, depth: self.depth, charged: self.charged };
        p.ws();
        let r = f(&mut p, vm);
        if let Err(ParseError::Fail(_)) = r {
            return r.map_err(into_fail);
        }
        let consumed = p.pos.saturating_sub(p.charged);
        (self.pos, self.depth, self.charged) = (p.pos, p.depth, p.pos);
        vm.charge(1 + (consumed / SLOW_BYTES_PER_UNIT) as u64)?;
        r.map_err(into_fail)
    }

    /// Returns the error for a value that does not match `what`.
    fn mismatch(p: &Parser<'_>, what: &str) -> ParseError {
        match p.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => ParseError::Parse(format!("expected {what}")),
            _ => p.unexpected(),
        }
    }

    /// Returns the key of the current map entry, if the next operation reads
    /// a key.
    fn take_key(&mut self) -> Option<String> {
        self.key.take()
    }

    fn invalid_key(key: &str) -> DecodeFail {
        DecodeFail::Mismatch(format!("invalid map key `{}`", crate::stdlib::truncate(key, crate::codec::QUOTED_KEY)))
    }

    fn container_key(&mut self, what: &str) -> DecodeFail {
        self.key = None;
        DecodeFail::Invalid(format!("cannot decode map keys of type {what}"))
    }

    fn number<'a>(p: &mut Parser<'a>, what: &str) -> Result<super::Number<'a>, ParseError> {
        match p.peek() {
            Some(b'-' | b'0'..=b'9') => p.number(),
            _ => Err(Self::mismatch(p, what)),
        }
    }

    fn int_value(p: &mut Parser<'_>) -> Result<i64, ParseError> {
        let n = Self::number(p, "int")?;
        n.int().ok_or_else(|| {
            let f = n.float();
            ParseError::Parse(if f.is_finite() && f.fract() == 0.0 { "integer out of range" } else { "expected int" }.into())
        })
    }

    /// Opens an array or object at `open`, as `what` for a mismatch.
    fn open(&mut self, vm: &mut Vm, open: u8, what: &str) -> DecodeResult<()> {
        self.parse(vm, |p, _| {
            if p.peek() != Some(open) {
                return Err(Self::mismatch(p, what));
            }
            p.enter()
        })?;
        self.open.push(Container { is_first: true });
        Ok(())
    }

    /// Moves to the next element of the innermost container that ends with
    /// `close`. Returns `false` after its last element.
    fn next(&mut self, vm: &mut Vm, close: u8) -> DecodeResult<bool> {
        let Some(&Container { is_first }) = self.open.last() else { return Err(Fail::internal("json: no open container").into()) };
        let more = self.parse(vm, |p, _| {
            if p.eat(close) {
                p.depth -= 1;
                return Ok(false);
            }
            if !is_first && !p.eat(b',') {
                return Err(p.unexpected());
            }
            Ok(true)
        })?;
        match more {
            true => {
                if let Some(c) = self.open.last_mut() {
                    c.is_first = false;
                }
            }
            false => _ = self.open.pop(),
        }
        Ok(more)
    }

    /// Reads the next key of the innermost object, or returns `None` after
    /// its last member.
    fn next_key(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        if !self.next(vm, b'}')? {
            return Ok(None);
        }
        let (start, end, owned) = self.parse(vm, |p, _| {
            p.ws();
            if p.peek() != Some(b'"') {
                return Err(p.unexpected());
            }
            let start = p.pos + 1;
            let key = p.string(true)?;
            let end = p.pos - 1;
            p.ws();
            if !p.eat(b':') {
                return Err(p.unexpected());
            }
            Ok((start, end, matches!(key, Cow::Owned(_)).then(|| key.into_owned())))
        })?;
        Ok(Some(match owned {
            Some(key) => Cow::Owned(key),
            None => Cow::Borrowed(&self.text[start..end]),
        }))
    }
}

fn into_fail(e: ParseError) -> DecodeFail {
    match e {
        ParseError::Parse(m) if m.starts_with("expected") || m.starts_with("integer out of range") || m.starts_with("invalid base64") => {
            DecodeFail::Mismatch(m)
        }
        ParseError::Parse(m) => DecodeFail::Syntax(m),
        ParseError::Fail(f) => DecodeFail::Fail(f),
    }
}

impl DecodeFormat for TextDecoder {
    fn unit(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        if let Some(key) = self.take_key() {
            return Err(Self::invalid_key(&key));
        }
        self.parse(vm, |p, _| match p.peek() {
            Some(b'n') => p.literal(b"null"),
            _ => Err(Self::mismatch(p, "null")),
        })
    }

    fn bool(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        if let Some(key) = self.take_key() {
            return match &*key {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(Self::invalid_key(&key)),
            };
        }
        self.parse(vm, |p, _| match p.peek() {
            Some(b't') => p.literal(b"true").map(|()| true),
            Some(b'f') => p.literal(b"false").map(|()| false),
            _ => Err(Self::mismatch(p, "bool")),
        })
    }

    fn int(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        if let Some(key) = self.take_key() {
            return key.parse().map_err(|_| Self::invalid_key(&key));
        }
        self.parse(vm, |p, _| Self::int_value(p))
    }

    fn float(&mut self, vm: &mut Vm) -> DecodeResult<f64> {
        if let Some(key) = self.take_key() {
            return Err(self.container_key_for(&key, "float"));
        }
        self.parse(vm, |p, _| Ok(Self::number(p, "float")?.float()))
    }

    fn duration(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        self.int(vm)
    }

    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        if let Some(key) = self.take_key() {
            vm.charge_bytes(key.len())?;
            return Ok(vm.new_str(key));
        }
        self.parse(vm, |p, vm| {
            if p.peek() != Some(b'"') {
                return Err(Self::mismatch(p, "str"));
            }
            let s = p.string(true)?;
            if s.len() > 1024 {
                vm.check_memory(s.len())?;
            }
            Ok(vm.new_str(&*s))
        })
    }

    fn bytes(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        if let Some(key) = self.take_key() {
            return Err(self.container_key_for(&key, "bytes"));
        }
        self.parse(vm, |p, vm| {
            if p.peek() != Some(b'"') {
                return Err(Self::mismatch(p, "base64 str"));
            }
            let s = p.string(true)?;
            let b = base64::decode(&s, false).map_err(|_| ParseError::Parse("invalid base64".into()))?;
            vm.check_memory(b.len())?;
            Ok(vm.heap.new_bytes(b))
        })
    }

    fn native_text(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<String> {
        if let Some(key) = self.take_key() {
            return Ok(key);
        }
        self.parse(vm, |p, _| {
            if p.peek() != Some(b'"') {
                return Err(Self::mismatch(p, name));
            }
            Ok(p.string(true)?.into_owned())
        })
    }

    fn range(&mut self, vm: &mut Vm) -> DecodeResult<(i64, i64, bool)> {
        if let Some(key) = self.take_key() {
            return Err(self.container_key_for(&key, "Range"));
        }
        self.open(vm, b'{', "object")?;
        let (mut start, mut end, mut inclusive) = (None, None, false);
        while let Some(key) = self.next_key(vm)?.map(Cow::into_owned) {
            match &*key {
                "start" => start = Some(self.parse(vm, |p, _| Self::int_value(p))?),
                "end" => end = Some(self.parse(vm, |p, _| Self::int_value(p))?),
                "inclusive" => {
                    inclusive = self.parse(vm, |p, _| match p.peek() {
                        Some(b't') => p.literal(b"true").map(|()| true),
                        Some(b'f') => p.literal(b"false").map(|()| false),
                        _ => Err(Self::mismatch(p, "bool")),
                    })?
                }
                _ => self.skip(vm)?,
            }
        }
        match (start, end) {
            (Some(s), Some(e)) => Ok((s, e, inclusive)),
            (None, _) => Err(DecodeFail::Mismatch("missing field `start`".into())),
            _ => Err(DecodeFail::Mismatch("missing field `end`".into())),
        }
    }

    fn some(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        if self.key.is_some() {
            return Ok(true);
        }
        self.parse(vm, |p, _| {
            if p.peek() == Some(b'n') {
                p.literal(b"null")?;
                return Ok(false);
            }
            Ok(true)
        })
    }

    fn begin_seq(&mut self, vm: &mut Vm, kind: SeqKind) -> DecodeResult<()> {
        if self.key.is_some() {
            return Err(self.container_key(match kind {
                SeqKind::List => "list",
                SeqKind::Set => "Set",
                SeqKind::Tuple(_) => "tuple",
            }));
        }
        let what = match kind {
            SeqKind::Tuple(n) => format!("array of length {n}"),
            _ => "array".into(),
        };
        self.open(vm, b'[', &what)
    }

    fn next_item(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        self.next(vm, b']')
    }

    fn begin_map(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        if self.key.is_some() {
            return Err(self.container_key("map"));
        }
        self.open(vm, b'{', "object")
    }

    fn next_entry(&mut self, vm: &mut Vm) -> DecodeResult<Option<String>> {
        let key = self.next_key(vm)?.map(Cow::into_owned);
        self.key.clone_from(&key);
        Ok(key)
    }

    fn map_value(&mut self, _: &mut Vm) -> DecodeResult<()> {
        self.key = None;
        Ok(())
    }

    fn begin_fields(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()> {
        if self.key.is_some() {
            return Err(self.container_key(if name.is_empty() { "variant" } else { name }));
        }
        self.open(vm, b'{', "object")
    }

    fn next_field(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        self.next_key(vm)
    }

    fn skip(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        self.parse(vm, |p, _| p.skip())
    }

    fn variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)> {
        if let Some(key) = self.take_key() {
            return Ok((Cow::Owned(key), false));
        }
        let is_object = self.parse(vm, |p, _| match p.peek() {
            Some(b'"') => Ok(false),
            Some(b'{') => Ok(true),
            _ => Err(Self::mismatch(p, name)),
        })?;
        if !is_object {
            let tag = self.parse(vm, |p, _| Ok(p.string(true)?.into_owned()))?;
            return Ok((Cow::Owned(tag), false));
        }
        self.open(vm, b'{', name)?;
        match self.next_key(vm)?.map(Cow::into_owned) {
            Some(tag) => Ok((Cow::Owned(tag), true)),
            None => Err(DecodeFail::Mismatch(format!("expected a variant of {name}"))),
        }
    }

    fn end_variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()> {
        if self.next(vm, b'}')? {
            return Err(DecodeFail::Mismatch(format!("expected a single variant of {name}")));
        }
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        if let Some(key) = self.take_key() {
            return Err(self.container_key_for(&key, "Json"));
        }
        let ids = JsonIds::new(&vm.prog)?;
        let (text, pos, depth, charged) = (&self.text, self.pos, self.depth, self.charged);
        let meter = &vm.meter;
        let r = vm.heap.staged(|_, st| {
            let mut p = Parser { src: text, b: text.as_bytes(), pos, depth, charged };
            let mut b = Builder::new(st, meter);
            let r = p.parse_json(&mut b, &ids);
            p.charge(meter)?;
            r.map(|v| (v, p.pos, p.depth))
        });
        let (v, pos, depth) = r.map_err(into_fail)?;
        (self.pos, self.depth, self.charged) = (pos, depth, pos);
        Ok(v)
    }

    fn finish(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        self.parse(vm, |p, _| p.finish())
    }

    fn value(&mut self, vm: &mut Vm) -> DecodeResult<Value> {
        let v = self.json(vm)?;
        Ok(json_value(&json_from_vm(vm, v)?))
    }

    fn mark(&self) -> FormatMark {
        FormatMark(Box::new((self.pos, self.depth, self.charged, self.open.len(), self.open.last().copied(), self.key.clone())))
    }

    fn rollback(&mut self, mark: FormatMark) {
        let Ok(m) = mark.0.downcast::<(usize, usize, usize, usize, Option<Container>, Option<String>)>() else { return };
        let (pos, depth, charged, open, top, key) = *m;
        self.open.truncate(open);
        if let (Some(top), Some(last)) = (top, self.open.last_mut()) {
            *last = top;
        }
        (self.pos, self.depth, self.charged, self.key) = (pos, depth, charged, key);
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }
}

impl TextDecoder {
    /// Returns the error for a map key of a type without a JSON key form.
    fn container_key_for(&mut self, _key: &str, what: &str) -> DecodeFail {
        self.container_key(what)
    }
}

/// A `Json` array or object that [`TreeEncoder`] is building.
enum Node {
    Root(Option<Json>),
    Arr(Vec<Json>),
    /// An object, the struct or variant whose names `field` uses, and the key
    /// of the next member.
    Obj(indexmap::IndexMap<String, Json>, Option<(u32, bool)>, Option<String>),
    /// A map, and the key of the next value once a key is written.
    Map(indexmap::IndexMap<String, Json>, Option<String>),
    /// The outer object of a variant with a payload: the variant's name.
    Wrapper(String),
    /// `Some` and its payload, once written.
    Some(Option<Json>),
}

/// Builds a `json.Json` value.
pub(crate) struct TreeEncoder {
    stack: Vec<Node>,
    /// The number of open arrays and objects.
    depth: usize,
    /// Bytes of Rust memory that the tree holds.
    allocated: usize,
    budget: Budget,
}

impl TreeEncoder {
    pub(crate) fn new() -> TreeEncoder {
        TreeEncoder { stack: vec![Node::Root(None)], depth: 0, allocated: 0, budget: Budget::default() }
    }

    /// Returns the encoded `Json` value.
    pub(crate) fn json_value(&self) -> Result<Json, Fail> {
        match self.stack.as_slice() {
            [Node::Root(Some(json))] => Ok(json.clone()),
            _ => internal("json.to_json: no value"),
        }
    }

    fn reserve(&mut self, vm: &mut Vm, bytes: usize) -> Result<(), Fail> {
        self.budget.grow(vm, self.allocated.saturating_add(bytes))?;
        self.allocated += bytes;
        Ok(())
    }

    /// Adds `json` to the innermost container, or uses it as the key of a map
    /// entry.
    fn push(&mut self, vm: &mut Vm, json: Json) -> Result<(), Fail> {
        self.prepare(vm)?;
        self.place(json)
    }

    /// Charges for a value and counts its memory before any change.
    fn prepare(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        vm.charge(1)?;
        self.reserve(vm, size_of::<Json>() + 16)
    }

    fn place(&mut self, json: Json) -> Result<(), Fail> {
        let mut json = json;
        loop {
            match self.stack.last_mut() {
                Some(Node::Root(root)) => *root = Some(json),
                Some(Node::Arr(items)) => items.push(json),
                Some(Node::Obj(members, _, key)) => {
                    members.insert(key.take().unwrap_or_default(), json);
                }
                Some(Node::Map(members, key)) => match key.take() {
                    Some(k) => {
                        members.insert(k, json);
                    }
                    None => *key = Some(Self::key_text(json)?),
                },
                Some(Node::Wrapper(name)) => {
                    let name = std::mem::take(name);
                    self.stack.pop();
                    self.depth -= 1;
                    json = Json::Obj(indexmap::IndexMap::from([(name, json)]));
                    continue;
                }
                Some(Node::Some(payload)) => *payload = Some(json),
                None => return internal("json.to_json: no open value"),
            }
            return Ok(());
        }
    }

    /// Returns the object key for the map key `key`.
    fn key_text(key: Json) -> Result<String, Fail> {
        match key {
            Json::Str(s) => Ok(s),
            Json::Int(i) => Ok(i.to_string()),
            Json::Bool(b) => Ok(b.to_string()),
            _ => invalid_key_error(),
        }
    }

    fn in_key(&self) -> bool {
        matches!(self.stack.last(), Some(Node::Map(_, None)))
    }

    /// Opens the containers `nodes`, after the checks for all of them.
    fn open(&mut self, vm: &mut Vm, nodes: impl ExactSizeIterator<Item = Node>) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        if self.depth + nodes.len() > MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to convert to JSON");
        }
        self.reserve(vm, 64 * nodes.len())?;
        for node in nodes {
            self.depth += 1;
            self.stack.push(node);
        }
        Ok(())
    }
}

impl EncodeFormat for TreeEncoder {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.push(vm, Json::Null)
    }
    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail> {
        self.push(vm, Json::Bool(b))
    }
    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail> {
        self.push(vm, Json::Int(i))
    }
    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.push(vm, if f.is_finite() { Json::Float(f) } else { Json::Null })
    }
    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail> {
        self.push(vm, Json::Int(nanos))
    }
    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail> {
        vm.charge_bytes(s.len())?;
        self.reserve(vm, s.len())?;
        self.push(vm, Json::Str(s.to_string()))
    }
    fn bytes(&mut self, vm: &mut Vm, b: &[u8]) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        vm.charge_slow_bytes(b.len())?;
        self.str(vm, &base64::encode(b, false))
    }
    fn native(&mut self, vm: &mut Vm, _: &str, text: &str) -> Result<(), Fail> {
        self.str(vm, text)
    }
    fn range(&mut self, vm: &mut Vm, start: i64, end: i64, inclusive: bool) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        let mut m = indexmap::IndexMap::new();
        m.insert("start".to_string(), Json::Int(start));
        m.insert("end".to_string(), Json::Int(end));
        if inclusive {
            m.insert("inclusive".to_string(), Json::Bool(true));
        }
        self.push(vm, Json::Obj(m))
    }
    fn begin_seq(&mut self, vm: &mut Vm, _: SeqKind, len: usize) -> Result<(), Fail> {
        self.open(vm, [Node::Arr(Vec::with_capacity(len.min(1 << 10)))].into_iter())
    }
    fn begin_map(&mut self, vm: &mut Vm, _: usize) -> Result<(), Fail> {
        self.open(vm, [Node::Map(indexmap::IndexMap::new(), None)].into_iter())
    }
    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        self.open(vm, [Node::Obj(indexmap::IndexMap::new(), Some((def, false)), None)].into_iter())
    }
    fn field(&mut self, vm: &mut Vm, index: usize) -> Result<(), Fail> {
        let Some(Node::Obj(_, Some((id, variant)), _)) = self.stack.last() else { return internal("json.to_json: no open struct") };
        let (id, variant) = (*id, *variant);
        let names = if variant {
            vm.prog.variants.get(id as usize).and_then(|v| v.field_names.as_deref())
        } else {
            vm.prog.types.get(id as usize).map(|t| &t.fields[..])
        };
        let Some(name) = names.and_then(|n| n.get(index)).cloned() else { return internal("json.to_json: unknown field") };
        if let Some(Node::Obj(_, _, key)) = self.stack.last_mut() {
            *key = Some(name);
        }
        Ok(())
    }
    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        if variant == vm.prog.ids.none {
            return self.none(vm);
        }
        let Some(name) = vm.prog.variants.get(variant as usize).map(|v| v.name.clone()) else {
            return internal("json.to_json: unknown variant");
        };
        self.push(vm, Json::Str(name))
    }
    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let Some(v) = vm.prog.variants.get(variant as usize) else { return internal("json.to_json: unknown variant") };
        let (name, named) = (v.name.clone(), v.field_names.is_some());
        let inner = if named { Node::Obj(indexmap::IndexMap::new(), Some((variant, true)), None) } else { Node::Arr(Vec::new()) };
        self.open(vm, [Node::Wrapper(name), inner].into_iter())
    }
    fn begin_some(&mut self, _: &mut Vm) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.stack.push(Node::Some(None));
        Ok(())
    }
    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit(vm)
    }
    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.prepare(vm)?;
        let json = match self.stack.pop() {
            Some(Node::Arr(items)) => Json::Arr(items),
            Some(Node::Obj(members, ..) | Node::Map(members, _)) => Json::Obj(members),
            Some(Node::Some(Some(payload))) => return self.place(payload),
            _ => return internal("json.to_json: no open container"),
        };
        self.depth -= 1;
        self.place(json)
    }
    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail> {
        emit_json(vm, json, self)
    }
    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        write_json_form(self, vm, value)
    }
    fn mark(&self) -> FormatMark {
        FormatMark(Box::new((self.stack.len(), self.depth, self.allocated)))
    }

    // A failed write changes only the containers that it opened: it places
    // its value in the container below only when the value ends.
    fn rollback(&mut self, mark: FormatMark) {
        let Ok(m) = mark.0.downcast::<(usize, usize, usize)>() else { return };
        let (stack, depth, allocated) = *m;
        self.stack.truncate(stack);
        (self.depth, self.allocated) = (depth, allocated);
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
