//! Rust interfaces of the Sand `Encode` and `Decode` traits.
//!
//! The engine encodes a Sand value by walking it and writing each part to an
//! [`EncodeSink`]. It decodes a Sand value by reading each part from a
//! [`DecodeSource`]. JSON text, the `json.Json` tree, and the host
//! [`Value`](crate::value::Value) each provide a sink or a source.
//!
//! A native type implements the Sand traits only if its derive attribute
//! declares them. `#[sand(encode, decode)]` derives them from the structure of
//! the type. `#[sand(encode = "manual", decode = "manual")]` uses the Rust
//! traits [`Encode`] and [`Decode`] of this module:
//!
//! ```
//! # use sandlang::native::codec::{Decode, DecodeSource, Encode, EncodeSink};
//! # use sandlang::native::convert::{HostError, IntoVmValue};
//! # use sandlang::native::ty::VmVal;
//! # use sandlang::vm::value::VmValue;
//! # use sandlang::vm::{Fail, Vm};
//! /// An amount of money in cents.
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, sandlang::SandStruct)]
//! #[sand(module = "bank", encode = "manual", decode = "manual")]
//! pub struct Money {
//!     cents: i64,
//! }
//!
//! impl Encode for Money {
//!     fn encode(vm: &mut Vm, value: VmVal<Money>, sink: &mut dyn EncodeSink) -> Result<(), HostError<Fail>> {
//!         let Some(VmValue::Int(cents)) = vm.heap.field(value.raw(), 0) else { return Err(Fail::internal("not money").into()) };
//!         sink.int(cents)
//!     }
//! }
//!
//! impl Decode for Money {
//!     fn decode(vm: &mut Vm, source: &mut dyn DecodeSource) -> Result<VmVal<Money>, HostError<Fail>> {
//!         let cents = source.int()?;
//!         Ok(VmVal::from_raw(Money { cents }.into_vm_value(vm)?))
//!     }
//! }
//! ```

use super::convert::HostError;
use super::ty::VmVal;
use crate::value::{StructDesc, VariantDesc};
use crate::vm::value::VmValue;
use crate::vm::{Fail, Vm};
use std::borrow::Cow;
use std::sync::Arc;

/// Result of an [`EncodeSink`] or [`DecodeSource`] request.
pub type CodecResult<T = ()> = Result<T, HostError<Fail>>;

/// Fails with the memory limit.
#[cold]
pub(crate) fn memory_limit<T>() -> CodecResult<T> {
    Err(Fail::limit(crate::engine::LimitKind::Memory).into())
}

/// Receives a single Sand value from the encode walker.
///
/// The walker writes a scalar with a single call. It writes a container with
/// a `begin_` call, the parts of the container, and [`EncodeSink::end`].
pub trait EncodeSink: Send {
    fn unit(&mut self) -> CodecResult;
    fn bool(&mut self, value: bool) -> CodecResult;
    fn int(&mut self, value: i64) -> CodecResult;
    fn float(&mut self, value: f64) -> CodecResult;
    /// Writes a `duration` as a count of nanoseconds.
    fn duration(&mut self, nanos: i64) -> CodecResult;
    fn str(&mut self, value: &str) -> CodecResult;
    fn bytes(&mut self, value: &[u8]) -> CodecResult;
    fn range(&mut self, start: i64, end: i64, inclusive: bool) -> CodecResult;
    /// Writes a value of the native value type `type_name`, such as
    /// `time.PlainDate`, as its text form.
    fn native(&mut self, type_name: &str, text: &str) -> CodecResult;
    /// Writes `None`.
    fn none(&mut self) -> CodecResult;
    /// Begins `Some`. Its single part is the payload.
    fn begin_some(&mut self) -> CodecResult;
    fn begin_list(&mut self, len: usize) -> CodecResult;
    fn begin_tuple(&mut self, len: usize) -> CodecResult;
    fn begin_set(&mut self, len: usize) -> CodecResult;
    /// Begins a map of `len` entries. Its parts alternate between keys and
    /// values.
    fn begin_map(&mut self, len: usize) -> CodecResult;
    /// Begins a struct. Its parts are the fields in declaration order.
    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult;
    /// Begins an enum variant. Its `len` parts are the payload values.
    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult;
    /// Ends the innermost container.
    fn end(&mut self) -> CodecResult;
    /// Returns the bytes of memory that the output holds. The engine counts
    /// them against the memory limit of the run.
    fn allocated(&self) -> usize;
}

/// The kind of the next input value of a [`DecodeSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, crate::SandEnum)]
#[sand(module = "prelude")]
pub enum InputKind {
    /// `()`, `None`, or JSON `null`.
    Null,
    Bool,
    Int,
    Float,
    Duration,
    /// A string, or the text of a native value.
    Str,
    Bytes,
    /// A list, set, or tuple, or a JSON array.
    List,
    /// A map, struct, or range, or a JSON object.
    Map,
    /// An enum variant other than `None`.
    Variant,
}

/// The payload form of an enum variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadForm {
    /// A variant without a payload.
    Unit,
    /// Positional payload values. Call [`DecodeSource::next`] before each
    /// value.
    Positional(usize),
    /// Named payload fields. The walker reads them with
    /// [`DecodeSource::next_field`].
    Named,
}

/// Supplies a single Sand value to the decode walker.
///
/// The walker requests each part by its type. A request for a container
/// begins it, and [`DecodeSource::end`] ends it. A source returns an error of
/// kind `"parse"` if the input does not match a request, with the path of the
/// part in the message, such as `$.items[2].price: expected int`.
pub trait DecodeSource: Send {
    /// Returns the kind of the next value without reading it.
    fn kind(&mut self) -> CodecResult<InputKind>;
    fn unit(&mut self) -> CodecResult;
    fn bool(&mut self) -> CodecResult<bool>;
    fn int(&mut self) -> CodecResult<i64>;
    fn float(&mut self) -> CodecResult<f64>;
    /// Reads a `duration` as a count of nanoseconds.
    fn duration(&mut self) -> CodecResult<i64>;
    fn str(&mut self) -> CodecResult<Cow<'_, str>>;
    fn bytes(&mut self) -> CodecResult<Cow<'_, [u8]>>;
    fn range(&mut self) -> CodecResult<(i64, i64, bool)>;
    /// Reads the text form of a value of the native value type `type_name`,
    /// such as `time.PlainDate`.
    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>>;
    /// Reads `None` and returns `false`, or begins `Some` and returns `true`.
    /// The payload of `Some` follows, and then [`DecodeSource::end`].
    fn option(&mut self) -> CodecResult<bool>;
    /// Begins a list or a set. Call [`DecodeSource::next`] before each
    /// element.
    fn begin_list(&mut self) -> CodecResult;
    /// Begins a tuple of `len` elements. Call [`DecodeSource::next`] before
    /// each element.
    fn begin_tuple(&mut self, len: usize) -> CodecResult;
    /// Begins a map. Call [`DecodeSource::next`] before each entry, and then
    /// read its key and its value.
    fn begin_map(&mut self) -> CodecResult;
    /// Returns `true` if another element or entry follows, or `false` at the
    /// end of the container.
    fn next(&mut self) -> CodecResult<bool>;
    /// Begins a struct of type `desc`. Call [`DecodeSource::next_field`]
    /// before each field.
    fn begin_struct(&mut self, desc: &StructDesc) -> CodecResult;
    /// Returns the name of the next field of a struct or of a named payload,
    /// or `None` at the end. The value of the field follows.
    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>>;
    /// Begins a variant of the enum `type_name`. Returns the variant name.
    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>>;
    /// Begins the payload of the current variant in `form`.
    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult;
    /// Skips the next value.
    fn skip(&mut self) -> CodecResult;
    /// Ends the innermost container.
    fn end(&mut self) -> CodecResult;
    /// Returns the path of the current position, such as `$.items[2]`.
    fn path(&self) -> String;
    /// Returns the bytes of memory that the source holds. The engine counts
    /// them against the memory limit of the run.
    fn allocated(&self) -> usize;
    /// Returns the work since the last call, in bytes of input. The engine
    /// charges fuel for it.
    fn work(&mut self) -> usize;
}

/// Encodes a native type for `#[sand(encode = "manual")]`.
pub trait Encode: Sized + 'static {
    /// Writes `value` to `sink` as a single value.
    fn encode(vm: &mut Vm, value: VmVal<Self>, sink: &mut dyn EncodeSink) -> CodecResult;
}

/// Decodes a native type for `#[sand(decode = "manual")]`.
pub trait Decode: Sized + 'static {
    /// Reads a single value from `source`.
    fn decode(vm: &mut Vm, source: &mut dyn DecodeSource) -> CodecResult<VmVal<Self>>;
}

/// A manual encode function of a native type.
pub type EncodeFn = fn(&mut Vm, VmValue, &mut dyn EncodeSink) -> CodecResult;

/// A manual decode function of a native type.
pub type DecodeFn = fn(&mut Vm, &mut dyn DecodeSource) -> CodecResult<VmValue>;

/// How a native type implements a codec trait.
#[derive(Clone, Copy, Debug)]
pub enum NativeCodec<F> {
    /// The engine walks the structure of the type.
    Derived,
    /// A Rust function encodes or decodes the type.
    Manual(F),
}

/// The codec traits that a native type declares.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeCodecs {
    pub encode: Option<NativeCodec<EncodeFn>>,
    pub decode: Option<NativeCodec<DecodeFn>>,
}

#[doc(hidden)]
pub fn manual_encode<T: Encode>(vm: &mut Vm, value: VmValue, sink: &mut dyn EncodeSink) -> CodecResult {
    T::encode(vm, VmVal::from_raw(value), sink)
}

#[doc(hidden)]
pub fn manual_decode<T: Decode>(vm: &mut Vm, source: &mut dyn DecodeSource) -> CodecResult<VmValue> {
    T::decode(vm, source).map(VmVal::raw)
}
