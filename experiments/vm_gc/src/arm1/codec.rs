//! The Arm 1 codec: one walker for structural values and custom `Encode`
//! and `Decode` callbacks, and the `Encoder` and `Decoder` objects that hold
//! the codec state while Sand code runs.
//!
//! The walkers keep the format state in Rust locals and move it into the
//! `Encoder` or `Decoder` object only for a callback. They read the input of
//! a decode through its rooted string handle and a byte offset, and they
//! root every container that they walk or build.

pub use super::codec_objects::*;
use super::heap::{Handle, Obj, Value};
use super::vm::{Args, NativeModule, Vm, type_error};
use crate::shared::format::{Event, FormatReader, FormatWriter, StrSpan, absent_fields, all_fields, charge_input, find_field};
use crate::shared::isa::{DECODE_METHOD, ENCODE_METHOD, Program, Ty};
use crate::shared::json::{Json, JsonWriter};
use crate::shared::{BYTES_PER_FUEL, Fail, Result, SHORT_STR_MAX, ShortStr, text_fuel};
use std::any::Any;
use std::mem::size_of;

/// Reserves output capacity for `need` more bytes.
#[inline]
fn grow_output(vm: &mut Vm, out: &mut String, need: usize) -> Result<()> {
    if out.capacity() - out.len() >= need {
        return Ok(());
    }
    let capacity = (out.capacity() * 2).max(out.len() + need).max(256);
    vm.reserve(capacity - out.capacity())?;
    out.reserve_exact(capacity - out.len());
    Ok(())
}

struct EncodeCx<W> {
    state: Option<Box<EncodeState<W>>>,
    /// Root index of the `Encoder` object, or of `Unit` until a callback needs one.
    encoder: usize,
}

impl<W: FormatWriter> EncodeCx<W> {
    #[inline]
    fn state(&mut self) -> &mut EncodeState<W> {
        self.state.as_deref_mut().expect("encode state")
    }
}

#[inline(always)]
fn write<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, event: Event<'_>) -> Result<()> {
    if event.text_len() >= BYTES_PER_FUEL {
        vm.charge(text_fuel(event.text_len()))?;
    }
    let state = cx.state();
    let need = state.writer.max_len(&event, event.text_len());
    grow_output(vm, &mut state.out, need)?;
    state.writer.write(&mut state.out, event);
    Ok(())
}

/// Writes the long string `v` in two steps, because the output may grow
/// (and collect) only while no heap string is borrowed.
#[inline]
fn write_heap_str<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, v: Value, key: bool) -> Result<()> {
    let len = vm.str(&v)?.len();
    vm.charge(text_fuel(len))?;
    let probe = if key { Event::Key("") } else { Event::Str("") };
    let state = cx.state();
    let need = state.writer.max_len(&probe, len);
    grow_output(vm, &mut state.out, need)?;
    let text = vm.heap.str(&v).ok_or_else(|| type_error("str"))?;
    let state = cx.state.as_deref_mut().expect("encode state");
    state.writer.write(&mut state.out, if key { Event::Key(text) } else { Event::Str(text) });
    Ok(())
}

/// Encodes `v` with `writer` and returns the output as a Sand string. The
/// caller must root `v`.
pub fn encode_with<W: FormatWriter>(vm: &mut Vm, writer: W, v: Value) -> Result<Value> {
    vm.reserve(size_of::<EncodeState<W>>())?;
    // The root slot of the `Encoder` object that the first callback creates.
    let encoder = vm.push_root(Value::Unit)?;
    let mut cx = EncodeCx { state: Some(Box::new(EncodeState { writer, out: String::new() })), encoder };
    let program = vm.program.clone();
    let result = encode_value(vm, &mut cx, &program, v);
    vm.release(size_of::<EncodeState<W>>());
    let out = cx.state.take().map(|s| s.out).unwrap_or_default();
    match result {
        Ok(()) => vm.new_str_from_reserved(out),
        Err(e) => {
            vm.release(out.capacity());
            Err(e)
        }
    }
}

fn encode_value<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: Value) -> Result<()> {
    vm.charge(1)?;
    match v {
        Value::Unit | Value::None => write(vm, cx, Event::None),
        Value::Bool(b) => write(vm, cx, Event::Bool(b)),
        Value::Int(i) => write(vm, cx, Event::Int(i)),
        Value::Float(f) => write(vm, cx, Event::Float(f)),
        Value::Short(s) => write(vm, cx, Event::Str(s.as_str())),
        Value::Tag(tag) => match p.enum_method(p.tags[tag as usize].enum_id, ENCODE_METHOD) {
            Some(func) => encode_callback(vm, cx, func, v),
            None => write(vm, cx, Event::Tag(&p.variant(tag).name)),
        },
        Value::Obj(h) => match vm.heap.get(h) {
            Obj::Str(_) => write_heap_str(vm, cx, v, false),
            Obj::Struct { ty, .. } => {
                let ty = *ty;
                match p.struct_method(ty, ENCODE_METHOD) {
                    Some(func) => encode_callback(vm, cx, func, v),
                    None => encode_struct(vm, cx, p, v, ty),
                }
            }
            Obj::Variant { tag, .. } => {
                let tag = *tag;
                match p.enum_method(p.tags[tag as usize].enum_id, ENCODE_METHOD) {
                    Some(func) => encode_callback(vm, cx, func, v),
                    None => encode_variant(vm, cx, p, h, tag),
                }
            }
            Obj::List(_) => encode_list(vm, cx, p, v),
            Obj::Map(_) => encode_map(vm, cx, p, h),
            _ => Err(Fail::error("value cannot be encoded")),
        },
    }
}

#[inline(never)]
fn encode_struct<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: Value, ty: u16) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.push_root(v)?;
    let fields = &p.structs[ty as usize].fields;
    write(vm, cx, Event::OpenMap(fields.len()))?;
    for (i, field) in fields.iter().enumerate() {
        write(vm, cx, Event::Key(&field.name))?;
        let field = vm.field(v, i)?;
        encode_value(vm, cx, p, field)?;
    }
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseMap)
}

#[inline(never)]
fn encode_variant<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, h: Handle, tag: u16) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.push_root(Value::Obj(h))?;
    let variant = p.variant(tag);
    let n = variant.payload.len();
    write(vm, cx, Event::OpenVariant(&variant.name))?;
    let names = variant.field_names.as_deref();
    write(vm, cx, if names.is_some() { Event::OpenMap(n) } else { Event::OpenList(n) })?;
    for i in 0..n {
        if let Some(names) = names {
            write(vm, cx, Event::Key(&names[i]))?;
        }
        let Obj::Variant { fields, .. } = vm.heap.get(h) else { unreachable!() };
        let field = fields[i];
        encode_value(vm, cx, p, field)?;
    }
    write(vm, cx, if names.is_some() { Event::CloseMap } else { Event::CloseList })?;
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseVariant)
}

#[inline(never)]
fn encode_list<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: Value) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.push_root(v)?;
    write(vm, cx, Event::OpenList(vm.list_len(v)?))?;
    let mut i = 0;
    while let Some(item) = vm.list_get(v, i)? {
        encode_value(vm, cx, p, item)?;
        i += 1;
    }
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseList)
}

#[inline(never)]
fn encode_map<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, h: Handle) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.push_root(Value::Obj(h))?;
    let Obj::Map(entries) = vm.heap.get(h) else { unreachable!() };
    write(vm, cx, Event::OpenMap(entries.len()))?;
    let mut i = 0;
    loop {
        let Obj::Map(entries) = vm.heap.get(h) else { unreachable!() };
        let Some(&(key, value)) = entries.get(i) else { break };
        match key {
            Value::Short(s) => write(vm, cx, Event::Key(s.as_str()))?,
            _ => write_heap_str(vm, cx, key, true)?,
        }
        encode_value(vm, cx, p, value)?;
        i += 1;
    }
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseMap)
}

/// Runs a custom `encode` method with the format state in the `Encoder`.
#[cold]
#[inline(never)]
fn encode_callback<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, func: u16, v: Value) -> Result<()> {
    vm.custom_calls += 1;
    let mut encoder = vm.root(cx.encoder);
    if encoder == Value::Unit {
        let mark = vm.push_root(v)?;
        vm.reserve_object(size_of::<EncoderObj>())?;
        encoder = vm.insert(Obj::Encoder(Box::new(EncoderObj { state: None, walk: encode_entry::<W> })));
        vm.set_root(cx.encoder, encoder);
        vm.truncate_roots(mark);
    }
    let state: Box<dyn DynEncodeState> = cx.state.take().expect("encode state");
    encoder_obj(vm, encoder)?.state = Some(state);
    let result = vm.call_function(func, Value::Unit, &[v, encoder]);
    let state = encoder_obj(vm, encoder)?.state.take().ok_or_else(|| Fail::error("encoder state is missing"))?;
    cx.state = Some(downcast_state(state)?);
    result.map(|_| ())
}

fn downcast_state<W: FormatWriter>(state: Box<dyn DynEncodeState>) -> Result<Box<EncodeState<W>>> {
    (state as Box<dyn Any + Send>).downcast().map_err(|_| Fail::error("encoder format mismatch"))
}

fn encoder_obj(vm: &mut Vm, v: Value) -> Result<&mut EncoderObj> {
    let Value::Obj(h) = v else { return Err(type_error("Encoder")) };
    match vm.heap.get_mut(h) {
        Obj::Encoder(e) => Ok(e),
        _ => Err(type_error("Encoder")),
    }
}

/// Encodes `v` into `encoder` from Sand code.
fn encode_entry<W: FormatWriter>(vm: &mut Vm, encoder: Value, v: Value) -> Result<()> {
    let mark = vm.push_root(encoder)?;
    let state = encoder_obj(vm, encoder)?.state.take().ok_or_else(|| Fail::error("encoder is not active"))?;
    let mut cx = EncodeCx { state: Some(downcast_state::<W>(state)?), encoder: mark };
    let program = vm.program.clone();
    let result = encode_value(vm, &mut cx, &program, v);
    let state: Box<dyn DynEncodeState> = cx.state.take().expect("encode state");
    encoder_obj(vm, encoder)?.state = Some(state);
    result
}

/// Runs `f` on the format state of `encoder` for a Sand `Encoder` method.
fn with_encoder(vm: &mut Vm, encoder: Value, f: impl FnOnce(&mut Vm, &mut dyn DynEncodeState) -> Result<()>) -> Result<Value> {
    let mut state = encoder_obj(vm, encoder)?.state.take().ok_or_else(|| Fail::error("encoder is not active"))?;
    let result = f(vm, &mut *state);
    encoder_obj(vm, encoder)?.state = Some(state);
    result.map(|_| Value::Unit)
}

fn encoder_event(vm: &mut Vm, args: Args, event: Event<'static>) -> Result<Value> {
    let encoder = vm.arg(args, 0);
    with_encoder(vm, encoder, |vm, state| {
        let need = state.max_len(&event, 0);
        grow_output(vm, state.out(), need)?;
        state.write(event);
        Ok(())
    })
}

/// Writes string argument 1 as a `Str`, `Key`, `Tag`, or `OpenVariant` event.
fn encoder_text(vm: &mut Vm, args: Args, event: for<'a> fn(&'a str) -> Event<'a>) -> Result<Value> {
    let (encoder, text) = (vm.arg(args, 0), vm.arg(args, 1));
    with_encoder(vm, encoder, |vm, state| {
        let len = vm.str(&text)?.len();
        vm.charge(text_fuel(len))?;
        let need = state.max_len(&event(""), len);
        grow_output(vm, state.out(), need)?;
        state.write(event(vm.str(&text)?));
        Ok(())
    })
}

fn int_arg(vm: &Vm, args: Args, i: usize) -> Result<i64> {
    match vm.arg(args, i) {
        Value::Int(n) => Ok(n),
        _ => Err(type_error("int")),
    }
}

// -------------------------------------------------------------------------
// Decoding
// -------------------------------------------------------------------------

struct DecodeCx<R: 'static> {
    reader: &'static R,
    /// The rooted input string.
    input: Value,
    pos: usize,
    /// The input position up to which the decode has charged fuel.
    charged: usize,
    scratch: String,
    /// Root index of the `Decoder` object, or of `Unit` until a callback needs one.
    decoder: usize,
}

/// Decodes `text` as `program.types[ty]` with `reader`. The caller must root `text`.
pub fn decode_with<R: FormatReader>(vm: &mut Vm, reader: &'static R, text: Value, ty: u16) -> Result<Value> {
    vm.str(&text)?;
    let program = vm.program.clone();
    let ty = program.types.get(ty as usize).ok_or_else(|| type_error("type id"))?;
    // The root slot of the `Decoder` object that the first callback creates.
    let decoder = vm.push_root(Value::Unit)?;
    let mut cx = DecodeCx { reader, input: text, pos: 0, charged: 0, scratch: String::new(), decoder };
    let v = decode_value(vm, &mut cx, &program, ty)?;
    cx.reader.end(vm.heap.str(&cx.input).expect("decode input"), &mut cx.pos)?;
    charge_input(&mut vm.fuel, &mut cx.charged, cx.pos)?;
    Ok(v)
}

macro_rules! src {
    ($vm:expr, $cx:expr) => {
        $vm.heap.str(&$cx.input).expect("decode input")
    };
}

fn decode_value<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, p: &Program, mut ty: &Ty) -> Result<Value> {
    vm.charge(1)?;
    charge_input(&mut vm.fuel, &mut cx.charged, cx.pos)?;
    loop {
        return match ty {
            Ty::Option(inner) => {
                if cx.reader.null(src!(vm, cx), &mut cx.pos)? {
                    Ok(Value::None)
                } else {
                    ty = inner;
                    continue;
                }
            }
            Ty::List(item) => decode_list(vm, cx, p, item),
            Ty::Map(value) => decode_map(vm, cx, p, value),
            Ty::Struct(id) => match p.struct_method(*id, DECODE_METHOD) {
                Some(func) => decode_callback(vm, cx, func),
                None => {
                    let fields = &p.structs[*id as usize].fields;
                    decode_record(vm, cx, p, fields.iter().map(|f| (f.name.as_str(), &f.ty)), Record::Struct(*id))
                }
            },
            Ty::Enum(id) => match p.enum_method(*id, DECODE_METHOD) {
                Some(func) => decode_callback(vm, cx, func),
                None => decode_enum(vm, cx, p, *id),
            },
            _ => decode_scalar(vm, cx, ty),
        };
    }
}

/// Decodes an `int`, `float`, `bool`, or `str`. A separate function keeps
/// the recursive path of [`decode_value`] small on the stack.
#[inline(never)]
fn decode_scalar<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, ty: &Ty) -> Result<Value> {
    Ok(match ty {
        Ty::Int => Value::Int(cx.reader.int(src!(vm, cx), &mut cx.pos)?),
        Ty::Float => Value::Float(cx.reader.float(src!(vm, cx), &mut cx.pos)?),
        Ty::Bool => Value::Bool(cx.reader.bool(src!(vm, cx), &mut cx.pos)?),
        _ => {
            let span = cx.reader.str(src!(vm, cx), &mut cx.pos, &mut cx.scratch)?;
            new_str_from_span(vm, cx, span)?
        }
    })
}

#[inline(never)]
fn decode_list<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, p: &Program, item: &Ty) -> Result<Value> {
    vm.check_stack()?;
    cx.reader.open_list(src!(vm, cx), &mut cx.pos)?;
    let mark = vm.root_mark();
    while cx.reader.next_item(src!(vm, cx), &mut cx.pos)? {
        let v = decode_value(vm, cx, p, item)?;
        vm.push_root(v)?;
    }
    let n = vm.root_mark() - mark;
    vm.reserve_object(n * size_of::<Value>())?;
    let items = vm.roots_from(mark).to_vec();
    vm.truncate_roots(mark);
    Ok(vm.insert(Obj::List(items)))
}

#[inline(never)]
fn decode_map<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, p: &Program, value: &Ty) -> Result<Value> {
    vm.check_stack()?;
    cx.reader.open_map(src!(vm, cx), &mut cx.pos)?;
    let mark = vm.root_mark();
    while let Some(span) = cx.reader.next_key(src!(vm, cx), &mut cx.pos, &mut cx.scratch)? {
        let key = new_str_from_span(vm, cx, span)?;
        vm.push_root(key)?;
        let v = decode_value(vm, cx, p, value)?;
        vm.push_root(v)?;
    }
    let n = (vm.root_mark() - mark) / 2;
    vm.reserve_object(n * size_of::<(Value, Value)>())?;
    let entries = vm.roots_from(mark).as_chunks::<2>().0.iter().map(|kv| (kv[0], kv[1])).collect();
    vm.truncate_roots(mark);
    Ok(vm.insert(Obj::Map(entries)))
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
    p: &'p Program,
    fields: impl Iterator<Item = (&'p str, &'p Ty)> + Clone,
    record: Record,
) -> Result<Value> {
    vm.check_stack()?;
    cx.reader.open_map(src!(vm, cx), &mut cx.pos)?;
    let mark = vm.root_mark();
    let n = fields.clone().count();
    for _ in 0..n {
        vm.push_root(Value::Unit)?;
    }
    let (mut hint, mut seen) = (0, 0u64);
    while let Some(span) = cx.reader.next_key(src!(vm, cx), &mut cx.pos, &mut cx.scratch)? {
        let key = span.resolve(src!(vm, cx), &cx.scratch);
        match find_field(fields.clone().map(|(name, _)| name), key, hint) {
            Some(i) => {
                let ty = fields.clone().nth(i).expect("field").1;
                let v = decode_value(vm, cx, p, ty)?;
                vm.set_root(mark + i, v);
                hint = i + 1;
                seen |= 1 << i;
            }
            None => {
                cx.reader.skip(src!(vm, cx), &mut cx.pos)?;
                charge_input(&mut vm.fuel, &mut cx.charged, cx.pos)?;
            }
        }
    }
    if seen != all_fields(n) {
        let absent = absent_fields(fields, seen)?;
        (0..n).filter(|i| absent & (1 << i) != 0).for_each(|i| vm.set_root(mark + i, Value::None));
    }
    vm.reserve_object(n * size_of::<Value>())?;
    let fields = vm.roots_from(mark).into();
    vm.truncate_roots(mark);
    Ok(vm.insert(match record {
        Record::Struct(ty) => Obj::Struct { ty, fields },
        Record::Variant(tag) => Obj::Variant { tag, fields },
    }))
}

#[inline(never)]
fn decode_enum<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, p: &Program, id: u16) -> Result<Value> {
    let def = &p.enums[id as usize];
    let (span, has_payload) = cx.reader.open_variant(src!(vm, cx), &mut cx.pos, &mut cx.scratch)?;
    let name = span.resolve(src!(vm, cx), &cx.scratch);
    let variant =
        def.variants.iter().find(|v| v.name == name).ok_or_else(|| Fail::error(format!("unknown variant `{name}` of {}", def.name)))?;
    if variant.payload.is_empty() == has_payload {
        return Err(Fail::error(format!("variant `{}` payload mismatch", variant.name)));
    }
    if !has_payload {
        return Ok(Value::Tag(variant.tag));
    }
    let v = match &variant.field_names {
        Some(names) => decode_record(vm, cx, p, names.iter().map(String::as_str).zip(&variant.payload), Record::Variant(variant.tag))?,
        None => {
            vm.check_stack()?;
            cx.reader.open_list(src!(vm, cx), &mut cx.pos)?;
            let mark = vm.root_mark();
            for ty in &variant.payload {
                if !cx.reader.next_item(src!(vm, cx), &mut cx.pos)? {
                    return Err(Fail::error("variant payload is too short"));
                }
                let v = decode_value(vm, cx, p, ty)?;
                vm.push_root(v)?;
            }
            if cx.reader.next_item(src!(vm, cx), &mut cx.pos)? {
                return Err(Fail::error("variant payload is too long"));
            }
            vm.reserve_object(variant.payload.len() * size_of::<Value>())?;
            let fields = vm.roots_from(mark).into();
            vm.truncate_roots(mark);
            vm.insert(Obj::Variant { tag: variant.tag, fields })
        }
    };
    cx.reader.close_variant(src!(vm, cx), &mut cx.pos)?;
    Ok(v)
}

/// Creates a string from a decoded span.
#[inline]
fn new_str_from_span<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, span: StrSpan) -> Result<Value> {
    let text = span.resolve(src!(vm, cx), &cx.scratch);
    if let Some(short) = ShortStr::new(text) {
        return Ok(Value::Short(short));
    }
    let len = text.len();
    debug_assert!(len > SHORT_STR_MAX);
    vm.reserve_object(len)?;
    let text: Box<str> = span.resolve(src!(vm, cx), &cx.scratch).into();
    Ok(vm.insert(Obj::Str(text)))
}

/// Runs a custom `decode` method with the input position in the `Decoder`.
#[cold]
#[inline(never)]
fn decode_callback<R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<R>, func: u16) -> Result<Value> {
    vm.custom_calls += 1;
    let mut decoder = vm.root(cx.decoder);
    if decoder == Value::Unit {
        vm.reserve_object(size_of::<DecoderObj>())?;
        let obj = DecoderObj { input: cx.input, pos: 0, charged: 0, scratch: String::new(), reader: cx.reader, walk: decode_entry::<R> };
        decoder = vm.insert(Obj::Decoder(Box::new(obj)));
        vm.set_root(cx.decoder, decoder);
    }
    let d = decoder_obj(vm, decoder)?;
    (d.pos, d.charged) = (cx.pos, cx.charged);
    d.scratch = std::mem::take(&mut cx.scratch);
    let result = vm.call_function(func, Value::Unit, &[decoder]);
    let d = decoder_obj(vm, decoder)?;
    (cx.pos, cx.charged) = (d.pos, d.charged);
    cx.scratch = std::mem::take(&mut d.scratch);
    result
}

fn decoder_obj(vm: &mut Vm, v: Value) -> Result<&mut DecoderObj> {
    let Value::Obj(h) = v else { return Err(type_error("Decoder")) };
    match vm.heap.get_mut(h) {
        Obj::Decoder(d) => Ok(d),
        _ => Err(type_error("Decoder")),
    }
}

/// Decodes a nested value from a `Decoder` in Sand code.
fn decode_entry<R: FormatReader>(vm: &mut Vm, decoder: Value, ty: u16) -> Result<Value> {
    let d = decoder_obj(vm, decoder)?;
    let input = d.input;
    let reader: &'static R = (d.reader as &dyn Any).downcast_ref().ok_or_else(|| Fail::error("decoder format mismatch"))?;
    let (pos, charged, scratch) = (d.pos, d.charged, std::mem::take(&mut d.scratch));
    let mut cx = DecodeCx { reader, input, pos, charged, scratch, decoder: vm.push_root(decoder)? };
    let program = vm.program.clone();
    let ty = program.types.get(ty as usize).ok_or_else(|| type_error("type id"))?;
    let result = decode_value(vm, &mut cx, &program, ty);
    let d = decoder_obj(vm, decoder)?;
    (d.pos, d.charged) = (cx.pos, cx.charged);
    d.scratch = cx.scratch;
    result
}

/// Runs `f` on the input of `decoder` for a Sand `Decoder` method, then
/// converts a string span result to a Sand string.
fn with_decoder<T>(vm: &mut Vm, args: Args, f: impl FnOnce(&dyn FormatReader, &str, &mut usize, &mut String) -> Result<T>) -> Result<T> {
    let decoder = vm.arg(args, 0);
    let d = decoder_obj(vm, decoder)?;
    let (input, reader, mut pos, mut scratch) = (d.input, d.reader, d.pos, std::mem::take(&mut d.scratch));
    let src = vm.heap.str(&input).ok_or_else(|| type_error("str"))?;
    let result = f(reader, src, &mut pos, &mut scratch);
    let d = decoder_obj(vm, decoder)?;
    d.pos = pos;
    d.scratch = scratch;
    let mut charged = d.charged;
    let charge = charge_input(&mut vm.fuel, &mut charged, pos);
    decoder_obj(vm, decoder)?.charged = charged;
    charge?;
    result
}

/// Converts a span that `with_decoder` returned for `decoder`.
fn span_to_str(vm: &mut Vm, args: Args, span: StrSpan) -> Result<Value> {
    let decoder = vm.arg(args, 0);
    let d = decoder_obj(vm, decoder)?;
    let input = d.input;
    let scratch = std::mem::take(&mut d.scratch);
    let text = span.resolve(vm.heap.str(&input).ok_or_else(|| type_error("str"))?, &scratch);
    let result = match ShortStr::new(text) {
        Some(short) => Ok(Value::Short(short)),
        None => {
            let len = text.len();
            vm.reserve_object(len).map(|_| {
                let text: Box<str> = span.resolve(vm.heap.str(&input).expect("input"), &scratch).into();
                vm.insert(Obj::Str(text))
            })
        }
    };
    decoder_obj(vm, decoder)?.scratch = scratch;
    result
}

/// The `json`, `enc`, `dec`, and `vm` native modules.
pub fn core_module() -> NativeModule {
    static JSON: Json = Json;
    let mut m = NativeModule::default();
    m.function("json.encode", |vm, args| {
        let v = vm.arg(args, 0);
        encode_with(vm, JsonWriter::default(), v)
    });
    m.function("json.decode", |vm, args| {
        let (text, ty) = (vm.arg(args, 0), int_arg(vm, args, 1)? as u16);
        decode_with(vm, &JSON, text, ty)
    });
    m.function("vm.fail", |vm, args| Err(Fail::error(vm.str(&vm.arg(args, 0))?.to_string())));
    m.function("vm.collect", |vm, _| {
        vm.collect();
        Ok(Value::Unit)
    });

    m.function("enc.none", |vm, args| encoder_event(vm, args, Event::None));
    m.function("enc.close_map", |vm, args| encoder_event(vm, args, Event::CloseMap));
    m.function("enc.close_list", |vm, args| encoder_event(vm, args, Event::CloseList));
    m.function("enc.close_variant", |vm, args| encoder_event(vm, args, Event::CloseVariant));
    m.function("enc.open_map", |vm, args| {
        let n = int_arg(vm, args, 1)? as usize;
        encoder_event(vm, args, Event::OpenMap(n))
    });
    m.function("enc.open_list", |vm, args| {
        let n = int_arg(vm, args, 1)? as usize;
        encoder_event(vm, args, Event::OpenList(n))
    });
    m.function("enc.int", |vm, args| {
        let i = int_arg(vm, args, 1)?;
        encoder_event(vm, args, Event::Int(i))
    });
    m.function("enc.float", |vm, args| match vm.arg(args, 1) {
        Value::Float(f) => encoder_event(vm, args, Event::Float(f)),
        _ => Err(type_error("float")),
    });
    m.function("enc.bool", |vm, args| match vm.arg(args, 1) {
        Value::Bool(b) => encoder_event(vm, args, Event::Bool(b)),
        _ => Err(type_error("bool")),
    });
    m.function("enc.str", |vm, args| encoder_text(vm, args, |s| Event::Str(s)));
    m.function("enc.key", |vm, args| encoder_text(vm, args, |s| Event::Key(s)));
    m.function("enc.tag", |vm, args| encoder_text(vm, args, |s| Event::Tag(s)));
    m.function("enc.open_variant", |vm, args| encoder_text(vm, args, |s| Event::OpenVariant(s)));
    m.function("enc.value", |vm, args| {
        let encoder = vm.arg(args, 0);
        let (walk, v) = (encoder_obj(vm, encoder)?.walk, vm.arg(args, 1));
        walk(vm, encoder, v)?;
        Ok(Value::Unit)
    });

    m.function("dec.int", |vm, args| Ok(Value::Int(with_decoder(vm, args, |r, src, pos, _| r.int(src, pos))?)));
    m.function("dec.float", |vm, args| Ok(Value::Float(with_decoder(vm, args, |r, src, pos, _| r.float(src, pos))?)));
    m.function("dec.bool", |vm, args| Ok(Value::Bool(with_decoder(vm, args, |r, src, pos, _| r.bool(src, pos))?)));
    m.function("dec.null", |vm, args| Ok(Value::Bool(with_decoder(vm, args, |r, src, pos, _| r.null(src, pos))?)));
    m.function("dec.str", |vm, args| {
        let span = with_decoder(vm, args, |r, src, pos, scratch| r.str(src, pos, scratch))?;
        span_to_str(vm, args, span)
    });
    m.function("dec.open_map", |vm, args| with_decoder(vm, args, |r, src, pos, _| r.open_map(src, pos)).map(|_| Value::Unit));
    m.function("dec.open_list", |vm, args| with_decoder(vm, args, |r, src, pos, _| r.open_list(src, pos)).map(|_| Value::Unit));
    m.function("dec.next_item", |vm, args| Ok(Value::Bool(with_decoder(vm, args, |r, src, pos, _| r.next_item(src, pos))?)));
    m.function("dec.next_key", |vm, args| match with_decoder(vm, args, |r, src, pos, scratch| r.next_key(src, pos, scratch))? {
        Some(span) => span_to_str(vm, args, span),
        None => Ok(Value::None),
    });
    m.function("dec.open_variant", |vm, args| {
        let span = with_decoder(vm, args, |r, src, pos, scratch| r.open_variant(src, pos, scratch))?.0;
        span_to_str(vm, args, span)
    });
    m.function("dec.close_variant", |vm, args| with_decoder(vm, args, |r, src, pos, _| r.close_variant(src, pos)).map(|_| Value::Unit));
    m.function("dec.skip", |vm, args| with_decoder(vm, args, |r, src, pos, _| r.skip(src, pos)).map(|_| Value::Unit));
    m.function("dec.value", |vm, args| {
        let decoder = vm.arg(args, 0);
        let ty = int_arg(vm, args, 1)? as u16;
        let walk = decoder_obj(vm, decoder)?.walk;
        walk(vm, decoder, ty)
    });
    m
}
