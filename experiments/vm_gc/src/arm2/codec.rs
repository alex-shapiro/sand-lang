//! The Arm 2 codec: one walker for structural values and custom `Encode`
//! and `Decode` callbacks, written against the scoped host API.
//!
//! The walker follows the Arm 1 algorithm. Every function that may collect
//! takes a [`GcScope`], so a handle that the walker does not root cannot
//! cross an allocation or a callback without an explicit `unbind`.

use super::scope::{GcScope, NativeModule, NoGcScope, Record, Scoped, Value, Vm, enter};
use crate::arm1::codec::{DecoderObj, DynEncodeState, EncodeState, EncoderObj};
use crate::arm1::heap::{Obj, Value as RawValue};
use crate::arm1::vm::{self as raw, Args, type_error};
use crate::shared::format::{Event, FormatReader, FormatWriter, StrSpan, find_field};
use crate::shared::isa::{DECODE_METHOD, ENCODE_METHOD, Program, Ty};
use crate::shared::json::{Json, JsonWriter};
use crate::shared::{Fail, Result};
use std::any::Any;
use std::mem::size_of;

/// The implementation of `method` for struct `ty`.
#[inline(always)]
fn struct_impl(p: &Program, ty: u16, method: u16) -> Option<u16> {
    p.struct_methods[ty as usize][method as usize]
}

/// The implementation of `method` for enum `id`.
#[inline(always)]
fn enum_impl(p: &Program, id: u16, method: u16) -> Option<u16> {
    p.enum_methods[id as usize][method as usize]
}

/// Reserves output capacity for `need` more bytes.
#[inline]
fn grow_output(vm: &mut Vm, out: &mut String, need: usize, gc: GcScope<'_, '_>) -> Result<()> {
    if out.capacity() - out.len() >= need {
        return Ok(());
    }
    let capacity = (out.capacity() * 2).max(out.len() + need).max(256);
    vm.reserve(capacity - out.capacity(), gc)?;
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
fn write<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, event: Event<'_>, gc: GcScope<'_, '_>) -> Result<()> {
    let state = cx.state();
    let need = state.writer.max_len(&event, event.text_len());
    grow_output(vm, &mut state.out, need, gc)?;
    state.writer.write(&mut state.out, event);
    Ok(())
}

/// Writes the long string `v` in two steps, because the output may grow
/// (and collect) only while no heap string is borrowed.
#[inline]
fn write_heap_str<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, v: Value<'_>, key: bool, mut gc: GcScope<'_, '_>) -> Result<()> {
    let len = vm.str(&v, gc.nogc())?.len();
    let probe = if key { Event::Key("") } else { Event::Str("") };
    let state = cx.state();
    let need = state.writer.max_len(&probe, len);
    grow_output(vm, &mut state.out, need, gc.reborrow())?;
    // `v` is rooted by the container that holds it.
    let v = v.bind(gc.nogc());
    let text = vm.str(&v, gc.nogc())?;
    let state = cx.state.as_deref_mut().expect("encode state");
    state.writer.write(&mut state.out, if key { Event::Key(text) } else { Event::Str(text) });
    Ok(())
}

/// Encodes `v` with `writer` and returns the output as a Sand string. The
/// caller must keep `v` rooted.
pub fn encode_with<'a, W: FormatWriter>(vm: &mut Vm, writer: W, v: Value<'_>, mut gc: GcScope<'a, '_>) -> Result<Value<'a>> {
    vm.reserve(size_of::<EncodeState<W>>(), gc.reborrow())?;
    // The root slot of the `Encoder` object that the first callback creates.
    let encoder = vm.push_root(Value::UNIT, gc.reborrow())?;
    let mut cx = EncodeCx { state: Some(Box::new(EncodeState { writer, out: String::new() })), encoder };
    let program = vm.program().clone();
    let result = encode_value(vm, &mut cx, &program, v, gc.reborrow());
    vm.release(size_of::<EncodeState<W>>());
    let out = cx.state.take().map(|s| s.out).unwrap_or_default();
    match result {
        Ok(()) => vm.new_str_from_reserved(out, gc),
        Err(e) => {
            vm.release(out.capacity());
            Err(e)
        }
    }
}

fn encode_value<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: Value<'_>, gc: GcScope<'_, '_>) -> Result<()> {
    vm.charge(1)?;
    // `v` stays unbound so that the container calls below forward it unchanged as tail calls.
    match v.raw() {
        RawValue::Unit | RawValue::None => write(vm, cx, Event::None, gc),
        RawValue::Bool(b) => write(vm, cx, Event::Bool(b), gc),
        RawValue::Int(i) => write(vm, cx, Event::Int(i), gc),
        RawValue::Float(f) => write(vm, cx, Event::Float(f), gc),
        RawValue::Short(s) => write(vm, cx, Event::Str(s.as_str()), gc),
        RawValue::Tag(tag) => match enum_impl(p, p.tags[tag as usize].enum_id, ENCODE_METHOD) {
            Some(func) => encode_callback(vm, cx, func, v, gc),
            None => write(vm, cx, Event::Tag(&p.variant(tag).name), gc),
        },
        RawValue::Obj(_) => match vm.obj(v, gc.nogc()) {
            Some(Obj::Str(_)) => write_heap_str(vm, cx, v, false, gc),
            Some(Obj::Struct { ty, .. }) => {
                let ty = *ty;
                match struct_impl(p, ty, ENCODE_METHOD) {
                    Some(func) => encode_callback(vm, cx, func, v, gc),
                    None => encode_struct(vm, cx, p, v, ty, gc),
                }
            }
            Some(Obj::Variant { tag, .. }) => {
                let tag = *tag;
                match enum_impl(p, p.tags[tag as usize].enum_id, ENCODE_METHOD) {
                    Some(func) => encode_callback(vm, cx, func, v, gc),
                    None => encode_variant(vm, cx, p, v, tag, gc),
                }
            }
            Some(Obj::List(_)) => encode_list(vm, cx, p, v, gc),
            Some(Obj::Map(_)) => encode_map(vm, cx, p, v, gc),
            _ => Err(Fail::error("value cannot be encoded")),
        },
    }
}

#[inline(never)]
fn encode_struct<W: FormatWriter>(
    vm: &mut Vm,
    cx: &mut EncodeCx<W>,
    p: &Program,
    v: Value<'_>,
    ty: u16,
    mut gc: GcScope<'_, '_>,
) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.root_mark();
    let v = v.scope(vm, gc.reborrow())?;
    let fields = &p.structs[ty as usize].fields;
    write(vm, cx, Event::OpenMap(fields.len()), gc.reborrow())?;
    for (i, field) in fields.iter().enumerate() {
        write(vm, cx, Event::Key(&field.name), gc.reborrow())?;
        let field = vm.field(v.get(vm, gc.nogc()), i, gc.nogc())?;
        encode_value(vm, cx, p, field.unbind(), gc.reborrow())?;
    }
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseMap, gc)
}

#[inline(never)]
fn encode_variant<W: FormatWriter>(
    vm: &mut Vm,
    cx: &mut EncodeCx<W>,
    p: &Program,
    v: Value<'_>,
    tag: u16,
    mut gc: GcScope<'_, '_>,
) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.root_mark();
    let v = v.scope(vm, gc.reborrow())?;
    let variant = p.variant(tag);
    let n = variant.payload.len();
    write(vm, cx, Event::OpenVariant(&variant.name), gc.reborrow())?;
    let names = variant.field_names.as_deref();
    write(vm, cx, if names.is_some() { Event::OpenMap(n) } else { Event::OpenList(n) }, gc.reborrow())?;
    for i in 0..n {
        if let Some(names) = names {
            write(vm, cx, Event::Key(&names[i]), gc.reborrow())?;
        }
        let field = vm.variant_field(v.get(vm, gc.nogc()), i, gc.nogc())?;
        encode_value(vm, cx, p, field.unbind(), gc.reborrow())?;
    }
    write(vm, cx, if names.is_some() { Event::CloseMap } else { Event::CloseList }, gc.reborrow())?;
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseVariant, gc)
}

#[inline(never)]
fn encode_list<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: Value<'_>, mut gc: GcScope<'_, '_>) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.root_mark();
    let v = v.scope(vm, gc.reborrow())?;
    let n = vm.list_len(v.get(vm, gc.nogc()), gc.nogc())?;
    write(vm, cx, Event::OpenList(n), gc.reborrow())?;
    let mut i = 0;
    while let Some(item) = vm.list_get(v.get(vm, gc.nogc()), i, gc.nogc())? {
        encode_value(vm, cx, p, item.unbind(), gc.reborrow())?;
        i += 1;
    }
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseList, gc)
}

#[inline(never)]
fn encode_map<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, p: &Program, v: Value<'_>, mut gc: GcScope<'_, '_>) -> Result<()> {
    vm.check_stack()?;
    let mark = vm.root_mark();
    let v = v.scope(vm, gc.reborrow())?;
    let Some(Obj::Map(entries)) = vm.obj(v.get(vm, gc.nogc()), gc.nogc()) else { unreachable!() };
    let n = entries.len();
    write(vm, cx, Event::OpenMap(n), gc.reborrow())?;
    let mut i = 0;
    while let Some((key, value)) = vm.map_entry(v.get(vm, gc.nogc()), i, gc.nogc())? {
        let value = value.unbind();
        match key.raw() {
            RawValue::Short(s) => write(vm, cx, Event::Key(s.as_str()), gc.reborrow())?,
            _ => write_heap_str(vm, cx, key.unbind(), true, gc.reborrow())?,
        }
        // `value` is rooted by the map.
        encode_value(vm, cx, p, value, gc.reborrow())?;
        i += 1;
    }
    vm.truncate_roots(mark);
    write(vm, cx, Event::CloseMap, gc)
}

/// Runs a custom `encode` method with the format state in the `Encoder`.
#[cold]
#[inline(never)]
fn encode_callback<W: FormatWriter>(vm: &mut Vm, cx: &mut EncodeCx<W>, func: u16, v: Value<'_>, mut gc: GcScope<'_, '_>) -> Result<()> {
    let mut v = v.bind(gc.nogc());
    let mut encoder = vm.root(cx.encoder, gc.nogc());
    if encoder.is_unit() {
        let mark = vm.root_mark();
        let rooted = v.unbind().scope(vm, gc.reborrow())?;
        let created = vm.new_encoder(EncoderObj { state: None, walk: encode_entry::<W> }, gc.reborrow())?.unbind();
        vm.set_root(cx.encoder, created);
        v = rooted.get(vm, gc.nogc());
        vm.truncate_roots(mark);
        encoder = created.bind(gc.nogc());
    }
    let state: Box<dyn DynEncodeState> = cx.state.take().expect("encode state");
    vm.encoder_obj(encoder, gc.nogc())?.state = Some(state);
    let result = vm.call_function(func, &[v.unbind(), encoder.unbind()], gc.reborrow()).map(|_| ());
    let encoder = vm.root(cx.encoder, gc.nogc());
    let state = vm.encoder_obj(encoder, gc.nogc())?.state.take().ok_or_else(|| Fail::error("encoder state is missing"))?;
    cx.state = Some(downcast_state(state)?);
    result
}

fn downcast_state<W: FormatWriter>(state: Box<dyn DynEncodeState>) -> Result<Box<EncodeState<W>>> {
    (state as Box<dyn Any + Send>).downcast().map_err(|_| Fail::error("encoder format mismatch"))
}

/// Encodes a value into an `Encoder` from Sand code.
fn encode_entry<W: FormatWriter>(vm: &mut raw::Vm, encoder: RawValue, v: RawValue) -> Result<()> {
    enter(|gc| encode_nested::<W>(&mut Vm::new(vm), encoder, v, gc))
}

fn encode_nested<W: FormatWriter>(vm: &mut Vm, encoder: RawValue, v: RawValue, mut gc: GcScope<'_, '_>) -> Result<()> {
    let encoder = Value::from_raw(encoder, gc.nogc());
    // The Sand caller's argument registers root `v`.
    let v = Value::from_raw(v, gc.nogc()).unbind();
    let state = vm.encoder_obj(encoder, gc.nogc())?.state.take().ok_or_else(|| Fail::error("encoder is not active"))?;
    let mark = vm.push_root(encoder.unbind(), gc.reborrow())?;
    let mut cx = EncodeCx { state: Some(downcast_state::<W>(state)?), encoder: mark };
    let program = vm.program().clone();
    let result = encode_value(vm, &mut cx, &program, v, gc.reborrow());
    let state: Box<dyn DynEncodeState> = cx.state.take().expect("encode state");
    let encoder = vm.root(mark, gc.nogc());
    vm.encoder_obj(encoder, gc.nogc())?.state = Some(state);
    result
}

/// Runs `f` on the format state of the `Encoder` in argument 0.
fn with_encoder<'a>(
    vm: &mut Vm,
    args: Args,
    mut gc: GcScope<'a, '_>,
    f: impl FnOnce(&mut Vm, &mut dyn DynEncodeState, GcScope<'_, '_>) -> Result<()>,
) -> Result<Value<'a>> {
    let encoder = vm.arg(args, 0, gc.nogc());
    let mut state = vm.encoder_obj(encoder, gc.nogc())?.state.take().ok_or_else(|| Fail::error("encoder is not active"))?;
    let result = f(vm, &mut *state, gc.reborrow());
    let encoder = vm.arg(args, 0, gc.nogc());
    vm.encoder_obj(encoder, gc.nogc())?.state = Some(state);
    result.map(|_| Value::UNIT)
}

fn encoder_event<'a>(vm: &mut Vm, args: Args, event: Event<'static>, gc: GcScope<'a, '_>) -> Result<Value<'a>> {
    with_encoder(vm, args, gc, |vm, state, gc| {
        let need = state.max_len(&event, 0);
        grow_output(vm, state.out(), need, gc)?;
        state.write(event);
        Ok(())
    })
}

/// Writes string argument 1 as a `Str`, `Key`, `Tag`, or `OpenVariant` event.
fn encoder_text<'a>(vm: &mut Vm, args: Args, event: for<'t> fn(&'t str) -> Event<'t>, gc: GcScope<'a, '_>) -> Result<Value<'a>> {
    with_encoder(vm, args, gc, |vm, state, mut gc| {
        let len = vm.str(&vm.arg(args, 1, gc.nogc()), gc.nogc())?.len();
        let need = state.max_len(&event(""), len);
        grow_output(vm, state.out(), need, gc.reborrow())?;
        let text = vm.arg(args, 1, gc.nogc());
        state.write(event(vm.str(&text, gc.nogc())?));
        Ok(())
    })
}

fn int_arg(vm: &Vm, args: Args, i: usize, gc: NoGcScope<'_, '_>) -> Result<i64> {
    vm.arg(args, i, gc).as_int().ok_or_else(|| type_error("int"))
}

// -------------------------------------------------------------------------
// Decoding
// -------------------------------------------------------------------------

struct DecodeCx<'s, R: 'static> {
    reader: &'static R,
    input: Scoped<'s>,
    pos: usize,
    scratch: String,
    /// Root index of the `Decoder` object, or of `Unit` until a callback needs one.
    decoder: usize,
}

/// Decodes `text` as `program.types[ty]` with `reader`.
pub fn decode_with<'a, R: FormatReader>(
    vm: &mut Vm,
    reader: &'static R,
    text: Value<'_>,
    ty: u16,
    mut gc: GcScope<'a, '_>,
) -> Result<Value<'a>> {
    vm.str(&text, gc.nogc())?;
    let program = vm.program().clone();
    let ty = program.types.get(ty as usize).ok_or_else(|| type_error("type id"))?;
    let input = text.unbind().scope(vm, gc.reborrow())?;
    // The root slot of the `Decoder` object that the first callback creates.
    let decoder = vm.push_root(Value::UNIT, gc.reborrow())?;
    let mut cx = DecodeCx { reader, input, pos: 0, scratch: String::new(), decoder };
    decode_value(vm, &mut cx, &program, ty, gc)
}

macro_rules! src {
    ($vm:expr, $cx:expr, $gc:expr) => {
        $cx.input.str($vm, $gc.nogc()).expect("decode input")
    };
}

fn decode_value<'a, 's, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<'s, R>,
    p: &Program,
    mut ty: &Ty,
    gc: GcScope<'a, 's>,
) -> Result<Value<'a>> {
    vm.charge(1)?;
    loop {
        return match ty {
            Ty::Int => Ok(Value::int(cx.reader.int(src!(vm, cx, gc), &mut cx.pos)?)),
            Ty::Float => Ok(Value::float(cx.reader.float(src!(vm, cx, gc), &mut cx.pos)?)),
            Ty::Bool => Ok(Value::bool(cx.reader.bool(src!(vm, cx, gc), &mut cx.pos)?)),
            Ty::Str => {
                let span = cx.reader.str(src!(vm, cx, gc), &mut cx.pos, &mut cx.scratch)?;
                let input = cx.input.get(vm, gc.nogc()).unbind();
                vm.new_str_from_span(input, span, &cx.scratch, gc)
            }
            Ty::Option(inner) => {
                if cx.reader.null(src!(vm, cx, gc), &mut cx.pos)? {
                    Ok(Value::NONE)
                } else {
                    ty = inner;
                    continue;
                }
            }
            Ty::List(item) => decode_list(vm, cx, p, item, gc),
            Ty::Map(value) => decode_map(vm, cx, p, value, gc),
            Ty::Struct(id) => match p.struct_method(*id, DECODE_METHOD) {
                Some(func) => decode_callback(vm, cx, func, gc),
                None => {
                    let fields = &p.structs[*id as usize].fields;
                    decode_record(vm, cx, p, fields.iter().map(|f| (f.name.as_str(), &f.ty)), Record::Struct(*id), gc)
                }
            },
            Ty::Enum(id) => match p.enum_method(*id, DECODE_METHOD) {
                Some(func) => decode_callback(vm, cx, func, gc),
                None => decode_enum(vm, cx, p, *id, gc),
            },
        };
    }
}

#[inline(never)]
fn decode_list<'a, 's, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<'s, R>,
    p: &Program,
    item: &Ty,
    mut gc: GcScope<'a, 's>,
) -> Result<Value<'a>> {
    vm.check_stack()?;
    cx.reader.open_list(src!(vm, cx, gc), &mut cx.pos)?;
    let mark = vm.root_mark();
    while cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {
        let v = decode_value(vm, cx, p, item, gc.reborrow())?.unbind();
        vm.push_root(v, gc.reborrow())?;
    }
    vm.new_list_from_roots(mark, gc)
}

#[inline(never)]
fn decode_map<'a, 's, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<'s, R>,
    p: &Program,
    value: &Ty,
    mut gc: GcScope<'a, 's>,
) -> Result<Value<'a>> {
    vm.check_stack()?;
    cx.reader.open_map(src!(vm, cx, gc), &mut cx.pos)?;
    let mark = vm.root_mark();
    while let Some(span) = cx.reader.next_key(src!(vm, cx, gc), &mut cx.pos, &mut cx.scratch)? {
        let input = cx.input.get(vm, gc.nogc()).unbind();
        let key = vm.new_str_from_span(input, span, &cx.scratch, gc.reborrow())?.unbind();
        vm.push_root(key, gc.reborrow())?;
        let v = decode_value(vm, cx, p, value, gc.reborrow())?.unbind();
        vm.push_root(v, gc.reborrow())?;
    }
    vm.new_map_from_roots(mark, gc)
}

/// Decodes a map of named `fields` into a struct or variant.
#[inline(never)]
fn decode_record<'a, 's, 'p, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<'s, R>,
    p: &'p Program,
    fields: impl Iterator<Item = (&'p str, &'p Ty)> + Clone,
    record: Record,
    mut gc: GcScope<'a, 's>,
) -> Result<Value<'a>> {
    vm.check_stack()?;
    cx.reader.open_map(src!(vm, cx, gc), &mut cx.pos)?;
    let mark = vm.root_mark();
    let n = fields.clone().count();
    for _ in 0..n {
        vm.push_root(Value::UNIT, gc.reborrow())?;
    }
    let mut hint = 0;
    while let Some(span) = cx.reader.next_key(src!(vm, cx, gc), &mut cx.pos, &mut cx.scratch)? {
        let key = span.resolve(src!(vm, cx, gc), &cx.scratch);
        match find_field(fields.clone().map(|(name, _)| name), key, hint) {
            Some(i) => {
                let ty = fields.clone().nth(i).expect("field").1;
                let v = decode_value(vm, cx, p, ty, gc.reborrow())?.unbind();
                vm.set_root(mark + i, v);
                hint = i + 1;
            }
            None => skip_value(vm, cx, gc.nogc())?,
        }
    }
    for (i, (name, ty)) in fields.enumerate() {
        if vm.root(mark + i, gc.nogc()).is_unit() {
            if !matches!(ty, Ty::Option(_)) {
                return Err(Fail::error(format!("missing field `{name}`")));
            }
            vm.set_root(mark + i, Value::NONE);
        }
    }
    vm.new_record_from_roots(mark, record, gc)
}

/// Skips the value of an unknown key.
#[cold]
#[inline(never)]
fn skip_value<'s, R: FormatReader>(vm: &mut Vm, cx: &mut DecodeCx<'s, R>, gc: NoGcScope<'_, 's>) -> Result<()> {
    let (reader, pos) = (cx.reader, &mut cx.pos);
    vm.with_input_and_fuel(cx.input, gc, |src, fuel| reader.skip(src, pos, fuel))?
}

#[inline(never)]
fn decode_enum<'a, 's, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<'s, R>,
    p: &Program,
    id: u16,
    mut gc: GcScope<'a, 's>,
) -> Result<Value<'a>> {
    let def = &p.enums[id as usize];
    let (span, has_payload) = cx.reader.open_variant(src!(vm, cx, gc), &mut cx.pos, &mut cx.scratch)?;
    let name = span.resolve(src!(vm, cx, gc), &cx.scratch);
    let variant =
        def.variants.iter().find(|v| v.name == name).ok_or_else(|| Fail::error(format!("unknown variant `{name}` of {}", def.name)))?;
    if variant.payload.is_empty() == has_payload {
        return Err(Fail::error(format!("variant `{}` payload mismatch", variant.name)));
    }
    if !has_payload {
        return Ok(Value::from_raw(RawValue::Tag(variant.tag), gc.into_nogc()));
    }
    let v = match &variant.field_names {
        Some(names) => {
            decode_record(vm, cx, p, names.iter().map(String::as_str).zip(&variant.payload), Record::Variant(variant.tag), gc.reborrow())?
                .unbind()
        }
        None => {
            vm.check_stack()?;
            cx.reader.open_list(src!(vm, cx, gc), &mut cx.pos)?;
            let mark = vm.root_mark();
            for ty in &variant.payload {
                if !cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {
                    return Err(Fail::error("variant payload is too short"));
                }
                let v = decode_value(vm, cx, p, ty, gc.reborrow())?.unbind();
                vm.push_root(v, gc.reborrow())?;
            }
            if cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {
                return Err(Fail::error("variant payload is too long"));
            }
            vm.new_record_from_roots(mark, Record::Variant(variant.tag), gc.reborrow())?.unbind()
        }
    };
    // Closing the variant only scans, so `v` needs no root.
    cx.reader.close_variant(src!(vm, cx, gc), &mut cx.pos)?;
    Ok(v.bind(gc.into_nogc()))
}

/// Runs a custom `decode` method with the input position in the `Decoder`.
#[cold]
#[inline(never)]
fn decode_callback<'a, 's, R: FormatReader>(
    vm: &mut Vm,
    cx: &mut DecodeCx<'s, R>,
    func: u16,
    mut gc: GcScope<'a, 's>,
) -> Result<Value<'a>> {
    let mut decoder = vm.root(cx.decoder, gc.nogc());
    if decoder.is_unit() {
        let input = cx.input.get(vm, gc.nogc()).raw();
        let obj = DecoderObj { input, pos: 0, scratch: String::new(), reader: cx.reader, walk: decode_entry::<R> };
        let created = vm.new_decoder(obj, gc.reborrow())?.unbind();
        vm.set_root(cx.decoder, created);
        decoder = created.bind(gc.nogc());
    }
    let d = vm.decoder_obj(decoder, gc.nogc())?;
    d.pos = cx.pos;
    d.scratch = std::mem::take(&mut cx.scratch);
    let result = vm.call_function(func, &[decoder.unbind()], gc.reborrow()).map(Value::unbind);
    let decoder = vm.root(cx.decoder, gc.nogc());
    let d = vm.decoder_obj(decoder, gc.nogc())?;
    cx.pos = d.pos;
    cx.scratch = std::mem::take(&mut d.scratch);
    Ok(result?.bind(gc.into_nogc()))
}

/// Decodes a nested value from a `Decoder` in Sand code.
fn decode_entry<R: FormatReader>(vm: &mut raw::Vm, decoder: RawValue, ty: u16) -> Result<RawValue> {
    enter(|gc| decode_nested::<R>(&mut Vm::new(vm), decoder, ty, gc).map(Value::raw))
}

fn decode_nested<'a, R: FormatReader>(vm: &mut Vm, decoder: RawValue, ty: u16, mut gc: GcScope<'a, '_>) -> Result<Value<'a>> {
    let decoder = Value::from_raw(decoder, gc.nogc());
    let d = vm.decoder_obj(decoder, gc.nogc())?;
    let input = Value::from_raw(d.input, gc.nogc()).unbind();
    let reader: &'static R = (d.reader as &dyn Any).downcast_ref().ok_or_else(|| Fail::error("decoder format mismatch"))?;
    let (pos, scratch) = (d.pos, std::mem::take(&mut d.scratch));
    let decoder = vm.push_root(decoder.unbind(), gc.reborrow())?;
    // The `Decoder` roots `input`; the walker reads it through its own root.
    let input = input.scope(vm, gc.reborrow())?;
    let mut cx = DecodeCx { reader, input, pos, scratch, decoder };
    let program = vm.program().clone();
    let ty = program.types.get(ty as usize).ok_or_else(|| type_error("type id"))?;
    let result = decode_value(vm, &mut cx, &program, ty, gc.reborrow()).map(Value::unbind);
    let d = vm.decoder_obj(vm.root(decoder, gc.nogc()), gc.nogc())?;
    d.pos = cx.pos;
    d.scratch = cx.scratch;
    Ok(result?.bind(gc.into_nogc()))
}

/// Runs `f` on the input of the `Decoder` in argument 0.
fn with_decoder<T>(
    vm: &mut Vm,
    args: Args,
    gc: NoGcScope<'_, '_>,
    f: impl FnOnce(&dyn FormatReader, &str, &mut usize, &mut String, &mut u64) -> Result<T>,
) -> Result<T> {
    let decoder = vm.arg(args, 0, gc);
    vm.with_decoder_input(decoder, gc, f)
}

/// Runs `f` on the `Decoder` in argument 0 and converts its span to a string.
fn decoder_text<'a>(
    vm: &mut Vm,
    args: Args,
    gc: GcScope<'a, '_>,
    f: impl FnOnce(&dyn FormatReader, &str, &mut usize, &mut String) -> Result<StrSpan>,
) -> Result<Value<'a>> {
    let span = with_decoder(vm, args, gc.nogc(), |r, src, pos, scratch, _| f(r, src, pos, scratch))?;
    let decoder = vm.arg(args, 0, gc.nogc()).unbind();
    vm.decoder_span_to_str(decoder, span, gc)
}

/// The `json`, `enc`, `dec`, and `vm` native modules.
pub fn core_module() -> NativeModule {
    static JSON: Json = Json;
    let mut m = NativeModule::default();
    m.function("json.encode", |vm, args, gc| {
        let v = vm.arg(args, 0, gc.nogc()).unbind();
        encode_with(vm, JsonWriter::default(), v, gc)
    });
    m.function("json.decode", |vm, args, gc| {
        let (text, ty) = (vm.arg(args, 0, gc.nogc()).unbind(), int_arg(vm, args, 1, gc.nogc())? as u16);
        decode_with(vm, &JSON, text, ty, gc)
    });
    m.function("vm.fail", |vm, args, gc| Err(Fail::error(vm.str(&vm.arg(args, 0, gc.nogc()), gc.nogc())?.to_string())));
    m.function("vm.collect", |vm, _, gc| {
        vm.collect(gc);
        Ok(Value::UNIT)
    });

    m.function("enc.none", |vm, args, gc| encoder_event(vm, args, Event::None, gc));
    m.function("enc.close_map", |vm, args, gc| encoder_event(vm, args, Event::CloseMap, gc));
    m.function("enc.close_list", |vm, args, gc| encoder_event(vm, args, Event::CloseList, gc));
    m.function("enc.close_variant", |vm, args, gc| encoder_event(vm, args, Event::CloseVariant, gc));
    m.function("enc.open_map", |vm, args, gc| {
        let n = int_arg(vm, args, 1, gc.nogc())? as usize;
        encoder_event(vm, args, Event::OpenMap(n), gc)
    });
    m.function("enc.open_list", |vm, args, gc| {
        let n = int_arg(vm, args, 1, gc.nogc())? as usize;
        encoder_event(vm, args, Event::OpenList(n), gc)
    });
    m.function("enc.int", |vm, args, gc| {
        let i = int_arg(vm, args, 1, gc.nogc())?;
        encoder_event(vm, args, Event::Int(i), gc)
    });
    m.function("enc.float", |vm, args, gc| match vm.arg(args, 1, gc.nogc()).as_float() {
        Some(f) => encoder_event(vm, args, Event::Float(f), gc),
        None => Err(type_error("float")),
    });
    m.function("enc.bool", |vm, args, gc| match vm.arg(args, 1, gc.nogc()).as_bool() {
        Some(b) => encoder_event(vm, args, Event::Bool(b), gc),
        None => Err(type_error("bool")),
    });
    m.function("enc.str", |vm, args, gc| encoder_text(vm, args, |s| Event::Str(s), gc));
    m.function("enc.key", |vm, args, gc| encoder_text(vm, args, |s| Event::Key(s), gc));
    m.function("enc.tag", |vm, args, gc| encoder_text(vm, args, |s| Event::Tag(s), gc));
    m.function("enc.open_variant", |vm, args, gc| encoder_text(vm, args, |s| Event::OpenVariant(s), gc));
    m.function("enc.value", |vm, args, mut gc| {
        let (encoder, v) = (vm.arg(args, 0, gc.nogc()).unbind(), vm.arg(args, 1, gc.nogc()).unbind());
        vm.run_encode_walk(encoder, v, gc.reborrow())?;
        Ok(Value::UNIT)
    });

    m.function("dec.int", |vm, args, gc| Ok(Value::int(with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.int(src, pos))?)));
    m.function("dec.float", |vm, args, gc| Ok(Value::float(with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.float(src, pos))?)));
    m.function("dec.bool", |vm, args, gc| Ok(Value::bool(with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.bool(src, pos))?)));
    m.function("dec.null", |vm, args, gc| Ok(Value::bool(with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.null(src, pos))?)));
    m.function("dec.str", |vm, args, gc| decoder_text(vm, args, gc, |r, src, pos, scratch| r.str(src, pos, scratch)));
    m.function("dec.open_map", |vm, args, gc| {
        with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.open_map(src, pos)).map(|_| Value::UNIT)
    });
    m.function("dec.open_list", |vm, args, gc| {
        with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.open_list(src, pos)).map(|_| Value::UNIT)
    });
    m.function("dec.next_item", |vm, args, gc| {
        Ok(Value::bool(with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.next_item(src, pos))?))
    });
    m.function("dec.next_key", |vm, args, gc| {
        match with_decoder(vm, args, gc.nogc(), |r, src, pos, scratch, _| r.next_key(src, pos, scratch))? {
            Some(span) => {
                let decoder = vm.arg(args, 0, gc.nogc()).unbind();
                vm.decoder_span_to_str(decoder, span, gc)
            }
            None => Ok(Value::NONE),
        }
    });
    m.function("dec.open_variant", |vm, args, gc| {
        decoder_text(vm, args, gc, |r, src, pos, scratch| Ok(r.open_variant(src, pos, scratch)?.0))
    });
    m.function("dec.close_variant", |vm, args, gc| {
        with_decoder(vm, args, gc.nogc(), |r, src, pos, _, _| r.close_variant(src, pos)).map(|_| Value::UNIT)
    });
    m.function("dec.skip", |vm, args, gc| {
        with_decoder(vm, args, gc.nogc(), |r, src, pos, _, fuel| r.skip(src, pos, fuel)).map(|_| Value::UNIT)
    });
    m.function("dec.value", |vm, args, gc| {
        let decoder = vm.arg(args, 0, gc.nogc()).unbind();
        let ty = int_arg(vm, args, 1, gc.nogc())? as u16;
        vm.run_decode_walk(decoder, ty, gc)
    });
    m
}
