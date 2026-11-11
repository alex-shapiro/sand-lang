//! Sand values in a Rust form that does not reference the VM.

use crate::stdlib::time::{PlainMonthDay, PlainYearMonth, Span, TimeZone, ZonedDateTime};
use std::sync::Arc;

/// The Rust form of a Sand value whose type implements `Encode`.
///
/// A `Value` does not reference the VM, so it remains valid after the run ends.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// The unit value `()`.
    Unit,
    /// A `bool`.
    Bool(bool),
    /// An `int`.
    Int(i64),
    /// A `float`.
    Float(f64),
    /// A `duration`.
    Duration(jiff::SignedDuration),
    /// A `str`.
    Str(String),
    /// A `bytes` value.
    Bytes(Vec<u8>),
    /// A list.
    List(Vec<Value>),
    /// A tuple.
    Tuple(Vec<Value>),
    /// A set, with its elements in iteration order.
    Set(Vec<Value>),
    /// A map, with its entries in iteration order.
    Map(Vec<(Value, Value)>),
    /// An integer range: `start..end`, or `start..=end` if `inclusive` is true.
    Range { start: i64, end: i64, inclusive: bool },
    /// A struct.
    Struct(Box<StructValue>),
    /// An enum variant.
    Variant(Box<VariantValue>),
    /// A `time.Instant`.
    Instant(jiff::Timestamp),
    /// A `time.PlainDateTime`.
    PlainDateTime(jiff::civil::DateTime),
    /// A `time.PlainDate`.
    PlainDate(jiff::civil::Date),
    /// A `time.PlainTime`.
    PlainTime(jiff::civil::Time),
    /// A `time.PlainYearMonth`.
    PlainYearMonth(PlainYearMonth),
    /// A `time.PlainMonthDay`.
    PlainMonthDay(PlainMonthDay),
    /// A `time.ZonedDateTime`.
    ZonedDateTime(Box<ZonedDateTime>),
    /// A `time.Span`.
    Span(Box<Span>),
    /// A `time.TimeZone`.
    TimeZone(TimeZone),
    /// A value of a native value type without its own variant, from the
    /// standard library or a host.
    Native(Box<NativeValue>),
    /// A value whose type does not implement `Encode`, such as a function, a
    /// task, a channel, or a native object. It holds only the type name.
    Opaque { type_name: Arc<str> },
}

/// A struct with its type name and fields.
#[derive(Clone, Debug, PartialEq)]
pub struct StructValue {
    /// The module-qualified type name, such as `shop.Order`.
    pub type_name: Arc<str>,
    /// The field names and values in declaration order.
    pub fields: Vec<(Arc<str>, Value)>,
}

/// An enum variant with its enum name, variant name, and payload.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantValue {
    /// The module-qualified enum name, such as `shop.Status`.
    pub type_name: Arc<str>,
    /// The variant name, such as `Shipped`.
    pub name: Arc<str>,
    /// The payload of the variant.
    pub payload: Payload,
}

/// The payload of an enum variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Positional fields in declaration order. A variant without a payload has
    /// no fields.
    Positional(Vec<Value>),
    /// Named fields in declaration order.
    Named(Vec<(Arc<str>, Value)>),
}

/// A value of a native value type with its type name and text.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeValue {
    /// The module-qualified type name, such as `net.SocketAddr`.
    pub type_name: Arc<str>,
    /// The `Display` text of the value.
    /// [`ValueType::parse`](crate::vm::value::ValueType::parse) turns it back
    /// into an equal value.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn value_is_at_most_32_bytes() {
        assert!(size_of::<Value>() <= 32, "Value is {} bytes", size_of::<Value>());
    }
}
