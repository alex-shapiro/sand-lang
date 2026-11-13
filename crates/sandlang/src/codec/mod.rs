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
//! failure. Both objects check that each call of an implementation writes or
//! reads exactly one value (`protocol.rs`).

pub(crate) mod fast;
mod protocol;
pub(crate) mod value_format;
pub(crate) mod value_input;

use crate::native::convert::{FromVmValue, HostError, IntoVmValue};
use crate::native::ty::{CodecDict, Param, VmVal};
use crate::native::{NativeModule, NativeObject};
use crate::stdlib::core;
use crate::syntax::lexer::is_ident;
use crate::types::traits;
use crate::value::Value;
use crate::vm::value::{Obj, ObjectValues, VALUE_SIZE, VmValue};
use crate::vm::{Fail, Vm, internal};
use compact_str::CompactString;
use fast::SchemaId;
use protocol::{DecodeOp, DecodeProtocol, EncodeProtocol, Ending, ErrorOrigin, LevelKind};
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
pub(crate) enum SeqKind {
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
pub(crate) trait EncodeFormat: Any + Send {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail>;
    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail>;
    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail>;
    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail>;
    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail>;
    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail>;
    /// Returns `true` if the format keeps Sand values, so that the
    /// `Encoder` lends it its values ([`EncodeFormat::swap_values`]) and
    /// writes a `str` with [`EncodeFormat::str_value`].
    fn keeps_values(&self) -> bool {
        false
    }
    /// Writes the Sand `str` `s`, for a format that keeps Sand values and
    /// can share `s`.
    fn str_value(&mut self, vm: &mut Vm, s: VmValue) -> Result<(), Fail> {
        let text = vm.heap.str(s).to_string();
        self.str(vm, &text)
    }
    /// Swaps the Sand values that the format keeps with `values`. The
    /// `Encoder` keeps the values, where the collector traces them, and
    /// lends them to a format that [`EncodeFormat::keeps_values`] for the
    /// length of each native call. The format holds no other Sand value
    /// between calls.
    fn swap_values(&mut self, _values: &mut ObjectValues) {}
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
}

/// The bytes of a format's Rust buffers that the heap counts.
#[derive(Default)]
pub(crate) struct Budget {
    counted: usize,
}

impl Budget {
    /// Counts `needed` bytes against the memory limit before a format grows
    /// to that size. Call it before any change to the format's state.
    #[inline(always)]
    pub(crate) fn grow(&mut self, vm: &mut Vm, needed: usize) -> Result<(), Fail> {
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

    pub(crate) fn counted(&self) -> usize {
        self.counted
    }

    /// Stops counting the buffers, which the format no longer holds, and
    /// returns the bytes that the heap counted for them. The caller passes
    /// them to [`Heap::account`](crate::vm::heap::Heap::account).
    pub(crate) fn release(&mut self) -> usize {
        std::mem::take(&mut self.counted)
    }
}

/// A failure to decode.
#[derive(Debug)]
pub(crate) enum DecodeFail {
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

impl DecodeFail {
    /// Returns the error of the failure at the path that `path` returns, or
    /// the failure that ends the run.
    fn into_error(self, path: impl FnOnce() -> String) -> Result<core::Error, Fail> {
        let (kind, message) = match self {
            DecodeFail::Mismatch(m) => ("parse", format!("{}: {m}", path())),
            DecodeFail::Invalid(m) => ("invalid", format!("{}: {m}", path())),
            DecodeFail::Syntax(m) => ("parse", m),
            DecodeFail::Fail(fail) => return Err(fail),
        };
        Ok(core::Error { message, kind: kind.into() })
    }
}

pub(crate) type DecodeResult<T> = Result<T, DecodeFail>;

/// A source format for `Decode`.
///
/// After [`DecodeFormat::next_entry`] returns a key, the format reads the
/// map key with the next operations, until [`DecodeFormat::map_value`]. The
/// caller charges a fuel unit for each value, and the fuel of the input that
/// the format read (see [`DecodeFormat::take_input_fuel`]).
pub(crate) trait DecodeFormat: Any + Send {
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
        fast::decode(vm, self, path, schema)
    }
    /// Returns the fuel of the input that the format read since the last
    /// call, such as a unit for each 8 bytes of text.
    fn take_input_fuel(&mut self) -> u64 {
        0
    }
    /// Keeps the state of the format before a value, and returns the index
    /// of the mark for [`DecodeFormat::rewind`].
    fn mark(&mut self) -> usize;
    /// Returns to the state of mark `mark`, after a read failed before it
    /// ended, so that the read can run again. The mark stays.
    fn rewind(&mut self, mark: usize);
    /// Forgets mark `mark` and the marks after it.
    fn release(&mut self, mark: usize);
    /// Returns to the start of the input, as a new format of the input
    /// would be.
    fn rewind_to_start(&mut self);
    /// Returns `true` if the format keeps Sand values, so that the
    /// `Decoder` lends it its values ([`DecodeFormat::swap_values`]).
    fn keeps_values(&self) -> bool {
        false
    }
    /// Swaps the Sand values that the format keeps with `values`, as
    /// [`EncodeFormat::swap_values`] does.
    fn swap_values(&mut self, _values: &mut ObjectValues) {}
    /// The message for a tuple or positional payload without `len` elements.
    fn length_mismatch(&self, len: usize) -> String {
        format!("expected array of length {len}")
    }
    /// Bytes of Rust memory that the format holds and that the heap counts.
    fn heap_size(&self) -> usize;
    /// Swaps the text that the format reads with `text`. A `Decoder` lends
    /// its input `str` to the format for the length of each native call,
    /// so that the format reads the text without a copy. A format that does
    /// not read text keeps this default.
    fn swap_input(&mut self, _text: &mut CompactString) {}
}

/// A destination of `Encode`, created by a format such as `json.encode`.
///
/// Each call of an `encode` writes exactly one value, usually with
/// `e.encode(x)`. A call that writes no value or two stops the encode with a
/// runtime error. After the encode ends, each use of the encoder is a runtime
/// error.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Encoder {
    format: Option<Box<dyn EncodeFormat>>,
    /// A host value that the fast path wrote whole, and the bytes of Rust
    /// memory that it holds.
    encoded: Option<(std::sync::Arc<Value>, usize)>,
    protocol: EncodeProtocol,
}

/// A source of `Decode`, created by a format such as `json.decode`.
///
/// Each call of a `decode` reads exactly one value, usually with
/// `d.decode()`. A call that reads no value or two fails with an error of
/// kind `"invalid"`. A failed `d.decode()` leaves the input at the start of
/// its value, so a `decode` can read the value as another type. After
/// `finish`, each use of the decoder is a runtime error.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Decoder {
    // The Sand values of a decoder are the values of its format: the input
    // `str` of a format that reads text, or the values that a format keeps
    // (see `DecodeFormat::keeps_values`). The `Error` values of the errors
    // that the decoder made follow them, so that the decoder knows which
    // errors already have their path (`DecodeProtocol::errors`).
    format: Option<Box<dyn DecodeFormat>>,
    protocol: DecodeProtocol,
}

impl NativeObject for Encoder {
    fn size(&self) -> usize {
        64 + self.format.as_ref().map_or(0, |f| f.heap_size()) + self.encoded.as_ref().map_or(0, |e| e.1)
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
pub(crate) struct DecodePath(Vec<Frame>);

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
    pub(crate) fn new(format: Box<dyn EncodeFormat>) -> Encoder {
        Encoder { format: Some(format), encoded: None, protocol: EncodeProtocol::NEW }
    }

    /// Returns the value that a host `Value` encoder holds.
    pub(crate) fn encoded_value(&self) -> Option<&std::sync::Arc<Value>> {
        if let Some((value, _)) = &self.encoded {
            return Some(value);
        }
        let format: &dyn Any = self.format.as_deref()?;
        format.downcast_ref::<value_format::ValueEncoder>()?.encoded()
    }
}

impl Decoder {
    /// Returns a new decoder for `format`.
    pub(crate) fn new(format: Box<dyn DecodeFormat>) -> Decoder {
        Decoder { format: Some(format), protocol: DecodeProtocol::default() }
    }

    /// Returns a decoder that reads the host value `value`. The value can
    /// also have the JSON form of the target type, such as a Base64
    /// `Value::Str` for `bytes`.
    pub fn from_value(value: Value) -> Decoder {
        Decoder::new(Box::new(value_format::ValueDecoder::new(value)))
    }
}

/// The `Encoder` of a native parameter of type `Encoded<N>`: one that holds
/// the value of a type with a fast path, or one that the Sand
/// implementations of the type write to.
#[derive(crate::SandEnum)]
#[sand(module = "prelude", private, no_value)]
enum ValueEncoding {
    Done(VmVal<Encoder>),
    Driven(VmVal<Encoder>),
}

/// How an operation of an `Encoder` changes its levels.
#[derive(Clone, Copy)]
pub(crate) enum EncodeStep {
    /// Writes a value.
    Value,
    /// Opens a container, which is a value of the current level.
    Open(LevelKind),
    /// Starts field `index` of the innermost struct or named payload.
    Field(usize),
    /// Closes the innermost container.
    End,
    /// Takes the encoded value, which ends the encode.
    Finish,
}

/// Runs `op`, which takes `step`, on the format of `e`, and checks the
/// protocol of `e`.
pub(crate) fn with_encoder<R>(
    vm: &mut Vm,
    e: VmVal<Encoder>,
    step: EncodeStep,
    op: impl FnOnce(&mut dyn EncodeFormat, &mut Vm) -> Result<R, Fail>,
) -> Result<R, Fail> {
    let obj = e.get_mut(&mut vm.heap)?;
    if obj.protocol.ended || (matches!(step, EncodeStep::Value | EncodeStep::Open(_)) && !obj.protocol.takes_value()) {
        return encoder_misuse(vm, e);
    }
    let Some(mut format) = obj.format.take() else { return internal("the encoder is in use") };
    let r = if format.keeps_values() {
        let mut values = vm.heap.take_native_values::<Encoder>(e.raw()).unwrap_or_default();
        format.swap_values(&mut values);
        let r = op(&mut *format, vm);
        format.swap_values(&mut values);
        vm.heap.put_native_values::<Encoder>(e.raw(), values);
        r
    } else {
        op(&mut *format, vm)
    };
    let obj = e.get_mut(&mut vm.heap)?;
    obj.format = Some(format);
    if r.is_ok() {
        match step {
            EncodeStep::Value => obj.protocol.add_value(None),
            EncodeStep::Open(kind) => obj.protocol.add_value(Some(kind)),
            EncodeStep::Field(index) => obj.protocol.field(index),
            EncodeStep::End => obj.protocol.end(),
            EncodeStep::Finish => obj.protocol.ended = true,
        }
    }
    r
}

/// Returns the error for a use of `e` after its encode ended, or for a value
/// that breaks its protocol.
#[cold]
fn encoder_misuse<R>(vm: &mut Vm, e: VmVal<Encoder>) -> Result<R, Fail> {
    let obj = e.get_mut(&mut vm.heap)?;
    if obj.protocol.ended {
        return protocol::encoder_ended();
    }
    let p = std::mem::replace(&mut obj.protocol, EncodeProtocol::NEW);
    let r = p.check_value(vm);
    e.get_mut(&mut vm.heap)?.protocol = p;
    r?;
    internal("encode: no broken protocol")
}

/// Charges a fuel unit for a value, and runs `op`, which writes the value or
/// opens it, on the format of `e`.
fn write_part<R>(
    vm: &mut Vm,
    e: VmVal<Encoder>,
    step: EncodeStep,
    op: impl FnOnce(&mut dyn EncodeFormat, &mut Vm) -> Result<R, Fail>,
) -> Result<R, Fail> {
    vm.charge(1)?;
    with_encoder(vm, e, step, op)
}

/// Runs `op`, which writes one whole value to `e`. If a memory check fails,
/// the format returns to its state before `op`, so that the native function
/// can run again.
pub(crate) fn write_whole(
    vm: &mut Vm,
    e: VmVal<Encoder>,
    op: impl FnOnce(&mut dyn EncodeFormat, &mut Vm) -> Result<(), Fail>,
) -> Result<(), Fail> {
    with_encoder(vm, e, EncodeStep::Value, |f, vm| {
        f.save();
        let r = op(f, vm);
        if r.as_ref().is_err_and(|e| e.as_limit().is_some()) {
            f.restore();
        }
        r
    })
}

/// Runs `op` on `f` with the text of the `str` `input`, if any, lent to `f`
/// (see [`DecodeFormat::swap_input`]).
pub(crate) fn with_input<F: DecodeFormat + ?Sized, R>(
    vm: &mut Vm,
    input: Option<VmValue>,
    f: &mut F,
    op: impl FnOnce(&mut F, &mut Vm) -> R,
) -> Result<R, Fail> {
    let Some(input) = input else { return Ok(op(f, vm)) };
    // Each swap exchanges the texts in place, which leaves the empty text of
    // the format in the `str` meanwhile.
    let Some(text) = vm.heap.str_text_mut(input) else { return internal("decode: the input is not a str") };
    f.swap_input(text);
    let r = op(f, vm);
    if let Some(text) = vm.heap.str_text_mut(input) {
        f.swap_input(text);
    }
    Ok(r)
}

/// Charges a fuel unit for a value, and runs `op`, which reads the value or
/// opens it, on `d` and its format (see [`with_decoder`]).
fn read_part<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut DecodePath, &mut Vm) -> DecodeResult<R>,
) -> Result<R, HostError> {
    vm.charge(1)?;
    with_decoder(vm, d, DecodeOp::Value, |_| true, op)
}

/// Runs `op` on `format`, the format of `d`, with the values of the format
/// lent to it (see [`Decoder`]): its input `input`, or the values that it
/// keeps if `d` holds `errors` errors after them.
fn lend<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    format: &mut dyn DecodeFormat,
    input: Option<VmValue>,
    errors: u32,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut Vm) -> R,
) -> Result<R, Fail> {
    if !format.keeps_values() {
        return with_input(vm, input, format, op);
    }
    let Some(mut values) = vm.heap.take_native_values::<Decoder>(d.raw()) else { return internal("not a decoder") };
    let errors: ObjectValues = values.drain(values.len().saturating_sub(errors as usize)..).collect();
    format.swap_values(&mut values);
    let r = op(format, vm);
    format.swap_values(&mut values);
    let (capacity, needed) = (values.capacity(), values.len() + errors.len());
    if needed > capacity {
        values.reserve_exact(needed - values.len());
        vm.heap.account(capacity * VALUE_SIZE, values.capacity() * VALUE_SIZE);
    }
    values.extend(errors);
    vm.heap.put_native_values::<Decoder>(d.raw(), values);
    Ok(r)
}

/// Runs `op` on the format and the protocol state of `d`, with the values of
/// the format lent to it (see [`lend`]). The input of a format that reads
/// text is lent only if `reads`.
#[inline]
fn with_decoder_state<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    reads: bool,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut DecodeProtocol, &mut Vm) -> R,
) -> Result<R, Fail> {
    let Some((obj, values)) = vm.heap.native_and_values::<Decoder>(d.raw()) else { return internal("not a decoder") };
    let errors = obj.protocol.errors;
    let input = values.first().copied().filter(|_| reads && values.len() > errors as usize);
    let Some(mut format) = obj.format.take() else { return internal("the decoder is in use") };
    let mut p = std::mem::take(&mut obj.protocol);
    let r = lend(vm, d, &mut *format, input, errors, |f, vm| op(f, &mut p, vm));
    let obj = d.get_mut(&mut vm.heap)?;
    obj.format = Some(format);
    obj.protocol = p;
    r
}

/// Runs `op`, an operation of kind `kind`, on `d` and its format, checks the
/// protocol of `d`, and charges the fuel of the input that the format read.
/// `reads` returns `true` if the result of `op` is a value that it read. An
/// error gets the decoder's path.
#[inline]
fn with_decoder<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    kind: DecodeOp,
    reads: impl FnOnce(&R) -> bool,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut DecodePath, &mut Vm) -> DecodeResult<R>,
) -> Result<R, HostError> {
    let Some((obj, values)) = vm.heap.native_and_values::<Decoder>(d.raw()) else { return Err(Fail::internal("not a decoder").into()) };
    if !obj.protocol.is_ready(kind) {
        return with_decoder_protocol(vm, d, kind, reads, op);
    }
    // The operation needs no work of the protocol, so only the path leaves
    // the protocol state.
    let errors = obj.protocol.errors;
    let input = values.first().copied().filter(|_| values.len() > errors as usize);
    let Some(mut format) = obj.format.take() else { return Err(Fail::internal("the decoder is in use").into()) };
    let mut path = std::mem::take(&mut obj.protocol.path);
    let level = path.0.len();
    let r = lend(vm, d, &mut *format, input, errors, |f, vm| charged(f, vm, |f, vm| op(f, &mut path, vm)))?;
    let r = r.map_err(|e| e.into_error(|| path_text(vm, &path.0)));
    let obj = d.get_mut(&mut vm.heap)?;
    obj.format = Some(format);
    obj.protocol.path = path;
    match r {
        Ok(v) => {
            obj.protocol.after(level, reads(&v));
            Ok(v)
        }
        Err(error) => Err(decoder_error(vm, d, error?)?),
    }
}

/// Runs `op` as [`with_decoder`] does, after the work of the protocol: a
/// return to the start of a value after a failed read, or a check of a
/// broken protocol.
#[cold]
fn with_decoder_protocol<R>(
    vm: &mut Vm,
    d: VmVal<Decoder>,
    kind: DecodeOp,
    reads: impl FnOnce(&R) -> bool,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut DecodePath, &mut Vm) -> DecodeResult<R>,
) -> Result<R, HostError> {
    let r = with_decoder_state(vm, d, true, |f, p, vm| {
        if let Err(message) = p.before(vm, f, kind)? {
            return Ok(Err(core::Error { message, kind: protocol::VIOLATION_KIND.into() }));
        }
        let level = p.path.0.len();
        match charged(f, vm, |f, vm| op(f, &mut p.path, vm)) {
            Ok(v) => {
                p.after(level, reads(&v));
                Ok(Ok(v))
            }
            Err(e) => e.into_error(|| p.path_text(vm)).map(Err),
        }
    })??;
    match r {
        Ok(v) => Ok(v),
        Err(error) => Err(decoder_error(vm, d, error)?),
    }
}

/// Runs `op` on `f`, and charges the fuel of the input that `f` read.
#[inline(always)]
fn charged<R>(
    f: &mut dyn DecodeFormat,
    vm: &mut Vm,
    op: impl FnOnce(&mut dyn DecodeFormat, &mut Vm) -> DecodeResult<R>,
) -> DecodeResult<R> {
    let r = op(f, vm);
    vm.charge(f.take_input_fuel())?;
    r
}

/// Keeps the `Error` value `e`, which the decoder `d` made (see
/// [`Decoder`]). It fails the read that made it.
fn keep_error(vm: &mut Vm, d: VmVal<Decoder>, e: VmValue) -> Result<(), Fail> {
    vm.unchecked(|vm| d.push_value(vm, e))?;
    d.get_mut(&mut vm.heap)?.protocol.add_error();
    Ok(())
}

/// Returns the error `error`, which the decoder `d` made, and keeps it.
fn decoder_error<C>(vm: &mut Vm, d: VmVal<Decoder>, error: core::Error) -> Result<HostError<C>, Fail> {
    let err = decoder_err(vm, d, error.clone())?;
    Ok(HostError::made(err, error.kind, error.message))
}

/// Returns the `Err` value of `error`, which the decoder `d` made, and keeps
/// it.
fn decoder_err(vm: &mut Vm, d: VmVal<Decoder>, error: core::Error) -> Result<VmValue, Fail> {
    let e = vm.unchecked(|vm| error.into_vm_value(vm))?;
    keep_error(vm, d, e)?;
    Ok(vm.unchecked(|vm| vm.err(e)))
}

/// The error of the `Result` value of a decode, or of a call of a
/// hand-written `Decode`.
struct ResultError {
    origin: ErrorOrigin,
    /// The `Error` value.
    value: VmValue,
    error: core::Error,
}

/// Returns the error of the `Result` value `r` of a decode with `d`, or
/// `None` if `r` is `Ok`.
fn result_error(vm: &Vm, d: VmVal<Decoder>, r: VmValue) -> Result<Option<ResultError>, Fail> {
    if vm.heap.variant_id(r) != vm.prog.ids.err {
        return Ok(None);
    }
    let Some(value) = vm.heap.field(r, 0) else { return internal("decode: an Err without its error") };
    let error = core::Error::from_vm_value(vm, value, &mut crate::native::convert::Conversion::new())?;
    let (Some(values), errors) = (vm.heap.native_values::<Decoder>(d.raw()), d.get(&vm.heap)?.protocol.errors as usize) else {
        return internal("not a decoder");
    };
    let made = values[values.len().saturating_sub(errors)..].contains(&value);
    let origin = if made { ErrorOrigin::Decoder } else { ErrorOrigin::Implementation };
    Ok(Some(ResultError { origin, value, error }))
}

/// Ends the innermost call of a hand-written `Decode` with the `Result`
/// value `r`. An error of the implementation gets the decoder's path, and a
/// call that read no value fails.
fn leave_decode(vm: &mut Vm, d: VmVal<Decoder>, r: VmValue) -> Result<VmValue, Fail> {
    let error = result_error(vm, d, r)?;
    let origin = error.as_ref().map(|e| e.origin);
    let (ending, dropped) = with_decoder_state(vm, d, false, |f, p, vm| {
        let errors = p.errors;
        p.leave(vm, f, origin).map(|ending| (ending, errors - p.errors))
    })??;
    if dropped > 0 {
        let values = vm.heap.native_values::<Decoder>(d.raw()).map_or(0, |v| v.len());
        d.truncate_values(&mut vm.heap, values - dropped as usize)?;
    }
    apply_ending(vm, d, ending, r, error)
}

/// Returns the result of a call or a decode that ends with `ending`, the
/// `Result` value `r`, and its error `error`. Keeps the error of the result,
/// if any (see [`Decoder`]).
fn apply_ending(vm: &mut Vm, d: VmVal<Decoder>, ending: Ending, r: VmValue, error: Option<ResultError>) -> Result<VmValue, Fail> {
    match (ending, error) {
        (Ending::Result, Some(error)) => {
            keep_error(vm, d, error.value)?;
            Ok(r)
        }
        (Ending::Result, None) => Ok(r),
        (Ending::Locate(path), Some(ResultError { error, .. })) => {
            decoder_err(vm, d, core::Error { message: format!("{path}: {}", error.message), kind: error.kind })
        }
        (Ending::Locate(_), None) => internal("decode: a path for no error"),
        (Ending::Fail(e), _) => decoder_err(vm, d, e),
    }
}

/// Ends the decode of `d` with the `Result` value `r` of the outermost
/// `Decode`: checks that no input follows a value, and adds the path to an
/// error of an implementation.
fn end_decode(vm: &mut Vm, d: VmVal<Decoder>, r: VmValue) -> Result<VmValue, Fail> {
    let error = result_error(vm, d, r)?;
    let origin = error.as_ref().map(|e| e.origin);
    let ending = with_decoder_state(vm, d, true, |f, p, vm| {
        let mut ending = p.end(vm, f, origin)?;
        if origin.is_none() && matches!(ending, Ending::Result) {
            let end = f.finish(vm);
            if let Err(e) = vm.charge(f.take_input_fuel()).map_err(DecodeFail::Fail).and(end) {
                // A failed memory check returns before the decode closes,
                // so that the native call can run again.
                ending = Ending::Fail(e.into_error(|| p.path_text(vm))?);
            }
        }
        p.close();
        Ok::<_, Fail>(ending)
    })??;
    apply_ending(vm, d, ending, r, error)
}

/// The `Encode` dictionary of the first type parameter of a native function.
pub(crate) type EncodeDict = CodecDict<0, traits::Encode>;

/// The `Decode` dictionary of the first type parameter of a native function.
pub(crate) type DecodeDict = CodecDict<0, traits::Decode>;

/// Returns the fast-path schema of the type whose `Encode` or `Decode`
/// dictionary is `dict`, or `None` if the type has no fast-path `impl`.
pub(crate) fn fast_schema<B>(vm: &Vm, dict: CodecDict<0, B>) -> Option<SchemaId> {
    let Obj::Tuple(slots) = vm.heap.get(dict.raw()) else { return None };
    match vm.heap.get(*slots.first()?) {
        Obj::Closure(proto, _) => vm.prog.proto_schemas.get(*proto as usize).copied().filter(|&s| s != fast::NO_SCHEMA),
        _ => None,
    }
}

/// Reads one whole value of the closed type with fast-path schema `schema`
/// from `f`, and checks that no input follows. `f` belongs to this native
/// call only, so a failed read needs no mark. A decode error gets its
/// path.
pub(crate) fn read_whole<F: DecodeFormat>(vm: &mut Vm, f: &mut F, schema: SchemaId) -> Result<Result<VmValue, core::Error>, Fail> {
    let mut path = DecodePath::default();
    let r = f.read_fast(vm, &mut path, schema).and_then(|v| f.finish(vm).map(|()| v));
    vm.charge(f.take_input_fuel())?;
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(e) => e.into_error(|| path_text(vm, &path.0)).map(Err),
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
    m.bind::<ValueEncoding>();
    m.function("value_encoder", |vm: &mut Vm, encode: EncodeDict, value: Param<0>| -> Result<ValueEncoding, Fail> {
        let mut f = value_format::ValueEncoder::new();
        let Some(schema) = fast_schema(vm, encode) else { return vm.new_object(Encoder::new(Box::new(f))).map(ValueEncoding::Driven) };
        let r = f.write_fast(vm, value.raw(), schema);
        // The new object counts the bytes of the value.
        vm.heap.account(f.heap_size(), 0);
        r?;
        let Some(encoded) = f.into_encoded() else { return internal("encode: no value") };
        vm.new_object(Encoder { format: None, encoded: Some(encoded), protocol: EncodeProtocol::NEW }).map(ValueEncoding::Done)
    });
    encoder_methods(m);
    decoder_methods(m);
}

type Enc = VmVal<Encoder>;
type Dec = VmVal<Decoder>;
type Decoded<T> = Result<T, HostError>;

fn encoder_methods(m: &mut NativeModule) {
    use EncodeStep::{End, Field, Open, Value};
    m.function("Encoder.write_unit", |vm: &mut Vm, e: Enc| write_part(vm, e, Value, |f, vm| f.unit(vm)));
    m.function("Encoder.write_bool", |vm: &mut Vm, e: Enc, b: bool| write_part(vm, e, Value, |f, vm| f.bool(vm, b)));
    m.function("Encoder.write_int", |vm: &mut Vm, e: Enc, i: i64| write_part(vm, e, Value, |f, vm| f.int(vm, i)));
    m.function("Encoder.write_float", |vm: &mut Vm, e: Enc, x: f64| write_part(vm, e, Value, |f, vm| f.float(vm, x)));
    m.function("Encoder.write_duration", |vm: &mut Vm, e: Enc, d: jiff::SignedDuration| {
        write_part(vm, e, Value, |f, vm| f.duration(vm, d.as_nanos() as i64))
    });
    m.function("Encoder.write_str", |vm: &mut Vm, e: Enc, s: VmVal<String>| write_part(vm, e, Value, |f, vm| f.str_value(vm, s.raw())));
    m.function("Encoder.write_bytes", |vm: &mut Vm, e: Enc, b: VmVal<Vec<u8>>| {
        let bytes = b.bytes(&vm.heap).to_vec();
        write_part(vm, e, Value, |f, vm| f.bytes(vm, &bytes))
    });
    m.function("Encoder.write_range", |vm: &mut Vm, e: Enc, r: VmVal<crate::native::ty::Range>| {
        let Obj::Range(start, end, inclusive) = *vm.heap.get(r.raw()) else { return internal("write_range: not a range") };
        write_part(vm, e, Value, |f, vm| f.range(vm, start, end, inclusive))
    });
    m.function("Encoder.write_native", |vm: &mut Vm, e: Enc, x: Param<0>| {
        let Some((type_name, text)) = native_text(vm, x.raw()) else { return internal("write_native: not a native value") };
        write_part(vm, e, Value, |f, vm| f.native(vm, &type_name, &text))
    });
    m.function("Encoder.begin_list", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, Open(LevelKind::Seq), |f, vm| f.begin_seq(vm, SeqKind::List, to_index(len)))
    });
    m.function("Encoder.begin_set", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, Open(LevelKind::Seq), |f, vm| f.begin_seq(vm, SeqKind::Set, to_index(len)))
    });
    m.function("Encoder.begin_tuple", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, Open(LevelKind::Seq), |f, vm| f.begin_seq(vm, SeqKind::Tuple(to_index(len)), to_index(len)))
    });
    m.function("Encoder.begin_map", |vm: &mut Vm, e: Enc, len: i64| {
        write_part(vm, e, Open(LevelKind::Map), |f, vm| f.begin_map(vm, to_index(len)))
    });
    m.function("Encoder.begin_struct", |vm: &mut Vm, e: Enc, def: i64| {
        let kind = LevelKind::Fields { id: def as u32, variant: false };
        write_part(vm, e, Open(kind), |f, vm| f.begin_struct(vm, def as u32))
    });
    m.function("Encoder.field", |vm: &mut Vm, e: Enc, i: i64| with_encoder(vm, e, Field(to_index(i)), |f, vm| f.field(vm, to_index(i))));
    m.function("Encoder.write_variant", |vm: &mut Vm, e: Enc, v: i64| write_part(vm, e, Value, |f, vm| f.unit_variant(vm, v as u32)));
    m.function("Encoder.begin_variant", |vm: &mut Vm, e: Enc, v: i64| {
        let kind = LevelKind::Fields { id: v as u32, variant: true };
        write_part(vm, e, Open(kind), |f, vm| f.begin_variant(vm, v as u32))
    });
    m.function("Encoder.begin_some", |vm: &mut Vm, e: Enc| write_part(vm, e, Open(LevelKind::Some), |f, vm| f.begin_some(vm)));
    m.function("Encoder.write_none", |vm: &mut Vm, e: Enc| write_part(vm, e, Value, |f, vm| f.none(vm)));
    m.function("Encoder.end", |vm: &mut Vm, e: Enc| with_encoder(vm, e, End, |f, vm| f.end(vm)));
    m.function("Encoder.write_fast", |vm: &mut Vm, e: Enc, x: Param<0>, schema: i64| {
        with_encoder(vm, e, Value, |f, vm| f.write_fast(vm, x.raw(), schema as u32))
    });
    m.function("Encoder.enter_impl", |vm: &mut Vm, e: Enc, def: i64| {
        let p = &mut e.get_mut(&mut vm.heap)?.protocol;
        if p.ended || !p.takes_value() {
            return encoder_misuse(vm, e);
        }
        p.enter(def as u32);
        Ok(())
    });
    m.function("Encoder.leave_impl", |vm: &mut Vm, e: Enc| {
        let p = &mut e.get_mut(&mut vm.heap)?.protocol;
        if p.ended {
            return protocol::encoder_ended();
        }
        if p.leave() {
            return Ok(());
        }
        let p = std::mem::replace(&mut e.get_mut(&mut vm.heap)?.protocol, EncodeProtocol::NEW);
        let r = p.leave_error(vm);
        e.get_mut(&mut vm.heap)?.protocol = p;
        r
    });
}

/// Reads a `json.Json` value from `d`.
pub(crate) fn read_json(vm: &mut Vm, d: VmVal<Decoder>) -> Result<VmValue, HostError> {
    read_part(vm, d, |f, _, vm| f.json(vm))
}

/// Returns the qualified type name and the text of the native value `v`.
pub(crate) fn native_text(vm: &Vm, v: VmValue) -> Option<(String, String)> {
    let native = vm.heap.get(v).value()?;
    let module = crate::value::qualified_name_module(native.module_name());
    Some((crate::value::qualified_name(module, native.type_name()), native.text()))
}

fn decoder_methods(m: &mut NativeModule) {
    use DecodeOp::{Other, Value};
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
    m.function("Decoder.read_some", |vm: &mut Vm, d: Dec| -> Decoded<bool> {
        vm.charge(1)?;
        with_decoder(vm, d, DecodeOp::Option, |&some: &bool| !some, |f, _, vm| f.some(vm))
    });
    m.function("Decoder.begin_list", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_seq(f, &mut path.0, vm, SeqKind::List))
    });
    m.function("Decoder.begin_set", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_seq(f, &mut path.0, vm, SeqKind::Set))
    });
    m.function("Decoder.next_item", |vm: &mut Vm, d: Dec| -> Decoded<bool> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| dec_next_item(f, &mut path.0, vm))
    });
    m.function("Decoder.begin_tuple", |vm: &mut Vm, d: Dec, len: i64| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_seq(f, &mut path.0, vm, SeqKind::Tuple(to_index(len))))
    });
    m.function("Decoder.element", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| dec_element(f, &mut path.0, vm))
    });
    m.function("Decoder.end_tuple", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| dec_end_tuple(f, &mut path.0, vm))
    });
    m.function("Decoder.begin_map", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_map(f, &mut path.0, vm))
    });
    m.function("Decoder.next_entry", |vm: &mut Vm, d: Dec| -> Decoded<bool> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| dec_next_entry(f, &mut path.0, vm))
    });
    m.function("Decoder.map_value", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| dec_map_value(f, &mut path.0, vm))
    });
    m.function("Decoder.begin_struct", |vm: &mut Vm, d: Dec, def: i64| -> Decoded<()> {
        read_part(vm, d, |f, path, vm| dec_begin_struct(f, &mut path.0, vm, def as u32))
    });
    m.function("Decoder.begin_fields", |vm: &mut Vm, d: Dec, variant: i64| -> Decoded<()> {
        with_decoder(vm, d, Value, |_| true, |f, path, vm| dec_begin_fields(f, &mut path.0, vm, variant as u32))
    });
    m.function("Decoder.field", |vm: &mut Vm, d: Dec| -> Decoded<i64> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| next_field(f, &mut path.0, vm))
    });
    m.function("Decoder.missing", |vm: &mut Vm, d: Dec, id: i64, variant: bool, index: i64| -> Result<VmVal<core::Error>, Fail> {
        let message = with_decoder_state(vm, d, false, |_, p, vm| {
            format!("{}: {}", p.path_text(vm), missing_field(vm, id as u32, variant, to_index(index)))
        })?;
        let err = decoder_err(vm, d, core::Error { message, kind: "parse".into() })?;
        match vm.heap.field(err, 0) {
            Some(e) => Ok(VmVal::from_raw(e)),
            None => internal("decode: an Err without its error"),
        }
    });
    m.function("Decoder.variant", |vm: &mut Vm, d: Dec, first: i64, count: i64| -> Decoded<i64> {
        read_part(vm, d, |f, path, vm| read_variant(f, &mut path.0, vm, first as u32, to_index(count)))
    });
    m.function("Decoder.end_variant", |vm: &mut Vm, d: Dec| -> Decoded<()> {
        with_decoder(vm, d, Other, |_| false, |f, path, vm| dec_end_variant(f, &mut path.0, vm))
    });
    m.function("Decoder.read_fast", |vm: &mut Vm, d: Dec, schema: i64| -> Decoded<Param<0>> {
        with_decoder(
            vm,
            d,
            Value,
            |_| true,
            |f, path, vm| {
                // The native call runs again after a failed memory check.
                let mark = f.mark();
                let r = f.read_fast(vm, path, schema as u32);
                if let Err(DecodeFail::Fail(fail)) = &r
                    && fail.as_limit().is_some()
                {
                    f.rewind(mark);
                }
                f.release(mark);
                r
            },
        )
        .map(Param::from_raw)
    });
    m.function("Decoder.end_decode", |vm: &mut Vm, d: Dec, r: Param<0>| -> Result<Param<0>, Fail> {
        end_decode(vm, d, r.raw()).map(Param::from_raw)
    });
    m.function("Decoder.enter_impl", |vm: &mut Vm, d: Dec, def: i64| -> Result<(), Fail> {
        with_decoder_state(vm, d, false, |f, p, vm| p.enter(vm, f, def as u32))?
    });
    m.function("Decoder.leave_impl", |vm: &mut Vm, d: Dec, r: Param<0>| -> Result<Param<0>, Fail> {
        leave_decode(vm, d, r.raw()).map(Param::from_raw)
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
/// the named payload of variant `id`.
fn missing_field(vm: &Vm, id: u32, variant: bool, index: usize) -> String {
    let name = field_names(vm, id, variant).get(index).map_or("", String::as_str);
    format!("missing field `{name}`")
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
        write_part(vm, self, EncodeStep::Value, |f, vm| f.str(vm, s))
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
            let mark = f.mark();
            let r = f.value(vm);
            if let Err(DecodeFail::Fail(fail)) = &r
                && fail.as_limit().is_some()
            {
                f.rewind(mark);
            }
            f.release(mark);
            r
        })
    }

    /// Returns a decode error with the decoder's path and `message`, for a
    /// native `decode` to return. It fails the read, so that the caller can
    /// read the value as another type.
    pub fn error(self, vm: &mut Vm, message: &str) -> Result<HostError, Fail> {
        let message = with_decoder_state(vm, self, false, |_, p, vm| format!("{}: {message}", p.path_text(vm)))?;
        decoder_error(vm, self, core::Error { message, kind: "parse".into() })
    }
}
