//! The JSON text format of `Encoder` and `Decoder`.

mod read;
mod walk;

use super::{Builder, Error as ParseError, Json, JsonIds, Parser, SPACES, base64, json_from_vm, write_str};
use crate::codec::MAX_NESTING_DEPTH;
use crate::codec::value_format::write_json_form;
use crate::codec::{Budget, DecodeFail, DecodeFormat, DecodeResult, EncodeFormat, SeqKind};
use crate::value::Value;
use crate::vm::bytecode::Program;
use crate::vm::compare::str_hash;
use crate::vm::meter::{BYTES_PER_UNIT, SLOW_BYTES_PER_UNIT};
use crate::vm::value::{FxHashMap, KeyIndex, MAP_ENTRY_SIZE, MapObj, Obj, ObjectValues, VALUE_SIZE, VmValue, slot_fits};
use crate::vm::{Fail, Vm, fail, internal};
use compact_str::CompactString;
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

/// The state of a [`TextEncoder`] that [`EncodeFormat::save`] keeps.
struct EncoderState {
    out: usize,
    charged: usize,
    open: usize,
    top: Option<Open>,
    depth: usize,
}

/// The bytes of output that a new [`TextEncoder`] holds without growing.
/// The `str` that `json.encode` returns keeps this capacity, which the
/// heap counts, so it is the size that doubling from 8 bytes would give a
/// small text.
const SMALL_TEXT: usize = 64;

/// Writes JSON text.
pub(crate) struct TextEncoder {
    indent: usize,
    out: String,
    /// Bytes of `out` already charged to the meter.
    charged: usize,
    open: Vec<Open>,
    /// The number of open arrays and objects.
    depth: usize,
    saved: Option<EncoderState>,
    budget: Budget,
}

impl TextEncoder {
    pub(crate) fn new(indent: usize) -> TextEncoder {
        TextEncoder {
            indent,
            out: String::with_capacity(SMALL_TEXT),
            charged: 0,
            open: Vec::with_capacity(8),
            depth: 0,
            saved: None,
            budget: Budget::default(),
        }
    }

    /// Returns the text written, and the bytes that the heap counted for
    /// it (see [`Budget::release`]).
    pub(crate) fn take_text(&mut self) -> (String, usize) {
        (std::mem::take(&mut self.out), self.budget.release())
    }

    /// Counts `extra` more bytes of output, and the indentation of a new line,
    /// against the memory limit.
    #[inline(always)]
    fn room(&mut self, vm: &mut Vm, extra: usize) -> Result<(), Fail> {
        let line = if self.indent > 0 { 1 + self.indent * (self.depth + 1) } else { 0 };
        self.budget.grow(vm, self.out.len().saturating_add(extra).saturating_add(line + 2))
    }

    #[inline(always)]
    fn charge_written(&mut self, vm: &Vm) -> Result<(), Fail> {
        let written = self.out.len() - self.charged;
        self.charged = self.out.len();
        match written >= BYTES_PER_UNIT {
            true => vm.charge_bytes(written),
            false => Ok(()),
        }
    }

    fn in_key(&self) -> bool {
        matches!(self.open.last(), Some(Open { bracket: Bracket::Map(true), .. }))
    }

    /// Writes a scalar that `text` holds: as a value, or as a map key.
    #[inline(always)]
    fn scalar(&mut self, vm: &mut Vm, text: &str, quote: bool) -> Result<(), Fail> {
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
    #[inline(always)]
    fn value_only(&mut self, vm: &mut Vm, extra: usize) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.room(vm, extra)?;
        self.begin_member(None);
        Ok(())
    }

    /// Writes the separator and the key of the next member of the innermost
    /// array or object, and updates a map's key state.
    #[inline(always)]
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

    /// Starts a new line with the indentation of the current depth, if
    /// the text is indented.
    #[inline(always)]
    fn start_line(&mut self) {
        if self.indent > 0 {
            self.new_line();
        }
    }

    fn new_line(&mut self) {
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
    fn write_fast(&mut self, vm: &mut Vm, value: VmValue, schema: crate::codec::fast::SchemaId) -> Result<(), Fail> {
        self.write_schema(vm, value, schema)
    }

    #[inline(always)]
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.value_only(vm, 4)?;
        self.out.push_str("null");
        self.charge_written(vm)
    }

    #[inline(always)]
    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail> {
        self.scalar(vm, if b { "true" } else { "false" }, false)
    }

    #[inline(always)]
    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail> {
        self.scalar(vm, itoa::Buffer::new().format(i), false)
    }

    #[inline(always)]
    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail> {
        self.value_only(vm, 32)?;
        if f.is_finite() {
            _ = write!(self.out, "{f:?}");
        } else {
            self.out.push_str("null");
        }
        self.charge_written(vm)
    }

    #[inline(always)]
    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail> {
        self.int(vm, nanos)
    }

    #[inline(always)]
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

    #[inline(always)]
    fn begin_seq(&mut self, vm: &mut Vm, _: SeqKind, _: usize) -> Result<(), Fail> {
        self.value_only(vm, 1)?;
        self.open_bracket(Bracket::Array, None)
    }

    #[inline(always)]
    fn begin_map(&mut self, vm: &mut Vm, _: usize) -> Result<(), Fail> {
        self.value_only(vm, 1)?;
        self.open_bracket(Bracket::Map(true), None)
    }

    #[inline(always)]
    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        self.value_only(vm, 1)?;
        self.open_bracket(Bracket::Fields, Some((def, false)))
    }

    #[inline(always)]
    fn field(&mut self, vm: &mut Vm, index: usize) -> Result<(), Fail> {
        let Some(&Open { names: Some((id, variant)), .. }) = self.open.last() else { return internal("json.encode: no open struct") };
        let Some(len) = field_name(&vm.prog, id, variant, index).map(str::len) else { return internal("json.encode: unknown field") };
        self.room(vm, len * 6 + 4)?;
        self.begin_member(field_name(&vm.prog, id, variant, index));
        Ok(())
    }

    #[inline(always)]
    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        if variant == vm.prog.ids.none {
            return self.none(vm);
        }
        let prog = vm.prog.clone();
        let Some(v) = prog.variants.get(variant as usize) else { return internal("json.encode: unknown variant") };
        self.scalar(vm, &v.name, true)
    }

    #[inline(always)]
    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let prog = vm.prog.clone();
        let Some(v) = prog.variants.get(variant as usize) else { return internal("json.encode: unknown variant") };
        let (name, named) = (&v.name, v.field_names.is_some());
        self.value_only(vm, name.len() * 6 + 8 + 2 * self.indent * (self.depth + 2))?;
        self.open_bracket(Bracket::VariantWrapper, None)?;
        // The wrapper has one member: the variant name.
        if let Some(top) = self.open.last_mut() {
            top.is_empty = false;
        }
        self.start_line();
        write_str(&mut self.out, name);
        self.out.push(':');
        if self.indent > 0 {
            self.out.push(' ');
        }
        if named { self.open_bracket(Bracket::Fields, Some((variant, true))) } else { self.open_bracket(Bracket::Array, None) }
    }

    #[inline(always)]
    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.room(vm, 1)?;
        self.begin_member(None);
        self.open_bracket(Bracket::Some, None)
    }

    #[inline(always)]
    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit(vm)
    }

    #[inline(always)]
    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.room(vm, 2)?;
        self.close_bracket()?;
        self.charge_written(vm)
    }

    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail> {
        self.write_json_value(vm, json)
    }

    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        write_json_form(self, vm, value)
    }

    fn save(&mut self) {
        let (out, charged, open, top, depth) = (self.out.len(), self.charged, self.open.len(), self.open.last().copied(), self.depth);
        self.saved = Some(EncoderState { out, charged, open, top, depth });
    }

    fn restore(&mut self) {
        let Some(s) = self.saved.take() else { return };
        self.out.truncate(s.out);
        self.open.truncate(s.open);
        if let (Some(top), Some(last)) = (s.top, self.open.last_mut()) {
            *last = top;
        }
        (self.charged, self.depth) = (s.charged, s.depth);
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }
}

/// Returns the name of field `index` of struct definition `id`, or of the
/// named payload of variant `id`.
#[inline(always)]
pub(super) fn field_name(prog: &Program, id: u32, variant: bool, index: usize) -> Option<&str> {
    let names = match variant {
        true => prog.variants.get(id as usize)?.field_names.as_deref()?,
        false => &prog.types.get(id as usize)?.fields[..],
    };
    names.get(index).map(String::as_str)
}

/// Writes the Sand `Json` value `root` to `f` with the format's operations,
/// and charges a fuel unit for each value.
pub(super) fn emit_json<F: EncodeFormat + ?Sized>(vm: &mut Vm, root: VmValue, f: &mut F) -> Result<(), Fail> {
    /// An open array or object, the index of its next element or entry
    /// slot, and the value of the entry whose key was written last.
    enum Open {
        Arr(VmValue, usize),
        Obj(VmValue, usize, Option<VmValue>),
    }
    let ids = JsonIds::new(&vm.prog)?;
    let mut stack: Vec<Open> = Vec::new();
    let mut next = Some(root);
    loop {
        if let Some(v) = next.take() {
            vm.charge(1)?;
            let id = vm.heap.variant_id(v);
            match vm.heap.field(v, 0) {
                _ if id == ids.null => f.unit(vm)?,
                Some(VmValue::Bool(b)) if id == ids.bool_ => f.bool(vm, b)?,
                Some(VmValue::Int(i)) if id == ids.int => f.int(vm, i)?,
                Some(VmValue::Float(x)) if id == ids.float => f.float(vm, x)?,
                Some(p) if id == ids.str_ => {
                    let s = heap_text(vm, p);
                    f.str(vm, &s)?;
                }
                Some(p) if id == ids.arr => {
                    let len = vm.heap.list(p).len();
                    f.begin_seq(vm, SeqKind::List, len)?;
                    stack.push(Open::Arr(p, 0));
                }
                Some(p) if id == ids.obj => {
                    let Obj::Map(m) = vm.heap.get(p) else { return internal("json: not an object") };
                    f.begin_map(vm, m.live)?;
                    stack.push(Open::Obj(p, 0, None));
                }
                _ => return internal("json: not a Json value"),
            }
        }
        let Some(top) = stack.last_mut() else { return Ok(()) };
        match top {
            Open::Arr(list, i) => match vm.heap.list(*list).get(*i) {
                Some(&x) => {
                    *i += 1;
                    next = Some(x);
                }
                None => {
                    f.end(vm)?;
                    stack.pop();
                }
            },
            Open::Obj(map, i, value) => {
                if let Some(x) = value.take() {
                    next = Some(x);
                    continue;
                }
                let Obj::Map(m) = vm.heap.get(*map) else { return internal("json: not an object") };
                match m.entries.get(*i..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((k, (*e)?)))) {
                    Some((k, (key, x))) => {
                        *i += k + 1;
                        *value = Some(x);
                        let key = heap_text(vm, key);
                        f.str(vm, &key)?;
                    }
                    None => {
                        f.end(vm)?;
                        stack.pop();
                    }
                }
            }
        }
    }
}

/// Returns a copy of the text of the Sand `str` `v`, which needs no
/// allocation for a short text.
fn heap_text(vm: &Vm, v: VmValue) -> CompactString {
    match vm.heap.get(v) {
        Obj::Str(s) => s.clone(),
        _ => vm.heap.str(v).into(),
    }
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

/// The text of the map key that the next operations of [`TextDecoder`]
/// read: a span of the input, or the unescaped text of a key with escapes.
#[derive(Clone)]
enum KeyText {
    Span(usize, usize),
    Owned(String),
}

/// The state of a [`TextDecoder`] that [`DecodeFormat::save`] keeps.
#[derive(Clone)]
struct DecoderState {
    pos: usize,
    depth: usize,
    charged: usize,
    open: usize,
    top: Option<Container>,
    key: Option<KeyText>,
}

/// Reads JSON text.
pub(crate) struct TextDecoder {
    /// The input, which the Sand `str` lends to the decoder for the length
    /// of one native call (see [`DecodeFormat::swap_input`]).
    text: CompactString,
    pos: usize,
    depth: usize,
    /// The input up to this position has been charged as fuel.
    charged: usize,
    open: Vec<Container>,
    /// The key of the map entry that the next operations read.
    key: Option<KeyText>,
    saved: Option<DecoderState>,
}

impl TextDecoder {
    /// Returns a decoder at the start of the input that
    /// [`DecodeFormat::swap_input`] lends to it.
    pub(crate) fn new() -> TextDecoder {
        TextDecoder { text: CompactString::default(), pos: 0, depth: 0, charged: 0, open: Vec::with_capacity(16), key: None, saved: None }
    }

    /// Runs `f` on a parser at the current position. On an error, the
    /// position does not move, so a run of the native function again starts
    /// from the same input.
    #[inline(always)]
    fn parse<R>(&mut self, vm: &mut Vm, f: impl FnOnce(&mut Parser<'_>, &mut Vm) -> Result<R, ParseError>) -> DecodeResult<R> {
        let mut p = Parser { src: &self.text, b: self.text.as_bytes(), pos: self.pos, depth: self.depth, charged: self.charged };
        p.ws();
        let r = f(&mut p, vm);
        if let Err(ParseError::Fail(_)) = r {
            return r.map_err(into_fail);
        }
        (self.pos, self.depth) = (p.pos, p.depth);
        r.map_err(into_fail)
    }

    /// Returns the error for a value that does not match `what`.
    fn mismatch(p: &Parser<'_>, what: &str) -> ParseError {
        match p.peek() {
            Some(b'n' | b't' | b'f' | b'"' | b'-' | b'0'..=b'9' | b'[' | b'{') => ParseError::Mismatch(format!("expected {what}")),
            _ => p.unexpected(),
        }
    }

    /// Returns the text of `key`.
    fn key_str<'s>(&'s self, key: &'s KeyText) -> &'s str {
        match key {
            &KeyText::Span(start, end) => self.text.get(start..end).unwrap_or_default(),
            KeyText::Owned(s) => s,
        }
    }

    fn invalid_key(key: &str) -> DecodeFail {
        DecodeFail::Mismatch(format!("invalid map key `{}`", crate::stdlib::truncate(key, crate::codec::QUOTED_KEY)))
    }

    /// Returns the error for a map key of a type without a JSON key form.
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
            ParseError::Mismatch(if f.is_finite() && f.fract() == 0.0 { "integer out of range" } else { "expected int" }.into())
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
    #[inline(always)]
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
    #[inline(always)]
    fn next_key(&mut self, vm: &mut Vm) -> DecodeResult<Option<KeyText>> {
        if !self.next(vm, b'}')? {
            return Ok(None);
        }
        let key = self.parse(vm, |p, _| {
            if p.peek() != Some(b'"') {
                return Err(p.unexpected());
            }
            let start = p.pos + 1;
            let key = match p.string(true)? {
                Cow::Borrowed(_) => KeyText::Span(start, p.pos - 1),
                Cow::Owned(s) => KeyText::Owned(s),
            };
            p.ws();
            if !p.eat(b':') {
                return Err(p.unexpected());
            }
            Ok(key)
        })?;
        Ok(Some(key))
    }

    /// Returns `key` as a `Cow` of the text.
    fn key_cow(&self, key: KeyText) -> Cow<'_, str> {
        match key {
            KeyText::Span(start, end) => Cow::Borrowed(self.text.get(start..end).unwrap_or_default()),
            KeyText::Owned(s) => Cow::Owned(s),
        }
    }
}

fn into_fail(e: ParseError) -> DecodeFail {
    match e {
        ParseError::Mismatch(m) => DecodeFail::Mismatch(m),
        ParseError::Parse(m) => DecodeFail::Syntax(m),
        ParseError::Fail(f) => DecodeFail::Fail(f),
    }
}

impl DecodeFormat for TextDecoder {
    fn read_fast(
        &mut self,
        vm: &mut Vm,
        path: &mut crate::codec::DecodePath,
        schema: crate::codec::fast::SchemaId,
    ) -> DecodeResult<VmValue> {
        let fuel = vm.meter.fuel();
        match self.read_schema(vm, schema) {
            Ok(v) => Ok(v),
            Err(read::Fallback::Limit(f)) => Err(f.into()),
            // The general walker reads the value again, with the exact
            // error and path, and charges as if the read had not run.
            Err(read::Fallback::General) => {
                vm.meter.refund_to(fuel);
                crate::codec::read_fast_general(vm, self, path, schema)
            }
        }
    }

    #[inline]
    fn unit(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        if let Some(key) = self.key.take() {
            return Err(Self::invalid_key(self.key_str(&key)));
        }
        self.parse(vm, |p, _| match p.peek() {
            Some(b'n') => p.literal(b"null"),
            _ => Err(Self::mismatch(p, "null")),
        })
    }

    #[inline]
    fn bool(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        if let Some(key) = self.key.take() {
            return match self.key_str(&key) {
                "true" => Ok(true),
                "false" => Ok(false),
                key => Err(Self::invalid_key(key)),
            };
        }
        self.parse(vm, |p, _| match p.peek() {
            Some(b't') => p.literal(b"true").map(|()| true),
            Some(b'f') => p.literal(b"false").map(|()| false),
            _ => Err(Self::mismatch(p, "bool")),
        })
    }

    #[inline]
    fn int(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        if let Some(key) = self.key.take() {
            let key = self.key_str(&key);
            return key.parse().map_err(|_| Self::invalid_key(key));
        }
        self.parse(vm, |p, _| Self::int_value(p))
    }

    #[inline]
    fn float(&mut self, vm: &mut Vm) -> DecodeResult<f64> {
        if self.key.is_some() {
            return Err(self.container_key("float"));
        }
        self.parse(vm, |p, _| Ok(Self::number(p, "float")?.float()))
    }

    #[inline]
    fn duration(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        self.int(vm)
    }

    #[inline]
    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        if let Some(key) = self.key.take() {
            let key = self.key_str(&key);
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
        if self.key.is_some() {
            return Err(self.container_key("bytes"));
        }
        self.parse(vm, |p, vm| {
            if p.peek() != Some(b'"') {
                return Err(Self::mismatch(p, "base64 str"));
            }
            let s = p.string(true)?;
            let b = base64::decode(&s, false).map_err(|_| ParseError::Mismatch("invalid base64".into()))?;
            vm.check_memory(b.len())?;
            Ok(vm.heap.new_bytes(b))
        })
    }

    fn native_text(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<String> {
        if let Some(key) = self.key.take() {
            return Ok(self.key_str(&key).to_owned());
        }
        self.parse(vm, |p, _| {
            if p.peek() != Some(b'"') {
                return Err(Self::mismatch(p, name));
            }
            Ok(p.string(true)?.into_owned())
        })
    }

    fn range(&mut self, vm: &mut Vm) -> DecodeResult<(i64, i64, bool)> {
        if self.key.is_some() {
            return Err(self.container_key("Range"));
        }
        self.open(vm, b'{', "object")?;
        let (mut start, mut end, mut inclusive) = (None, None, false);
        while let Some(key) = self.next_key(vm)? {
            match self.key_str(&key) {
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

    #[inline]
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

    #[inline]
    fn begin_seq(&mut self, vm: &mut Vm, kind: SeqKind) -> DecodeResult<()> {
        if self.key.is_some() {
            return Err(self.container_key(match kind {
                SeqKind::List => "list",
                SeqKind::Set => "Set",
                SeqKind::Tuple(_) => "tuple",
            }));
        }
        match kind {
            SeqKind::Tuple(n) => self.open(vm, b'[', &format!("array of length {n}")),
            _ => self.open(vm, b'[', "array"),
        }
    }

    #[inline]
    fn next_item(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        self.next(vm, b']')
    }

    #[inline]
    fn begin_map(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        if self.key.is_some() {
            return Err(self.container_key("map"));
        }
        self.open(vm, b'{', "object")
    }

    #[inline]
    fn next_entry(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        let key = self.next_key(vm)?;
        self.key.clone_from(&key);
        Ok(key.map(|key| self.key_cow(key)))
    }

    #[inline]
    fn map_value(&mut self, _: &mut Vm) -> DecodeResult<()> {
        self.key = None;
        Ok(())
    }

    #[inline]
    fn begin_fields(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()> {
        if self.key.is_some() {
            return Err(self.container_key(if name.is_empty() { "variant" } else { name }));
        }
        self.open(vm, b'{', "object")
    }

    #[inline]
    fn next_field(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        let key = self.next_key(vm)?;
        Ok(key.map(|key| self.key_cow(key)))
    }

    fn skip(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        self.parse(vm, |p, _| p.skip())
    }

    #[inline]
    fn variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)> {
        if let Some(key) = self.key.take() {
            return Ok((self.key_cow(key), false));
        }
        let is_object = self.parse(vm, |p, _| match p.peek() {
            Some(b'"') => Ok(false),
            Some(b'{') => Ok(true),
            _ => Err(Self::mismatch(p, name)),
        })?;
        if !is_object {
            let tag = self.parse(vm, |p, _| {
                let start = p.pos + 1;
                Ok(match p.string(true)? {
                    Cow::Borrowed(_) => KeyText::Span(start, p.pos - 1),
                    Cow::Owned(s) => KeyText::Owned(s),
                })
            })?;
            return Ok((self.key_cow(tag), false));
        }
        self.open(vm, b'{', name)?;
        match self.next_key(vm)? {
            Some(tag) => Ok((self.key_cow(tag), true)),
            None => Err(DecodeFail::Mismatch(format!("expected a variant of {name}"))),
        }
    }

    #[inline]
    fn end_variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()> {
        if self.next(vm, b'}')? {
            return Err(DecodeFail::Mismatch(format!("expected a single variant of {name}")));
        }
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        if self.key.is_some() {
            return Err(self.container_key("Json"));
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

    #[inline]
    fn take_input_fuel(&mut self) -> u64 {
        let read = self.pos.saturating_sub(self.charged);
        self.charged = self.pos;
        (read / SLOW_BYTES_PER_UNIT) as u64
    }

    fn save(&mut self) {
        let (pos, depth, charged, open, top) = (self.pos, self.depth, self.charged, self.open.len(), self.open.last().copied());
        self.saved = Some(DecoderState { pos, depth, charged, open, top, key: self.key.clone() });
    }

    fn restore(&mut self) {
        let Some(s) = self.saved.take() else { return };
        self.open.truncate(s.open);
        if let (Some(top), Some(last)) = (s.top, self.open.last_mut()) {
            *last = top;
        }
        (self.pos, self.depth, self.charged, self.key) = (s.pos, s.depth, s.charged, s.key);
    }

    fn heap_size(&self) -> usize {
        self.open.capacity() * size_of::<Container>()
    }

    fn swap_input(&mut self, text: &mut CompactString) {
        std::mem::swap(&mut self.text, text);
    }
}

/// An open container of a [`TreeEncoder`], with the index of its first part
/// in the encoder's values.
#[derive(Clone, Copy)]
enum TreeOpen {
    Arr(usize),
    /// An object whose member keys come from `field` calls on the struct or
    /// the variant `names`, or from the bounds of a `Range`.
    Fields(usize, Option<(u32, bool)>),
    /// A map, and `true` while the next value is a key.
    Map(usize, bool),
    /// The outer object of a variant with a payload: the variant name, then
    /// the payload.
    VariantWrapper(usize),
    /// `Some`, which writes only its payload.
    Some,
}

/// The state of a [`TreeEncoder`] that [`EncodeFormat::save`] keeps.
#[derive(Clone, Copy)]
struct TreeState {
    values: usize,
    open: usize,
    top: Option<TreeOpen>,
    depth: usize,
}

/// Builds a `json.Json` value as the encode runs.
pub(crate) struct TreeEncoder {
    /// The finished parts of the open containers, and then the finished
    /// value. The `Encoder` keeps them, where the collector traces them, and
    /// lends them to the format for each native call (see
    /// [`EncodeFormat::swap_values`]).
    values: ObjectValues,
    open: Vec<TreeOpen>,
    /// The number of open arrays and objects.
    depth: usize,
    saved: Option<TreeState>,
    /// The key `str` of each field and variant name that this native call
    /// made.
    names: FxHashMap<(u32, bool, u32), VmValue>,
    /// The hashes of the texts of those keys, by heap handle.
    name_hashes: FxHashMap<u32, u64>,
    /// The variant ids of `Json`, from the first call.
    ids: Option<JsonIds>,
    budget: Budget,
}

impl TreeEncoder {
    pub(crate) fn new() -> TreeEncoder {
        TreeEncoder {
            values: ObjectValues::new(),
            open: Vec::new(),
            depth: 0,
            saved: None,
            names: FxHashMap::default(),
            name_hashes: FxHashMap::default(),
            ids: None,
            budget: Budget::default(),
        }
    }

    /// Returns the encoded value.
    pub(crate) fn encoded(&self) -> Result<VmValue, Fail> {
        match (self.open.is_empty(), self.values.as_slice()) {
            (true, &[root]) => Ok(root),
            _ => internal("json.to_json: no value"),
        }
    }

    /// Stops counting the values that the encoder holds, after a native
    /// call that kept the encoder on the Rust stack.
    pub(crate) fn release(self, vm: &mut Vm) {
        // `room` counted the growth past the inline slot.
        let grown = self.values.capacity() - self.values.inline_size();
        vm.heap.account(grown * VALUE_SIZE + self.budget.counted(), 0);
    }

    fn in_key(&self) -> bool {
        matches!(self.open.last(), Some(TreeOpen::Map(_, true)))
    }

    fn state(&self) -> TreeState {
        TreeState { values: self.values.len(), open: self.open.len(), top: self.open.last().copied(), depth: self.depth }
    }

    fn rewind(&mut self, s: TreeState) {
        self.values.truncate(s.values);
        self.open.truncate(s.open);
        if let (Some(top), Some(last)) = (s.top, self.open.last_mut()) {
            *last = top;
        }
        self.depth = s.depth;
    }

    /// Runs `op`, which makes more than one change, and goes back to the
    /// state before it if it fails, so that the native function can run
    /// again.
    fn atomic(&mut self, vm: &mut Vm, op: impl FnOnce(&mut Self, &mut Vm) -> Result<(), Fail>) -> Result<(), Fail> {
        let before = self.state();
        let r = op(self, vm);
        if r.is_err() {
            self.rewind(before);
        }
        r
    }

    /// Notes that a value or a key of the innermost container ended.
    #[inline(always)]
    fn item_done(&mut self) {
        if let Some(TreeOpen::Map(_, is_key)) = self.open.last_mut() {
            *is_key = !*is_key;
        }
    }

    /// Counts room for one more value against the memory limit, before any
    /// change.
    #[inline(always)]
    fn room(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.values.len() < self.values.capacity() {
            return Ok(());
        }
        let (before, after) = (self.values.capacity(), self.values.capacity().max(4) * 2);
        vm.check_memory((after - before) * VALUE_SIZE)?;
        vm.heap.account(before * VALUE_SIZE, after * VALUE_SIZE);
        self.values.reserve_exact(after - self.values.len());
        Ok(())
    }

    /// Adds the finished part `v`, after [`TreeEncoder::room`].
    #[inline(always)]
    fn put(&mut self, v: VmValue) {
        self.values.push(v);
        self.item_done();
    }

    /// Adds the scalar `v`, which cannot be a map key.
    #[inline(always)]
    fn value(&mut self, vm: &mut Vm, v: VmValue) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.room(vm)?;
        self.put(v);
        Ok(())
    }

    #[inline(always)]
    fn ids(&mut self, vm: &Vm) -> Result<JsonIds, Fail> {
        match self.ids {
            Some(ids) => Ok(ids),
            None => Ok(*self.ids.insert(JsonIds::new(&vm.prog)?)),
        }
    }

    /// Adds the `str` `s`, as a `Json.Str` or as a map key.
    fn str_part(&mut self, vm: &mut Vm, s: VmValue) -> Result<(), Fail> {
        let str_ = self.ids(vm)?.str_;
        let v = if self.in_key() { s } else { vm.heap.new_variant(str_, &[s]) };
        self.room(vm)?;
        self.put(v);
        Ok(())
    }

    /// Adds a new `str` with the text `text`, as a `Json.Str` or as a map key.
    fn text(&mut self, vm: &mut Vm, text: &str) -> Result<(), Fail> {
        vm.charge_bytes(text.len())?;
        vm.check_memory(size_of::<Obj>() + text.len())?;
        let s = vm.heap.new_str(text);
        self.str_part(vm, s)
    }

    /// Returns the key `str` of field `index` of `owner`, or of the name of
    /// variant `owner` if `index` is `u32::MAX`.
    fn name(&mut self, vm: &mut Vm, owner: u32, variant: bool, index: u32) -> Result<VmValue, Fail> {
        if let Some(&s) = self.names.get(&(owner, variant, index)) {
            return Ok(s);
        }
        let text = match index {
            u32::MAX => vm.prog.variants.get(owner as usize).map(|v| v.name.clone()),
            _ => field_name(&vm.prog, owner, variant, index as usize).map(str::to_string),
        };
        let Some(text) = text else { return internal("json.to_json: unknown field or variant") };
        vm.check_memory(size_of::<Obj>() + text.len())?;
        vm.charge_bytes(text.len())?;
        let hash = str_hash(&text);
        let s = vm.heap.new_str(text);
        self.names.insert((owner, variant, index), s);
        if let Some(h) = s.handle() {
            self.name_hashes.insert(h, hash);
        }
        Ok(s)
    }

    /// Opens `open`, after the checks.
    fn begin(&mut self, vm: &mut Vm, open: TreeOpen) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        if self.depth >= MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to convert to JSON");
        }
        self.budget.grow(vm, (self.open.len() + 1) * size_of::<TreeOpen>())?;
        self.open.push(open);
        self.depth += 1;
        Ok(())
    }

    /// Makes the `Json` array or object of the parts that `open` holds.
    fn finish(&mut self, vm: &mut Vm, open: TreeOpen) -> Result<VmValue, Fail> {
        let ids = self.ids(vm)?;
        match open {
            TreeOpen::Arr(base) => {
                let items = &self.values[base..];
                vm.check_memory(size_of::<Obj>() + items.len() * VALUE_SIZE)?;
                let list = vm.heap.new_list(items.to_vec());
                Ok(vm.heap.new_variant(ids.arr, &[list]))
            }
            TreeOpen::Fields(base, _) | TreeOpen::Map(base, _) | TreeOpen::VariantWrapper(base) => {
                let map = self.object(vm, base, !matches!(open, TreeOpen::Map(..)))?;
                Ok(vm.heap.new_variant(ids.obj, &[map]))
            }
            TreeOpen::Some => internal("json.to_json: no open container"),
        }
    }

    /// Makes the map of the key and value pairs from `base`. A later pair
    /// replaces an earlier one with the same key, unless the keys are
    /// `distinct`.
    fn object(&self, vm: &mut Vm, base: usize, distinct: bool) -> Result<VmValue, Fail> {
        let members = &self.values[base..];
        let n = members.len() / 2;
        vm.check_memory(size_of::<Obj>() + n * (MAP_ENTRY_SIZE + 2 * VALUE_SIZE))?;
        let keys = members.iter().step_by(2).copied();
        let mut map = MapObj { entries: Vec::with_capacity(n), index: KeyIndex::for_keys(n, keys), ..MapObj::default() };
        for &[key, value] in members.as_chunks::<2>().0 {
            let hash = match key.handle().and_then(|h| self.name_hashes.get(&h)) {
                Some(&hash) => hash,
                None => {
                    let text = vm.heap.str(key);
                    vm.charge_bytes(text.len())?;
                    str_hash(text)
                }
            };
            if !distinct {
                let (heap, entries) = (&vm.heap, &map.entries);
                let same = |slot: usize| entries[slot].is_some_and(|(k, _)| heap.str(k) == heap.str(key));
                if let Some(slot) = map.index.find(key, hash, same) {
                    map.entries[slot] = Some((key, value));
                    continue;
                }
            }
            let slot = map.entries.len();
            if !slot_fits(slot) {
                return fail("limit", "map too large");
            }
            map.index.insert(key, hash, slot);
            map.entries.push(Some((key, value)));
        }
        map.live = map.entries.len();
        Ok(vm.heap.alloc(Obj::Map(Box::new(map))))
    }
}

impl EncodeFormat for TreeEncoder {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        let null = self.ids(vm)?.null;
        self.value(vm, VmValue::Tag(null))
    }

    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail> {
        if self.in_key() {
            return self.text(vm, if b { "true" } else { "false" });
        }
        let v = vm.heap.new_variant(self.ids(vm)?.bool_, &[VmValue::Bool(b)]);
        self.value(vm, v)
    }

    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail> {
        if self.in_key() {
            return self.text(vm, itoa::Buffer::new().format(i));
        }
        let v = vm.heap.new_variant(self.ids(vm)?.int, &[VmValue::Int(i)]);
        self.value(vm, v)
    }

    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail> {
        if !f.is_finite() {
            return self.unit(vm);
        }
        let v = vm.heap.new_variant(self.ids(vm)?.float, &[VmValue::Float(f)]);
        self.value(vm, v)
    }

    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail> {
        self.int(vm, nanos)
    }

    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail> {
        self.text(vm, s)
    }

    fn keeps_values(&self) -> bool {
        true
    }

    fn str_value(&mut self, vm: &mut Vm, s: VmValue) -> Result<(), Fail> {
        vm.charge_bytes(vm.heap.str(s).len())?;
        self.str_part(vm, s)
    }

    fn bytes(&mut self, vm: &mut Vm, b: &[u8]) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        vm.charge_slow_bytes(b.len())?;
        self.text(vm, &base64::encode(b, false))
    }

    fn native(&mut self, vm: &mut Vm, _: &str, text: &str) -> Result<(), Fail> {
        self.text(vm, text)
    }

    fn range(&mut self, vm: &mut Vm, start: i64, end: i64, inclusive: bool) -> Result<(), Fail> {
        self.atomic(vm, |f, vm| {
            f.begin(vm, TreeOpen::Fields(f.values.len(), None))?;
            let members = [("start", start, false), ("end", end, false)];
            for (name, bound, is_bool) in members.into_iter().chain(inclusive.then_some(("inclusive", 1, true))) {
                vm.charge_bytes(name.len())?;
                vm.check_memory(size_of::<Obj>() + name.len())?;
                let key = vm.heap.new_str(name);
                f.room(vm)?;
                f.values.push(key);
                let ids = f.ids(vm)?;
                let v = match is_bool {
                    true => vm.heap.new_variant(ids.bool_, &[VmValue::Bool(true)]),
                    false => vm.heap.new_variant(ids.int, &[VmValue::Int(bound)]),
                };
                f.room(vm)?;
                f.values.push(v);
            }
            f.end(vm)
        })
    }

    fn begin_seq(&mut self, vm: &mut Vm, _: SeqKind, _: usize) -> Result<(), Fail> {
        self.begin(vm, TreeOpen::Arr(self.values.len()))
    }

    fn begin_map(&mut self, vm: &mut Vm, _: usize) -> Result<(), Fail> {
        self.begin(vm, TreeOpen::Map(self.values.len(), true))
    }

    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        self.begin(vm, TreeOpen::Fields(self.values.len(), Some((def, false))))
    }

    fn field(&mut self, vm: &mut Vm, index: usize) -> Result<(), Fail> {
        let Some(&TreeOpen::Fields(_, Some((owner, variant)))) = self.open.last() else { return internal("json.to_json: no open struct") };
        let key = self.name(vm, owner, variant, index as u32)?;
        self.room(vm)?;
        self.values.push(key);
        Ok(())
    }

    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        if variant == vm.prog.ids.none {
            return self.none(vm);
        }
        let s = self.name(vm, variant, true, u32::MAX)?;
        self.str_part(vm, s)
    }

    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let Some(named) = vm.prog.variants.get(variant as usize).map(|v| v.field_names.is_some()) else {
            return internal("json.to_json: unknown variant");
        };
        if self.depth + 2 > MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to convert to JSON");
        }
        let key = self.name(vm, variant, true, u32::MAX)?;
        self.atomic(vm, |f, vm| {
            f.room(vm)?;
            f.begin(vm, TreeOpen::VariantWrapper(f.values.len()))?;
            f.values.push(key);
            match named {
                true => f.begin(vm, TreeOpen::Fields(f.values.len(), Some((variant, true)))),
                false => f.begin(vm, TreeOpen::Arr(f.values.len())),
            }
        })
    }

    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.budget.grow(vm, (self.open.len() + 1) * size_of::<TreeOpen>())?;
        self.open.push(TreeOpen::Some);
        Ok(())
    }

    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit(vm)
    }

    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        let Some(&top) = self.open.last() else { return internal("json.to_json: no open container") };
        if let TreeOpen::Some = top {
            self.open.pop();
            self.item_done();
            return Ok(());
        }
        // Builds the value, and the outer object of a variant, before any
        // change.
        self.room(vm)?;
        let mut v = self.finish(vm, top)?;
        let mut closed = 1;
        let mut base = match top {
            TreeOpen::Arr(base) | TreeOpen::Fields(base, _) | TreeOpen::Map(base, _) | TreeOpen::VariantWrapper(base) => base,
            TreeOpen::Some => 0,
        };
        if let Some(&TreeOpen::VariantWrapper(outer)) = self.open.len().checked_sub(2).and_then(|i| self.open.get(i)) {
            // The wrapper's key is in the values; its payload is `v`.
            self.values.truncate(base);
            self.values.push(v);
            let map = self.object(vm, outer, true);
            self.values.pop();
            v = vm.heap.new_variant(self.ids(vm)?.obj, &[map?]);
            (closed, base) = (2, outer);
        }
        for _ in 0..closed {
            self.open.pop();
            self.depth -= 1;
        }
        self.values.truncate(base);
        self.put(v);
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail> {
        emit_json(vm, json, self)
    }

    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        write_json_form(self, vm, value)
    }

    fn save(&mut self) {
        self.saved = Some(self.state());
    }

    fn restore(&mut self) {
        if let Some(s) = self.saved.take() {
            self.rewind(s);
        }
    }

    fn swap_values(&mut self, values: &mut ObjectValues) {
        std::mem::swap(&mut self.values, values);
        self.names.clear();
        self.name_hashes.clear();
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }
}
