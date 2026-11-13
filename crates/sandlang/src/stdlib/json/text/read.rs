//! The fast path of `Decode` for JSON text: a read of a value with its
//! [`Schema`] straight from the input of a [`TextDecoder`].
//!
//! The read handles valid input only. At an error, or at a part that it
//! leaves to the format operations (`Range`, `Set`, native values, `Json`,
//! map keys other than `str`), it returns [`Fallback::General`], and the
//! decoder runs the general walker of [`crate::codec::fast`] from the same
//! place. So errors, their paths, and the rare parts behave as on the
//! general path, and the differential property test checks that the values
//! are the same.

use super::super::{Number, Parser};
use super::TextDecoder;
use crate::codec::fast::{Schema, SchemaId};
use crate::vm::bytecode::Program;
use crate::vm::meter::SLOW_BYTES_PER_UNIT;
use crate::vm::value::{MapObj, Obj, VmValue};
use crate::vm::{Fail, Vm};

/// Why the read stopped before the end of its value.
pub(super) enum Fallback {
    /// A memory or fuel limit, which ends the native call.
    Limit(Fail),
    /// An error or a rare part, which the general walker reads.
    General,
}

impl From<Fail> for Fallback {
    fn from(f: Fail) -> Self {
        Fallback::Limit(f)
    }
}

impl From<super::super::Error> for Fallback {
    fn from(e: super::super::Error) -> Self {
        match e {
            super::super::Error::Fail(f) => Fallback::Limit(f),
            super::super::Error::Parse(_) | super::super::Error::Mismatch(_) => Fallback::General,
        }
    }
}

type Read<T> = Result<T, Fallback>;

/// An open container of the read.
enum Frame<'p> {
    List {
        elem: SchemaId,
        items: Vec<VmValue>,
    },
    /// A map with `str` keys, and the key of the entry whose value comes
    /// next.
    Map {
        val: SchemaId,
        map: VmValue,
        key: Option<VmValue>,
    },
    /// A tuple, or the positional payload of variant `variant`.
    Tuple {
        parts: &'p [SchemaId],
        base: usize,
        variant: Option<u32>,
    },
    /// The fields of struct definition `id`, or the named payload of
    /// variant `id` (`variant`), whose slots hold `Uninit` until read.
    Fields {
        parts: &'p [SchemaId],
        names: &'p [String],
        id: u32,
        variant: bool,
        base: usize,
        field: usize,
        guess: usize,
    },
    /// `Some`, whose payload is the next value.
    Some,
}

/// The state of a read.
struct Reader<'t, 'p> {
    p: Parser<'t>,
    prog: &'p Program,
    stack: Vec<Frame<'p>>,
    /// The parts read so far of the open tuples and structs.
    values: Vec<VmValue>,
    /// Whether the innermost array or object has no part yet.
    first: bool,
}

impl TextDecoder {
    /// Reads a value of schema `schema`, or returns why the general walker
    /// must read it. The decoder's state changes only on success.
    pub(super) fn read_schema(&mut self, vm: &mut Vm, schema: SchemaId) -> Read<VmValue> {
        if self.key.is_some() {
            return Err(Fallback::General);
        }
        let prog = vm.prog.clone();
        let p = Parser { src: &self.text, b: self.text.as_bytes(), pos: self.pos, depth: self.depth, charged: self.charged };
        let mut r = Reader { p, prog: &prog, stack: Vec::with_capacity(16), values: Vec::with_capacity(32), first: false };
        let v = r.run(vm, schema)?;
        (self.pos, self.depth, self.charged) = (r.p.pos, r.p.depth, r.p.charged);
        Ok(v)
    }
}

impl<'p> Reader<'_, 'p> {
    fn run(&mut self, vm: &mut Vm, schema: SchemaId) -> Read<VmValue> {
        let mut part = schema;
        let mut is_key = false;
        loop {
            if !is_key {
                self.charge_value(vm)?;
            }
            let mut done = self.open(vm, part, is_key)?;
            (part, is_key) = loop {
                if let Some(v) = done.take() {
                    let Some(frame) = self.stack.last_mut() else { return Ok(v) };
                    match frame {
                        Frame::List { items, .. } => {
                            if items.len() == items.capacity() {
                                let more = items.capacity().max(4);
                                vm.check_memory((items.capacity() + more) * size_of::<VmValue>())?;
                                items.reserve_exact(more);
                            }
                            items.push(v);
                        }
                        Frame::Map { map, key, .. } => match key.take() {
                            None => *key = Some(v),
                            Some(k) => vm.map_set(*map, k, v)?,
                        },
                        Frame::Tuple { .. } => self.values.push(v),
                        Frame::Fields { base, field, .. } => self.values[*base + *field] = v,
                        Frame::Some => {
                            self.stack.pop();
                            done = Some(vm.heap.new_variant(self.prog.ids.some, &[v]));
                            continue;
                        }
                    }
                }
                match self.next_part(vm)? {
                    Some(next) => break next,
                    None => done = Some(self.finish(vm)?),
                }
            };
        }
    }

    /// Charges as the general walker does for a value: a unit, and the
    /// input since the last value.
    fn charge_value(&mut self, vm: &Vm) -> Read<()> {
        let read = self.p.pos.saturating_sub(self.p.charged);
        self.p.charged = self.p.pos;
        Ok(vm.charge(1 + (read / SLOW_BYTES_PER_UNIT) as u64)?)
    }

    /// Reads a scalar, or opens a container and returns `None`.
    fn open(&mut self, vm: &mut Vm, s: SchemaId, is_key: bool) -> Read<Option<VmValue>> {
        let prog = self.prog;
        let p = &mut self.p;
        p.ws();
        if is_key {
            // The key of a map with `str` keys, as `next_entry` and `str`
            // read it.
            if p.peek() != Some(b'"') {
                return Err(Fallback::General);
            }
            let key = p.string(true)?;
            p.ws();
            if !p.eat(b':') {
                return Err(Fallback::General);
            }
            vm.charge_bytes(key.len())?;
            return Ok(Some(vm.new_str(&*key)));
        }
        let Some(schema) = prog.schemas.get(s as usize) else { return Err(Fallback::General) };
        Ok(Some(match schema {
            Schema::Unit => {
                expect(p, b'n')?;
                p.literal(b"null")?;
                VmValue::Unit
            }
            Schema::Bool => match p.peek() {
                Some(b't') => p.literal(b"true").map(|()| VmValue::Bool(true))?,
                Some(b'f') => p.literal(b"false").map(|()| VmValue::Bool(false))?,
                _ => return Err(Fallback::General),
            },
            Schema::Int => VmValue::Int(int(p)?),
            Schema::Duration => VmValue::Dur(int(p)?),
            Schema::Float => VmValue::Float(number(p)?.float()),
            Schema::Str => {
                expect(p, b'"')?;
                let s = p.string(true)?;
                if s.len() > 1024 {
                    vm.check_memory(s.len())?;
                }
                vm.new_str(&*s)
            }
            &Schema::Option(payload) => {
                if p.peek() == Some(b'n') {
                    p.literal(b"null")?;
                    return Ok(Some(VmValue::Tag(prog.ids.none)));
                }
                self.stack.push(Frame::Some);
                self.charge_value(vm)?;
                return self.open(vm, payload, false);
            }
            &Schema::List(elem) => {
                self.enter(b'[')?;
                self.stack.push(Frame::List { elem, items: Vec::new() });
                return Ok(None);
            }
            &Schema::Map(key, val) => {
                if !matches!(prog.schemas.get(key as usize), Some(Schema::Str)) {
                    return Err(Fallback::General);
                }
                self.enter(b'{')?;
                vm.check_memory(size_of::<Obj>() + size_of::<MapObj>())?;
                let map = vm.heap.alloc(Obj::Map(Box::default()));
                self.stack.push(Frame::Map { val, map, key: None });
                return Ok(None);
            }
            Schema::Tuple(parts) => {
                self.enter(b'[')?;
                self.stack.push(Frame::Tuple { parts, base: self.values.len(), variant: None });
                return Ok(None);
            }
            Schema::Struct(def, parts) => {
                self.enter(b'{')?;
                let names = prog.types.get(*def as usize).map_or(&[][..], |t| &t.fields[..]);
                self.open_fields(parts, names, *def, false);
                return Ok(None);
            }
            Schema::Enum(first, variants) => return self.open_variant(vm, *first, variants),
            Schema::Bytes | Schema::Range | Schema::Native(_) | Schema::Json | Schema::Set(_) => return Err(Fallback::General),
        }))
    }

    fn open_fields(&mut self, parts: &'p [SchemaId], names: &'p [String], id: u32, variant: bool) {
        let base = self.values.len();
        self.values.resize(base + parts.len(), VmValue::Uninit);
        self.stack.push(Frame::Fields { parts, names, id, variant, base, field: 0, guess: 0 });
    }

    /// Reads a variant of the enum whose first variant is `first`, as
    /// `TextDecoder::variant` and the general walker do.
    fn open_variant(&mut self, vm: &mut Vm, first: u32, variants: &'p [Box<[SchemaId]>]) -> Read<Option<VmValue>> {
        let prog = self.prog;
        let rts = prog.variants.get(first as usize..first as usize + variants.len()).unwrap_or_default();
        let is_object = match self.p.peek() {
            Some(b'"') => false,
            Some(b'{') => true,
            _ => return Err(Fallback::General),
        };
        if is_object {
            self.enter(b'{')?;
            if !self.next(b'}')? {
                return Err(Fallback::General);
            }
            self.key()?;
        }
        let tag = self.p.string(true)?;
        if is_object {
            self.p.ws();
            if !self.p.eat(b':') {
                return Err(Fallback::General);
            }
        }
        vm.charge(1 + (variants.len() / 16) as u64)?;
        let Some(k) = rts.iter().position(|v| v.name == *tag) else { return Err(Fallback::General) };
        let id = first + k as u32;
        let parts = &variants[k][..];
        if !is_object {
            return match parts.is_empty() {
                true => Ok(Some(VmValue::Tag(id))),
                false => Err(Fallback::General),
            };
        }
        self.p.ws();
        if parts.is_empty() {
            self.enter(b'[')?;
            if self.next(b']')? {
                return Err(Fallback::General);
            }
            self.end_variant()?;
            return Ok(Some(VmValue::Tag(id)));
        }
        match rts[k].field_names.as_deref() {
            Some(names) => {
                self.enter(b'{')?;
                self.open_fields(parts, names, id, true);
            }
            None => {
                self.enter(b'[')?;
                self.stack.push(Frame::Tuple { parts, base: self.values.len(), variant: Some(id) });
            }
        }
        Ok(None)
    }

    /// Opens an array or object at `open`.
    fn enter(&mut self, open: u8) -> Read<()> {
        self.p.ws();
        expect(&self.p, open)?;
        self.p.enter()?;
        self.first = true;
        Ok(())
    }

    /// Moves to the next part of the innermost array or object that ends
    /// with `close`, as `TextDecoder::next` does. Returns `false` after its
    /// last part.
    fn next(&mut self, close: u8) -> Read<bool> {
        self.p.ws();
        if self.p.eat(close) {
            self.p.depth -= 1;
            self.first = false;
            return Ok(false);
        }
        if !std::mem::replace(&mut self.first, false) && !self.p.eat(b',') {
            return Err(Fallback::General);
        }
        Ok(true)
    }

    /// Reads up to the opening quote of the next key of an object.
    fn key(&mut self) -> Read<()> {
        self.p.ws();
        expect(&self.p, b'"')
    }

    /// Closes a variant's wrapper object, which must have no other member.
    fn end_variant(&mut self) -> Read<()> {
        match self.next(b'}')? {
            true => Err(Fallback::General),
            false => Ok(()),
        }
    }

    /// Moves to the next part of the innermost frame. Returns its schema and
    /// whether it is a map key, or `None` after the last part.
    fn next_part(&mut self, vm: &mut Vm) -> Read<Option<(SchemaId, bool)>> {
        let len = self.values.len();
        let Some(frame) = self.stack.last_mut() else { return Err(Fallback::General) };
        let close = match frame {
            Frame::List { .. } | Frame::Tuple { .. } => b']',
            _ => b'}',
        };
        Ok(match frame {
            Frame::List { elem, .. } => {
                let elem = *elem;
                self.next(close)?.then_some((elem, false))
            }
            Frame::Map { val, key: Some(_), .. } => Some((*val, false)),
            Frame::Map { .. } => {
                if !self.next(close)? {
                    return Ok(None);
                }
                Some((0, true))
            }
            Frame::Tuple { parts, base, .. } => {
                let (parts, base) = (*parts, *base);
                let Some(&s) = parts.get(len - base) else { return Ok(None) };
                if !self.next(close)? {
                    return Err(Fallback::General);
                }
                Some((s, false))
            }
            Frame::Fields { .. } => loop {
                if !self.next(close)? {
                    return Ok(None);
                }
                self.key()?;
                let key = self.p.string(true)?;
                self.p.ws();
                if !self.p.eat(b':') {
                    return Err(Fallback::General);
                }
                let Some(Frame::Fields { parts, names, id, variant, field, guess, .. }) = self.stack.last_mut() else {
                    return Err(Fallback::General);
                };
                let found = match names.get(*guess).is_some_and(|n| *n == *key) {
                    true => Some(*guess),
                    false => {
                        vm.charge(1)?;
                        let by_name = match *variant {
                            true => self.prog.variants.get(*id as usize).map(|v| &v.by_name),
                            false => self.prog.types.get(*id as usize).map(|t| &t.by_name),
                        };
                        by_name.and_then(|by_name| crate::vm::bytecode::field_index(names, by_name, &key))
                    }
                };
                match found {
                    Some(i) => {
                        (*field, *guess) = (i, i + 1);
                        match parts.get(i) {
                            Some(&s) => break Some((s, false)),
                            None => return Err(Fallback::General),
                        }
                    }
                    None => self.p.skip()?,
                }
            },
            Frame::Some => return Err(Fallback::General),
        })
    }

    /// Ends the innermost frame after its last part and builds its value.
    fn finish(&mut self, vm: &mut Vm) -> Read<VmValue> {
        let Some(frame) = self.stack.pop() else { return Err(Fallback::General) };
        Ok(match frame {
            Frame::List { items, .. } => {
                vm.check_memory(size_of::<Obj>())?;
                vm.heap.new_list(items)
            }
            Frame::Map { map, .. } => map,
            Frame::Tuple { parts, base, variant } => {
                if self.next(b']')? {
                    return Err(Fallback::General);
                }
                vm.check_memory(size_of::<Obj>() + parts.len() * size_of::<VmValue>())?;
                let v = match variant {
                    None => vm.heap.new_tuple(&self.values[base..]),
                    Some(id) => {
                        self.end_variant()?;
                        vm.heap.new_variant(id, &self.values[base..])
                    }
                };
                self.values.truncate(base);
                v
            }
            Frame::Fields { parts, id, variant, base, .. } => {
                for (i, &part) in parts.iter().enumerate() {
                    if !matches!(self.values[base + i], VmValue::Uninit) {
                        continue;
                    }
                    if !matches!(self.prog.schemas.get(part as usize), Some(Schema::Option(_))) {
                        return Err(Fallback::General);
                    }
                    self.values[base + i] = VmValue::Tag(self.prog.ids.none);
                }
                vm.check_memory(size_of::<Obj>() + parts.len() * size_of::<VmValue>())?;
                let v = if variant {
                    self.end_variant()?;
                    vm.heap.new_variant(id, &self.values[base..])
                } else {
                    let fields = vm.heap.boxed(&self.values[base..]);
                    vm.heap.alloc(Obj::Struct(id, fields))
                };
                self.values.truncate(base);
                v
            }
            Frame::Some => return Err(Fallback::General),
        })
    }
}

/// Fails unless the next byte is `c`.
fn expect(p: &Parser<'_>, c: u8) -> Read<()> {
    match p.peek() == Some(c) {
        true => Ok(()),
        false => Err(Fallback::General),
    }
}

fn number<'a>(p: &mut Parser<'a>) -> Read<Number<'a>> {
    match p.peek() {
        Some(b'-' | b'0'..=b'9') => Ok(p.number()?),
        _ => Err(Fallback::General),
    }
}

fn int(p: &mut Parser<'_>) -> Read<i64> {
    number(p)?.int().ok_or(Fallback::General)
}
