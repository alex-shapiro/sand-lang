//! The Arm 3 codec: one walker for structural values and custom `Encode`
//! and `Decode` callbacks, and the `Encoder` and `Decoder` objects that hold
//! the codec state while Sand code runs.
//!
//! The walkers borrow immutable values and decode input in place, and they
//! build decoded containers in Rust locals. They lock a mutable container
//! only to read one item, so that callbacks can use the container.

use super::heap::Heap;
use super::value::*;
use super::vm::{Args, NativeModule, Vm, type_error};
use crate::shared::format::{Event, FormatReader, FormatWriter, absent_fields, all_fields, charge_input, find_field};
use crate::shared::isa::{DECODE_METHOD, ENCODE_METHOD, Program, Ty};
use crate::shared::json::{Json, JsonWriter};
use crate::shared::{BYTES_PER_FUEL, Fail, Result, text_fuel};
use std::any::Any;
use std::mem::size_of;
use std::sync::Arc;

/// Runs a nested encode for an `Encoder` of one format.
type EncodeWalk = fn(&mut Vm, &Value, &Value) -> Result<()>;
/// Runs a nested decode of `program.types[ty]` for a `Decoder` of one format.
type DecodeWalk = fn(&mut Vm, &Value, u16) -> Result<Value>;

/// Format state of an encode: the writer and its output.
pub struct EncodeState<W> {
    writer: W,
    out: String,
}

/// The object-safe view of an [`EncodeState`] for Sand `Encoder` methods.
trait DynEncodeState: Any + Send {
    fn write(&mut self, event: Event<'_>);
    fn max_len(&self, event: &Event<'_>) -> usize;
    fn out(&mut self) -> &mut String;
}

impl<W: FormatWriter> DynEncodeState for EncodeState<W> {
    fn write(&mut self, event: Event<'_>) {
        self.writer.write(&mut self.out, event)
    }
    fn max_len(&self, event: &Event<'_>) -> usize {
        self.writer.max_len(event)
    }
    fn out(&mut self) -> &mut String {
        &mut self.out
    }
}

/// The Sand `Encoder`: holds the format state while a callback runs.
pub struct EncoderObj {
    state: Option<Box<dyn DynEncodeState>>,
    walk: EncodeWalk,
}

/// The Sand `Decoder`: holds the input and position while a callback runs.
pub struct DecoderObj {
    input: Value,
    pos: usize,
    /// The input position up to which the decode has charged fuel.
    charged: usize,
    scratch: String,
    reader: &'static dyn FormatReader,
    walk: DecodeWalk,
}

impl Contents for EncoderObj {
    fn each_mut(&mut self, _f: &mut dyn FnMut(&mut Value)) {}
}

impl Contents for DecoderObj {
    fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        f(&mut self.input);
    }
}

/// Charges output growth that a format made beyond its `max_len`.
#[inline(always)]
fn charge_overrun(heap: &mut Heap, capacity: usize, out: &String) -> Result<()> {
    if out.capacity() == capacity { Ok(()) } else { heap.reserve(out.capacity() - capacity) }
}

/// Reserves output capacity for `need` more bytes.
#[inline]
fn grow_output(heap: &mut Heap, out: &mut String, need: usize) -> Result<()> {
    if out.capacity() - out.len() >= need {
        return Ok(());
    }
    let capacity = (out.capacity() * 2).max(out.len() + need).max(256);
    heap.reserve(capacity - out.capacity())?;
    out.reserve_exact(capacity - out.len());
    Ok(())
}

struct EncodeCx<W> {
    state: Option<Box<EncodeState<W>>>,
    /// The `Encoder` object, once a callback needs one.
    encoder: Option<Value>,
}

#[inline(always)]
fn write<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, event: Event<'_>) -> Result<()> {
    if event.text_len() >= BYTES_PER_FUEL {
        vm.charge(text_fuel(event.text_len()))?;
    }
    let state = cx.state.as_deref_mut().expect("encode state");
    let need = state.writer.max_len(&event);
    grow_output(&mut vm.heap, &mut state.out, need)?;
    let capacity = state.out.capacity();
    state.writer.write(&mut state.out, event);
    charge_overrun(&mut vm.heap, capacity, &state.out)
}

/// Encodes `v` with `writer` and returns the output as a Sand string.
pub fn encode_with<W: FormatWriter>(vm: &mut Vm, writer: W, v: &Value) -> Result<Value> {
    vm.reserve(size_of::<EncodeState<W>>())?;
    let mut cx = EncodeCx { state: Some(Box::new(EncodeState { writer, out: String::new() })), encoder: None };
    let program = vm.program.clone();
    let result = encode_value(vm, &mut cx, &program, v);
    vm.release(size_of::<EncodeState<W>>());
    let out = cx.state.take().map(|s| s.out).unwrap_or_default();
    match result {
        Ok(()) => vm.heap.new_str_from_reserved(out),
        Err(e) => {
            vm.release(out.capacity());
            Err(e)
        }
    }
}

fn encode_value<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: &Value) -> Result<()> {
    vm.charge(1)?;
    match v {
        Value::Unit | Value::None => write(vm, cx, Event::None),
        Value::Bool(b) => write(vm, cx, Event::Bool(*b)),
        Value::Int(i) => write(vm, cx, Event::Int(*i)),
        Value::Float(f) => write(vm, cx, Event::Float(*f)),
        Value::Short(s) => write(vm, cx, Event::Str(s.as_str())),
        Value::Str(s) => write(vm, cx, Event::Str(&s.text)),
        Value::Tag(tag) => match p.enum_method(p.tags[*tag as usize].enum_id, ENCODE_METHOD) {
            Some(func) => encode_callback(vm, cx, func, v),
            None => write(vm, cx, Event::Tag(&p.variant(*tag).name)),
        },
        Value::Struct(o) => match p.struct_method(o.data.ty, ENCODE_METHOD) {
            Some(func) => encode_callback(vm, cx, func, v),
            None => encode_struct(vm, cx, p, &o.data),
        },
        Value::MutStruct(o) => {
            let ty = o.lock()?.ty;
            match p.struct_method(ty, ENCODE_METHOD) {
                Some(func) => encode_callback(vm, cx, func, v),
                None => encode_mut_struct(vm, cx, p, o, ty),
            }
        }
        Value::Variant(o) => match p.enum_method(p.tags[o.data.tag as usize].enum_id, ENCODE_METHOD) {
            Some(func) => encode_callback(vm, cx, func, v),
            None => encode_variant(vm, cx, p, &o.data),
        },
        Value::List(l) => encode_list(vm, cx, p, l),
        Value::Map(m) => encode_map(vm, cx, p, m),
        _ => Err(Fail::error("value cannot be encoded")),
    }
}

#[inline(never)]
fn encode_struct<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, s: &StructObj) -> Result<()> {
    vm.check_stack()?;
    let defs = &p.structs[s.ty as usize].fields;
    write(vm, cx, Event::OpenMap(defs.len()))?;
    for (def, field) in defs.iter().zip(&s.fields) {
        write(vm, cx, Event::Key(&def.name))?;
        encode_value(vm, cx, p, field)?;
    }
    write(vm, cx, Event::CloseMap)
}

#[inline(never)]
fn encode_mut_struct<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, s: &Shared<StructObj>, ty: u16) -> Result<()> {
    vm.check_stack()?;
    let defs = &p.structs[ty as usize].fields;
    write(vm, cx, Event::OpenMap(defs.len()))?;
    for (i, def) in defs.iter().enumerate() {
        write(vm, cx, Event::Key(&def.name))?;
        let field = s.lock()?.fields[i].clone();
        encode_value(vm, cx, p, &field)?;
    }
    write(vm, cx, Event::CloseMap)
}

#[inline(never)]
fn encode_variant<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, o: &VariantObj) -> Result<()> {
    vm.check_stack()?;
    let variant = p.variant(o.tag);
    let n = o.fields.len();
    write(vm, cx, Event::OpenVariant(&variant.name))?;
    let names = variant.field_names.as_deref();
    write(vm, cx, if names.is_some() { Event::OpenMap(n) } else { Event::OpenList(n) })?;
    for (i, field) in o.fields.iter().enumerate() {
        if let Some(names) = names {
            write(vm, cx, Event::Key(&names[i]))?;
        }
        encode_value(vm, cx, p, field)?;
    }
    write(vm, cx, if names.is_some() { Event::CloseMap } else { Event::CloseList })?;
    write(vm, cx, Event::CloseVariant)
}

#[inline(never)]
fn encode_list<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, l: &Shared<ListObj>) -> Result<()> {
    vm.check_stack()?;
    let n = l.lock()?.0.len();
    write(vm, cx, Event::OpenList(n))?;
    let mut i = 0;
    loop {
        let Some(item) = l.lock()?.0.get(i).cloned() else { break };
        encode_value(vm, cx, p, &item)?;
        i += 1;
    }
    write(vm, cx, Event::CloseList)
}

#[inline(never)]
fn encode_map<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, m: &Shared<MapObj>) -> Result<()> {
    vm.check_stack()?;
    let n = m.lock()?.0.len();
    write(vm, cx, Event::OpenMap(n))?;
    let mut i = 0;
    loop {
        let Some((key, value)) = m.lock()?.0.get(i).cloned() else { break };
        write(vm, cx, Event::Key(key.str().ok_or_else(|| type_error("str key"))?))?;
        encode_value(vm, cx, p, &value)?;
        i += 1;
    }
    write(vm, cx, Event::CloseMap)
}

/// Runs a custom `encode` method with the format state in the `Encoder`.
#[cold]
#[inline(never)]
fn encode_callback<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, func: u16, v: &Value) -> Result<()> {
    vm.custom_calls += 1;
    let encoder = match &cx.encoder {
        Some(e) => e.clone(),
        None => {
            let bytes = Heap::shared_size::<EncoderObj>(0);
            vm.heap.reserve_container(bytes)?;
            let e = Value::Encoder(vm.heap.insert_shared(EncoderObj { state: None, walk: encode_entry::<W> }, bytes));
            cx.encoder = Some(e.clone());
            e
        }
    };
    let state: Box<dyn DynEncodeState> = cx.state.take().expect("encode state");
    encoder_obj(&encoder)?.lock()?.state = Some(state);
    let result = vm.call_function(func, Value::Unit, &[v.clone(), encoder.clone()]);
    let state = encoder_obj(&encoder)?.lock()?.state.take().ok_or_else(|| Fail::error("encoder state is missing"))?;
    cx.state = Some(downcast_state(state)?);
    result.map(|_| ())
}

fn downcast_state<W: FormatWriter>(state: Box<dyn DynEncodeState>) -> Result<Box<EncodeState<W>>> {
    (state as Box<dyn Any + Send>).downcast().map_err(|_| Fail::error("encoder format mismatch"))
}

fn encoder_obj(v: &Value) -> Result<&Arc<Shared<EncoderObj>>> {
    match v {
        Value::Encoder(e) => Ok(e),
        _ => Err(type_error("Encoder")),
    }
}

/// Encodes `v` into `encoder` from Sand code.
fn encode_entry<W: FormatWriter>(vm: &mut Vm, encoder: &Value, v: &Value) -> Result<()> {
    let state = encoder_obj(encoder)?.lock()?.state.take().ok_or_else(|| Fail::error("encoder is not active"))?;
    let mut cx = EncodeCx { state: Some(downcast_state::<W>(state)?), encoder: Some(encoder.clone()) };
    let program = vm.program.clone();
    let result = encode_value(vm, &mut cx, &program, v);
    let state: Box<dyn DynEncodeState> = cx.state.take().expect("encode state");
    encoder_obj(encoder)?.lock()?.state = Some(state);
    result
}

/// Runs `f` on the format state of the `Encoder` in argument 0.
fn with_encoder(vm: &mut Vm, args: Args, f: impl FnOnce(&mut Vm, &mut dyn DynEncodeState) -> Result<()>) -> Result<Value> {
    let encoder = vm.arg(args, 0).clone();
    let mut state = encoder_obj(&encoder)?.lock()?.state.take().ok_or_else(|| Fail::error("encoder is not active"))?;
    let result = f(vm, &mut *state);
    encoder_obj(&encoder)?.lock()?.state = Some(state);
    result.map(|_| Value::Unit)
}

fn encoder_event(vm: &mut Vm, args: Args, event: Event<'static>) -> Result<Value> {
    with_encoder(vm, args, |vm, state| {
        let need = state.max_len(&event);
        grow_output(&mut vm.heap, state.out(), need)?;
        let capacity = state.out().capacity();
        state.write(event);
        charge_overrun(&mut vm.heap, capacity, state.out())
    })
}

/// Writes string argument 1 as a `Str`, `Key`, `Tag`, or `OpenVariant` event.
fn encoder_text(vm: &mut Vm, args: Args, event: for<'a> fn(&'a str) -> Event<'a>) -> Result<Value> {
    let text = vm.arg(args, 1).clone();
    with_encoder(vm, args, |vm, state| {
        let text = text.str().ok_or_else(|| type_error("str"))?;
        vm.charge(text_fuel(text.len()))?;
        let need = state.max_len(&event(text));
        grow_output(&mut vm.heap, state.out(), need)?;
        let capacity = state.out().capacity();
        state.write(event(text));
        charge_overrun(&mut vm.heap, capacity, state.out())
    })
}

fn int_arg(vm: &Vm, args: Args, i: usize) -> Result<i64> {
    match vm.arg(args, i) {
        Value::Int(n) => Ok(*n),
        _ => Err(type_error("int")),
    }
}

// -------------------------------------------------------------------------
// Decoding
// -------------------------------------------------------------------------

struct DecodeCx<R: 'static> {
    reader: &'static R,
    /// The input string, which keeps `src` alive.
    input: Value,
    pos: usize,
    /// The input position up to which the decode has charged fuel.
    charged: usize,
    scratch: String,
    /// The `Decoder` object, once a callback needs one.
    decoder: Option<Value>,
}

/// Decodes `text` as `program.types[ty]` with `reader`.
pub fn decode_with<R: FormatReader>(vm: &mut Vm, reader: &'static R, text: &Value, ty: u16) -> Result<Value> {
    let src = text.str().ok_or_else(|| type_error("str"))?;
    let program = vm.program.clone();
    let ty = program.types.get(ty as usize).ok_or_else(|| type_error("type id"))?;
    let mut cx = DecodeCx { reader, input: text.clone(), pos: 0, charged: 0, scratch: String::new(), decoder: None };
    let v = decode_value(vm, &mut cx, src, &program, ty)?;
    cx.reader.end(src, &mut cx.pos)?;
    charge_input(&mut vm.fuel, &mut cx.charged, cx.pos)?;
    Ok(v)
}

fn decode_value<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, src: &str, p: &Program, mut ty: &Ty) -> Result<Value> {
    vm.charge(1)?;
    charge_input(&mut vm.fuel, &mut cx.charged, cx.pos)?;
    loop {
        return match ty {
            Ty::Option(inner) => {
                if cx.reader.null(src, &mut cx.pos)? {
                    Ok(Value::None)
                } else {
                    ty = inner;
                    continue;
                }
            }
            Ty::List(item) => decode_list(vm, cx, src, p, item),
            Ty::Map(value) => decode_map(vm, cx, src, p, value),
            Ty::Struct(id) => match p.struct_method(*id, DECODE_METHOD) {
                Some(func) => decode_callback(vm, cx, func),
                None => {
                    let fields = &p.structs[*id as usize].fields;
                    decode_record(vm, cx, src, p, fields.iter().map(|f| (f.name.as_str(), &f.ty)), Record::Struct(*id))
                }
            },
            Ty::Enum(id) => match p.enum_method(*id, DECODE_METHOD) {
                Some(func) => decode_callback(vm, cx, func),
                None => decode_enum(vm, cx, src, p, *id),
            },
            _ => decode_scalar(vm, cx, src, ty),
        };
    }
}

/// Decodes an `int`, `float`, `bool`, or `str`. A separate function keeps
/// the recursive path of [`decode_value`] small on the stack.
#[inline(never)]
fn decode_scalar<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, src: &str, ty: &Ty) -> Result<Value> {
    Ok(match ty {
        Ty::Int => Value::Int(cx.reader.int(src, &mut cx.pos)?),
        Ty::Float => Value::Float(cx.reader.float(src, &mut cx.pos)?),
        Ty::Bool => Value::Bool(cx.reader.bool(src, &mut cx.pos)?),
        _ => {
            let span = cx.reader.str(src, &mut cx.pos, &mut cx.scratch)?;
            vm.heap.new_str(span.resolve(src, &cx.scratch))?
        }
    })
}

/// Pushes `v` onto `items`, reserving tracked bytes when the buffer grows.
#[inline]
fn push_tracked<T>(heap: &mut Heap, items: &mut Vec<T>, v: T) -> Result<()> {
    if items.len() == items.capacity() {
        let grown = (items.capacity() * 2).max(4);
        heap.reserve((grown - items.capacity()) * size_of::<T>())?;
        items.reserve_exact(grown - items.len());
    }
    items.push(v);
    Ok(())
}

/// Runs `fill` on a tracked buffer and releases the buffer's bytes if it fails.
fn with_buffer<T>(vm: &mut Vm, fill: impl FnOnce(&mut Vm, &mut Vec<T>) -> Result<()>) -> Result<Vec<T>> {
    let mut items = Vec::new();
    match fill(vm, &mut items) {
        Ok(()) => Ok(items),
        Err(e) => {
            vm.release(items.capacity() * size_of::<T>());
            Err(e)
        }
    }
}

#[inline(never)]
fn decode_list<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, src: &str, p: &Program, item: &Ty) -> Result<Value> {
    vm.check_stack()?;
    cx.reader.open_list(src, &mut cx.pos)?;
    let items = with_buffer(vm, |vm, items| {
        while cx.reader.next_item(src, &mut cx.pos)? {
            let v = decode_value(vm, cx, src, p, item)?;
            push_tracked(&mut vm.heap, items, v)?;
        }
        Ok(())
    })?;
    let header = Heap::shared_size::<ListObj>(0);
    let bytes = header + items.capacity() * size_of::<Value>();
    if let Err(e) = vm.heap.reserve_container(header) {
        vm.release(items.capacity() * size_of::<Value>());
        return Err(e);
    }
    Ok(Value::List(vm.heap.insert_shared(ListObj(items), bytes)))
}

#[inline(never)]
fn decode_map<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, src: &str, p: &Program, value: &Ty) -> Result<Value> {
    vm.check_stack()?;
    cx.reader.open_map(src, &mut cx.pos)?;
    let entries = with_buffer(vm, |vm, entries| {
        while let Some(span) = cx.reader.next_key(src, &mut cx.pos, &mut cx.scratch)? {
            let key = vm.heap.new_str(span.resolve(src, &cx.scratch))?;
            let v = decode_value(vm, cx, src, p, value)?;
            push_tracked(&mut vm.heap, entries, (key, v))?;
        }
        Ok(())
    })?;
    let header = Heap::shared_size::<MapObj>(0);
    let bytes = header + entries.capacity() * size_of::<(Value, Value)>();
    if let Err(e) = vm.heap.reserve_container(header) {
        vm.release(entries.capacity() * size_of::<(Value, Value)>());
        return Err(e);
    }
    Ok(Value::Map(vm.heap.insert_shared(MapObj(entries), bytes)))
}

/// The object that [`decode_record`] builds.
#[derive(Clone, Copy)]
enum Record {
    Struct(u16),
    /// A variant with named payload fields.
    Variant(u16),
}

/// Decodes a map of named `fields` into a struct or variant.
#[inline(never)]
fn decode_record<'p, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<R>,
    src: &str,
    p: &'p Program,
    fields: impl Iterator<Item = (&'p str, &'p Ty)> + Clone,
    record: Record,
) -> Result<Value> {
    vm.check_stack()?;
    cx.reader.open_map(src, &mut cx.pos)?;
    let n = fields.clone().count();
    let (mutable, bytes) = match record {
        Record::Struct(ty) if p.structs[ty as usize].mutable => (true, Heap::shared_size::<StructObj>(n * size_of::<Value>())),
        Record::Struct(_) => (false, Heap::frozen_size::<StructObj>(n)),
        Record::Variant(_) => (false, Heap::frozen_size::<VariantObj>(n)),
    };
    vm.reserve(bytes)?;
    let mut values = vec![Value::Unit; n];
    // Release the reservation if the decode fails.
    macro_rules! attempt {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    vm.release(bytes);
                    return Err(e);
                }
            }
        };
    }
    let (mut hint, mut seen) = (0, 0u64);
    while let Some(span) = attempt!(cx.reader.next_key(src, &mut cx.pos, &mut cx.scratch)) {
        let key = span.resolve(src, &cx.scratch);
        match find_field(fields.clone().map(|(name, _)| name), key, hint) {
            Some(i) => {
                let ty = fields.clone().nth(i).expect("field").1;
                values[i] = attempt!(decode_value(vm, cx, src, p, ty));
                hint = i + 1;
                seen |= 1 << i;
            }
            None => {
                attempt!(cx.reader.skip(src, &mut cx.pos));
                attempt!(charge_input(&mut vm.fuel, &mut cx.charged, cx.pos));
            }
        }
    }
    if seen != all_fields(n) {
        let absent = attempt!(absent_fields(fields, seen));
        (0..n).filter(|i| absent & (1 << i) != 0).for_each(|i| values[i] = Value::None);
    }
    attempt!(vm.heap.reserve_container(0));
    let values = values.into_boxed_slice();
    Ok(match record {
        Record::Struct(ty) => vm.heap.insert_struct(ty, values, mutable, bytes),
        Record::Variant(tag) => Value::Variant(vm.heap.insert_frozen(VariantObj { tag, fields: values }, bytes)),
    })
}

#[inline(never)]
fn decode_enum<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, src: &str, p: &Program, id: u16) -> Result<Value> {
    let def = &p.enums[id as usize];
    let (span, has_payload) = cx.reader.open_variant(src, &mut cx.pos, &mut cx.scratch)?;
    let name = span.resolve(src, &cx.scratch);
    let variant =
        def.variants.iter().find(|v| v.name == name).ok_or_else(|| Fail::error(format!("unknown variant `{name}` of {}", def.name)))?;
    if variant.payload.is_empty() == has_payload {
        return Err(Fail::error(format!("variant `{}` payload mismatch", variant.name)));
    }
    if !has_payload {
        return Ok(Value::Tag(variant.tag));
    }
    let v = match &variant.field_names {
        Some(names) => decode_record(vm, cx, src, p, names.iter().map(String::as_str).zip(&variant.payload), Record::Variant(variant.tag))?,
        None => {
            vm.check_stack()?;
            cx.reader.open_list(src, &mut cx.pos)?;
            let bytes = Heap::frozen_size::<VariantObj>(variant.payload.len());
            vm.reserve(bytes)?;
            let mut fields = Vec::with_capacity(variant.payload.len());
            let result = (|| {
                for ty in &variant.payload {
                    if !cx.reader.next_item(src, &mut cx.pos)? {
                        return Err(Fail::error("variant payload is too short"));
                    }
                    fields.push(decode_value(vm, cx, src, p, ty)?);
                }
                if cx.reader.next_item(src, &mut cx.pos)? {
                    return Err(Fail::error("variant payload is too long"));
                }
                vm.heap.reserve_container(0)
            })();
            if let Err(e) = result {
                vm.release(bytes);
                return Err(e);
            }
            Value::Variant(vm.heap.insert_frozen(VariantObj { tag: variant.tag, fields: fields.into_boxed_slice() }, bytes))
        }
    };
    cx.reader.close_variant(src, &mut cx.pos)?;
    Ok(v)
}

/// Runs a custom `decode` method with the input position in the `Decoder`.
#[cold]
#[inline(never)]
fn decode_callback<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, func: u16) -> Result<Value> {
    vm.custom_calls += 1;
    let decoder = match &cx.decoder {
        Some(d) => d.clone(),
        None => {
            let bytes = Heap::shared_size::<DecoderObj>(0);
            vm.heap.reserve_container(bytes)?;
            let obj = DecoderObj {
                input: cx.input.clone(),
                pos: 0,
                charged: 0,
                scratch: String::new(),
                reader: cx.reader,
                walk: decode_entry::<R>,
            };
            let d = Value::Decoder(vm.heap.insert_shared(obj, bytes));
            cx.decoder = Some(d.clone());
            d
        }
    };
    {
        let mut d = decoder_obj(&decoder)?.lock()?;
        (d.pos, d.charged) = (cx.pos, cx.charged);
        d.scratch = std::mem::take(&mut cx.scratch);
    }
    let result = vm.call_function(func, Value::Unit, std::slice::from_ref(&decoder));
    let mut d = decoder_obj(&decoder)?.lock()?;
    (cx.pos, cx.charged) = (d.pos, d.charged);
    cx.scratch = std::mem::take(&mut d.scratch);
    result
}

fn decoder_obj(v: &Value) -> Result<&Arc<Shared<DecoderObj>>> {
    match v {
        Value::Decoder(d) => Ok(d),
        _ => Err(type_error("Decoder")),
    }
}

/// Decodes a nested value from a `Decoder` in Sand code.
fn decode_entry<R: FormatReader>(vm: &mut Vm, decoder: &Value, ty: u16) -> Result<Value> {
    let (input, reader, pos, charged, scratch) = {
        let mut d = decoder_obj(decoder)?.lock()?;
        (d.input.clone(), d.reader, d.pos, d.charged, std::mem::take(&mut d.scratch))
    };
    let reader: &'static R = (reader as &dyn Any).downcast_ref().ok_or_else(|| Fail::error("decoder format mismatch"))?;
    let src = input.str().ok_or_else(|| type_error("str"))?;
    let mut cx = DecodeCx { reader, input: input.clone(), pos, charged, scratch, decoder: Some(decoder.clone()) };
    let program = vm.program.clone();
    let ty = program.types.get(ty as usize).ok_or_else(|| type_error("type id"))?;
    let result = decode_value(vm, &mut cx, src, &program, ty);
    let mut d = decoder_obj(decoder)?.lock()?;
    (d.pos, d.charged) = (cx.pos, cx.charged);
    d.scratch = cx.scratch;
    result
}

/// Runs `f` on the input of the `Decoder` in argument 0 for a Sand
/// `Decoder` method. `f` may allocate its result.
fn with_decoder<T>(
    vm: &mut Vm,
    args: Args,
    f: impl FnOnce(&mut Vm, &dyn FormatReader, &str, &mut usize, &mut String) -> Result<T>,
) -> Result<T> {
    let decoder = vm.arg(args, 0).clone();
    let mut d = decoder_obj(&decoder)?.lock()?;
    let d = &mut *d;
    let src = d.input.str().ok_or_else(|| type_error("str"))?;
    let result = f(vm, d.reader, src, &mut d.pos, &mut d.scratch);
    charge_input(&mut vm.fuel, &mut d.charged, d.pos)?;
    result
}

/// The `json`, `enc`, `dec`, and `vm` native modules.
pub fn core_module() -> NativeModule {
    static JSON: Json = Json;
    let mut m = NativeModule::default();
    m.function("json.encode", 1, |vm, args| {
        let v = vm.arg(args, 0).clone();
        encode_with(vm, JsonWriter::default(), &v)
    });
    m.function("json.decode", 2, |vm, args| {
        let (text, ty) = (vm.arg(args, 0).clone(), int_arg(vm, args, 1)? as u16);
        decode_with(vm, &JSON, &text, ty)
    });
    m.function("vm.fail", 1, |vm, args| Err(Fail::error(vm.arg(args, 0).str().ok_or_else(|| type_error("str"))?.to_string())));
    m.function("vm.collect", 0, |vm, _| {
        vm.collect();
        Ok(Value::Unit)
    });

    m.function("enc.none", 1, |vm, args| encoder_event(vm, args, Event::None));
    m.function("enc.close_map", 1, |vm, args| encoder_event(vm, args, Event::CloseMap));
    m.function("enc.close_list", 1, |vm, args| encoder_event(vm, args, Event::CloseList));
    m.function("enc.close_variant", 1, |vm, args| encoder_event(vm, args, Event::CloseVariant));
    m.function("enc.open_map", 2, |vm, args| {
        let n = int_arg(vm, args, 1)? as usize;
        encoder_event(vm, args, Event::OpenMap(n))
    });
    m.function("enc.open_list", 2, |vm, args| {
        let n = int_arg(vm, args, 1)? as usize;
        encoder_event(vm, args, Event::OpenList(n))
    });
    m.function("enc.int", 2, |vm, args| {
        let i = int_arg(vm, args, 1)?;
        encoder_event(vm, args, Event::Int(i))
    });
    m.function("enc.float", 2, |vm, args| match *vm.arg(args, 1) {
        Value::Float(f) => encoder_event(vm, args, Event::Float(f)),
        _ => Err(type_error("float")),
    });
    m.function("enc.bool", 2, |vm, args| match *vm.arg(args, 1) {
        Value::Bool(b) => encoder_event(vm, args, Event::Bool(b)),
        _ => Err(type_error("bool")),
    });
    m.function("enc.str", 2, |vm, args| encoder_text(vm, args, |s| Event::Str(s)));
    m.function("enc.key", 2, |vm, args| encoder_text(vm, args, |s| Event::Key(s)));
    m.function("enc.tag", 2, |vm, args| encoder_text(vm, args, |s| Event::Tag(s)));
    m.function("enc.open_variant", 2, |vm, args| encoder_text(vm, args, |s| Event::OpenVariant(s)));
    m.function("enc.value", 2, |vm, args| {
        let (encoder, v) = (vm.arg(args, 0).clone(), vm.arg(args, 1).clone());
        let walk = encoder_obj(&encoder)?.lock()?.walk;
        walk(vm, &encoder, &v)?;
        Ok(Value::Unit)
    });

    m.function("dec.int", 1, |vm, args| Ok(Value::Int(with_decoder(vm, args, |_, r, src, pos, _| r.int(src, pos))?)));
    m.function("dec.float", 1, |vm, args| Ok(Value::Float(with_decoder(vm, args, |_, r, src, pos, _| r.float(src, pos))?)));
    m.function("dec.bool", 1, |vm, args| Ok(Value::Bool(with_decoder(vm, args, |_, r, src, pos, _| r.bool(src, pos))?)));
    m.function("dec.null", 1, |vm, args| Ok(Value::Bool(with_decoder(vm, args, |_, r, src, pos, _| r.null(src, pos))?)));
    m.function("dec.str", 1, |vm, args| {
        with_decoder(vm, args, |vm, r, src, pos, scratch| {
            let span = r.str(src, pos, scratch)?;
            vm.heap.new_str(span.resolve(src, scratch))
        })
    });
    m.function("dec.open_map", 1, |vm, args| with_decoder(vm, args, |_, r, src, pos, _| r.open_map(src, pos)).map(|_| Value::Unit));
    m.function("dec.open_list", 1, |vm, args| with_decoder(vm, args, |_, r, src, pos, _| r.open_list(src, pos)).map(|_| Value::Unit));
    m.function("dec.next_item", 1, |vm, args| Ok(Value::Bool(with_decoder(vm, args, |_, r, src, pos, _| r.next_item(src, pos))?)));
    m.function("dec.next_key", 1, |vm, args| {
        with_decoder(vm, args, |vm, r, src, pos, scratch| match r.next_key(src, pos, scratch)? {
            Some(span) => vm.heap.new_str(span.resolve(src, scratch)),
            None => Ok(Value::None),
        })
    });
    m.function("dec.open_variant", 1, |vm, args| {
        with_decoder(vm, args, |vm, r, src, pos, scratch| {
            let (span, _) = r.open_variant(src, pos, scratch)?;
            vm.heap.new_str(span.resolve(src, scratch))
        })
    });
    m.function("dec.close_variant", 1, |vm, args| {
        with_decoder(vm, args, |_, r, src, pos, _| r.close_variant(src, pos)).map(|_| Value::Unit)
    });
    m.function("dec.skip", 1, |vm, args| with_decoder(vm, args, |_, r, src, pos, _| r.skip(src, pos)).map(|_| Value::Unit));
    m.function("dec.value", 2, |vm, args| {
        let decoder = vm.arg(args, 0).clone();
        let ty = int_arg(vm, args, 1)? as u16;
        let walk = decoder_obj(&decoder)?.lock()?.walk;
        walk(vm, &decoder, ty)
    });
    m
}
