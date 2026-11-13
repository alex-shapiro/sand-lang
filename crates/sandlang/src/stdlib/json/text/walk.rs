//! The fast path of `Encode` for JSON text: a walk of a value with its
//! [`Schema`] that writes the text of a [`TextEncoder`] directly.
//!
//! The general walker of [`crate::codec::fast`] calls the operations of
//! `TextEncoder`, and each operation finds its place in the text again from
//! the encoder's bracket stack. This walk knows the place from its own
//! frames. The differential property test checks that both write the same
//! text.

use super::{Bracket, JsonIds, Open, TextEncoder, write_str};
use crate::codec::fast::{self, OPEN_FRAMES, OPEN_VALUES, Schema, SchemaId};
use crate::codec::{EncodeFormat, MAX_NESTING_DEPTH};
use crate::vm::bytecode::Program;
use crate::vm::value::{Obj, VmValue};
use crate::vm::{Fail, Vm, internal};
use std::fmt::Write;

/// Bytes of output that a check of the memory limit before each part
/// covers: the separator, the start of a line, and a scalar other than a
/// long `str`.
const PART_ROOM: usize = 64;

/// The longest `str` that [`PART_ROOM`] covers with its escapes.
const SHORT_STR: usize = 8;

/// An open array or object of the walk, with the index of its next part.
enum Frame<'p> {
    List {
        list: VmValue,
        next: usize,
        elem: SchemaId,
    },
    /// A set, and the slot of its next entry.
    Set {
        set: VmValue,
        next: usize,
        elem: SchemaId,
        first: bool,
    },
    /// A map, and the slot of its next entry.
    Map {
        map: VmValue,
        next: usize,
        key: SchemaId,
        val: SchemaId,
        first: bool,
    },
    /// The elements of a tuple, the fields of a struct (`names`), or a
    /// variant payload (`wrapped`, whose wrapper object closes after it).
    /// The parts are on the walk's value stack from `base`.
    Parts {
        base: usize,
        next: usize,
        parts: &'p [SchemaId],
        names: Option<&'p [String]>,
        wrapped: bool,
    },
}

impl TextEncoder {
    /// Writes `value`, whose closed type has schema `schema`.
    pub(super) fn write_schema(&mut self, vm: &mut Vm, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
        // A map key needs the checks of the format operations.
        if self.in_key() {
            return fast::encode(vm, self, value, schema);
        }
        let (out, charged, open, top, depth) = (self.out.len(), self.charged, self.open.len(), self.open.last().copied(), self.depth);
        let prog = vm.prog.clone();
        let r = self.walk(vm, &prog, value, schema);
        if r.as_ref().is_err_and(|e| e.as_limit().is_some()) {
            self.out.truncate(out);
            self.open.truncate(open);
            if let (Some(top), Some(last)) = (top, self.open.last_mut()) {
                *last = top;
            }
            (self.charged, self.depth) = (charged, depth);
        }
        r
    }

    fn walk(&mut self, vm: &mut Vm, prog: &Program, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
        self.room(vm, PART_ROOM)?;
        self.begin_member(None);
        // The operations that the walk calls for rare values write no
        // separator in this bracket.
        self.open.push(Open { bracket: Bracket::Some, is_empty: true, names: None });
        let mut stack: Vec<Frame<'_>> = Vec::with_capacity(OPEN_FRAMES);
        let mut values: Vec<VmValue> = Vec::with_capacity(OPEN_VALUES);
        let (mut v, mut s) = (value, schema);
        loop {
            vm.charge(1)?;
            if let Some(frame) = self.value(vm, prog, &mut values, v, s)? {
                stack.push(frame);
            }
            (v, s) = loop {
                let Some(frame) = stack.last_mut() else {
                    self.open.pop();
                    return Ok(());
                };
                if let Some(part) = self.next_part(vm, prog, frame, &values)? {
                    break part;
                }
                match stack.pop() {
                    Some(Frame::List { next, .. }) => self.close(vm, ']', next == 0)?,
                    Some(Frame::Set { first, .. }) => self.close(vm, ']', first)?,
                    Some(Frame::Map { first, .. }) => self.close(vm, '}', first)?,
                    Some(Frame::Parts { base, parts, names, wrapped, .. }) => {
                        values.truncate(base);
                        self.close(vm, if names.is_some() { '}' } else { ']' }, parts.is_empty())?;
                        if wrapped {
                            self.close(vm, '}', false)?;
                        }
                    }
                    None => return internal("json.encode: no open container"),
                }
            };
        }
    }

    /// Writes the scalar `v`, or opens the container `v` and returns its
    /// frame.
    #[inline(always)]
    fn value<'p>(
        &mut self,
        vm: &mut Vm,
        prog: &'p Program,
        values: &mut Vec<VmValue>,
        mut v: VmValue,
        mut s: SchemaId,
    ) -> Result<Option<Frame<'p>>, Fail> {
        loop {
            match (schema(prog, s)?, v) {
                (&Schema::Option(payload), _) => match vm.heap.payload_if(&v, prog.ids.some) {
                    VmValue::Uninit => self.literal(vm, "null")?,
                    x => {
                        vm.charge(1)?;
                        (v, s) = (x, payload);
                        continue;
                    }
                },
                (Schema::Unit, _) => self.literal(vm, "null")?,
                (Schema::Bool, VmValue::Bool(b)) => self.literal(vm, if b { "true" } else { "false" })?,
                (Schema::Int, VmValue::Int(i)) | (Schema::Duration, VmValue::Dur(i)) => self.literal(vm, itoa::Buffer::new().format(i))?,
                (Schema::Float, VmValue::Float(x)) => {
                    match x.is_finite() {
                        true => _ = write!(self.out, "{x:?}"),
                        false => self.out.push_str("null"),
                    }
                    self.charge_written(vm)?;
                }
                (Schema::Str, _) => {
                    let len = vm.heap.str(v).len();
                    if len > SHORT_STR {
                        self.room(vm, len * 6 + 4)?;
                    }
                    write_str(&mut self.out, vm.heap.str(v));
                    self.charge_written(vm)?;
                }
                (&Schema::List(elem), _) => {
                    self.open_bracket_at(vm, '[')?;
                    return Ok(Some(Frame::List { list: v, next: 0, elem }));
                }
                (&Schema::Set(elem), _) => {
                    self.open_bracket_at(vm, '[')?;
                    return Ok(Some(Frame::Set { set: v, next: 0, elem, first: true }));
                }
                (&Schema::Map(key, val), _) => {
                    self.open_bracket_at(vm, '{')?;
                    return Ok(Some(Frame::Map { map: v, next: 0, key, val, first: true }));
                }
                (Schema::Tuple(parts), _) => {
                    self.open_bracket_at(vm, '[')?;
                    return copy_parts(vm, values, v, parts)
                        .map(|base| Some(Frame::Parts { base, next: 0, parts, names: None, wrapped: false }));
                }
                (Schema::Struct(def, parts), _) => {
                    self.open_bracket_at(vm, '{')?;
                    let names = prog.types.get(*def as usize).map(|t| &t.fields[..]);
                    return copy_parts(vm, values, v, parts).map(|base| Some(Frame::Parts { base, next: 0, parts, names, wrapped: false }));
                }
                (Schema::Enum(first, variants), _) => {
                    let id = vm.heap.variant_id(v);
                    let (Some(parts), Some(rt)) = (variants.get(id.wrapping_sub(*first) as usize), prog.variants.get(id as usize)) else {
                        return internal("json.encode: unknown variant");
                    };
                    if parts.is_empty() {
                        self.room(vm, rt.name.len() * 6 + 4)?;
                        write_str(&mut self.out, &rt.name);
                        self.charge_written(vm)?;
                        return Ok(None);
                    }
                    // `{"Name": [...]}` or `{"Name": {...}}`, as `begin_variant`.
                    self.room(vm, rt.name.len() * 6 + 8 + 2 * self.indent * (self.depth + 2))?;
                    self.open_bracket_at(vm, '{')?;
                    self.start_line();
                    write_str(&mut self.out, &rt.name);
                    self.out.push(':');
                    if self.indent > 0 {
                        self.out.push(' ');
                    }
                    let names = rt.field_names.as_deref();
                    self.open_bracket_at(vm, if names.is_some() { '{' } else { '[' })?;
                    return copy_parts(vm, values, v, parts).map(|base| Some(Frame::Parts { base, next: 0, parts, names, wrapped: true }));
                }
                (Schema::Bytes, _) => {
                    let bytes = vm.heap.bytes_of(v).to_vec();
                    self.bytes(vm, &bytes)?;
                }
                (Schema::Range, _) => {
                    let Obj::Range(start, end, inclusive) = *vm.heap.get(v) else { return internal("json.encode: not a range") };
                    self.range(vm, start, end, inclusive)?;
                }
                (Schema::Native(_), _) => {
                    let Some((type_name, text)) = crate::codec::native_text(vm, v) else {
                        return internal("json.encode: not a native value");
                    };
                    self.native(vm, &type_name, &text)?;
                }
                (Schema::Json, _) => self.json(vm, v)?,
                _ => return internal("json.encode: the value does not match its schema"),
            }
            return Ok(None);
        }
    }

    /// Moves to the next part of `frame`, and writes the separator and the
    /// key before it. Returns `None` after the last part.
    #[inline(always)]
    fn next_part(
        &mut self,
        vm: &mut Vm,
        prog: &Program,
        frame: &mut Frame<'_>,
        values: &[VmValue],
    ) -> Result<Option<(VmValue, SchemaId)>, Fail> {
        Ok(match frame {
            Frame::List { list, next, elem } => {
                let Some(&x) = vm.heap.list(*list).get(*next) else { return Ok(None) };
                self.member(vm, *next == 0, None)?;
                *next += 1;
                Some((x, *elem))
            }
            Frame::Set { set, next, elem, first } => {
                let Obj::Set(s) = vm.heap.get(*set) else { return internal("json.encode: not a set") };
                let Some((k, x)) = s.entries.get(*next..).and_then(|rest| rest.iter().enumerate().find_map(|(k, x)| Some((k, (*x)?))))
                else {
                    return Ok(None);
                };
                *next += k + 1;
                self.member(vm, std::mem::replace(first, false), None)?;
                Some((x, *elem))
            }
            Frame::Map { map, next, key, val, first } => {
                let Obj::Map(m) = vm.heap.get(*map) else { return internal("json.encode: not a map") };
                let Some((k, (key_value, x))) =
                    m.entries.get(*next..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((k, (*e)?))))
                else {
                    return Ok(None);
                };
                *next += k + 1;
                let is_first = std::mem::replace(first, false);
                match schema(prog, *key)? {
                    Schema::Str => {
                        self.room(vm, vm.heap.str(key_value).len() * 6 + 4)?;
                        self.member(vm, is_first, None)?;
                        write_str(&mut self.out, vm.heap.str(key_value));
                        self.out.push(':');
                        if self.indent > 0 {
                            self.out.push(' ');
                        }
                    }
                    // Other keys go through the format operations, which
                    // check that the key has a JSON key form.
                    _ => {
                        self.open.push(Open { bracket: Bracket::Map(true), is_empty: is_first, names: None });
                        fast::encode_key(vm, self, key_value, *key)?;
                        self.open.pop();
                    }
                }
                Some((x, *val))
            }
            Frame::Parts { base, next, parts, names, .. } => {
                let Some(&s) = parts.get(*next) else { return Ok(None) };
                let name = names.and_then(|names| names.get(*next)).map(String::as_str);
                self.member(vm, *next == 0, name)?;
                let x = values[*base + *next];
                *next += 1;
                Some((x, s))
            }
        })
    }

    /// Writes the Sand `json.Json` value `root`, and charges a fuel unit for
    /// each value, as `emit_json` does with the format operations.
    pub(super) fn write_json_value(&mut self, vm: &mut Vm, root: VmValue) -> Result<(), Fail> {
        /// An open array or object, and the index of its next element or
        /// entry slot.
        enum JsonFrame {
            Arr(VmValue, usize),
            Obj(VmValue, usize, bool),
        }
        // A map key needs the checks of the format operations.
        if self.in_key() {
            return super::emit_json(vm, root, self);
        }
        let ids = JsonIds::new(&vm.prog)?;
        self.room(vm, PART_ROOM)?;
        self.begin_member(None);
        self.open.push(Open { bracket: Bracket::Some, is_empty: true, names: None });
        let mut stack: Vec<JsonFrame> = Vec::new();
        let mut next = Some(root);
        loop {
            if let Some(v) = next.take() {
                vm.charge(1)?;
                let id = vm.heap.variant_id(v);
                match vm.heap.field(v, 0) {
                    _ if id == ids.null => self.literal(vm, "null")?,
                    Some(VmValue::Bool(b)) if id == ids.bool_ => self.literal(vm, if b { "true" } else { "false" })?,
                    Some(VmValue::Int(i)) if id == ids.int => self.literal(vm, itoa::Buffer::new().format(i))?,
                    Some(VmValue::Float(x)) if id == ids.float => {
                        match x.is_finite() {
                            true => _ = write!(self.out, "{x:?}"),
                            false => self.out.push_str("null"),
                        }
                        self.charge_written(vm)?;
                    }
                    Some(p) if id == ids.str_ => {
                        let len = vm.heap.str(p).len();
                        if len > SHORT_STR {
                            self.room(vm, len * 6 + 4)?;
                        }
                        write_str(&mut self.out, vm.heap.str(p));
                        self.charge_written(vm)?;
                    }
                    Some(p) if id == ids.arr => {
                        self.open_bracket_at(vm, '[')?;
                        stack.push(JsonFrame::Arr(p, 0));
                    }
                    Some(p) if id == ids.obj => {
                        self.open_bracket_at(vm, '{')?;
                        stack.push(JsonFrame::Obj(p, 0, true));
                    }
                    _ => return internal("json: not a Json value"),
                }
            }
            let Some(top) = stack.last_mut() else {
                self.open.pop();
                return Ok(());
            };
            match top {
                JsonFrame::Arr(list, i) => match vm.heap.list(*list).get(*i) {
                    Some(&x) => {
                        self.member(vm, *i == 0, None)?;
                        *i += 1;
                        next = Some(x);
                    }
                    None => {
                        let is_empty = *i == 0;
                        stack.pop();
                        self.close(vm, ']', is_empty)?;
                    }
                },
                JsonFrame::Obj(map, i, first) => {
                    let Obj::Map(m) = vm.heap.get(*map) else { return internal("json: not an object") };
                    match m.entries.get(*i..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((k, (*e)?)))) {
                        Some((k, (key, x))) => {
                            *i += k + 1;
                            let is_first = std::mem::replace(first, false);
                            self.room(vm, vm.heap.str(key).len() * 6 + 4)?;
                            self.member(vm, is_first, None)?;
                            write_str(&mut self.out, vm.heap.str(key));
                            self.out.push(':');
                            if self.indent > 0 {
                                self.out.push(' ');
                            }
                            next = Some(x);
                        }
                        None => {
                            let is_empty = *first;
                            stack.pop();
                            self.close(vm, '}', is_empty)?;
                        }
                    }
                }
            }
        }
    }

    /// Writes the separator and the key `name` of the next member of an
    /// array or object, as `begin_member` does, after a memory check that
    /// also covers a short value.
    #[inline(always)]
    fn member(&mut self, vm: &mut Vm, is_first: bool, name: Option<&str>) -> Result<(), Fail> {
        self.room(vm, PART_ROOM + name.map_or(0, |n| n.len() * 6))?;
        if !is_first {
            self.out.push(',');
        }
        self.start_line();
        if let Some(name) = name {
            write_str(&mut self.out, name);
            self.out.push(':');
            if self.indent > 0 {
                self.out.push(' ');
            }
        }
        Ok(())
    }

    /// Writes `text`, a scalar without quotes that [`PART_ROOM`] covers.
    #[inline(always)]
    fn literal(&mut self, vm: &mut Vm, text: &str) -> Result<(), Fail> {
        self.out.push_str(text);
        self.charge_written(vm)
    }

    /// Opens an array or object with `ch`, as `open_bracket` does.
    #[inline(always)]
    fn open_bracket_at(&mut self, vm: &mut Vm, ch: char) -> Result<(), Fail> {
        if self.depth >= MAX_NESTING_DEPTH {
            return super::nesting_limit_error();
        }
        self.room(vm, PART_ROOM)?;
        self.depth += 1;
        self.out.push(ch);
        Ok(())
    }

    /// Closes the innermost array or object with `ch`, as `close_bracket`
    /// does.
    #[inline(always)]
    fn close(&mut self, vm: &mut Vm, ch: char, is_empty: bool) -> Result<(), Fail> {
        self.room(vm, 2)?;
        self.depth -= 1;
        if !is_empty {
            self.start_line();
        }
        self.out.push(ch);
        self.charge_written(vm)
    }
}

fn schema(prog: &Program, id: SchemaId) -> Result<&Schema, Fail> {
    match prog.schemas.get(id as usize) {
        Some(s) => Ok(s),
        None => internal("codec: unknown schema"),
    }
}

/// Copies the parts of the tuple, struct, or variant `v` to `values`, and
/// returns where they start.
#[inline(always)]
fn copy_parts(vm: &Vm, values: &mut Vec<VmValue>, v: VmValue, parts: &[SchemaId]) -> Result<usize, Fail> {
    let base = values.len();
    match v {
        VmValue::Obj(_) => values.extend_from_slice(vm.heap.fields(v)),
        // A pair of ints, or a variant with its payload inline.
        _ => values.extend((0..parts.len()).map_while(|i| vm.heap.field(v, i))),
    }
    if values.len() - base != parts.len() {
        return internal("json.encode: the value does not match its schema");
    }
    Ok(base)
}
