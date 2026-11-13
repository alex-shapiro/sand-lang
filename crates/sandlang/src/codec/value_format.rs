//! The host [`Value`] format of `Encoder` and `Decoder`.
//!
//! The encoder builds the `Value` that a host receives through `Encoded<N>`
//! and run results. The decoder reads a host `Value`, which can also have
//! the JSON form of the target type, such as a Base64 `Value::Str` for
//! `bytes`.

use super::fast::{Schema, SchemaId};
use super::value_input::{self, ValueReader};
use super::{Budget, DecodeFail, DecodeFormat, DecodeResult, EncodeFormat, SeqKind};
use crate::codec::MAX_NESTING_DEPTH;
use crate::stdlib::json::Json;
use crate::value::{NativeValue, StructValue, Value, VariantDesc, VariantValue, is_json_variant, is_option_variant};
use crate::vm::bytecode::Program;
use crate::vm::value::{Obj, VmValue};
use crate::vm::{Fail, Vm, fail, internal};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::sync::Arc;

/// A container that the encoder is building.
enum Partial {
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    /// Map entries, and `true` while the last entry waits for its value.
    Map(Vec<(Value, Value)>, bool),
    Struct(Arc<crate::value::StructDesc>, Vec<Value>),
    Variant(Arc<VariantDesc>, Vec<Value>),
}

/// Builds a [`Value`].
pub(crate) struct ValueEncoder {
    /// The finished value, which [`Encoded`](crate::native::ty::Encoded)
    /// parameters share.
    root: Option<Arc<Value>>,
    stack: Vec<Partial>,
    /// Bytes of Rust memory that the value holds.
    allocated: usize,
    budget: Budget,
    /// The state that [`EncodeFormat::save`] kept.
    saved: Option<EncoderMark>,
}

/// The state of a [`ValueEncoder`] before a value.
#[derive(Clone, Copy)]
struct EncoderMark {
    /// The open containers.
    open: usize,
    allocated: usize,
}

impl ValueEncoder {
    pub(crate) fn new() -> ValueEncoder {
        ValueEncoder { root: None, stack: Vec::new(), allocated: 0, budget: Budget::default(), saved: None }
    }

    /// Returns the finished value, or `None` if a container is open or no
    /// value was written.
    pub(crate) fn encoded(&self) -> Option<&Arc<Value>> {
        if self.stack.is_empty() { self.root.as_ref() } else { None }
    }

    /// Returns the finished value and the bytes of Rust memory that it
    /// holds.
    pub(crate) fn into_encoded(self) -> Option<(Arc<Value>, usize)> {
        let allocated = self.allocated;
        self.stack.is_empty().then_some(self.root?).map(|v| (v, allocated))
    }

    /// Counts `bytes` more of Rust memory against the memory limit.
    fn reserve(&mut self, vm: &mut Vm, bytes: usize) -> Result<(), Fail> {
        self.budget.grow(vm, self.allocated.saturating_add(bytes))?;
        self.allocated += bytes;
        Ok(())
    }

    fn push(&mut self, vm: &mut Vm, v: Value) -> Result<(), Fail> {
        if let Some(at) = self.stack.len().checked_sub(1) {
            self.prepare(vm, at)?;
        }
        self.place(v)
    }

    /// Counts the growth of container `at` of the stack for a new value
    /// against the memory limit, before any change.
    fn prepare(&mut self, vm: &mut Vm, at: usize) -> Result<(), Fail> {
        let grow = match self.stack.get(at) {
            Some(Partial::Map(entries, false)) if entries.len() == entries.capacity() => entries.len().max(4) * size_of::<(Value, Value)>(),
            Some(
                Partial::List(items) | Partial::Tuple(items) | Partial::Set(items) | Partial::Struct(_, items) | Partial::Variant(_, items),
            ) if items.len() == items.capacity() => items.len().max(4) * size_of::<Value>(),
            _ => 0,
        };
        if grow > 0 {
            self.reserve(vm, grow)?;
        }
        Ok(())
    }

    /// Adds `v` to the innermost container.
    fn place(&mut self, v: Value) -> Result<(), Fail> {
        match self.stack.last_mut() {
            Some(
                Partial::List(items) | Partial::Tuple(items) | Partial::Set(items) | Partial::Struct(_, items) | Partial::Variant(_, items),
            ) => items.push(v),
            Some(Partial::Map(entries, has_key)) => {
                match entries.last_mut() {
                    Some(entry) if *has_key => entry.1 = v,
                    _ => entries.push((v, Value::Unit)),
                }
                *has_key = !*has_key;
            }
            None => self.root = Some(Arc::new(v)),
        }
        Ok(())
    }

    fn open(&mut self, partial: Partial) -> Result<(), Fail> {
        if self.stack.len() >= MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to copy");
        }
        self.stack.push(partial);
        Ok(())
    }

    fn text(&mut self, vm: &mut Vm, s: &str) -> Result<String, Fail> {
        vm.charge_bytes(s.len())?;
        self.reserve(vm, s.len())?;
        Ok(s.to_string())
    }

    fn variant_desc(vm: &Vm, id: u32) -> Result<Arc<VariantDesc>, Fail> {
        match vm.prog.variant_desc(id) {
            Some(d) => Ok(d.clone()),
            None => internal("encode: unknown variant"),
        }
    }
}

/// Returns the bytes of Rust memory that a copy of `value` holds besides the
/// `Value` itself.
fn owned_size(value: &Value) -> usize {
    let mut size = 0;
    let mut todo = vec![value];
    while let Some(v) = todo.pop() {
        size += match v {
            Value::Str(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Native(n) => size_of::<NativeValue>() + n.text.len(),
            Value::List(items) | Value::Tuple(items) | Value::Set(items) => {
                todo.extend(items);
                items.len() * size_of::<Value>()
            }
            Value::Map(entries) => {
                todo.extend(entries.iter().flat_map(|(k, v)| [k, v]));
                entries.len() * size_of::<(Value, Value)>()
            }
            Value::Struct(s) => {
                todo.extend(&s.fields[..]);
                s.fields.len() * size_of::<Value>()
            }
            Value::Variant(v) => {
                todo.extend(&v.payload[..]);
                v.payload.len() * size_of::<Value>()
            }
            _ => 0,
        };
    }
    size
}

/// A container that [`ValueEncoder::build`] is copying, with its next part.
enum BuildFrame<'p> {
    List {
        list: VmValue,
        next: usize,
        elem: SchemaId,
        items: Vec<Value>,
    },
    Set {
        set: VmValue,
        next: usize,
        elem: SchemaId,
        items: Vec<Value>,
    },
    /// A map, its next entry, and the value of the entry whose key the walk
    /// copied last.
    Map {
        map: VmValue,
        next: usize,
        key: SchemaId,
        val: SchemaId,
        entries: Vec<(Value, Value)>,
        value: Option<VmValue>,
    },
    /// The parts of a tuple, struct, or variant payload, which are on the
    /// walk's value stack from `base`.
    Parts {
        base: usize,
        parts: &'p [SchemaId],
        items: Vec<Value>,
        of: PartsOf,
    },
    /// `Some` and its payload, until the walk copies it.
    Some {
        payload: Option<(VmValue, SchemaId)>,
        items: Vec<Value>,
    },
}

/// The kind of value that the parts of a [`BuildFrame::Parts`] make.
enum PartsOf {
    Tuple,
    Struct(Arc<crate::value::StructDesc>),
    Variant(Arc<VariantDesc>),
}

impl ValueEncoder {
    /// Copies `value`, whose closed type has the fast-path schema `schema`,
    /// to a [`Value`], as the general walker and the operations of this
    /// format would. It charges a fuel unit for each value except map keys.
    fn build(&mut self, vm: &mut Vm, value: VmValue, schema: SchemaId) -> Result<Value, Fail> {
        let prog = vm.prog.clone();
        // Most values that a host receives are small: their walk allocates
        // nothing for its own state.
        let mut stack: SmallVec<[BuildFrame<'_>; 8]> = SmallVec::new();
        let mut values: SmallVec<[VmValue; 16]> = SmallVec::new();
        vm.charge(1)?;
        match self.build_part(vm, &prog, &mut values, value, schema)? {
            Ok(copy) => return Ok(copy),
            Err(_) if self.stack.len() >= MAX_NESTING_DEPTH => return fail("limit", "value too deeply nested to copy"),
            Err(frame) => stack.push(frame),
        }
        loop {
            let Some(frame) = stack.last_mut() else { return internal("encode: no open value") };
            // The part is read and copied in one step, so that the walk
            // keeps no `VmValue` across steps.
            let next = match frame {
                BuildFrame::List { list, next, elem, .. } => {
                    let item = vm.heap.list(*list).get(*next).copied();
                    *next += 1;
                    item.map(|x| (x, *elem, false))
                }
                BuildFrame::Set { set, next, elem, .. } => {
                    let Obj::Set(set) = vm.heap.get(*set) else { return internal("encode: not a set") };
                    let found = set.entries.get(*next..).and_then(|rest| rest.iter().enumerate().find_map(|(k, x)| Some((k, (*x)?))));
                    found.map(|(k, x)| {
                        *next += k + 1;
                        (x, *elem, false)
                    })
                }
                BuildFrame::Map { map, next, key, val, value, .. } => match value.take() {
                    Some(x) => Some((x, *val, false)),
                    None => {
                        let Obj::Map(m) = vm.heap.get(*map) else { return internal("encode: not a map") };
                        let found = m.entries.get(*next..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((k, (*e)?))));
                        found.map(|(k, (key_value, x))| {
                            *next += k + 1;
                            *value = Some(x);
                            (key_value, *key, true)
                        })
                    }
                },
                BuildFrame::Parts { base, parts, items, .. } => parts.get(items.len()).map(|&p| (values[*base + items.len()], p, false)),
                BuildFrame::Some { payload, .. } => payload.take().map(|(x, p)| (x, p, false)),
            };
            let copy = match next {
                Some((v, s, is_key)) => {
                    if !is_key {
                        vm.charge(1)?;
                    }
                    match self.build_part(vm, &prog, &mut values, v, s)? {
                        Ok(copy) => copy,
                        Err(frame) => {
                            if self.stack.len() + stack.len() >= MAX_NESTING_DEPTH {
                                return fail("limit", "value too deeply nested to copy");
                            }
                            stack.push(frame);
                            continue;
                        }
                    }
                }
                None => {
                    let Some(frame) = stack.pop() else { return internal("encode: no open value") };
                    match frame {
                        BuildFrame::List { items, .. } => Value::List(items),
                        BuildFrame::Set { items, .. } => Value::Set(items),
                        BuildFrame::Map { entries, .. } => Value::Map(entries),
                        BuildFrame::Parts { base, items, of, .. } => {
                            values.truncate(base);
                            match of {
                                PartsOf::Tuple => Value::Tuple(items),
                                PartsOf::Struct(ty) => Value::Struct(StructValue { ty, fields: items.into_boxed_slice() }),
                                PartsOf::Variant(variant) => Value::Variant(VariantValue { variant, payload: items.into_boxed_slice() }),
                            }
                        }
                        BuildFrame::Some { items, .. } => Value::Variant(VariantValue {
                            variant: Self::variant_desc(vm, prog.ids.some)?,
                            payload: items.into_boxed_slice(),
                        }),
                    }
                }
            };
            match stack.last_mut() {
                None => return Ok(copy),
                Some(BuildFrame::List { items, .. } | BuildFrame::Set { items, .. }) => items.push(copy),
                Some(BuildFrame::Parts { items, .. } | BuildFrame::Some { items, .. }) => items.push(copy),
                Some(BuildFrame::Map { entries, value, .. }) => match entries.last_mut() {
                    Some(entry) if value.is_none() => entry.1 = copy,
                    _ => entries.push((copy, Value::Unit)),
                },
            }
        }
    }

    /// Copies the scalar `v`, or opens the container `v` and returns its
    /// frame.
    #[inline(always)]
    fn build_part<'p>(
        &mut self,
        vm: &mut Vm,
        prog: &'p Program,
        values: &mut SmallVec<[VmValue; 16]>,
        v: VmValue,
        s: SchemaId,
    ) -> Result<Result<Value, BuildFrame<'p>>, Fail> {
        let Some(schema) = prog.schemas.get(s as usize) else { return internal("codec: unknown schema") };
        let mut parts = |vm: &Vm, parts: &'p [SchemaId], of: PartsOf| {
            let base = values.len();
            match v {
                VmValue::Obj(_) => values.extend_from_slice(vm.heap.fields(v)),
                // A pair of ints, or a variant with its payload inline.
                _ => values.extend((0..parts.len()).map_while(|i| vm.heap.field(v, i))),
            }
            if values.len() - base != parts.len() {
                return internal("encode: the value does not match its schema");
            }
            Ok(Err(BuildFrame::Parts { base, parts, items: Vec::with_capacity(parts.len()), of }))
        };
        let value = match (schema, v) {
            (Schema::Unit, _) => Value::Unit,
            (Schema::Bool, VmValue::Bool(b)) => Value::Bool(b),
            (Schema::Int, VmValue::Int(i)) => Value::Int(i),
            (Schema::Float, VmValue::Float(x)) => Value::Float(x),
            (Schema::Duration, VmValue::Dur(d)) => Value::Duration(jiff::SignedDuration::from_nanos(d)),
            (Schema::Str, _) => {
                let len = vm.heap.str(v).len();
                vm.charge_bytes(len)?;
                self.reserve(vm, len)?;
                Value::Str(vm.heap.str(v).to_string())
            }
            (Schema::Bytes, _) => {
                let len = vm.heap.bytes_of(v).len();
                vm.charge_bytes(len)?;
                self.reserve(vm, len)?;
                Value::Bytes(vm.heap.bytes_of(v).to_vec())
            }
            (Schema::Range, _) => {
                let Obj::Range(start, end, inclusive) = *vm.heap.get(v) else { return internal("encode: not a range") };
                Value::Range { start, end, inclusive }
            }
            (Schema::Native(_), _) => {
                let Some((type_name, text)) = super::native_text(vm, v) else { return internal("encode: not a native value") };
                let text = self.text(vm, &text)?;
                let type_name: Arc<str> = self.text(vm, &type_name)?.into();
                Value::Native(Box::new(NativeValue { type_name, text }))
            }
            (Schema::Json, _) => crate::stdlib::json::json_host_value(vm, v, &mut |vm, bytes| self.reserve(vm, bytes))?,
            (&Schema::List(elem), _) => {
                let len = vm.heap.list(v).len();
                self.reserve(vm, len.min(1 << 16) * size_of::<Value>())?;
                return Ok(Err(BuildFrame::List { list: v, next: 0, elem, items: Vec::with_capacity(len.min(1 << 16)) }));
            }
            (&Schema::Set(elem), _) => {
                let Obj::Set(set) = vm.heap.get(v) else { return internal("encode: not a set") };
                let len = set.live.min(1 << 16);
                self.reserve(vm, len * size_of::<Value>())?;
                return Ok(Err(BuildFrame::Set { set: v, next: 0, elem, items: Vec::with_capacity(len) }));
            }
            (&Schema::Map(key, val), _) => {
                let Obj::Map(map) = vm.heap.get(v) else { return internal("encode: not a map") };
                let len = map.live.min(1 << 16);
                self.reserve(vm, len * size_of::<(Value, Value)>())?;
                let entries = Vec::with_capacity(len);
                return Ok(Err(BuildFrame::Map { map: v, next: 0, key, val, entries, value: None }));
            }
            (&Schema::Option(payload), _) => match vm.heap.payload_if(&v, prog.ids.some) {
                VmValue::Uninit => {
                    Value::Variant(VariantValue { variant: Self::variant_desc(vm, prog.ids.none)?, payload: Box::default() })
                }
                x => {
                    self.reserve(vm, size_of::<Value>())?;
                    return Ok(Err(BuildFrame::Some { payload: Some((x, payload)), items: Vec::with_capacity(1) }));
                }
            },
            (Schema::Tuple(ps), _) => {
                self.reserve(vm, ps.len() * size_of::<Value>())?;
                return parts(vm, ps, PartsOf::Tuple);
            }
            (Schema::Struct(def, ps, _), _) => {
                let Some(t) = prog.types.get(*def as usize) else { return internal("encode: unknown struct") };
                self.reserve(vm, ps.len() * size_of::<Value>())?;
                return parts(vm, ps, PartsOf::Struct(t.desc().clone()));
            }
            (Schema::Enum(first, variants), _) => {
                let id = vm.heap.variant_id(v);
                let Some(ps) = variants.get(id.wrapping_sub(*first) as usize) else { return internal("encode: unknown variant") };
                let variant = Self::variant_desc(vm, id)?;
                if ps.is_empty() {
                    Value::Variant(VariantValue { variant, payload: Box::default() })
                } else {
                    self.reserve(vm, ps.len() * size_of::<Value>())?;
                    return parts(vm, ps, PartsOf::Variant(variant));
                }
            }
            _ => return internal("encode: the value does not match its schema"),
        };
        Ok(Ok(value))
    }
}

impl EncodeFormat for ValueEncoder {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.push(vm, Value::Unit)
    }

    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail> {
        self.push(vm, Value::Bool(b))
    }

    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail> {
        self.push(vm, Value::Int(i))
    }

    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail> {
        self.push(vm, Value::Float(f))
    }

    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail> {
        self.push(vm, Value::Duration(jiff::SignedDuration::from_nanos(nanos)))
    }

    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail> {
        let s = self.text(vm, s)?;
        self.push(vm, Value::Str(s))
    }

    fn bytes(&mut self, vm: &mut Vm, b: &[u8]) -> Result<(), Fail> {
        vm.charge_bytes(b.len())?;
        self.reserve(vm, b.len())?;
        self.push(vm, Value::Bytes(b.to_vec()))
    }

    fn native(&mut self, vm: &mut Vm, type_name: &str, text: &str) -> Result<(), Fail> {
        let text = self.text(vm, text)?;
        let type_name: Arc<str> = self.text(vm, type_name)?.into();
        self.push(vm, Value::Native(Box::new(NativeValue { type_name, text })))
    }

    fn range(&mut self, vm: &mut Vm, start: i64, end: i64, inclusive: bool) -> Result<(), Fail> {
        self.push(vm, Value::Range { start, end, inclusive })
    }

    fn begin_seq(&mut self, vm: &mut Vm, kind: SeqKind, len: usize) -> Result<(), Fail> {
        let len = len.min(1 << 16);
        self.reserve(vm, len * size_of::<Value>())?;
        let partial = match kind {
            SeqKind::List => Partial::List(Vec::with_capacity(len)),
            SeqKind::Set => Partial::Set(Vec::with_capacity(len)),
            SeqKind::Tuple(_) => Partial::Tuple(Vec::with_capacity(len)),
        };
        self.open(partial)
    }

    fn begin_map(&mut self, vm: &mut Vm, len: usize) -> Result<(), Fail> {
        let len = len.min(1 << 16);
        self.reserve(vm, len * size_of::<(Value, Value)>())?;
        self.open(Partial::Map(Vec::with_capacity(len), false))
    }

    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        let Some(t) = vm.prog.types.get(def as usize) else { return internal("encode: unknown struct") };
        let desc = t.desc().clone();
        let n = desc.fields.len();
        self.reserve(vm, n * size_of::<Value>())?;
        self.open(Partial::Struct(desc, Vec::with_capacity(n)))
    }

    fn field(&mut self, _: &mut Vm, _: usize) -> Result<(), Fail> {
        Ok(())
    }

    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let desc = Self::variant_desc(vm, variant)?;
        self.push(vm, Value::Variant(VariantValue { variant: desc, payload: Box::default() }))
    }

    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let desc = Self::variant_desc(vm, variant)?;
        self.open(Partial::Variant(desc, Vec::new()))
    }

    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        let desc = Self::variant_desc(vm, vm.prog.ids.some)?;
        self.open(Partial::Variant(desc, Vec::with_capacity(1)))
    }

    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit_variant(vm, vm.prog.ids.none)
    }

    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.stack.is_empty() {
            return internal("encode: no open container");
        }
        if let Some(at) = self.stack.len().checked_sub(2) {
            self.prepare(vm, at)?;
        }
        let Some(top) = self.stack.pop() else { return internal("encode: no open value") };
        let v = match top {
            Partial::List(items) => Value::List(items),
            Partial::Tuple(items) => Value::Tuple(items),
            Partial::Set(items) => Value::Set(items),
            Partial::Map(entries, _) => Value::Map(entries),
            Partial::Struct(ty, fields) => Value::Struct(StructValue { ty, fields: fields.into_boxed_slice() }),
            Partial::Variant(variant, payload) => Value::Variant(VariantValue { variant, payload: payload.into_boxed_slice() }),
        };
        self.place(v)
    }

    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        self.reserve(vm, owned_size(value))?;
        self.push(vm, value.clone())
    }

    fn write_fast(&mut self, vm: &mut Vm, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
        self.save();
        let r = self.build(vm, value, schema).and_then(|v| self.push(vm, v));
        if r.is_err() {
            self.restore();
        }
        r
    }

    fn save(&mut self) {
        self.saved = Some(EncoderMark { open: self.stack.len(), allocated: self.allocated });
    }

    // A failed write changes only the containers that it opened: it places
    // its value in the container below only when the value ends.
    fn restore(&mut self) {
        let Some(mark) = self.saved.take() else { return };
        self.stack.truncate(mark.open);
        self.allocated = mark.allocated;
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }
}

/// Reads a host [`Value`] with a [`ValueReader`], which keeps its position
/// in the value.
pub(crate) struct ValueDecoder {
    reader: ValueReader<'static>,
}

impl ValueDecoder {
    pub(crate) fn new(value: Value) -> ValueDecoder {
        ValueDecoder { reader: ValueReader::owned(value) }
    }
}

impl DecodeFormat for ValueDecoder {
    fn unit(&mut self, _: &mut Vm) -> DecodeResult<()> {
        Ok(self.reader.read(|n, _| n.unit())?)
    }

    fn bool(&mut self, _: &mut Vm) -> DecodeResult<bool> {
        Ok(self.reader.read(|n, in_key| n.bool(in_key))?)
    }

    fn int(&mut self, _: &mut Vm) -> DecodeResult<i64> {
        Ok(self.reader.read(|n, in_key| n.int(in_key))?)
    }

    fn float(&mut self, _: &mut Vm) -> DecodeResult<f64> {
        Ok(self.reader.read(|n, _| n.float())?)
    }

    fn duration(&mut self, _: &mut Vm) -> DecodeResult<i64> {
        Ok(self.reader.read(|n, in_key| n.duration(in_key))?)
    }

    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let s = self.reader.peek(|n, _| n.str())?;
        if s.len() > 1024 {
            vm.check_memory(s.len())?;
        }
        vm.charge_bytes(s.len())?;
        let v = vm.new_str(s);
        self.reader.skip();
        Ok(v)
    }

    fn bytes(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let b = self.reader.peek(|n, _| n.bytes())?;
        vm.check_memory(b.len())?;
        vm.charge_bytes(b.len())?;
        let v = vm.heap.new_bytes(b.into_owned());
        self.reader.skip();
        Ok(v)
    }

    fn native_text(&mut self, _: &mut Vm, name: &str) -> DecodeResult<String> {
        Ok(self.reader.read(|n, _| n.native_text(name).map(str::to_owned))?)
    }

    fn range(&mut self, _: &mut Vm) -> DecodeResult<(i64, i64, bool)> {
        Ok(self.reader.range()?)
    }

    fn some(&mut self, _: &mut Vm) -> DecodeResult<bool> {
        Ok(self.reader.read_some()?)
    }

    fn begin_seq(&mut self, _: &mut Vm, kind: SeqKind) -> DecodeResult<()> {
        Ok(self.reader.open_seq(kind)?)
    }

    fn next_item(&mut self, _: &mut Vm) -> DecodeResult<bool> {
        Ok(self.reader.advance_item()?)
    }

    fn begin_map(&mut self, _: &mut Vm) -> DecodeResult<()> {
        Ok(self.reader.open_map()?)
    }

    fn next_entry(&mut self, _: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        Ok(self.reader.next_entry()?)
    }

    fn map_value(&mut self, _: &mut Vm) -> DecodeResult<()> {
        Ok(self.reader.enter_map_value()?)
    }

    fn begin_fields(&mut self, _: &mut Vm, name: &str) -> DecodeResult<()> {
        Ok(self.reader.open_fields(name)?)
    }

    fn next_field(&mut self, _: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        Ok(self.reader.next_field_name()?.map(Cow::Borrowed))
    }

    fn skip(&mut self, _: &mut Vm) -> DecodeResult<()> {
        self.reader.skip();
        Ok(())
    }

    fn variant(&mut self, _: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)> {
        let (tag, payload) = self.reader.read_variant(name)?;
        Ok((Cow::Borrowed(tag), payload))
    }

    fn end_variant(&mut self, _: &mut Vm, _: &str) -> DecodeResult<()> {
        self.reader.end_variant();
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let json = ValueReader::new(self.reader.node()?).decode::<Json>().map_err(|e| DecodeFail::Mismatch(e.message().to_string()))?;
        let v = crate::stdlib::json::json_into_vm(vm, json)?;
        self.reader.skip();
        Ok(v)
    }

    fn finish(&mut self, _: &mut Vm) -> DecodeResult<()> {
        Ok(())
    }

    fn value(&mut self, _: &mut Vm) -> DecodeResult<Value> {
        let node = self.reader.node()?.clone();
        self.reader.skip();
        Ok(node)
    }

    fn length_mismatch(&self, len: usize) -> String {
        value_input::length_mismatch(len)
    }

    fn mark(&mut self) -> usize {
        self.reader.mark()
    }

    fn rewind(&mut self, mark: usize) {
        self.reader.rewind(mark);
    }

    fn release(&mut self, mark: usize) {
        self.reader.release(mark);
    }

    fn rewind_to_start(&mut self) {
        self.reader.rewind_to_start();
    }

    fn heap_size(&self) -> usize {
        self.reader.heap_size()
    }
}

/// Writes `value` to `f` in its JSON form, with the operations of the
/// format: a struct as a map, a variant as its name or a map from its name to
/// its payload, and `Some(x)` as `x`.
pub(crate) fn write_json_form(f: &mut dyn EncodeFormat, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
    fn pairs<'a>(f: &mut dyn EncodeFormat, vm: &mut Vm, fields: impl ExactSizeIterator<Item = (&'a str, &'a Value)>) -> Result<(), Fail> {
        f.begin_map(vm, fields.len())?;
        for (k, v) in fields {
            f.str(vm, k)?;
            write_json_form(f, vm, v)?;
        }
        f.end(vm)
    }
    match value {
        Value::Unit => f.unit(vm),
        Value::Bool(b) => f.bool(vm, *b),
        Value::Int(i) => f.int(vm, *i),
        Value::Float(x) => f.float(vm, *x),
        Value::Duration(d) => f.int(vm, d.as_nanos() as i64),
        Value::Str(s) => f.str(vm, s),
        Value::Bytes(b) => f.bytes(vm, b),
        Value::List(items) | Value::Tuple(items) | Value::Set(items) => {
            f.begin_seq(vm, SeqKind::List, items.len())?;
            for x in items {
                write_json_form(f, vm, x)?;
            }
            f.end(vm)
        }
        Value::Map(entries) => {
            f.begin_map(vm, entries.len())?;
            for (k, v) in entries {
                write_json_form(f, vm, k)?;
                write_json_form(f, vm, v)?;
            }
            f.end(vm)
        }
        Value::Range { start, end, inclusive } => f.range(vm, *start, *end, *inclusive),
        Value::Struct(s) => pairs(f, vm, s.named_fields().collect::<Vec<_>>().into_iter()),
        Value::Variant(v) if is_json_variant(v) || is_option_variant(v) => match v.payload.first() {
            Some(p) => write_json_form(f, vm, p),
            None => f.unit(vm),
        },
        Value::Variant(v) if v.payload.is_empty() => f.str(vm, &v.variant.name),
        Value::Variant(v) => {
            f.begin_map(vm, 1)?;
            f.str(vm, &v.variant.name)?;
            match &v.variant.fields {
                Some(names) => pairs(f, vm, names.iter().map(String::as_str).zip(v.payload.iter()).collect::<Vec<_>>().into_iter())?,
                None => {
                    f.begin_seq(vm, SeqKind::List, v.payload.len())?;
                    for x in &v.payload {
                        write_json_form(f, vm, x)?;
                    }
                    f.end(vm)?;
                }
            }
            f.end(vm)
        }
        Value::Native(n) => f.str(vm, &n.text),
    }
}
