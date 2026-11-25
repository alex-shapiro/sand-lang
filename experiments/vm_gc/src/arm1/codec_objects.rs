//! The `Encoder` and `Decoder` heap objects of Arms 1 and 2, which hold
//! codec state while a custom `encode` or `decode` method runs.

use super::heap::Value;
use super::vm::Vm;
use crate::shared::Result;
use crate::shared::format::{Event, FormatReader, FormatWriter};
use std::any::Any;
use std::mem::size_of;

/// Runs a nested encode for an `Encoder` of one format.
pub type EncodeWalk = fn(&mut Vm, Value, Value) -> Result<()>;
/// Runs a nested decode of `program.types[ty]` for a `Decoder` of one format.
pub type DecodeWalk = fn(&mut Vm, Value, u16) -> Result<Value>;

/// Format state of an encode: the writer and its output.
pub struct EncodeState<W> {
    pub writer: W,
    pub out: String,
}

/// The object-safe view of an [`EncodeState`] for Sand `Encoder` methods.
pub trait DynEncodeState: Any + Send {
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
    pub state: Option<Box<dyn DynEncodeState>>,
    pub walk: EncodeWalk,
}

impl EncoderObj {
    pub fn size(&self) -> usize {
        size_of::<EncoderObj>()
    }
}

/// The Sand `Decoder`: holds the input and position while a callback runs.
pub struct DecoderObj {
    pub input: Value,
    pub pos: usize,
    /// The input position up to which the decode has charged fuel.
    pub charged: usize,
    pub scratch: String,
    pub reader: &'static dyn FormatReader,
    pub walk: DecodeWalk,
}

impl DecoderObj {
    pub fn size(&self) -> usize {
        size_of::<DecoderObj>()
    }
}
