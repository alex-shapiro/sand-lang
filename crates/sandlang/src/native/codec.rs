//! Rust interfaces for the Sand `Encode` and `Decode` traits.
//!
//! The encode walker visits a Sand value and writes each part to an
//! [`EncodeSink`]. The decode walker reads each part from a [`DecodeSource`].
//! Both JSON text and [`Value`](crate::value::Value) implement a sink and a
//! source.
//!
//! A native type implements `Encode` or `Decode` in one of two ways:
//!
//! - `#[sand(derive(Encode, Decode))]` derives the traits from the type's
//!   structure.
//! - The module source declares an `impl` block with a native method. Its Rust
//!   function writes through [`Encoder::write`] or reads through
//!   [`Decoder::read`].
//!
//! ```
//! # use sandlang::native::NativeModule;
//! # use sandlang::native::codec::{Decoder, Encoder};
//! # use sandlang::native::convert::{HostError, IntoVmValue};
//! # use sandlang::native::ty::VmVal;
//! # use sandlang::vm::value::VmValue;
//! # use sandlang::vm::{Fail, Vm};
//! /// An amount of money in cents.
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, sandlang::SandStruct)]
//! #[sand(module = "bank")]
//! pub struct Money {
//!     cents: i64,
//! }
//!
//! let mut module = NativeModule::new("bank");
//! module.bind::<Money>();
//! module.source(
//!     "impl Encode for Money {\n    native fn encode(self, encoder: Encoder) -> Result<()>\n}\n\
//!      impl Decode for Money {\n    native fn decode(decoder: Decoder) -> Result<Money>\n}\n",
//! );
//! module.function("Money.encode", |vm: &mut Vm, money: VmVal<Money>, encoder: VmVal<Encoder>| -> Result<(), HostError> {
//!     let Some(VmValue::Int(cents)) = vm.heap.field(money.raw(), 0) else { return Err(Fail::internal("not money").into()) };
//!     Encoder::write(vm, encoder, |_, sink| sink.int(cents))
//! });
//! module.function("Money.decode", |vm: &mut Vm, decoder: VmVal<Decoder>| -> Result<VmVal<Money>, HostError> {
//!     Decoder::read(vm, decoder, |vm, source| {
//!         let cents = source.int()?;
//!         Ok(VmVal::from_raw(Money { cents }.into_vm_value(vm)?))
//!     })
//! });
//! ```

use super::convert::HostError;
use crate::value::{StructDesc, VariantDesc};
use crate::vm::Fail;
use std::borrow::Cow;
use std::sync::Arc;

pub use crate::stdlib::codec::{Decoder, Encoder};

/// Result returned by [`EncodeSink`] and [`DecodeSource`] methods.
pub type CodecResult<T = ()> = Result<T, HostError<Fail>>;

/// Returns a memory limit error.
#[cold]
pub(crate) fn memory_limit_error<T>() -> CodecResult<T> {
    Err(Fail::limit(crate::engine::LimitKind::Memory).into())
}

/// Destination that receives a Sand value from the encode walker.
///
/// The walker writes each scalar with a single method call. To write a
/// container, the walker calls a `begin_*` method. It then writes each part and
/// calls [`EncodeSink::end`].
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
    /// Opens `Some`. The caller writes the payload value next.
    fn begin_some(&mut self) -> CodecResult;
    fn begin_list(&mut self, len: usize) -> CodecResult;
    fn begin_tuple(&mut self, len: usize) -> CodecResult;
    fn begin_set(&mut self, len: usize) -> CodecResult;
    /// Opens a map of `len` entries.
    ///
    /// For each entry, the caller writes the key and then the value.
    fn begin_map(&mut self, len: usize) -> CodecResult;
    /// Opens the struct described by `desc`.
    ///
    /// The caller writes each field in declaration order.
    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult;
    /// Opens the enum variant described by `desc` with `len` payload values.
    ///
    /// The caller writes each payload value in declaration order.
    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult;
    /// Closes the innermost open container or `Some` wrapper.
    fn end(&mut self) -> CodecResult;
}

/// Kind of the next value available from a `Decoder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, crate::SandEnum)]
#[sand(module = "prelude", derive(Encode, Decode))]
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
    /// A native value.
    Native,
    /// An enum variant other than `None`.
    Variant,
}

/// Layout of an enum variant's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadForm {
    /// Empty payload.
    Unit,
    /// Positional payload with the given value count.
    ///
    /// The caller calls [`DecodeSource::next`] before each value.
    Positional(usize),
    /// Struct-like payload with named fields.
    ///
    /// The caller calls [`DecodeSource::next_field`] before each field.
    Named,
}

/// Input stream that supplies a Sand value to the decode walker.
///
/// The walker requests each part according to the target type. A container
/// request opens the container. [`DecodeSource::end`] closes it. If the input
/// does not match a request, the source returns a `"parse"` error at the
/// current path.
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
    /// Reads an `Option` tag.
    ///
    /// For `None`, consumes the value and returns `false`. For `Some`, opens
    /// the wrapper and returns `true`. The caller then reads the payload and
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
    /// Advances to the next element or entry.
    ///
    /// Returns `false` at the end of the container.
    fn next(&mut self) -> CodecResult<bool>;
    /// Opens a struct described by `desc`. Call [`DecodeSource::next_field`]
    /// before each field.
    fn begin_struct(&mut self, desc: &StructDesc) -> CodecResult;
    /// Advances to the next field of a struct or named payload.
    ///
    /// Returns `None` at the end of the fields.
    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>>;
    /// Opens a variant of enum `type_name`. Returns the variant name.
    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>>;
    /// Opens the payload of the current variant with layout `form`.
    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult;
    /// Discards the next value.
    fn skip(&mut self) -> CodecResult;
    /// Closes the innermost open container.
    fn end(&mut self) -> CodecResult;
    /// Formats the path to the current input position.
    ///
    /// Paths start at `$`, as in `$.items[2]`.
    fn path(&self) -> String;
    /// Drains the input byte count consumed since the previous
    /// `take_consumed_bytes` call for fuel accounting.
    fn take_consumed_bytes(&mut self) -> usize;
}
