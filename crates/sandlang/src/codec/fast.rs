//! The native fast path of `Encode` and `Decode` for closed types with no
//! hand-written part.
//!
//! For such a type, such as `[User]` or `Page<User>`, the checker gives the
//! type an `impl` whose method calls `Encoder.write_fast` or
//! `Decoder.read_fast` with the [`Schema`] of the type. These natives walk the
//! value or the input with an explicit stack, and call the same format
//! operations, in the same order, as the synthesized Sand implementations.
//! So the formats, the errors, and the paths are the same on both paths.
//!
//! A walk is one native call. If a memory check fails during the walk, the
//! format and the decoder's path go back to their state before the walk, so
//! that the engine can run the call again after a garbage collection.

use super::{DecodeFail, DecodeFormat, DecodeResult, EncodeFormat};
use super::{Decoder, Encoder, Frame, SeqKind};
use crate::native::convert::HostError;
use crate::native::ty::VmVal;
use crate::vm::bytecode::Program;
use crate::vm::value::{MapObj, Obj, SetObj, VmValue};
use crate::vm::{Fail, Vm, internal};

/// The index of a [`Schema`] in the program's schema table.
pub type SchemaId = u32;

/// The shape of a closed type that the fast path encodes and decodes. Each
/// node mirrors the synthesized or prelude implementation of its type.
#[derive(Clone, Debug, PartialEq)]
pub enum Schema {
    Unit,
    Bool,
    Int,
    Float,
    Duration,
    Str,
    Bytes,
    Range,
    /// A native value type, by its runtime type definition.
    Native(u32),
    /// `json.Json`.
    Json,
    List(SchemaId),
    Set(SchemaId),
    Map(SchemaId, SchemaId),
    Option(SchemaId),
    Tuple(Box<[SchemaId]>),
    /// A struct: its runtime definition and the schema of each field.
    Struct(u32, Box<[SchemaId]>),
    /// An enum: the runtime id of its first variant, and the payload
    /// schemas of each variant.
    Enum(u32, Box<[Box<[SchemaId]>]>),
}

/// Writes `value`, whose type has schema `schema`, to the encoder `e`.
pub(super) fn encode(vm: &mut Vm, e: VmVal<Encoder>, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
    let Some(mut format) = e.get_mut(&mut vm.heap)?.format.take() else { return internal("the encoder is in use") };
    let mark = format.mark();
    let prog = vm.prog.clone();
    let r = encode_walk(vm, &prog, &mut *format, value, schema);
    if r.as_ref().is_err_and(|f| f.as_limit().is_some()) {
        format.rollback(mark);
    }
    e.get_mut(&mut vm.heap)?.format = Some(format);
    r
}

/// An open container of the encode walk, with the index of its next part.
enum EncodeFrame {
    /// A list, or the entries of a set.
    Items { obj: VmValue, next: usize, elem: SchemaId, set: bool },
    /// The entries of a map; `value` is `true` while the value of entry
    /// `next` comes next.
    Entries { obj: VmValue, next: usize, key: SchemaId, val: SchemaId, value: bool },
    /// The elements of a tuple, the fields of a struct, or the payload of
    /// variant `variant` of the enum `schema`. `named` means that a field
    /// call comes before each part.
    Parts { obj: VmValue, next: usize, schema: SchemaId, variant: Option<usize>, named: bool },
    /// The payload of `Some`.
    Some { payload: Option<VmValue>, schema: SchemaId },
}

fn encode_walk(vm: &mut Vm, prog: &Program, f: &mut dyn EncodeFormat, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
    let mut stack: Vec<EncodeFrame> = Vec::new();
    let mut part = Some((value, schema));
    loop {
        if let Some((v, s)) = part.take()
            && let Some(frame) = encode_one(vm, prog, f, v, s)?
        {
            stack.push(frame);
        }
        let Some(frame) = stack.last_mut() else { return Ok(()) };
        part = match frame {
            EncodeFrame::Items { obj, next, elem, set } => {
                let item = match vm.heap.get(*obj) {
                    Obj::List(items) if !*set => items.get(*next).copied(),
                    Obj::Set(s) => {
                        let found = s.entries[(*next).min(s.entries.len())..].iter().position(Option::is_some);
                        found.and_then(|k| {
                            *next += k;
                            s.entries[*next]
                        })
                    }
                    _ => None,
                };
                *next += 1;
                item.map(|x| (x, *elem))
            }
            EncodeFrame::Entries { obj, next, key, val, value } => {
                let Obj::Map(m) = vm.heap.get(*obj) else { return internal("encode: not a map") };
                if !*value {
                    *next += m.entries[(*next).min(m.entries.len())..].iter().position(Option::is_some).unwrap_or(m.entries.len());
                }
                match m.entries.get(*next).copied().flatten() {
                    Some((k, _)) if !*value => {
                        *value = true;
                        Some((k, *key))
                    }
                    Some((_, x)) => {
                        *value = false;
                        *next += 1;
                        Some((x, *val))
                    }
                    None => None,
                }
            }
            EncodeFrame::Parts { obj, next, schema, variant, named } => {
                let parts = match (&prog_schema(prog, *schema)?, *variant) {
                    (Schema::Tuple(parts) | Schema::Struct(_, parts), None) => parts.len(),
                    (Schema::Enum(_, variants), Some(k)) => variants.get(k).map_or(0, |p| p.len()),
                    _ => return internal("encode: no parts"),
                };
                if *next < parts {
                    let i = *next;
                    *next += 1;
                    let s = match (prog_schema(prog, *schema)?, *variant) {
                        (Schema::Tuple(p) | Schema::Struct(_, p), None) => p[i],
                        (Schema::Enum(_, variants), Some(k)) => variants[k][i],
                        _ => return internal("encode: no parts"),
                    };
                    if *named {
                        f.field(vm, i)?;
                    }
                    let Some(x) = vm.heap.field(*obj, i) else { return internal("encode: missing part") };
                    Some((x, s))
                } else {
                    None
                }
            }
            EncodeFrame::Some { payload, schema } => payload.take().map(|x| (x, *schema)),
        };
        if part.is_none() {
            f.end(vm)?;
            stack.pop();
        }
    }
}

fn prog_schema(prog: &Program, id: SchemaId) -> Result<&Schema, Fail> {
    match prog.schemas.get(id as usize) {
        Some(s) => Ok(s),
        None => internal("codec: unknown schema"),
    }
}

/// Writes the scalar `v`, or opens the container `v` and returns its frame.
fn encode_one(vm: &mut Vm, prog: &Program, f: &mut dyn EncodeFormat, v: VmValue, s: SchemaId) -> Result<Option<EncodeFrame>, Fail> {
    vm.charge(1)?;
    let frame = match (prog_schema(prog, s)?, v) {
        (Schema::Unit, _) => return f.unit(vm).map(|()| None),
        (Schema::Bool, VmValue::Bool(b)) => return f.bool(vm, b).map(|()| None),
        (Schema::Int, VmValue::Int(i)) => return f.int(vm, i).map(|()| None),
        (Schema::Float, VmValue::Float(x)) => return f.float(vm, x).map(|()| None),
        (Schema::Duration, VmValue::Dur(d)) => return f.duration(vm, d).map(|()| None),
        (Schema::Str, _) => {
            let text = match vm.heap.get(v) {
                Obj::Str(s) => s.clone(),
                _ => vm.heap.str(v).into(),
            };
            return f.str(vm, &text).map(|()| None);
        }
        (Schema::Bytes, _) => {
            let bytes = vm.heap.bytes_of(v).to_vec();
            return f.bytes(vm, &bytes).map(|()| None);
        }
        (Schema::Range, _) => {
            let Obj::Range(start, end, inclusive) = *vm.heap.get(v) else { return internal("encode: not a range") };
            return f.range(vm, start, end, inclusive).map(|()| None);
        }
        (Schema::Native(_), _) => {
            let Some((type_name, text)) = super::native_text(vm, v) else { return internal("encode: not a native value") };
            return f.native(vm, &type_name, &text).map(|()| None);
        }
        (Schema::Json, _) => return f.json(vm, v).map(|()| None),
        (&Schema::List(elem), _) => {
            let len = vm.heap.list(v).len();
            f.begin_seq(vm, SeqKind::List, len)?;
            EncodeFrame::Items { obj: v, next: 0, elem, set: false }
        }
        (&Schema::Set(elem), _) => {
            let Obj::Set(set) = vm.heap.get(v) else { return internal("encode: not a set") };
            f.begin_seq(vm, SeqKind::Set, set.live)?;
            EncodeFrame::Items { obj: v, next: 0, elem, set: true }
        }
        (&Schema::Map(key, val), _) => {
            let Obj::Map(map) = vm.heap.get(v) else { return internal("encode: not a map") };
            f.begin_map(vm, map.live)?;
            EncodeFrame::Entries { obj: v, next: 0, key, val, value: false }
        }
        (&Schema::Option(payload), _) => match vm.heap.payload_if(&v, prog.ids.some) {
            VmValue::Uninit => return f.none(vm).map(|()| None),
            x => {
                f.begin_some(vm)?;
                EncodeFrame::Some { payload: Some(x), schema: payload }
            }
        },
        (Schema::Tuple(parts), _) => {
            f.begin_seq(vm, SeqKind::Tuple(parts.len()), parts.len())?;
            EncodeFrame::Parts { obj: v, next: 0, schema: s, variant: None, named: false }
        }
        (&Schema::Struct(def, _), _) => {
            f.begin_struct(vm, def)?;
            EncodeFrame::Parts { obj: v, next: 0, schema: s, variant: None, named: true }
        }
        (&Schema::Enum(first, ref variants), _) => {
            let id = vm.heap.variant_id(v);
            let k = id.wrapping_sub(first) as usize;
            let Some(payload) = variants.get(k) else { return internal("encode: unknown variant") };
            if payload.is_empty() {
                return f.unit_variant(vm, id).map(|()| None);
            }
            f.begin_variant(vm, id)?;
            let named = prog.variants.get(id as usize).is_some_and(|x| x.field_names.is_some());
            EncodeFrame::Parts { obj: v, next: 0, schema: s, variant: Some(k), named }
        }
        _ => return internal("encode: the value does not match its schema"),
    };
    Ok(Some(frame))
}

/// Reads a value whose type has schema `schema` from the decoder `d`.
pub(super) fn decode(vm: &mut Vm, d: VmVal<Decoder>, schema: SchemaId) -> Result<VmValue, HostError> {
    super::with_decoder(vm, d, |f, frames, vm| {
        let mark = (f.mark(), frames.len(), frames.last().cloned());
        let prog = vm.prog.clone();
        let r = decode_walk(vm, &prog, f, frames, schema);
        if let Err(DecodeFail::Fail(fail)) = &r
            && fail.as_limit().is_some()
        {
            let (format, len, top) = mark;
            f.rollback(format);
            frames.truncate(len);
            if let (Some(top), Some(last)) = (top, frames.last_mut()) {
                *last = top;
            }
        }
        r
    })
}

/// A container that the decode walk is reading. The parts that a list,
/// tuple, or struct has read are on the walk's value stack from `base`.
enum DecodeFrame {
    List {
        elem: SchemaId,
        base: usize,
    },
    Set {
        elem: SchemaId,
        set: VmValue,
    },
    /// A map; `key` holds the key of the entry whose value comes next.
    Map {
        key_schema: SchemaId,
        val: SchemaId,
        map: VmValue,
        key: Option<VmValue>,
    },
    /// A tuple, or the positional payload of variant `variant`.
    Tuple {
        schema: SchemaId,
        variant: Option<usize>,
        base: usize,
    },
    /// The fields of a struct, or the named payload of variant `variant`,
    /// whose slots hold `Uninit` until read; `field` is the field being read.
    Fields {
        schema: SchemaId,
        variant: Option<usize>,
        base: usize,
        field: usize,
    },
    /// `Some`, and the schema of its payload until the walk reads it.
    Some(Option<SchemaId>),
}

/// The result of reading the start of a value.
enum Opened {
    Value(VmValue),
    Frame(DecodeFrame),
}

/// The state of a decode walk.
struct DecodeWalk<'p> {
    prog: &'p Program,
    stack: Vec<DecodeFrame>,
    /// The parts read so far of the open lists, tuples, and structs.
    values: Vec<VmValue>,
}

fn decode_walk(vm: &mut Vm, prog: &Program, f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, schema: SchemaId) -> DecodeResult<VmValue> {
    let mut w = DecodeWalk { prog, stack: Vec::new(), values: Vec::new() };
    let mut part = schema;
    loop {
        let mut done = match w.open(vm, f, frames, part)? {
            Opened::Value(v) => Some(v),
            Opened::Frame(frame) => {
                w.stack.push(frame);
                None
            }
        };
        // Gives finished values to their containers until a container needs
        // its next part.
        part = loop {
            if let Some(v) = done.take() {
                let Some(frame) = w.stack.last_mut() else { return Ok(v) };
                match frame {
                    DecodeFrame::List { .. } | DecodeFrame::Tuple { .. } => w.values.push(v),
                    DecodeFrame::Set { set, .. } => _ = vm.set_add(*set, v)?,
                    DecodeFrame::Map { map, key, .. } => match key.take() {
                        None => {
                            *key = Some(v);
                            super::dec_map_value(f, frames, vm)?;
                        }
                        Some(k) => vm.map_set(*map, k, v)?,
                    },
                    DecodeFrame::Fields { base, field, .. } => w.values[*base + *field] = v,
                    DecodeFrame::Some(_) => {
                        w.stack.pop();
                        done = Some(vm.heap.new_variant(prog.ids.some, &[v]));
                        continue;
                    }
                }
            }
            match w.next_part(vm, f, frames)? {
                Some(s) => break s,
                None => {
                    let frame = w.stack.pop().unwrap_or_else(|| unreachable!());
                    done = Some(w.finish(vm, f, frames, frame)?);
                }
            }
        };
    }
}

impl DecodeWalk<'_> {
    /// Moves to the next part of the innermost frame. Returns the schema of
    /// the part, or `None` after the last part.
    fn next_part(&mut self, vm: &mut Vm, f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>) -> DecodeResult<Option<SchemaId>> {
        let prog = self.prog;
        let len = self.values.len();
        let Some(frame) = self.stack.last_mut() else { return Err(Fail::internal("decode: no open container").into()) };
        Ok(match frame {
            DecodeFrame::List { elem, .. } | DecodeFrame::Set { elem, .. } => super::dec_next_item(f, frames, vm)?.then_some(*elem),
            DecodeFrame::Map { val, key: Some(_), .. } => Some(*val),
            DecodeFrame::Map { key_schema, key: None, .. } => super::dec_next_entry(f, frames, vm)?.then_some(*key_schema),
            DecodeFrame::Tuple { schema, variant, base } => match parts(prog, *schema, *variant)?.get(len - *base) {
                Some(&s) => {
                    super::dec_element(f, frames, vm)?;
                    Some(s)
                }
                None => None,
            },
            DecodeFrame::Fields { schema, variant, field, .. } => {
                let i = super::next_field(f, frames, vm)?;
                if i < 0 {
                    return Ok(None);
                }
                *field = i as usize;
                match parts(prog, *schema, *variant)?.get(*field) {
                    Some(&s) => Some(s),
                    None => return Err(Fail::internal("decode: unknown field").into()),
                }
            }
            DecodeFrame::Some(payload) => payload.take(),
        })
    }

    /// Builds the value of `frame` after its last part.
    fn finish(&mut self, vm: &mut Vm, f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, frame: DecodeFrame) -> DecodeResult<VmValue> {
        let prog = self.prog;
        Ok(match frame {
            DecodeFrame::List { base, .. } => {
                let n = self.values.len() - base;
                vm.check_memory(size_of::<Obj>() + n * size_of::<VmValue>())?;
                vm.heap.new_list(self.values.drain(base..).collect())
            }
            DecodeFrame::Set { set, .. } => set,
            DecodeFrame::Map { map, .. } => map,
            DecodeFrame::Tuple { schema, variant, base } => {
                super::dec_end_tuple(f, frames, vm)?;
                vm.check_memory(size_of::<Obj>() + (self.values.len() - base) * size_of::<VmValue>())?;
                let v = match variant {
                    None => vm.heap.new_tuple(&self.values[base..]),
                    Some(k) => {
                        super::dec_end_variant(f, frames, vm)?;
                        vm.heap.new_variant(variant_id(prog, schema, k)?, &self.values[base..])
                    }
                };
                self.values.truncate(base);
                v
            }
            DecodeFrame::Fields { schema, variant, base, .. } => {
                let parts = parts(prog, schema, variant)?;
                for (i, &part) in parts.iter().enumerate() {
                    if !matches!(self.values[base + i], VmValue::Uninit) {
                        continue;
                    }
                    if !matches!(prog_schema(prog, part)?, Schema::Option(_)) {
                        let (id, is_variant) = match variant {
                            Some(k) => (variant_id(prog, schema, k)?, true),
                            None => (struct_def(prog, schema)?, false),
                        };
                        return Err(DecodeFail::Syntax(super::missing_message(vm, frames, id, is_variant, i)));
                    }
                    self.values[base + i] = VmValue::Tag(prog.ids.none);
                }
                vm.check_memory(size_of::<Obj>() + parts.len() * size_of::<VmValue>())?;
                let v = match variant {
                    None => {
                        let fields = vm.heap.boxed(&self.values[base..]);
                        vm.heap.alloc(Obj::Struct(struct_def(prog, schema)?, fields))
                    }
                    Some(k) => {
                        super::dec_end_variant(f, frames, vm)?;
                        vm.heap.new_variant(variant_id(prog, schema, k)?, &self.values[base..])
                    }
                };
                self.values.truncate(base);
                v
            }
            DecodeFrame::Some(_) => return Err(Fail::internal("decode: Some without a payload").into()),
        })
    }

    /// Reads a scalar, or opens a container and returns its frame.
    fn open(&mut self, vm: &mut Vm, f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, s: SchemaId) -> DecodeResult<Opened> {
        vm.charge(1)?;
        let prog = self.prog;
        let base = self.values.len();
        let v = match prog_schema(prog, s)? {
            Schema::Unit => f.unit(vm).map(|()| VmValue::Unit)?,
            Schema::Bool => VmValue::Bool(f.bool(vm)?),
            Schema::Int => VmValue::Int(f.int(vm)?),
            Schema::Float => VmValue::Float(f.float(vm)?),
            Schema::Duration => VmValue::Dur(f.duration(vm)?),
            Schema::Str => f.str(vm)?,
            Schema::Bytes => f.bytes(vm)?,
            Schema::Range => {
                let (start, end, inclusive) = f.range(vm)?;
                vm.check_memory(size_of::<Obj>())?;
                vm.heap.alloc(Obj::Range(start, end, inclusive))
            }
            &Schema::Native(def) => super::read_native_value(f, vm, def)?,
            Schema::Json => f.json(vm)?,
            &Schema::List(elem) => {
                super::dec_begin_seq(f, frames, vm, SeqKind::List)?;
                return Ok(Opened::Frame(DecodeFrame::List { elem, base }));
            }
            &Schema::Set(elem) => {
                super::dec_begin_seq(f, frames, vm, SeqKind::Set)?;
                vm.check_memory(size_of::<Obj>() + size_of::<SetObj>())?;
                let set = vm.heap.alloc(Obj::Set(Box::default()));
                return Ok(Opened::Frame(DecodeFrame::Set { elem, set }));
            }
            &Schema::Map(key_schema, val) => {
                super::dec_begin_map(f, frames, vm)?;
                vm.check_memory(size_of::<Obj>() + size_of::<MapObj>())?;
                let map = vm.heap.alloc(Obj::Map(Box::default()));
                return Ok(Opened::Frame(DecodeFrame::Map { key_schema, val, map, key: None }));
            }
            &Schema::Option(payload) => match f.some(vm)? {
                true => return Ok(Opened::Frame(DecodeFrame::Some(Some(payload)))),
                false => VmValue::Tag(prog.ids.none),
            },
            Schema::Tuple(parts) => {
                super::dec_begin_seq(f, frames, vm, SeqKind::Tuple(parts.len()))?;
                return Ok(Opened::Frame(DecodeFrame::Tuple { schema: s, variant: None, base }));
            }
            &Schema::Struct(def, ref fields) => {
                super::dec_begin_struct(f, frames, vm, def)?;
                self.values.resize(base + fields.len(), VmValue::Uninit);
                return Ok(Opened::Frame(DecodeFrame::Fields { schema: s, variant: None, base, field: 0 }));
            }
            &Schema::Enum(first, ref variants) => {
                let k = super::read_variant(f, frames, vm, first, variants.len())? as usize;
                let id = first + k as u32;
                let Some(payload) = variants.get(k) else { return Err(Fail::internal("decode: unknown variant").into()) };
                if payload.is_empty() {
                    VmValue::Tag(id)
                } else if prog.variants.get(id as usize).is_some_and(|x| x.field_names.is_some()) {
                    super::dec_begin_fields(f, frames, vm, id)?;
                    self.values.resize(base + payload.len(), VmValue::Uninit);
                    return Ok(Opened::Frame(DecodeFrame::Fields { schema: s, variant: Some(k), base, field: 0 }));
                } else {
                    super::dec_begin_seq(f, frames, vm, SeqKind::Tuple(payload.len()))?;
                    return Ok(Opened::Frame(DecodeFrame::Tuple { schema: s, variant: Some(k), base }));
                }
            }
        };
        Ok(Opened::Value(v))
    }
}

fn struct_def(prog: &Program, schema: SchemaId) -> Result<u32, Fail> {
    match prog_schema(prog, schema)? {
        &Schema::Struct(def, _) => Ok(def),
        _ => internal("decode: not a struct"),
    }
}

fn variant_id(prog: &Program, schema: SchemaId, k: usize) -> Result<u32, Fail> {
    match prog_schema(prog, schema)? {
        &Schema::Enum(first, _) => Ok(first + k as u32),
        _ => internal("decode: not an enum"),
    }
}

/// The schemas of the parts of a tuple or struct, or of the payload of
/// variant `variant` of an enum.
fn parts(prog: &Program, schema: SchemaId, variant: Option<usize>) -> Result<&[SchemaId], Fail> {
    match (prog_schema(prog, schema)?, variant) {
        (Schema::Tuple(p) | Schema::Struct(_, p), None) => Ok(p),
        (Schema::Enum(_, variants), Some(k)) => match variants.get(k) {
            Some(p) => Ok(p),
            None => internal("decode: unknown variant"),
        },
        _ => internal("decode: no parts"),
    }
}
