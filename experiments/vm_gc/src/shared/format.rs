//! The open streaming codec interface between value walkers and formats.
//!
//! A format implements [`FormatWriter`] and [`FormatReader`]. Neither trait
//! sees VM values, so a format works unchanged with every arm.

use super::isa::Ty;
use super::{BYTES_PER_FUEL, Fail, Result, charge};

/// One write to an `Encoder`.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    OpenList(usize),
    CloseList,
    /// Opens a map or struct with the given entry count.
    OpenMap(usize),
    CloseMap,
    /// A field or map key. The next event is its value.
    Key(&'a str),
    /// A payload-less enum variant.
    Tag(&'a str),
    /// Opens an enum variant. The next event is its payload.
    OpenVariant(&'a str),
    CloseVariant,
}

impl Event<'_> {
    /// Byte length of the text that the event carries.
    #[inline(always)]
    pub fn text_len(&self) -> usize {
        match self {
            Event::Str(s) | Event::Key(s) | Event::Tag(s) | Event::OpenVariant(s) => s.len(),
            _ => 0,
        }
    }
}

/// A target format of the encode walkers.
pub trait FormatWriter: Send + 'static {
    /// Appends `event` to `out`. The walker reserves
    /// [`max_len`](FormatWriter::max_len) bytes of capacity first, so `write`
    /// never grows `out`.
    fn write(&mut self, out: &mut String, event: Event<'_>);

    /// Upper bound on the bytes that `write` appends for an event of this
    /// kind whose text is `text_len` bytes long.
    fn max_len(&self, event: &Event<'_>, text_len: usize) -> usize;
}

/// A decoded string: a byte range of the input, or the scratch buffer if the
/// input escapes characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrSpan {
    Range(usize, usize),
    Scratch,
}

impl StrSpan {
    #[inline]
    pub fn resolve<'a>(self, src: &'a str, scratch: &'a str) -> &'a str {
        match self {
            StrSpan::Range(start, end) => &src[start..end],
            StrSpan::Scratch => scratch,
        }
    }
}

/// A source format of the decode walkers: a stateless scanner over an input
/// string and a byte offset.
pub trait FormatReader: std::any::Any + Send + Sync {
    fn int(&self, src: &str, pos: &mut usize) -> Result<i64>;
    fn float(&self, src: &str, pos: &mut usize) -> Result<f64>;
    fn bool(&self, src: &str, pos: &mut usize) -> Result<bool>;
    /// Consumes a null value and returns `true`, or returns `false`.
    fn null(&self, src: &str, pos: &mut usize) -> Result<bool>;
    fn str(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<StrSpan>;
    fn open_list(&self, src: &str, pos: &mut usize) -> Result<()>;
    /// Returns `true` before each item and consumes the list end.
    fn next_item(&self, src: &str, pos: &mut usize) -> Result<bool>;
    /// Opens a map or struct.
    fn open_map(&self, src: &str, pos: &mut usize) -> Result<()>;
    /// Reads the next key, or consumes the map end and returns `None`.
    fn next_key(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<Option<StrSpan>>;
    /// Reads a variant name and whether a payload follows it.
    fn open_variant(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<(StrSpan, bool)>;
    /// Ends a variant that has a payload.
    fn close_variant(&self, src: &str, pos: &mut usize) -> Result<()>;
    /// Skips one value.
    fn skip(&self, src: &str, pos: &mut usize) -> Result<()>;
    /// Fails unless only whitespace follows `pos`.
    fn end(&self, src: &str, pos: &mut usize) -> Result<()>;
}

/// Charges fuel for the input bytes from `charged` to `pos`, then moves
/// `charged` forward by the bytes it charged. A decode walker calls it as it
/// moves through its input, so formats do not meter their own loops.
#[inline(always)]
pub fn charge_input(fuel: &mut u64, charged: &mut usize, pos: usize) -> Result<()> {
    let units = (pos - *charged) / BYTES_PER_FUEL;
    if units > 0 {
        charge(fuel, units as u64)?;
        *charged += units * BYTES_PER_FUEL;
    }
    Ok(())
}

#[cold]
#[inline(never)]
pub fn syntax_error<T>(format: &str, what: &str, pos: usize) -> Result<T> {
    Err(Fail::Error(format!("{format}: expected {what} at byte {pos}")))
}

/// Returns the mask of the record fields that the input left out and that
/// default to `None`, or fails if the input left out a required field.
/// `seen` has bit `i` set for each field `i` that the input gave.
#[cold]
#[inline(never)]
pub fn absent_fields<'a>(mut fields: impl Iterator<Item = (&'a str, &'a Ty)>, seen: u64) -> Result<u64> {
    let mut absent = 0;
    for i in 0.. {
        let Some((name, ty)) = fields.next() else { break };
        if seen & (1 << i) == 0 {
            if !matches!(ty, Ty::Option(_)) {
                return Err(Fail::error(format!("missing field `{name}`")));
            }
            absent |= 1 << i;
        }
    }
    Ok(absent)
}

/// Returns the mask with bits `0..n` set.
#[inline(always)]
pub fn all_fields(n: usize) -> u64 {
    if n >= 64 { u64::MAX } else { (1 << n) - 1 }
}

/// Finds the struct field named `key`, trying `hint` first because inputs
/// usually list fields in declaration order.
#[inline]
pub fn find_field<'a>(names: impl Iterator<Item = &'a str> + Clone, key: &str, hint: usize) -> Option<usize> {
    if names.clone().nth(hint) == Some(key) {
        return Some(hint);
    }
    names.into_iter().position(|n| n == key)
}
