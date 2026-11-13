//! The host [`Value`] format of `Encoder` and `Decoder`.
//!
//! The encoder builds the `Value` that a host receives through `Encoded<N>`
//! and run results. The decoder reads a host `Value`, which can also have
//! the JSON form of the target type, such as a Base64 `Value::Str` for
//! `bytes`.

use super::{Budget, DecodeFail, DecodeFormat, DecodeResult, EncodeFormat, SeqKind};
use crate::codec::MAX_NESTING_DEPTH;
use crate::stdlib::base64;
use crate::stdlib::json::Json;
use crate::value::{
    Decode, Encode, NativeValue, StructValue, Value, VariantDesc, VariantValue, integral_float_to_int, is_option_variant, key_text,
    kind_name, without_json_variant,
};
use crate::vm::value::VmValue;
use crate::vm::{Fail, Vm, fail, internal};
use std::any::Any;
use std::borrow::Cow;
use std::sync::Arc;

/// A container that the encoder is building.
enum Partial {
    /// The finished value, which [`Encoded`](crate::native::ty::Encoded)
    /// parameters share.
    Root(Option<Arc<Value>>),
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
    stack: Vec<Partial>,
    /// Bytes of Rust memory that the value holds.
    allocated: usize,
    budget: Budget,
    /// The number of open containers that count as nesting levels.
    depth: usize,
    /// For each open container, whether it counts as a nesting level.
    counted: Vec<bool>,
    /// The lengths of `stack` and `counted`, the depth, and the allocated
    /// bytes that `save` kept.
    saved: Option<(usize, usize, usize, usize)>,
}

impl ValueEncoder {
    pub(crate) fn new() -> ValueEncoder {
        ValueEncoder {
            stack: vec![Partial::Root(None)],
            allocated: 0,
            budget: Budget::default(),
            depth: 0,
            counted: Vec::new(),
            saved: None,
        }
    }

    /// Returns the finished value, or `None` if a container is open or no
    /// value was written.
    pub(crate) fn encoded(&self) -> Option<&Arc<Value>> {
        match self.stack.as_slice() {
            [Partial::Root(v)] => v.as_ref(),
            _ => None,
        }
    }

    /// Counts `bytes` more of Rust memory against the memory limit.
    fn reserve(&mut self, vm: &mut Vm, bytes: usize) -> Result<(), Fail> {
        self.budget.grow(vm, self.allocated.saturating_add(bytes))?;
        self.allocated += bytes;
        Ok(())
    }

    fn push(&mut self, vm: &mut Vm, v: Value) -> Result<(), Fail> {
        self.prepare(vm, self.stack.len() - 1)?;
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
            Some(Partial::Root(root)) => *root = Some(Arc::new(v)),
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
            None => return internal("encode: no open value"),
        }
        Ok(())
    }

    fn open(&mut self, partial: Partial, counts: bool) -> Result<(), Fail> {
        if counts && self.depth >= MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to copy");
        }
        self.depth += usize::from(counts);
        self.counted.push(counts);
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
        self.open(partial, true)
    }

    fn begin_map(&mut self, vm: &mut Vm, len: usize) -> Result<(), Fail> {
        let len = len.min(1 << 16);
        self.reserve(vm, len * size_of::<(Value, Value)>())?;
        self.open(Partial::Map(Vec::with_capacity(len), false), true)
    }

    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        let Some(t) = vm.prog.types.get(def as usize) else { return internal("encode: unknown struct") };
        let desc = t.desc().clone();
        let n = desc.fields.len();
        self.reserve(vm, n * size_of::<Value>())?;
        self.open(Partial::Struct(desc, Vec::with_capacity(n)), true)
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
        self.open(Partial::Variant(desc, Vec::new()), true)
    }

    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        let desc = Self::variant_desc(vm, vm.prog.ids.some)?;
        self.open(Partial::Variant(desc, Vec::with_capacity(1)), true)
    }

    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit_variant(vm, vm.prog.ids.none)
    }

    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.stack.len() < 2 {
            return internal("encode: no open container");
        }
        self.prepare(vm, self.stack.len() - 2)?;
        let Some(top) = self.stack.pop() else { return internal("encode: no open value") };
        self.depth -= usize::from(self.counted.pop().unwrap_or(false));
        let v = match top {
            Partial::Root(_) => return internal("encode: no open container"),
            Partial::List(items) => Value::List(items),
            Partial::Tuple(items) => Value::Tuple(items),
            Partial::Set(items) => Value::Set(items),
            Partial::Map(entries, _) => Value::Map(entries),
            Partial::Struct(ty, fields) => Value::Struct(StructValue { ty, fields: fields.into_boxed_slice() }),
            Partial::Variant(variant, payload) => Value::Variant(VariantValue { variant, payload: payload.into_boxed_slice() }),
        };
        self.place(v)
    }

    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail> {
        let json = crate::stdlib::json::json_from_vm(vm, json)?;
        self.push(vm, json.encode())
    }

    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        self.push(vm, value.clone())
    }

    fn save(&mut self) {
        self.saved = Some((self.stack.len(), self.counted.len(), self.depth, self.allocated));
    }

    // A failed write changes only the containers that it opened: it places
    // its value in the container below only when the value ends.
    fn restore(&mut self) {
        let Some((stack, counted, depth, allocated)) = self.saved.take() else { return };
        self.stack.truncate(stack);
        self.counted.truncate(counted);
        (self.depth, self.allocated) = (depth, allocated);
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The value that the next operation reads.
#[derive(Clone)]
enum Next {
    Value(Value),
    /// The named payload of a variant, from its field names.
    Fields(Vec<(String, Value)>),
}

/// An open container of the decoder.
enum Open {
    Seq(std::vec::IntoIter<Value>),
    Map(std::vec::IntoIter<(Value, Value)>, Option<Value>),
    Fields(std::vec::IntoIter<(String, Value)>),
}

/// Reads a [`Value`].
pub(crate) struct ValueDecoder {
    next: Option<Next>,
    open: Vec<Open>,
    /// Whether the next operations read a map key.
    in_key: bool,
    /// The next value, the number of open containers, and `in_key`, that
    /// `save` kept.
    saved: Option<(Option<Next>, usize, bool)>,
}

impl ValueDecoder {
    pub(crate) fn new(value: Value) -> ValueDecoder {
        ValueDecoder { next: Some(Next::Value(value)), open: Vec::new(), in_key: false, saved: None }
    }

    fn take(&mut self) -> DecodeResult<Value> {
        match self.next.take() {
            Some(Next::Value(v)) => Ok(match v {
                Value::Variant(ref variant) if is_json_variant(variant) => without_json_variant(&v).clone(),
                v => v,
            }),
            Some(Next::Fields(fields)) => Ok(Value::Map(fields.into_iter().map(|(k, v)| (Value::Str(k), v)).collect())),
            None => Err(Fail::internal("decode: no value to read").into()),
        }
    }

    fn mismatch(&self, expected: &str, found: &Value) -> DecodeFail {
        if self.in_key {
            return self.invalid_key(found);
        }
        DecodeFail::Mismatch(format!("expected {expected}, found {}", kind_name(found)))
    }

    fn invalid_key(&self, key: &Value) -> DecodeFail {
        DecodeFail::Mismatch(format!("invalid map key `{}`", crate::stdlib::truncate(&key_text(key), super::QUOTED_KEY)))
    }

    fn int_of(&self, v: &Value) -> DecodeResult<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => match integral_float_to_int(*f) {
                Some(i) => Ok(i),
                None if f.is_finite() && f.fract() == 0.0 && !self.in_key => Err(DecodeFail::Mismatch("integer out of range".into())),
                None => Err(self.mismatch("int", v)),
            },
            Value::Str(s) if self.in_key => s.parse().map_err(|_| self.invalid_key(v)),
            v => Err(self.mismatch("int", v)),
        }
    }

    /// Checks the memory limit for the copy of a long text or byte string in
    /// `next`, before the value leaves `next`.
    fn check_text(&self, vm: &Vm) -> DecodeResult<()> {
        let len = match &self.next {
            Some(Next::Value(Value::Str(s))) => s.len(),
            Some(Next::Value(Value::Bytes(b))) => b.len(),
            _ => 0,
        };
        if len > 1024 {
            vm.check_memory(len)?;
        }
        Ok(())
    }

    fn enter(&mut self, open: Open) -> DecodeResult<()> {
        if self.open.len() >= MAX_NESTING_DEPTH {
            return Err(DecodeFail::Mismatch("value too deeply nested to decode".into()));
        }
        self.open.push(open);
        Ok(())
    }
}

fn is_json_variant(v: &VariantValue) -> bool {
    crate::value::is_qualified_name(&v.variant.type_name, Some("json"), "Json")
}

impl DecodeFormat for ValueDecoder {
    fn unit(&mut self, _: &mut Vm) -> DecodeResult<()> {
        match self.take()? {
            Value::Unit => Ok(()),
            v => Err(self.mismatch("()", &v)),
        }
    }

    fn bool(&mut self, _: &mut Vm) -> DecodeResult<bool> {
        match self.take()? {
            Value::Bool(b) => Ok(b),
            Value::Str(s) if self.in_key && (s == "true" || s == "false") => Ok(s == "true"),
            v => Err(self.mismatch("bool", &v)),
        }
    }

    fn int(&mut self, _: &mut Vm) -> DecodeResult<i64> {
        let v = self.take()?;
        self.int_of(&v)
    }

    fn float(&mut self, _: &mut Vm) -> DecodeResult<f64> {
        match self.take()? {
            Value::Float(f) => Ok(f),
            Value::Int(i) => Ok(i as f64),
            v => Err(self.mismatch("float", &v)),
        }
    }

    fn duration(&mut self, _: &mut Vm) -> DecodeResult<i64> {
        match self.take()? {
            Value::Duration(d) => i64::try_from(d.as_nanos()).map_err(|_| DecodeFail::Mismatch("integer out of range".into())),
            v => self.int_of(&v).map_err(|e| match e {
                DecodeFail::Mismatch(_) if !matches!(v, Value::Float(_)) => self.mismatch("duration", &v),
                e => e,
            }),
        }
    }

    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        self.check_text(vm)?;
        match self.take()? {
            Value::Str(s) => {
                vm.charge_bytes(s.len())?;
                Ok(vm.new_str(s))
            }
            v => Err(self.mismatch("str", &v)),
        }
    }

    fn bytes(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        self.check_text(vm)?;
        let b = match self.take()? {
            Value::Bytes(b) => b,
            Value::Str(s) => base64::decode(&s, false).map_err(|_| DecodeFail::Mismatch("invalid base64".into()))?,
            v => return Err(self.mismatch("bytes", &v)),
        };
        vm.charge_bytes(b.len())?;
        Ok(vm.heap.new_bytes(b))
    }

    fn native_text(&mut self, _: &mut Vm, name: &str) -> DecodeResult<String> {
        match self.take()? {
            Value::Native(native) => Ok(native.text),
            Value::Str(s) => Ok(s),
            v => Err(self.mismatch(name, &v)),
        }
    }

    fn range(&mut self, _: &mut Vm) -> DecodeResult<(i64, i64, bool)> {
        match self.take()? {
            Value::Range { start, end, inclusive } => Ok((start, end, inclusive)),
            Value::Map(entries) => {
                let (mut start, mut end, mut inclusive) = (None, None, false);
                for (key, value) in &entries {
                    let value = without_json_variant(value);
                    match without_json_variant(key) {
                        Value::Str(k) if k == "start" => start = Some(self.int_of(value)?),
                        Value::Str(k) if k == "end" => end = Some(self.int_of(value)?),
                        Value::Str(k) if k == "inclusive" => match value {
                            Value::Bool(b) => inclusive = *b,
                            v => return Err(self.mismatch("bool", v)),
                        },
                        Value::Str(_) => {}
                        key => return Err(self.mismatch("str key", key)),
                    }
                }
                match (start, end) {
                    (Some(s), Some(e)) => Ok((s, e, inclusive)),
                    (None, _) => Err(DecodeFail::Mismatch("missing field `start`".into())),
                    _ => Err(DecodeFail::Mismatch("missing field `end`".into())),
                }
            }
            v => Err(self.mismatch("Range", &v)),
        }
    }

    fn some(&mut self, _: &mut Vm) -> DecodeResult<bool> {
        let v = match self.next.take() {
            Some(Next::Value(v)) => v,
            other => {
                self.next = other;
                return Ok(true);
            }
        };
        match without_json_variant(&v) {
            Value::Unit => Ok(false),
            Value::Variant(option) if is_option_variant(option) && option.payload.len() <= 1 => match option.payload.first() {
                None => Ok(false),
                Some(payload) => {
                    self.next = Some(Next::Value(payload.clone()));
                    Ok(true)
                }
            },
            _ => {
                self.next = Some(Next::Value(v));
                Ok(true)
            }
        }
    }

    fn begin_seq(&mut self, _: &mut Vm, kind: SeqKind) -> DecodeResult<()> {
        let v = self.take()?;
        let items = match (kind, v) {
            (SeqKind::List, Value::List(items)) => items,
            (SeqKind::Set, Value::Set(items) | Value::List(items)) => items,
            (SeqKind::Tuple(_), Value::Tuple(items) | Value::List(items)) => items,
            (SeqKind::List, v) => return Err(self.mismatch("list", &v)),
            (SeqKind::Set, v) => return Err(self.mismatch("set", &v)),
            (SeqKind::Tuple(n), v) => return Err(self.mismatch(&format!("tuple of {n} elements"), &v)),
        };
        if let SeqKind::Tuple(n) = kind
            && items.len() != n
        {
            return Err(DecodeFail::Mismatch(self.length_mismatch(n)));
        }
        self.enter(Open::Seq(items.into_iter()))
    }

    fn next_item(&mut self, _: &mut Vm) -> DecodeResult<bool> {
        let Some(Open::Seq(items)) = self.open.last_mut() else { return Err(Fail::internal("next_item: no open sequence").into()) };
        match items.next() {
            Some(v) => {
                self.next = Some(Next::Value(v));
                Ok(true)
            }
            None => {
                self.open.pop();
                Ok(false)
            }
        }
    }

    fn begin_map(&mut self, _: &mut Vm) -> DecodeResult<()> {
        match self.take()? {
            Value::Map(entries) => self.enter(Open::Map(entries.into_iter(), None)),
            v => Err(self.mismatch("map", &v)),
        }
    }

    fn next_entry(&mut self, _: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        let Some(Open::Map(entries, value)) = self.open.last_mut() else { return Err(Fail::internal("next_entry: no open map").into()) };
        match entries.next() {
            Some((k, v)) => {
                let text = key_text(&k).into_owned();
                *value = Some(v);
                self.next = Some(Next::Value(k));
                self.in_key = true;
                Ok(Some(Cow::Owned(text)))
            }
            None => {
                self.open.pop();
                Ok(None)
            }
        }
    }

    fn map_value(&mut self, _: &mut Vm) -> DecodeResult<()> {
        let Some(Open::Map(_, value)) = self.open.last_mut() else { return Err(Fail::internal("map_value: no open map").into()) };
        self.next = value.take().map(Next::Value);
        self.in_key = false;
        Ok(())
    }

    fn begin_fields(&mut self, _: &mut Vm, name: &str) -> DecodeResult<()> {
        let v = match self.next.take() {
            Some(Next::Fields(fields)) => return self.enter(Open::Fields(fields.into_iter())),
            Some(Next::Value(v @ (Value::Struct(_) | Value::Map(_)))) => v,
            Some(Next::Value(v)) => without_json_variant(&v).clone(),
            None => return Err(Fail::internal("begin_fields: no value").into()),
        };
        let fields: Vec<(String, Value)> = match v {
            Value::Struct(s) => s.ty.fields.iter().cloned().zip(s.fields.into_vec()).collect(),
            Value::Map(entries) => {
                let mut fields = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    match without_json_variant(&k) {
                        Value::Str(k) => fields.push((k.clone(), v)),
                        k => return Err(self.mismatch("str key", k)),
                    }
                }
                fields
            }
            v => return Err(self.mismatch(if name.is_empty() { "map" } else { name }, &v)),
        };
        self.enter(Open::Fields(fields.into_iter()))
    }

    fn next_field(&mut self, _: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        let Some(Open::Fields(fields)) = self.open.last_mut() else { return Err(Fail::internal("next_field: no open struct").into()) };
        match fields.next() {
            Some((k, v)) => {
                self.next = Some(Next::Value(v));
                Ok(Some(Cow::Owned(k)))
            }
            None => {
                self.open.pop();
                Ok(None)
            }
        }
    }

    fn skip(&mut self, _: &mut Vm) -> DecodeResult<()> {
        self.next = None;
        Ok(())
    }

    fn variant(&mut self, _: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)> {
        let v = self.take()?;
        match v {
            Value::Str(tag) => Ok((Cow::Owned(tag), false)),
            Value::Variant(variant) if variant.payload.is_empty() => Ok((Cow::Owned(variant.variant.name.clone()), false)),
            Value::Variant(variant) => {
                let tag = variant.variant.name.clone();
                self.next = Some(match &variant.variant.fields {
                    Some(names) => Next::Fields(names.iter().cloned().zip(variant.payload.into_vec()).collect()),
                    None => Next::Value(Value::Tuple(variant.payload.into_vec())),
                });
                Ok((Cow::Owned(tag), true))
            }
            Value::Map(mut entries) => {
                if entries.len() != 1 {
                    let what = if entries.is_empty() { "expected a variant" } else { "expected a single variant" };
                    return Err(DecodeFail::Mismatch(format!("{what} of {name}")));
                }
                let (key, payload) = entries.pop().unwrap_or((Value::Unit, Value::Unit));
                match without_json_variant(&key) {
                    Value::Str(tag) => {
                        let tag = tag.clone();
                        self.next = Some(Next::Value(payload));
                        Ok((Cow::Owned(tag), true))
                    }
                    key => Err(self.mismatch("str key", key)),
                }
            }
            v => Err(self.mismatch(name, &v)),
        }
    }

    fn end_variant(&mut self, _: &mut Vm, _: &str) -> DecodeResult<()> {
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        // Convert before the value leaves `next`, so that a run of the native
        // function again after a memory check reads the same value.
        let json = match &self.next {
            Some(Next::Value(v)) => Json::decode(v).map_err(|e| DecodeFail::Mismatch(e.message().to_string()))?,
            _ => {
                let v = self.take()?;
                return Err(self.mismatch("Json", &v));
            }
        };
        let v = crate::stdlib::json::json_into_vm(vm, json)?;
        self.next = None;
        Ok(v)
    }

    fn finish(&mut self, _: &mut Vm) -> DecodeResult<()> {
        Ok(())
    }

    fn value(&mut self, _: &mut Vm) -> DecodeResult<Value> {
        self.take()
    }

    fn length_mismatch(&self, len: usize) -> String {
        format!("expected list of length {len}")
    }

    fn save(&mut self) {
        self.saved = Some((self.next.clone(), self.open.len(), self.in_key));
    }

    // A read takes its value from `next`, and changes only the containers
    // that it opens.
    fn restore(&mut self) {
        let Some((next, open, in_key)) = self.saved.take() else { return };
        self.open.truncate(open);
        (self.next, self.in_key) = (next, in_key);
    }

    fn heap_size(&self) -> usize {
        0
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
