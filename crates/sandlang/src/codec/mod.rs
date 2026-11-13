//! The `Encoder` and `Decoder` of the prelude traits `Encode` and `Decode`.
//!
//! An `Encoder` writes the data model of [`Value`](crate::value::Value) to a
//! format, and a `Decoder` reads it from a format. Each format implements
//! [`EncodeFormat`] or [`DecodeFormat`]: JSON text and `json.Json` values in
//! the `json` module, and host [`Value`](crate::value::Value)s in
//! [`value_format`]. Sand code drives both objects: the prelude implements
//! the traits for built-in types, and the compiler synthesizes the
//! implementations of structs, enums, and tuples.
//!
//! A `Decoder` keeps the path of the part being decoded. A failed decode
//! returns at once through `?`, so the path at the top is the path of the
//! failure.

pub(crate) mod fast;
pub(crate) mod value_format;

use crate::native::convert::HostError;
use crate::native::ty::{Param, VmVal};
use crate::native::{NativeModule, NativeObject};
use crate::stdlib::core;
use crate::syntax::lexer::is_ident;
use crate::value::Value;
use crate::vm::value::{Obj, VmValue};
use crate::vm::{Fail, Vm, internal};
use fast::SchemaId;
use std::any::Any;
use std::borrow::Cow;
use std::fmt::Write;

/// The maximum nesting depth of encoded and decoded values. A deeper value,
/// such as a cyclic one, fails with a limit error.
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// The longest key text that an error message quotes.
pub(crate) const QUOTED_KEY: usize = 64;

/// Bytes of a format's buffers that the heap counts before it checks the
/// memory limit again.
const ACCOUNT_STEP: usize = 4096;

/// The kind of a sequence in the data model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqKind {
    List,
    Set,
    /// A tuple, or the positional payload of a variant, with this many elements.
    Tuple(usize),
}

/// A destination format for `Encode`.
///
/// Each method checks the memory limit before it changes the format's
/// state, so that the engine can run a native function again after a
/// garbage collection (SPEC §16.3). The caller charges a fuel unit for each
/// value; a method charges only for the bytes that it writes.
pub trait EncodeFormat: Any + Send {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail>;
    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail>;
    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail>;
    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail>;
    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail>;
    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail>;
    fn bytes(&mut self, vm: &mut Vm, b: &[u8]) -> Result<(), Fail>;
    /// Writes a native value type as its qualified type name and its text.
    fn native(&mut self, vm: &mut Vm, type_name: &str, text: &str) -> Result<(), Fail>;
    fn range(&mut self, vm: &mut Vm, start: i64, end: i64, inclusive: bool) -> Result<(), Fail>;
    fn begin_seq(&mut self, vm: &mut Vm, kind: SeqKind, len: usize) -> Result<(), Fail>;
    fn begin_map(&mut self, vm: &mut Vm, len: usize) -> Result<(), Fail>;
    /// Opens the struct with runtime definition `def`. A call of
    /// [`EncodeFormat::field`] comes before each field.
    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail>;
    /// Starts field `index` of the innermost struct or named variant payload.
    fn field(&mut self, vm: &mut Vm, index: usize) -> Result<(), Fail>;
    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail>;
    /// Opens the payload of `variant`: positional elements, or fields that
    /// [`EncodeFormat::field`] starts.
    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail>;
    /// Opens `Some`, whose payload follows.
    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail>;
    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail>;
    /// Closes the innermost sequence, map, struct, variant, or `Some`.
    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail>;
    /// Writes the `json.Json` value `json`.
    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail>;
    /// Writes the host value `value`, which has the data model of the format.
    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail>;
    /// Writes `value`, whose closed type has the fast-path schema `schema`,
    /// as the synthesized `Encode` implementations would. Formats keep this
    /// default, which Rust compiles for each format.
    fn write_fast(&mut self, vm: &mut Vm, value: VmValue, schema: SchemaId) -> Result<(), Fail> {
        fast::encode(vm, self, value, schema)
    }
    /// Keeps the state of the format before a value, for
    /// [`EncodeFormat::restore`].
    fn save(&mut self);
    /// Returns to the state that [`EncodeFormat::save`] kept, after a write
    /// of one value failed before it ended, so that the write can run again.
    fn restore(&mut self);
    /// Bytes of Rust memory that the format holds and that the heap counts
    /// (see [`Budget`]).
    fn heap_size(&self) -> usize;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// The bytes of a format's Rust buffers that the heap counts.
#[derive(Default)]
pub struct Budget {
    counted: usize,
}

impl Budget {
    /// Counts `needed` bytes against the memory limit before a format grows
    /// to that size. Call it before any change to the format's state.
    #[inline(always)]
    pub fn grow(&mut self, vm: &mut Vm, needed: usize) -> Result<(), Fail> {
        if needed <= self.counted {
            return Ok(());
        }
        self.grow_counted(vm, needed)
    }

    #[inline(never)]
    fn grow_counted(&mut self, vm: &mut Vm, needed: usize) -> Result<(), Fail> {
        let after = needed.saturating_add(ACCOUNT_STEP - 1) / ACCOUNT_STEP * ACCOUNT_STEP;
        vm.check_memory(after - self.counted)?;
        vm.heap.account(self.counted, after);
        self.counted = after;
        Ok(())
    }

    pub fn counted(&self) -> usize {
        self.counted
    }

    /// Stops counting the buffers, which the format no longer holds, and
    /// returns the bytes that the heap counted for them. The caller passes
    /// them to [`Heap::account`](crate::vm::heap::Heap::account).
    pub fn release(&mut self) -> usize {
        std::mem::take(&mut self.counted)
    }
}

/// A failure to decode.
#[derive(Debug)]
pub enum DecodeFail {
    /// The input does not match the target type. The decoder adds the path.
    Mismatch(String),
    /// The target type cannot be decoded from the format, such as a tuple
    /// map key from JSON text. The decoder adds the path.
    Invalid(String),
    /// The input is not valid in the format, with its own location.
    Syntax(String),
    /// A limit, or another failure that ends the run.
    Fail(Fail),
}

impl From<Fail> for DecodeFail {
    fn from(f: Fail) -> Self {
        DecodeFail::Fail(f)
    }
}

pub type DecodeResult<T> = Result<T, DecodeFail>;

/// A source format for `Decode`.
///
/// After [`DecodeFormat::next_entry`] returns a key, the format reads the
/// map key with the next operations, until [`DecodeFormat::map_value`]. The
/// caller charges a fuel unit for each value, and the fuel of the input that
/// the format read (see [`DecodeFormat::take_input_fuel`]).
pub trait DecodeFormat: Any + Send {
    fn unit(&mut self, vm: &mut Vm) -> DecodeResult<()>;
    fn bool(&mut self, vm: &mut Vm) -> DecodeResult<bool>;
    fn int(&mut self, vm: &mut Vm) -> DecodeResult<i64>;
    fn float(&mut self, vm: &mut Vm) -> DecodeResult<f64>;
    fn duration(&mut self, vm: &mut Vm) -> DecodeResult<i64>;
    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue>;
    fn bytes(&mut self, vm: &mut Vm) -> DecodeResult<VmValue>;
    /// Reads the text of a value of the native value type `name`.
    fn native_text(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<String>;
    fn range(&mut self, vm: &mut Vm) -> DecodeResult<(i64, i64, bool)>;
    /// Returns `true` if a `Some` payload follows, or `false` for `None`.
    fn some(&mut self, vm: &mut Vm) -> DecodeResult<bool>;
    fn begin_seq(&mut self, vm: &mut Vm, kind: SeqKind) -> DecodeResult<()>;
    /// Returns `true` if the innermost sequence has another element.
    fn next_item(&mut self, vm: &mut Vm) -> DecodeResult<bool>;
    fn begin_map(&mut self, vm: &mut Vm) -> DecodeResult<()>;
    /// Returns the text of the next key of the innermost map for the path,
    /// or `None` after the last entry.
    fn next_entry(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>>;
    /// Ends the key of the current map entry.
    fn map_value(&mut self, vm: &mut Vm) -> DecodeResult<()>;
    /// Opens a struct, or the named payload of a variant, whose type is
    /// `name` for messages.
    fn begin_fields(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()>;
    /// Returns the name of the next field, or `None` after the last.
    fn next_field(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>>;
    /// Skips the value of an unknown field.
    fn skip(&mut self, vm: &mut Vm) -> DecodeResult<()>;
    /// Reads a variant of the enum `name`. Returns its name, and `true` if a
    /// payload follows.
    fn variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)>;
    /// Closes the variant of the enum `name` that [`DecodeFormat::variant`]
    /// opened with a payload.
    fn end_variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()>;
    /// Reads a `json.Json` value.
    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue>;
    /// Reads a host value in the data model of the format.
    fn value(&mut self, vm: &mut Vm) -> DecodeResult<Value>;
    /// Checks that no input follows the decoded value.
    fn finish(&mut self, vm: &mut Vm) -> DecodeResult<()>;
    /// Reads a value of the closed type whose fast-path schema is `schema`,
    /// as its synthesized `Decode` implementation would. Formats keep this
    /// default, which Rust compiles for each format.
    fn read_fast(&mut self, vm: &mut Vm, path: &mut DecodePath, schema: SchemaId) -> DecodeResult<VmValue> {
        fast::decode(vm, self, &mut path.0, schema)
    }
    /// Returns the fuel of the input that the format read since the last
    /// call, such as a unit for each 8 bytes of text.
    fn take_input_fuel(&mut self) -> u64 {
        0
    }
    /// Keeps the state of the format before a value, for
    /// [`DecodeFormat::restore`].
    fn save(&mut self);
    /// Returns to the state that [`DecodeFormat::save`] kept, after a read
    /// of one value failed before it ended, so that the read can run again.
    fn restore(&mut self);
    /// The message for a tuple or positional payload without `len` elements.
    fn length_mismatch(&self, len: usize) -> String {
        format!("expected array of length {len}")
    }
    /// Bytes of Rust memory that the format holds and that the heap counts.
    fn heap_size(&self) -> usize;
}

/// A destination of `Encode`, created by a format such as `json.encode`.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Encoder {
    format: Option<Box<dyn EncodeFormat>>,
}

/// A source of `Decode`, created by a format such as `json.decode`.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Decoder {
    format: Option<Box<dyn DecodeFormat>>,
    path: DecodePath,
    /// The message of the last error that the decoder made, which already
    /// has its path.
    last_error: Option<String>,
}

impl NativeObject for Encoder {
    fn size(&self) -> usize {
        64 + self.format.as_ref().map_or(0, |f| f.heap_size())
    }
}

impl NativeObject for Decoder {
    fn size(&self) -> usize {
        64 + self.format.as_ref().map_or(0, |f| f.heap_size())
    }
}

/// The open containers of a [`Decoder`], which give the path of the part
/// that it reads, such as `$.items[2].price`.
#[derive(Default)]
pub struct DecodePath(Vec<Frame>);

/// An open container of a [`Decoder`].
#[derive(Clone, Debug)]
enum Frame {
    /// A sequence and the index of its current element.
    Seq { index: Option<usize>, len: Option<usize> },
    /// A map, the text of the key of its current value, and the text of the
    /// key being read.
    Map { key: Option<String>, pending: Option<String> },
    /// The fields of a struct (`variant == false`, `id` is the definition) or
    /// of a named variant payload (`id` is the variant), the current field,
    /// and the index to try first for the next key.
    Fields { id: u32, variant: bool, field: Option<usize>, guess: usize },
    /// A variant payload.
    Variant { id: u32 },
}

impl Encoder {
    /// Returns a new encoder for `format`.
    pub fn new(format: Box<dyn EncodeFormat>) -> Encoder {
        Encoder { format: Some(format) }
    }

    /// Returns the format of `e` if it has type `F`.
    pub fn format_mut<F: EncodeFormat>(&mut self) -> Option<&mut F> {
        self.format.as_deref_mut()?.as_any_mut().downcast_mut::<F>()
    }

    /// Returns the value that a host `Value` encoder holds.
    pub(crate) fn encoded_value(&self) -> Option<&std::sync::Arc<Value>> {
        let format: &dyn Any = self.format.as_deref()?;
        format.downcast_ref::<value_format::ValueEncoder>()?.encoded()
    }
}

impl Decoder {
    /// Returns a new decoder for `format`.
    pub fn new(format: Box<dyn DecodeFormat>) -> Decoder {
        Decoder { format: Some(format), path: DecodePath::default(), last_error: None }
    }

    /// Returns a decoder that reads the host value `value`. The value can
    /// also have the JSON form of the target type, such as a Base64
    /// `Value::Str` for `bytes`.
    pub fn from_value(value: Value) -> Decoder {
        Decoder::new(Box::new(value_format::ValueDecoder::new(value)))
    }
}

/// Runs `op` on the format of `e`.
pub fn with_encoder<R>(
    vm: &mut Vm,
    e: VmVal<Encoder>,
    op: impl FnOnce(&mut dyn EncodeFormat, &mut Vm) -> Result<R, Fail>,
) -> Result<R, Fail> {
    let Some(mut format) = e.get_mut(&mut vm.heap)?.format.take() else { return internal("the encoder is in use") };
    let r = op(&mut *format, vm);
    e.get_mut(&mut vm.heap)?.format = Some(format);
    r
}

/// Charges a fuel unit for a value, and runs `op`, which writes the value or
/// opens it, on the format of `e`.
fn write_part<R>(vm: &mut Vm, e: VmVal<Encoder>, op: impl FnOnce(&mut dyn EncodeFormat, &mut Vm) -> Result<R, Fail>) -> Result<R, Fail> {
    vm.charge(1)?;
    with_encoder(vm, e, op)
}

/// Runs `op`, which writes one whole value to `e`. If a memory check fails,
/// the format returns to its state before `op`, so that the native function
/// can run again.
pub(crate) fn write_whole(
    vm: &mut Vm,
    e: VmVal<Encoder>,
    op: impl FnOnce(&mut dyn EncodeFormat, &mut Vm) -> Result<(), Fail>,
) -> Result<(), Fail> {
    with_encoder(vm, e, |f, vm| {
        f.save();
        let r = op(f, vm);
        if r.as_ref().is_err_and(|e| e.as_limit().is_some()) {
            f.restore();
        }
        r
    })
}

/// Reads a value of the closed type whose fast-path schema is `schema` with
/// the general walker, for a format that overrides
/// [`DecodeFormat::read_fast`].
pub(crate) fn read_fast_general<F: DecodeFormat + ?Sized>(
    vm: &mut Vm,
    f: &mut F,
    path: &mut DecodePath,
    schema: SchemaId,
) -> DecodeResult<VmValue> {
    fast::decode(vm, f, &mut path.0, schema)
}

/// Charges a fuel unit for a value, and runs `op`, which reads the value or
/// opens it, on `d` and its format (see [`with_decoder`]).
fn read_part<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut DecodePath, &mut Vm) -> DecodeResult<R>,
) -> Result<R, HostError> {
    vm.charge(1)?;
    with_decoder(vm, d, op)
}

/// Runs `op` on `d` and its format, and charges the fuel of the input that
/// the format read. An error gets the decoder's path.
fn with_decoder<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut DecodePath, &mut Vm) -> DecodeResult<R>,
) -> Result<R, HostError> {
    let obj = d.get_mut(&mut vm.heap)?;
    let Some(mut format) = obj.format.take() else { return Err(Fail::internal("the decoder is in use").into()) };
    let mut path = std::mem::take(&mut obj.path);
    let r = op(&mut *format, &mut path, vm);
    let r = match vm.charge(format.take_input_fuel()) {
        Ok(()) => r,
        Err(fail) => Err(fail.into()),
    };
    let error = match &r {
        Err(DecodeFail::Mismatch(m)) => Some(("parse", format!("{}: {m}", path_text(vm, &path.0)))),
        Err(DecodeFail::Invalid(m)) => Some(("invalid", format!("{}: {m}", path_text(vm, &path.0)))),
        Err(DecodeFail::Syntax(m)) => Some(("parse", m.clone())),
        _ => None,
    };
    let obj = d.get_mut(&mut vm.heap)?;
    obj.format = Some(format);
    obj.path = path;
    match (r, error) {
        (Ok(v), _) => Ok(v),
        (Err(DecodeFail::Fail(f)), _) => Err(f.into()),
        (Err(_), Some((kind, message))) => {
            obj.last_error = Some(message.clone());
            Err(HostError::new(kind, message))
        }
        (Err(_), None) => Err(Fail::internal("decode: lost error").into()),
    }
}

/// Formats the path of the innermost part of `frames`, such as
/// `$.items[2].price`.
fn path_text(vm: &Vm, frames: &[Frame]) -> String {
    let mut s = String::from("$");
    let key = |s: &mut String, k: &str| {
        let k = crate::stdlib::truncate(k, QUOTED_KEY);
        if is_ident(k) {
            s.push('.');
            s.push_str(k);
        } else {
            s.push('[');
            crate::stdlib::json::write_str(s, k);
            s.push(']');
        }
    };
    for f in frames {
        match f {
            Frame::Seq { index: Some(i), .. } => _ = write!(s, "[{i}]"),
            Frame::Map { key: Some(k), .. } => key(&mut s, k),
            Frame::Fields { id, variant, field: Some(i), .. } => {
                key(&mut s, field_names(vm, *id, *variant).get(*i).map_or("?", String::as_str))
            }
            Frame::Variant { id } => key(&mut s, vm.prog.variants.get(*id as usize).map_or("?", |v| v.name.as_str())),
            _ => {}
        }
    }
    s
}

/// Returns the field names of struct definition `id`, or of the named
/// payload of variant `id`.
fn field_names(vm: &Vm, id: u32, variant: bool) -> &[String] {
    let names = if variant {
        vm.prog.variants.get(id as usize).and_then(|v| v.field_names.as_deref())
    } else {
        vm.prog.types.get(id as usize).map(|t| &t.fields[..])
    };
    names.unwrap_or(&[])
}

/// Returns the qualified name of runtime type definition `def`, as messages
/// show it.
fn type_name(vm: &Vm, def: u32) -> String {
    vm.prog.types.get(def as usize).map_or_else(String::new, |t| t.name.clone())
}

fn to_index(i: i64) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

/// Registers the methods of `Encoder` and `Decoder` on the prelude.
pub(crate) fn register(m: &mut NativeModule) {
    m.bind::<Encoder>();
    m.bind::<Decoder>();
    m.function("value_encoder", |vm: &mut Vm| vm.new_object(Encoder::new(Box::new(value_format::ValueEncoder::new()))));
    encoder_methods(m);
    decoder_methods(m);
}

type Enc = VmVal<Encoder>;
type Dec = VmVal<Decoder>;
type Decoded<T> = Result<T, HostError>;

fn encoder_methods(m: &mut NativeModule) {
    m.function("Encoder.write_unit", |vm: &mut Vm, e: Enc| write_part(vm, e, |f, vm| f.unit(vm)));
    m.function("Encoder.write_bool", |vm: &mut Vm, e: Enc, b: bool| write_part(vm, e, |f, vm| f.bool(vm, b)));
    m.function("Encoder.write_int", |vm: &mut Vm, e: Enc, i: i64| write_part(vm, e, |f, vm| f.int(vm, i)));
    m.function("Encoder.write_float", |vm: &mut Vm, e: Enc, x: f64| write_part(vm, e, |f, vm| f.float(vm, x)));
    m.function("Encoder.write_duration", |vm: &mut Vm, e: Enc, d: jiff::SignedDuration| {
        write_part(vm, e, |f, vm| f.duration(vm, d.as_nanos() as i64))
    });
    m.function("Encoder.write_str", |vm: &mut Vm, e: Enc, s: VmVal<String>| {
        let text = s.str(&vm.heap).to_string();
        write_part(vm, e, |f, vm| f.str(vm, &text))
    });
    m.function("Encoder.write_bytes", |vm: &mut Vm, e: Enc, b: VmVal<Vec<u8>>| {
        let bytes = b.bytes(&vm.heap).to_vec();
        write_part(vm, e, |f, vm| f.bytes(vm, &bytes))
    });
    m.function("Encoder.write_range", |vm: &mut Vm, e: Enc, r: VmVal<crate::native::ty::Range>| {
        let Obj::Range(start, end, inclusive) = *vm.heap.get(r.raw()) else { return internal("write_range: not a range") };
        write_part(vm, e, |f, vm| f.range(vm, start, end, inclusive))
    });
    m.function("Encoder.write_native", |vm: &mut Vm, e: Enc, x: Param<0>| {
        let Some((type_name, text)) = native_text(vm, x.raw()) else { return internal("write_native: not a native value") };
        write_part(vm, e, |f, vm| f.native(vm, &type_name, &text))
    });
    m.function("Encoder.begin_list", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, |f, vm| f.begin_seq(vm, SeqKind::List, to_index(len)))
    });
    m.function("Encoder.begin_set", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, |f, vm| f.begin_seq(vm, SeqKind::Set, to_index(len)))
    });
    m.function("Encoder.begin_tuple", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, |f, vm| f.begin_seq(vm, SeqKind::Tuple(to_index(len)), to_index(len)))
    });
    m.function("Encoder.begin_map", |vm: &mut Vm, e: Enc, len: i64| write_part(vm, e, |f, vm| f.begin_map(vm, to_index(len))));
    m.function("Encoder.begin_struct", |vm: &mut Vm, e: Enc, def: i64| write_part(vm, e, |f, vm| f.begin_struct(vm, def as u32)));
    m.function("Encoder.field", |vm: &mut Vm, e: Enc, i: i64| with_encoder(vm, e, |f, vm| f.field(vm, to_index(i))));
    m.function("Encoder.write_variant", |vm: &mut Vm, e: Enc, v: i64| write_part(vm, e, |f, vm| f.unit_variant(vm, v as u32)));
    m.function("Encoder.begin_variant", |vm: &mut Vm, e: Enc, v: i64| write_part(vm, e, |f, vm| f.begin_variant(vm, v as u32)));
    m.function("Encoder.begin_some", |vm: &mut Vm, e: Enc| write_part(vm, e, |f, vm| f.begin_some(vm)));
    m.function("Encoder.write_none", |vm: &mut Vm, e: Enc| write_part(vm, e, |f, vm| f.none(vm)));
    m.function("Encoder.end", |vm: &mut Vm, e: Enc| with_encoder(vm, e, |f, vm| f.end(vm)));
    m.function("Encoder.write_fast", |vm: &mut Vm, e: Enc, x: Param<0>, schema: i64| {
        with_encoder(vm, e, |f, vm| f.write_fast(vm, x.raw(), schema as u32))
    });
}

/// Reads a `json.Json` value from `d`.
pub(crate) fn read_json(vm: &mut Vm, d: VmVal<Decoder>) -> Result<VmValue, HostError> {
    with_decoder(vm, d, |f, _, vm| f.json(vm))
}

/// Returns the qualified type name and the text of the native value `v`.
pub(crate) fn native_text(vm: &Vm, v: VmValue) -> Option<(String, String)> {
    let native = vm.heap.get(v).value()?;
    let module = crate::value::qualified_name_module(native.module_name());
    Some((crate::value::qualified_name(module, native.type_name()), native.text()))
}

fn decoder_methods(m: &mut NativeModule) {
    m.function("Decoder.read_unit", |vm: &mut Vm, d: Dec| -> Decoded<()> { read_part(vm, d, |f, _, vm| f.unit(vm)) });
    m.function("Decoder.read_bool", |vm: &mut Vm, d: Dec| -> Decoded<bool> { read_part(vm, d, |f, _, vm| f.bool(vm)) });
    m.function("Decoder.read_int", |vm: &mut Vm, d: Dec| -> Decoded<i64> { read_part(vm, d, |f, _, vm| f.int(vm)) });
    m.function("Decoder.read_float", |vm: &mut Vm, d: Dec| -> Decoded<f64> { read_part(vm, d, |f, _, vm| f.float(vm)) });
    m.function("Decoder.read_duration", |vm: &mut Vm, d: Dec| -> Decoded<jiff::SignedDuration> {
        read_part(vm, d, |f, _, vm| f.duration(vm)).map(jiff::SignedDuration::from_nanos)
    });
    m.function("Decoder.read_str", |vm: &mut Vm, d: Dec| -> Decoded<VmVal<String>> {
        read_part(vm, d, |f, _, vm| f.str(vm)).map(VmVal::from_raw)
    });
    m.function("Decoder.read_bytes", |vm: &mut Vm, d: Dec| -> Decoded<VmVal<Vec<u8>>> {
        read_part(vm, d, |f, _, vm| f.bytes(vm)).map(VmVal::from_raw)
    });
    m.function("Decoder.read_range", |vm: &mut Vm, d: Dec| -> Decoded<VmVal<crate::native::ty::Range>> {
        let (start, end, inclusive) = read_part(vm, d, |f, _, vm| f.range(vm))?;
        Ok(VmVal::from_raw(vm.heap.alloc(Obj::Range(start, end, inclusive))))
    });
    m.function("Decoder.read_native", |vm: &mut Vm, d: Dec, def: i64| -> Decoded<Param<0>> {
        read_part(vm, d, |f, _, vm| read_native_value(f, vm, def as u32)).map(Param::from_raw)
    });
    m.function("Decoder.read_some", |vm: &mut Vm, d: Dec| -> Decoded<bool> { read_part(vm, d, |f, _, vm| f.some(vm)) });
    m.function("Decoder.begin_list", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_seq(f, &mut path.0, vm, SeqKind::List))
    });
    m.function("Decoder.begin_set", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_seq(f, &mut path.0, vm, SeqKind::Set))
    });
    m.function("Decoder.next_item", |vm: &mut Vm, d: Dec| -> Decoded<bool> {
        with_decoder(vm, d, |f, path, vm| dec_next_item(f, &mut path.0, vm))
    });
    m.function("Decoder.begin_tuple", |vm: &mut Vm, d: Dec, len: i64| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_seq(f, &mut path.0, vm, SeqKind::Tuple(to_index(len))))
    });
    m.function("Decoder.element", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, |f, path, vm| dec_element(f, &mut path.0, vm))
    });
    m.function("Decoder.end_tuple", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, |f, path, vm| dec_end_tuple(f, &mut path.0, vm))
    });
    m.function("Decoder.begin_map", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_map(f, &mut path.0, vm))
    });
    m.function("Decoder.next_entry", |vm: &mut Vm, d: Dec| -> Decoded<bool> {
        with_decoder(vm, d, |f, path, vm| dec_next_entry(f, &mut path.0, vm))
    });
    m.function("Decoder.map_value", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, |f, path, vm| dec_map_value(f, &mut path.0, vm))
    });
    m.function("Decoder.begin_struct", |vm: &mut Vm, d: Dec, def: i64| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_struct(f, &mut path.0, vm, def as u32))
    });
    m.function("Decoder.begin_fields", |vm: &mut Vm, d: Dec, variant: i64| -> Decoded<()> {
        with_decoder(vm, d, |f, path, vm| dec_begin_fields(f, &mut path.0, vm, variant as u32))
    });
    m.function("Decoder.field", |vm: &mut Vm, d: Dec| -> Decoded<i64> {
        with_decoder(vm, d, |f, path, vm| next_field(f, &mut path.0, vm))
    });
    m.function("Decoder.missing", |vm: &mut Vm, d: Dec, id: i64, variant: bool, index: i64| -> Result<core::Error, Fail> {
        let message = missing_message(vm, &d.get(&vm.heap)?.path.0, id as u32, variant, to_index(index));
        d.get_mut(&mut vm.heap)?.last_error = Some(message.clone());
        Ok(core::Error { message, kind: "parse".into() })
    });
    m.function("Decoder.variant", |vm: &mut Vm, d: Dec, first: i64, count: i64| -> Decoded<i64> {
        read_part(vm, d, |f, path, vm| read_variant(f, &mut path.0, vm, first as u32, to_index(count)))
    });
    m.function("Decoder.end_variant", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, |f, path, vm| dec_end_variant(f, &mut path.0, vm))
    });
    m.function("Decoder.read_fast", |vm: &mut Vm, d: Dec, schema: i64| -> Decoded<Param<0>> {
        with_decoder(vm, d, |f, path, vm| f.read_fast(vm, path, schema as u32)).map(Param::from_raw)
    });
    m.function("Decoder.end_input", |vm: &mut Vm, d: Dec| -> Decoded<()> { with_decoder(vm, d, |f, _, vm| f.finish(vm)) });
    m.function("Decoder.locate", |vm: &mut Vm, d: Dec, e: core::Error| -> Result<core::Error, Fail> {
        let obj = d.get(&vm.heap)?;
        if obj.last_error.as_deref() == Some(e.message.as_str()) {
            return Ok(e);
        }
        Ok(core::Error { message: format!("{}: {}", path_text(vm, &obj.path.0), e.message), kind: e.kind })
    });
}

/// Reads the text of a value of the native value type with runtime
/// definition `def`, and parses it.
fn read_native_value<F: DecodeFormat + ?Sized>(f: &mut F, vm: &mut Vm, def: u32) -> DecodeResult<VmValue> {
    let prog = vm.prog.clone();
    let Some(t) = prog.types.get(def as usize) else { return Err(Fail::internal("read_native: unknown type").into()) };
    let Some(parse) = t.parse else { return Err(Fail::internal("read_native: not a value type").into()) };
    let text = f.native_text(vm, &t.name)?;
    vm.charge_bytes(text.len())?;
    let v = parse(&text).map_err(DecodeFail::Mismatch)?;
    Ok(vm.heap.alloc(v))
}

/// Opens a list, a set, or a tuple.
fn dec_begin_seq(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm, kind: SeqKind) -> DecodeResult<()> {
    f.begin_seq(vm, kind)?;
    let len = match kind {
        SeqKind::Tuple(n) => Some(n),
        SeqKind::List | SeqKind::Set => None,
    };
    frames.push(Frame::Seq { index: None, len });
    Ok(())
}

/// Moves to the next element of the innermost list or set. Returns `false`
/// after the last.
fn dec_next_item(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<bool> {
    let more = f.next_item(vm)?;
    match (frames.last_mut(), more) {
        (Some(Frame::Seq { index, .. }), true) => *index = Some(index.map_or(0, |i| i + 1)),
        (Some(Frame::Seq { .. }), false) => _ = frames.pop(),
        _ => return Err(Fail::internal("next_item: no open sequence").into()),
    }
    Ok(more)
}

/// Moves to the next element of the innermost tuple, which must have one.
fn dec_element(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<()> {
    let more = f.next_item(vm)?;
    let Some(Frame::Seq { index, len: Some(len) }) = frames.last_mut() else {
        return Err(Fail::internal("element: no open tuple").into());
    };
    if !more {
        let len = *len;
        frames.pop();
        return Err(DecodeFail::Mismatch(f.length_mismatch(len)));
    }
    *index = Some(index.map_or(0, |i| i + 1));
    Ok(())
}

/// Closes the innermost tuple, which must have no more elements.
fn dec_end_tuple(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<()> {
    let Some(Frame::Seq { len: Some(len), .. }) = frames.pop() else {
        return Err(Fail::internal("end_tuple: no open tuple").into());
    };
    if f.next_item(vm)? {
        return Err(DecodeFail::Mismatch(f.length_mismatch(len)));
    }
    Ok(())
}

fn dec_begin_map(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<()> {
    f.begin_map(vm)?;
    frames.push(Frame::Map { key: None, pending: None });
    Ok(())
}

/// Moves to the key of the next entry of the innermost map. Returns `false`
/// after the last entry.
fn dec_next_entry(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<bool> {
    let next = f.next_entry(vm)?.map(Cow::into_owned);
    let Some(Frame::Map { key, pending }) = frames.last_mut() else { return Err(Fail::internal("next_entry: no open map").into()) };
    // The key reads at the path of the map.
    *key = None;
    *pending = next;
    if pending.is_none() {
        frames.pop();
        return Ok(false);
    }
    Ok(true)
}

/// Moves from the key to the value of the current map entry.
fn dec_map_value(f: &mut dyn DecodeFormat, frames: &mut [Frame], vm: &mut Vm) -> DecodeResult<()> {
    f.map_value(vm)?;
    let Some(Frame::Map { key, pending }) = frames.last_mut() else { return Err(Fail::internal("map_value: no open map").into()) };
    *key = pending.take();
    Ok(())
}

/// Opens the struct with runtime definition `def`.
fn dec_begin_struct(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm, def: u32) -> DecodeResult<()> {
    let name = type_name(vm, def);
    f.begin_fields(vm, &name)?;
    frames.push(Frame::Fields { id: def, variant: false, field: None, guess: 0 });
    Ok(())
}

/// Opens the named payload of the variant with runtime id `variant`.
fn dec_begin_fields(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm, variant: u32) -> DecodeResult<()> {
    f.begin_fields(vm, "")?;
    frames.push(Frame::Fields { id: variant, variant: true, field: None, guess: 0 });
    Ok(())
}

/// Closes the innermost variant payload.
fn dec_end_variant(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<()> {
    let Some(Frame::Variant { id }) = frames.pop() else { return Err(Fail::internal("end_variant: no open variant").into()) };
    let name = vm.prog.variants.get(id as usize).map_or_else(String::new, |v| type_name(vm, v.def));
    f.end_variant(vm, &name)
}

/// The message of a missing field `index` of struct definition `id`, or of
/// the named payload of variant `id`, at the path of `frames`.
fn missing_message(vm: &Vm, frames: &[Frame], id: u32, variant: bool, index: usize) -> String {
    let name = field_names(vm, id, variant).get(index).cloned().unwrap_or_default();
    format!("{}: missing field `{name}`", path_text(vm, frames))
}

/// Reads the next field of the innermost struct or named payload. Returns
/// its index, or `-1` after the last field. Skips unknown fields.
fn next_field(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm) -> DecodeResult<i64> {
    let Some(&Frame::Fields { id, variant, guess, .. }) = frames.last() else { return Err(Fail::internal("field: no open struct").into()) };
    loop {
        let found = {
            let Some(key) = f.next_field(vm)? else {
                frames.pop();
                return Ok(-1);
            };
            let names = field_names(vm, id, variant);
            if names.get(guess).is_some_and(|n| *n == *key) {
                Some(guess)
            } else {
                vm.charge(1)?;
                let index = if variant {
                    vm.prog.variants.get(id as usize).map(|v| &v.by_name)
                } else {
                    vm.prog.types.get(id as usize).map(|t| &t.by_name)
                };
                index.and_then(|index| crate::vm::bytecode::field_index(names, index, &key))
            }
        };
        match found {
            Some(i) => {
                if let Some(Frame::Fields { field, guess, .. }) = frames.last_mut() {
                    *field = Some(i);
                    *guess = i + 1;
                }
                return Ok(i as i64);
            }
            None => f.skip(vm)?,
        }
    }
}

/// Reads a variant of the enum whose `count` variants have runtime ids from
/// `first`. Returns the index of the variant.
fn read_variant(f: &mut dyn DecodeFormat, frames: &mut Vec<Frame>, vm: &mut Vm, first: u32, count: usize) -> DecodeResult<i64> {
    let Some(def) = vm.prog.variants.get(first as usize).map(|v| v.def) else { return Err(Fail::internal("variant: unknown enum").into()) };
    let enum_name = type_name(vm, def);
    let (name, payload) = f.variant(vm, &enum_name)?;
    vm.charge(1 + (count / 16) as u64)?;
    let variants = &vm.prog.variants[first as usize..(first as usize + count).min(vm.prog.variants.len())];
    let Some(i) = variants.iter().position(|v| v.name == *name) else {
        let tag = crate::stdlib::truncate(&name, QUOTED_KEY).to_string();
        return Err(DecodeFail::Mismatch(format!("unknown variant `{tag}` of {enum_name}")));
    };
    let arity = variants[i].arity;
    if !payload {
        if arity > 0 {
            return Err(DecodeFail::Mismatch(format!("variant `{name}` of {enum_name} needs a payload")));
        }
        return Ok(i as i64);
    }
    frames.push(Frame::Variant { id: first + i as u32 });
    if arity == 0 {
        // A payload-less variant in object form has an empty payload.
        f.begin_seq(vm, SeqKind::Tuple(0))?;
        if f.next_item(vm)? {
            return Err(DecodeFail::Mismatch("expected array of length 0".into()));
        }
        frames.pop();
        f.end_variant(vm, &enum_name)?;
    }
    Ok(i as i64)
}

/// The Rust API of an `Encoder` for native implementations of `Encode`.
impl VmVal<Encoder> {
    /// Writes the text `s`.
    pub fn write_str(self, vm: &mut Vm, s: &str) -> Result<(), Fail> {
        write_part(vm, self, |f, vm| f.str(vm, s))
    }

    /// Writes `value`, which has the data model of the `Encoder`.
    pub fn write_value(self, vm: &mut Vm, value: &crate::value::Value) -> Result<(), Fail> {
        vm.charge(1)?;
        write_whole(vm, self, |f, vm| f.value(vm, value))
    }
}

/// The Rust API of a `Decoder` for native implementations of `Decode`.
impl VmVal<Decoder> {
    /// Reads a text.
    pub fn read_str(self, vm: &mut Vm) -> Result<String, HostError> {
        let v = read_part(vm, self, |f, _, vm| f.str(vm))?;
        Ok(vm.heap.str(v).to_string())
    }

    /// Reads a host value in the data model of the format.
    pub fn read_value(self, vm: &mut Vm) -> Result<Value, HostError> {
        read_part(vm, self, |f, _, vm| {
            f.save();
            let r = f.value(vm);
            if let Err(DecodeFail::Fail(fail)) = &r
                && fail.as_limit().is_some()
            {
                f.restore();
            }
            r
        })
    }

    /// Returns a decode error with the decoder's path and `message`.
    pub fn error(self, vm: &mut Vm, message: &str) -> Result<HostError, Fail> {
        let obj = self.get(&vm.heap)?;
        let message = format!("{}: {message}", path_text(vm, &obj.path.0));
        self.get_mut(&mut vm.heap)?.last_error = Some(message.clone());
        Ok(HostError::new("parse", message))
    }
}
