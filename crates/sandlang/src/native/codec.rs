//! Rust interfaces for the Sand `Encode` and `Decode` traits.
//!
//! The encode walker visits a Sand value and writes each part to an
//! [`EncodeSink`]. The decode walker reads each part from a [`DecodeSource`].
//! Both JSON text and [`Value`](crate::value::Value) implement a sink and a
//! source.
//!
//! A native type implements the Sand codec traits only if its derive attribute
//! opts in:
//!
//! - `#[sand(encode, decode)]` derives codec implementations from the type's
//!   structure.
//! - `#[sand(encode = "manual", decode = "manual")]` delegates to the Rust
//!   [`Encode`] and [`Decode`] traits in this module.
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

/// Result returned by [`EncodeSink`] and [`DecodeSource`] methods.
pub type CodecResult<T = ()> = Result<T, HostError<Fail>>;

/// Returns a memory limit error.
#[cold]
pub(crate) fn memory_limit<T>() -> CodecResult<T> {
    Err(Fail::limit(crate::engine::LimitKind::Memory).into())
}

/// Destination that receives one Sand value from the encode walker.
///
/// The walker writes each scalar with a single method call. To write a
/// container, the walker calls `begin_*`, writes each part, and calls
/// [`EncodeSink::end`].
pub trait EncodeSink: Send {
    fn unit(&mut self) -> CodecResult;
    fn bool(&mut self, value: bool) -> CodecResult;
    fn int(&mut self, value: i64) -> CodecResult;
    fn float(&mut self, value: f64) -> CodecResult;
    /// Writes a `duration` as a signed nanosecond count.
    fn duration(&mut self, nanos: i64) -> CodecResult;
    fn str(&mut self, value: &str) -> CodecResult;
    fn bytes(&mut self, value: &[u8]) -> CodecResult;
    fn range(&mut self, start: i64, end: i64, inclusive: bool) -> CodecResult;
    /// Writes the text form of a native value of type `type_name`.
    fn native(&mut self, type_name: &str, text: &str) -> CodecResult;
    /// Writes `None`.
    fn none(&mut self) -> CodecResult;
    /// Begins `Some` before writing its single payload value.
    fn begin_some(&mut self) -> CodecResult;
    fn begin_list(&mut self, len: usize) -> CodecResult;
    fn begin_tuple(&mut self, len: usize) -> CodecResult;
    fn begin_set(&mut self, len: usize) -> CodecResult;
    /// Begins a map of `len` entries before writing alternating keys and
    /// values.
    fn begin_map(&mut self, len: usize) -> CodecResult;
    /// Begins a struct before writing its fields in declaration order.
    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult;
    /// Begins an enum variant before writing its `len` payload values.
    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult;
    /// Ends the innermost open container.
    fn end(&mut self) -> CodecResult;
}

/// Kind of the next value available from a `Decoder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, crate::SandEnum)]
#[sand(module = "prelude", encode, decode)]
pub enum InputKind {
    /// `()`, `None`, or JSON `null`.
    Null,
    Bool,
    Int,
    Float,
    Duration,
    Str,
    Bytes,
    /// A list, a set, or a JSON array.
    List,
    /// A map or a JSON object.
    Map,
    Tuple,
    Struct,
    Range,
    /// A native value, such as `time.Instant`.
    Native,
    /// An enum variant other than `None`.
    Variant,
}

/// Layout of an enum variant's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadForm {
    /// Variant without a payload.
    Unit,
    /// Positional payload of `usize` values read after [`DecodeSource::next`].
    Positional(usize),
    /// Named payload fields read with [`DecodeSource::next_field`].
    Named,
}

/// Input stream that supplies one Sand value to the decode walker.
///
/// The walker requests each part according to the target type. A container
/// request opens the container. [`DecodeSource::end`] closes it. If the input
/// does not match a request, the source returns a `"parse"` error with the
/// current path, such as `$.items[2].price: expected int`.
pub trait DecodeSource: Send {
    /// Peeks at the kind of the next value without consuming it.
    fn kind(&mut self) -> CodecResult<InputKind>;
    fn unit(&mut self) -> CodecResult;
    fn bool(&mut self) -> CodecResult<bool>;
    fn int(&mut self) -> CodecResult<i64>;
    fn float(&mut self) -> CodecResult<f64>;
    /// Reads a `duration` as a signed nanosecond count.
    fn duration(&mut self) -> CodecResult<i64>;
    fn str(&mut self) -> CodecResult<Cow<'_, str>>;
    fn bytes(&mut self) -> CodecResult<Cow<'_, [u8]>>;
    fn range(&mut self) -> CodecResult<(i64, i64, bool)>;
    /// Reads the text form of a native value of type `type_name`.
    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>>;
    /// Consumes `None` and returns `false`, or opens `Some` and returns `true`.
    ///
    /// If this method returns `true`, the caller reads the payload and then
    /// calls [`DecodeSource::end`].
    fn option(&mut self) -> CodecResult<bool>;
    /// Opens a list or set. Call [`DecodeSource::next`] before each element.
    fn begin_list(&mut self) -> CodecResult;
    /// Opens a tuple of `len` elements. Call [`DecodeSource::next`] before each
    /// element.
    fn begin_tuple(&mut self, len: usize) -> CodecResult;
    /// Opens a map.
    ///
    /// Call [`DecodeSource::next`] before each entry. Read the key and then
    /// the value.
    fn begin_map(&mut self) -> CodecResult;
    /// Advances to the next element or entry, or returns `false` at the end of
    /// the container.
    fn next(&mut self) -> CodecResult<bool>;
    /// Opens a struct described by `desc`. Call [`DecodeSource::next_field`]
    /// before each field.
    fn begin_struct(&mut self, desc: &StructDesc) -> CodecResult;
    /// Advances to the next field of a struct or named payload, or returns
    /// `None` at the end.
    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>>;
    /// Opens a variant of enum `type_name` and returns the variant name.
    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>>;
    /// Opens the payload of the current variant with layout `form`.
    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult;
    /// Discards the next value.
    fn skip(&mut self) -> CodecResult;
    /// Closes the innermost open container.
    fn end(&mut self) -> CodecResult;
    /// Formats the path to the current input position, such as `$.items[2]`.
    fn path(&self) -> String;
    /// Drains the input byte count processed since the previous `work` call for
    /// fuel accounting.
    fn work(&mut self) -> usize;
}

/// Custom encoder for a native type marked `#[sand(encode = "manual")]`.
pub trait Encode: Sized + 'static {
    /// Writes `value` to `sink` as a single value.
    fn encode(vm: &mut Vm, value: VmVal<Self>, sink: &mut dyn EncodeSink) -> CodecResult;
}

/// Custom decoder for a native type marked `#[sand(decode = "manual")]`.
pub trait Decode: Sized + 'static {
    /// Reads a single value from `source`.
    fn decode(vm: &mut Vm, source: &mut dyn DecodeSource) -> CodecResult<VmVal<Self>>;
}

/// Function pointer for a manual native [`Encode`] implementation.
pub type EncodeFn = fn(&mut Vm, VmValue, &mut dyn EncodeSink) -> CodecResult;

/// Function pointer for a manual native [`Decode`] implementation.
pub type DecodeFn = fn(&mut Vm, &mut dyn DecodeSource) -> CodecResult<VmValue>;

/// Codec implementation strategy for a native type.
#[derive(Clone, Copy, Debug)]
pub enum NativeCodec<F> {
    /// Structural codec generated from the type's fields or variants.
    Derived,
    /// Custom Rust codec function.
    Manual(F),
}

/// Codec implementations registered for a native type.
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
