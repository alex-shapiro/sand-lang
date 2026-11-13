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
//! A walk is one native call, which Rust compiles once for each format
//! ([`EncodeFormat::write_fast`] and [`DecodeFormat::read_fast`]). It charges
//! a fuel unit for each value, as the native walkers of `json.parse` do. If a
//! memory check fails during the walk, the format goes back to its state
//! before the walk, so that the engine can run the call again after a garbage
//! collection.

use super::{DecodeFail, DecodeFormat, DecodeResult, EncodeFormat, Frame, SeqKind};
use crate::vm::bytecode::Program;
use crate::vm::value::{MapObj, Obj, SetObj, VmValue};
use crate::vm::{Fail, Vm, internal};

/// The open containers for which a walk allocates room at its start.
pub(crate) const OPEN_FRAMES: usize = 16;

/// The parts of open containers for which a walk allocates room at its
/// start.
pub(crate) const OPEN_VALUES: usize = 32;

/// The index of a [`Schema`] in the program's schema table.
pub type SchemaId = u32;

/// The [`SchemaId`] that stands for no schema.
pub(crate) const NO_SCHEMA: SchemaId = SchemaId::MAX;

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

/// Writes `value`, whose type has schema `schema`, to `f`.
pub(crate) fn encode<F: EncodeFormat + ?Sized>(vm: &mut Vm, f: &mut F, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
    f.save();
    let prog = vm.prog.clone();
    let mut w = EncodeWalk::new(&prog);
    let r = w.run(vm, f, value, schema, false);
    if r.as_ref().is_err_and(|e| e.as_limit().is_some()) {
        f.restore();
    }
    r
}

/// Writes the map key `key`, whose type has schema `schema`, to `f`, for a
/// format's own walk. It keeps no state of `f` (see [`EncodeFormat::save`]).
pub(crate) fn encode_key<F: EncodeFormat + ?Sized>(vm: &mut Vm, f: &mut F, key: VmValue, schema: SchemaId) -> Result<(), Fail> {
    let prog = vm.prog.clone();
    let mut w = EncodeWalk::new(&prog);
    w.run(vm, f, key, schema, true)
}

/// An open container of the encode walk, with the index of its next part.
enum EncodeFrame<'p> {
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
    },
    /// A map, the slot of its next entry, and the value of the entry whose
    /// key the walk wrote last.
    Map {
        map: VmValue,
        next: usize,
        key: SchemaId,
        val: SchemaId,
        value: Option<VmValue>,
    },
    /// The elements of a tuple, the fields of a struct, or a variant
    /// payload, which are on the walk's value stack from `base`. `named`
    /// means that a field call comes before each part.
    Parts {
        base: usize,
        next: usize,
        parts: &'p [SchemaId],
        named: bool,
    },
    /// The payload of `Some`, until the walk writes it.
    Some(Option<(VmValue, SchemaId)>),
}

/// A part that the encode walk writes next: the value, its schema, and
/// whether it is a map key, which the walk does not charge as a value.
type EncodePart = (VmValue, SchemaId, bool);

/// The state of an encode walk.
struct EncodeWalk<'p> {
    prog: &'p Program,
    stack: Vec<EncodeFrame<'p>>,
    /// The parts of the open tuples, structs, and variant payloads.
    values: Vec<VmValue>,
    /// A buffer for the text of a `str`.
    text: String,
}

impl<'p> EncodeWalk<'p> {
    fn new(prog: &'p Program) -> EncodeWalk<'p> {
        EncodeWalk { prog, stack: Vec::with_capacity(OPEN_FRAMES), values: Vec::with_capacity(OPEN_VALUES), text: String::new() }
    }

    fn run<F: EncodeFormat + ?Sized>(
        &mut self,
        vm: &mut Vm,
        f: &mut F,
        value: VmValue,
        schema: SchemaId,
        is_key: bool,
    ) -> Result<(), Fail> {
        let (mut v, mut s, mut is_key) = (value, schema, is_key);
        loop {
            if !is_key {
                vm.charge(1)?;
            }
            if let Some(frame) = self.open(vm, f, v, s)? {
                self.stack.push(frame);
            }
            (v, s, is_key) = loop {
                let Some(frame) = self.stack.last_mut() else { return Ok(()) };
                match next_encode_part(vm, f, frame, &self.values)? {
                    Some(part) => break part,
                    None => {
                        f.end(vm)?;
                        if let Some(EncodeFrame::Parts { base, .. }) = self.stack.pop() {
                            self.values.truncate(base);
                        }
                    }
                }
            };
        }
    }

    /// Writes the scalar `v`, or opens the container `v` and returns its
    /// frame.
    #[inline(always)]
    fn open<F: EncodeFormat + ?Sized>(&mut self, vm: &mut Vm, f: &mut F, v: VmValue, s: SchemaId) -> Result<Option<EncodeFrame<'p>>, Fail> {
        let prog = self.prog;
        let frame = match (prog_schema(prog, s)?, v) {
            (Schema::Unit, _) => return f.unit(vm).map(|()| None),
            (Schema::Bool, VmValue::Bool(b)) => return f.bool(vm, b).map(|()| None),
            (Schema::Int, VmValue::Int(i)) => return f.int(vm, i).map(|()| None),
            (Schema::Float, VmValue::Float(x)) => return f.float(vm, x).map(|()| None),
            (Schema::Duration, VmValue::Dur(d)) => return f.duration(vm, d).map(|()| None),
            (Schema::Str, _) if f.keeps_values() => return f.str_value(vm, v).map(|()| None),
            (Schema::Str, _) => {
                self.text.clear();
                self.text.push_str(vm.heap.str(v));
                return f.str(vm, &self.text).map(|()| None);
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
                EncodeFrame::List { list: v, next: 0, elem }
            }
            (&Schema::Set(elem), _) => {
                let Obj::Set(set) = vm.heap.get(v) else { return internal("encode: not a set") };
                f.begin_seq(vm, SeqKind::Set, set.live)?;
                EncodeFrame::Set { set: v, next: 0, elem }
            }
            (&Schema::Map(key, val), _) => {
                let Obj::Map(map) = vm.heap.get(v) else { return internal("encode: not a map") };
                f.begin_map(vm, map.live)?;
                EncodeFrame::Map { map: v, next: 0, key, val, value: None }
            }
            (&Schema::Option(payload), _) => match vm.heap.payload_if(&v, prog.ids.some) {
                VmValue::Uninit => return f.none(vm).map(|()| None),
                x => {
                    f.begin_some(vm)?;
                    EncodeFrame::Some(Some((x, payload)))
                }
            },
            (Schema::Tuple(parts), _) => {
                f.begin_seq(vm, SeqKind::Tuple(parts.len()), parts.len())?;
                self.parts(vm, v, parts, false)?
            }
            (Schema::Struct(def, parts), _) => {
                f.begin_struct(vm, *def)?;
                self.parts(vm, v, parts, true)?
            }
            (Schema::Enum(first, variants), _) => {
                let id = vm.heap.variant_id(v);
                let Some(parts) = variants.get(id.wrapping_sub(*first) as usize) else { return internal("encode: unknown variant") };
                if parts.is_empty() {
                    return f.unit_variant(vm, id).map(|()| None);
                }
                f.begin_variant(vm, id)?;
                let named = prog.variants.get(id as usize).is_some_and(|x| x.field_names.is_some());
                self.parts(vm, v, parts, named)?
            }
            _ => return internal("encode: the value does not match its schema"),
        };
        Ok(Some(frame))
    }

    /// Copies the parts of the tuple, struct, or variant `v` to the value
    /// stack, and returns the frame that writes them.
    #[inline(always)]
    fn parts(&mut self, vm: &Vm, v: VmValue, parts: &'p [SchemaId], named: bool) -> Result<EncodeFrame<'p>, Fail> {
        let base = self.values.len();
        match v {
            VmValue::Obj(_) => self.values.extend_from_slice(vm.heap.fields(v)),
            // A pair of ints, or a variant with its payload inline.
            _ => self.values.extend((0..parts.len()).map_while(|i| vm.heap.field(v, i))),
        }
        if self.values.len() - base != parts.len() {
            return internal("encode: the value does not match its schema");
        }
        Ok(EncodeFrame::Parts { base, next: 0, parts, named })
    }
}

/// Moves to the next part of `frame`. Returns `None` after its last part.
#[inline(always)]
fn next_encode_part<F: EncodeFormat + ?Sized>(
    vm: &mut Vm,
    f: &mut F,
    frame: &mut EncodeFrame<'_>,
    values: &[VmValue],
) -> Result<Option<EncodePart>, Fail> {
    Ok(match frame {
        EncodeFrame::List { list, next, elem } => {
            let item = vm.heap.list(*list).get(*next).copied();
            *next += 1;
            item.map(|x| (x, *elem, false))
        }
        EncodeFrame::Set { set, next, elem } => {
            let Obj::Set(s) = vm.heap.get(*set) else { return internal("encode: not a set") };
            let found = s.entries.get(*next..).and_then(|rest| rest.iter().enumerate().find_map(|(k, x)| Some((k, (*x)?))));
            found.map(|(k, x)| {
                *next += k + 1;
                (x, *elem, false)
            })
        }
        EncodeFrame::Map { map, next, key, val, value } => {
            if let Some(x) = value.take() {
                return Ok(Some((x, *val, false)));
            }
            let Obj::Map(m) = vm.heap.get(*map) else { return internal("encode: not a map") };
            let found = m.entries.get(*next..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((k, (*e)?))));
            found.map(|(k, (key_value, x))| {
                *next += k + 1;
                *value = Some(x);
                (key_value, *key, true)
            })
        }
        EncodeFrame::Parts { base, next, parts, named } => {
            let Some(&s) = parts.get(*next) else { return Ok(None) };
            let i = *next;
            *next += 1;
            if *named {
                f.field(vm, i)?;
            }
            Some((values[*base + i], s, false))
        }
        EncodeFrame::Some(payload) => payload.take().map(|(x, s)| (x, s, false)),
    })
}

fn prog_schema(prog: &Program, id: SchemaId) -> Result<&Schema, Fail> {
    match prog.schemas.get(id as usize) {
        Some(s) => Ok(s),
        None => internal("codec: unknown schema"),
    }
}

/// Reads a value whose type has schema `schema` from `f`. `path` holds the
/// open containers of the decoder before the walk; the walk adds its own if
/// it fails. After a failed memory check, the caller returns `f` to a mark
/// before the walk.
pub(super) fn decode<F: DecodeFormat + ?Sized>(vm: &mut Vm, f: &mut F, path: &mut Vec<Frame>, schema: SchemaId) -> DecodeResult<VmValue> {
    let prog = vm.prog.clone();
    let mut w = DecodeWalk {
        prog: &prog,
        stack: Vec::with_capacity(OPEN_FRAMES),
        values: Vec::with_capacity(OPEN_VALUES),
        key_texts: String::new(),
    };
    let r = w.run(vm, f, path, schema);
    if let Err(e) = &r
        && !matches!(e, DecodeFail::Fail(_))
    {
        w.extend_path(path);
    }
    r
}

/// A container that the decode walk is reading. The parts that a tuple or
/// struct has read are on the walk's value stack from `base`. `index` and
/// `field` are the part being read, for the path of an error.
enum DecodeFrame<'p> {
    List {
        elem: SchemaId,
        items: Vec<VmValue>,
        index: Option<usize>,
    },
    Set {
        elem: SchemaId,
        set: VmValue,
        index: Option<usize>,
    },
    /// A map. `entry_key` holds the key of the entry whose value comes
    /// next. The text of the current key starts at `text` in the walk's
    /// `key_texts`.
    Map {
        key: SchemaId,
        val: SchemaId,
        map: VmValue,
        entry_key: Option<VmValue>,
        text: usize,
    },
    /// A tuple, or the positional payload of the variant in the
    /// [`DecodeFrame::Variant`] frame below.
    Tuple {
        parts: &'p [SchemaId],
        base: usize,
        index: Option<usize>,
        variant: Option<u32>,
    },
    /// The fields of struct definition `id` (`variant == false`), or the
    /// named payload of variant `id`, whose slots hold `Uninit` until read.
    Fields {
        parts: &'p [SchemaId],
        names: &'p [String],
        id: u32,
        variant: bool,
        base: usize,
        field: Option<usize>,
        guess: usize,
    },
    /// The payload of variant `id`, which the frame above reads.
    Variant(u32),
    /// `Some`, and the schema of its payload until the walk reads it.
    Some(Option<SchemaId>),
}

/// The state of a decode walk.
struct DecodeWalk<'p> {
    prog: &'p Program,
    stack: Vec<DecodeFrame<'p>>,
    /// The parts read so far of the open lists, tuples, and structs.
    values: Vec<VmValue>,
    /// The texts of the current keys of the open maps.
    key_texts: String,
}

/// What reading the start of a value gives.
enum Opened<'p> {
    Value(VmValue),
    Frame(DecodeFrame<'p>),
}

impl<'p> DecodeWalk<'p> {
    fn run<F: DecodeFormat + ?Sized>(&mut self, vm: &mut Vm, f: &mut F, path: &[Frame], schema: SchemaId) -> DecodeResult<VmValue> {
        let mut part = schema;
        let mut is_key = false;
        loop {
            // A map key is part of its entry, as in `json.parse`: the walk
            // charges a unit for each value, and the input bytes since the
            // last value.
            if !is_key {
                vm.charge(1 + f.take_input_fuel())?;
            }
            let mut done = match self.open(vm, f, part)? {
                Opened::Value(v) => Some(v),
                Opened::Frame(frame) => {
                    self.stack.push(frame);
                    None
                }
            };
            // Gives finished values to their containers until a container
            // needs its next part.
            (part, is_key) = loop {
                if let Some(v) = done.take() {
                    let Some(frame) = self.stack.last_mut() else { return Ok(v) };
                    match frame {
                        DecodeFrame::List { items, .. } => push_item(vm, items, v)?,
                        DecodeFrame::Tuple { .. } => self.values.push(v),
                        DecodeFrame::Set { set, .. } => _ = vm.set_add(*set, v)?,
                        DecodeFrame::Map { map, entry_key, .. } => match entry_key.take() {
                            None => {
                                *entry_key = Some(v);
                                f.map_value(vm)?;
                            }
                            Some(k) => vm.map_set(*map, k, v)?,
                        },
                        DecodeFrame::Fields { base, field, .. } => {
                            let slot = *base + field.unwrap_or_default();
                            self.values[slot] = v;
                        }
                        DecodeFrame::Some(_) => {
                            self.stack.pop();
                            done = Some(vm.heap.new_variant(self.prog.ids.some, &[v]));
                            continue;
                        }
                        DecodeFrame::Variant(_) => return Err(Fail::internal("decode: a variant without its payload").into()),
                    }
                }
                match self.next_part(vm, f)? {
                    Some((s, is_key)) => match prog_schema(self.prog, s)? {
                        scalar if is_scalar(scalar) => {
                            if !is_key {
                                vm.charge(1 + f.take_input_fuel())?;
                            }
                            done = Some(self.read_scalar(vm, f, scalar)?);
                        }
                        _ => break (s, is_key),
                    },
                    None => done = Some(self.finish(vm, f, path)?),
                }
            };
        }
    }

    /// Moves to the next part of the innermost frame. Returns the schema of
    /// the part and whether it is a map key, or `None` after the last part.
    #[inline(always)]
    fn next_part<F: DecodeFormat + ?Sized>(&mut self, vm: &mut Vm, f: &mut F) -> DecodeResult<Option<(SchemaId, bool)>> {
        let len = self.values.len();
        let Some(frame) = self.stack.last_mut() else { return Err(Fail::internal("decode: no open container").into()) };
        Ok(match frame {
            DecodeFrame::List { elem, index, .. } | DecodeFrame::Set { elem, index, .. } => {
                if !f.next_item(vm)? {
                    return Ok(None);
                }
                *index = Some(index.map_or(0, |i| i + 1));
                Some((*elem, false))
            }
            DecodeFrame::Map { val, entry_key: Some(_), .. } => Some((*val, false)),
            DecodeFrame::Map { key, text, .. } => {
                let (key, text) = (*key, *text);
                let Some(key_text) = f.next_entry(vm)? else { return Ok(None) };
                self.key_texts.truncate(text);
                self.key_texts.push_str(&key_text);
                Some((key, true))
            }
            DecodeFrame::Tuple { parts, base, index, .. } => {
                let Some(&s) = parts.get(len - *base) else { return Ok(None) };
                if !f.next_item(vm)? {
                    let n = parts.len();
                    self.stack.pop();
                    return Err(DecodeFail::Mismatch(f.length_mismatch(n)));
                }
                *index = Some(index.map_or(0, |i| i + 1));
                Some((s, false))
            }
            DecodeFrame::Fields { parts, names, id, variant, field, guess, .. } => loop {
                let found = {
                    let Some(key) = f.next_field(vm)? else { return Ok(None) };
                    if names.get(*guess).is_some_and(|n| *n == *key) {
                        Some(*guess)
                    } else {
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
                        *field = Some(i);
                        *guess = i + 1;
                        match parts.get(i) {
                            Some(&s) => break Some((s, false)),
                            None => return Err(Fail::internal("decode: unknown field").into()),
                        }
                    }
                    None => f.skip(vm)?,
                }
            },
            DecodeFrame::Some(payload) => payload.take().map(|s| (s, false)),
            DecodeFrame::Variant(_) => return Err(Fail::internal("decode: a variant without its payload").into()),
        })
    }

    /// Ends the innermost frame after its last part and builds its value.
    /// The frame is not on the path of an error that ending it finds.
    fn finish<F: DecodeFormat + ?Sized>(&mut self, vm: &mut Vm, f: &mut F, path: &[Frame]) -> DecodeResult<VmValue> {
        let Some(frame) = self.stack.pop() else { return Err(Fail::internal("decode: no open container").into()) };
        Ok(match frame {
            DecodeFrame::List { items, .. } => {
                vm.check_memory(size_of::<Obj>())?;
                vm.heap.new_list(items)
            }
            DecodeFrame::Set { set, .. } => set,
            DecodeFrame::Map { map, text, .. } => {
                self.key_texts.truncate(text);
                map
            }
            DecodeFrame::Tuple { parts, base, variant, .. } => {
                if f.next_item(vm)? {
                    return Err(DecodeFail::Mismatch(f.length_mismatch(parts.len())));
                }
                vm.check_memory(size_of::<Obj>() + parts.len() * size_of::<VmValue>())?;
                let v = match variant {
                    None => vm.heap.new_tuple(&self.values[base..]),
                    Some(id) => {
                        self.end_variant(vm, f)?;
                        vm.heap.new_variant(id, &self.values[base..])
                    }
                };
                self.values.truncate(base);
                v
            }
            DecodeFrame::Fields { parts, id, variant, base, .. } => {
                for (i, &part) in parts.iter().enumerate() {
                    if !matches!(self.values[base + i], VmValue::Uninit) {
                        continue;
                    }
                    if !matches!(prog_schema(self.prog, part)?, Schema::Option(_)) {
                        let mut frames = path.to_vec();
                        self.extend_path(&mut frames);
                        return Err(DecodeFail::Syntax(super::missing_message(vm, &frames, id, variant, i)));
                    }
                    self.values[base + i] = VmValue::Tag(self.prog.ids.none);
                }
                vm.check_memory(size_of::<Obj>() + parts.len() * size_of::<VmValue>())?;
                let v = if variant {
                    self.end_variant(vm, f)?;
                    vm.heap.new_variant(id, &self.values[base..])
                } else {
                    let fields = vm.heap.boxed(&self.values[base..]);
                    vm.heap.alloc(Obj::Struct(id, fields))
                };
                self.values.truncate(base);
                v
            }
            DecodeFrame::Variant(_) | DecodeFrame::Some(_) => return Err(Fail::internal("decode: a frame without parts").into()),
        })
    }

    /// Closes the variant whose payload ended, which is the innermost frame.
    fn end_variant<F: DecodeFormat + ?Sized>(&mut self, vm: &mut Vm, f: &mut F) -> DecodeResult<()> {
        let Some(DecodeFrame::Variant(id)) = self.stack.pop() else { return Err(Fail::internal("decode: no open variant").into()) };
        let def = self.prog.variants.get(id as usize).map_or(0, |v| v.def);
        f.end_variant(vm, self.type_name(def))
    }

    /// Returns the name of runtime type definition `def`, as messages show
    /// it.
    fn type_name(&self, def: u32) -> &'p str {
        self.prog.types.get(def as usize).map_or("", |t| t.name.as_str())
    }

    /// Reads a scalar, or opens a container and returns its frame. A
    /// container adds nothing to the path until its first part.
    #[inline(always)]
    fn open<F: DecodeFormat + ?Sized>(&mut self, vm: &mut Vm, f: &mut F, s: SchemaId) -> DecodeResult<Opened<'p>> {
        let prog = self.prog;
        let base = self.values.len();
        let v = match prog_schema(prog, s)? {
            scalar if is_scalar(scalar) => self.read_scalar(vm, f, scalar)?,
            Schema::Range => {
                let (start, end, inclusive) = f.range(vm)?;
                vm.check_memory(size_of::<Obj>())?;
                vm.heap.alloc(Obj::Range(start, end, inclusive))
            }
            &Schema::Native(def) => super::read_native_value(f, vm, def)?,
            Schema::Json => f.json(vm)?,
            &Schema::List(elem) => {
                f.begin_seq(vm, SeqKind::List)?;
                return Ok(Opened::Frame(DecodeFrame::List { elem, items: Vec::new(), index: None }));
            }
            &Schema::Set(elem) => {
                f.begin_seq(vm, SeqKind::Set)?;
                vm.check_memory(size_of::<Obj>() + size_of::<SetObj>())?;
                let set = vm.heap.alloc(Obj::Set(Box::default()));
                return Ok(Opened::Frame(DecodeFrame::Set { elem, set, index: None }));
            }
            &Schema::Map(key, val) => {
                f.begin_map(vm)?;
                vm.check_memory(size_of::<Obj>() + size_of::<MapObj>())?;
                let map = vm.heap.alloc(Obj::Map(Box::default()));
                return Ok(Opened::Frame(DecodeFrame::Map { key, val, map, entry_key: None, text: self.key_texts.len() }));
            }
            &Schema::Option(payload) => match f.some(vm)? {
                true => return Ok(Opened::Frame(DecodeFrame::Some(Some(payload)))),
                false => VmValue::Tag(prog.ids.none),
            },
            Schema::Tuple(parts) => {
                f.begin_seq(vm, SeqKind::Tuple(parts.len()))?;
                return Ok(Opened::Frame(DecodeFrame::Tuple { parts, base, index: None, variant: None }));
            }
            Schema::Struct(def, parts) => {
                f.begin_fields(vm, self.type_name(*def))?;
                let names = prog.types.get(*def as usize).map_or(&[][..], |t| &t.fields[..]);
                self.values.resize(base + parts.len(), VmValue::Uninit);
                let frame = DecodeFrame::Fields { parts, names, id: *def, variant: false, base, field: None, guess: 0 };
                return Ok(Opened::Frame(frame));
            }
            Schema::Enum(first, variants) => return self.open_variant(vm, f, *first, variants),
            Schema::Unit | Schema::Bool | Schema::Int | Schema::Float | Schema::Duration | Schema::Str | Schema::Bytes => {
                return Err(Fail::internal("decode: a scalar schema").into());
            }
        };
        Ok(Opened::Value(v))
    }

    /// Reads a value of the scalar schema `s` (see [`is_scalar`]).
    #[inline(always)]
    fn read_scalar<F: DecodeFormat + ?Sized>(&self, vm: &mut Vm, f: &mut F, s: &Schema) -> DecodeResult<VmValue> {
        Ok(match s {
            Schema::Unit => f.unit(vm).map(|()| VmValue::Unit)?,
            Schema::Bool => VmValue::Bool(f.bool(vm)?),
            Schema::Int => VmValue::Int(f.int(vm)?),
            Schema::Float => VmValue::Float(f.float(vm)?),
            Schema::Duration => VmValue::Dur(f.duration(vm)?),
            Schema::Str => f.str(vm)?,
            Schema::Bytes => f.bytes(vm)?,
            _ => return Err(Fail::internal("decode: not a scalar schema").into()),
        })
    }

    /// Reads a variant of the enum whose first variant has runtime id
    /// `first`, and whose variants have the payloads `variants`.
    fn open_variant<F: DecodeFormat + ?Sized>(
        &mut self,
        vm: &mut Vm,
        f: &mut F,
        first: u32,
        variants: &'p [Box<[SchemaId]>],
    ) -> DecodeResult<Opened<'p>> {
        let prog = self.prog;
        let rts = prog.variants.get(first as usize..first as usize + variants.len()).unwrap_or_default();
        let enum_name = self.type_name(rts.first().map_or(0, |v| v.def));
        let (k, has_payload) = {
            let (name, has_payload) = f.variant(vm, enum_name)?;
            vm.charge(1 + (variants.len() / 16) as u64)?;
            let Some(k) = rts.iter().position(|v| v.name == *name) else {
                let tag = crate::stdlib::truncate(&name, super::QUOTED_KEY);
                return Err(DecodeFail::Mismatch(format!("unknown variant `{tag}` of {enum_name}")));
            };
            if !has_payload && !variants[k].is_empty() {
                return Err(DecodeFail::Mismatch(format!("variant `{name}` of {enum_name} needs a payload")));
            }
            (k, has_payload)
        };
        let id = first + k as u32;
        let parts = &variants[k][..];
        if !has_payload {
            return Ok(Opened::Value(VmValue::Tag(id)));
        }
        self.stack.push(DecodeFrame::Variant(id));
        if parts.is_empty() {
            // A payload-less variant in object form has an empty payload.
            f.begin_seq(vm, SeqKind::Tuple(0))?;
            if f.next_item(vm)? {
                return Err(DecodeFail::Mismatch("expected array of length 0".into()));
            }
            self.end_variant(vm, f)?;
            return Ok(Opened::Value(VmValue::Tag(id)));
        }
        let base = self.values.len();
        match rts[k].field_names.as_deref() {
            Some(names) => {
                f.begin_fields(vm, "")?;
                self.values.resize(base + parts.len(), VmValue::Uninit);
                Ok(Opened::Frame(DecodeFrame::Fields { parts, names, id, variant: true, base, field: None, guess: 0 }))
            }
            None => {
                f.begin_seq(vm, SeqKind::Tuple(parts.len()))?;
                Ok(Opened::Frame(DecodeFrame::Tuple { parts, base, index: None, variant: Some(id) }))
            }
        }
    }

    /// Adds the open containers of the walk to `path`, as the synthesized
    /// implementations would have them at the same point.
    fn extend_path(&self, path: &mut Vec<Frame>) {
        for (i, frame) in self.stack.iter().enumerate() {
            path.push(match *frame {
                DecodeFrame::List { index, .. } | DecodeFrame::Set { index, .. } => Frame::Seq { index, len: None },
                DecodeFrame::Tuple { parts, index, .. } => Frame::Seq { index, len: Some(parts.len()) },
                DecodeFrame::Map { entry_key, text, .. } => {
                    // The key of a map ends where the key of the next open map
                    // starts.
                    let end = self.stack[i + 1..].iter().find_map(|f| match f {
                        DecodeFrame::Map { text, .. } => Some(*text),
                        _ => None,
                    });
                    let key = self.key_texts.get(text..end.unwrap_or(self.key_texts.len())).unwrap_or_default();
                    Frame::Map { key: entry_key.is_some().then(|| key.to_owned()), pending: None }
                }
                DecodeFrame::Fields { id, variant, field, guess, .. } => Frame::Fields { id, variant, field, guess },
                DecodeFrame::Variant(id) => Frame::Variant { id },
                DecodeFrame::Some(_) => continue,
            });
        }
    }
}

/// Returns `true` if a value of schema `s` is one format operation.
#[inline(always)]
fn is_scalar(s: &Schema) -> bool {
    matches!(s, Schema::Unit | Schema::Bool | Schema::Int | Schema::Float | Schema::Duration | Schema::Str | Schema::Bytes)
}

/// Adds `v` to the list `items`, after a memory check if the list grows.
#[inline(always)]
fn push_item(vm: &Vm, items: &mut Vec<VmValue>, v: VmValue) -> Result<(), Fail> {
    if items.len() == items.capacity() {
        let more = items.capacity().max(4);
        vm.check_memory((items.capacity() + more) * size_of::<VmValue>())?;
        items.reserve_exact(more);
    }
    items.push(v);
    Ok(())
}
