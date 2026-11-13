//! The protocol of `Encoder` and `Decoder`, which every format shares.
//!
//! Each call of an `Encode` or `Decode` implementation writes or reads
//! exactly one value. The compiler wraps the method of each hand-written
//! implementation in `enter_impl` and `leave_impl`, so that the encoder or
//! decoder knows which call writes or reads each value. A call that writes
//! or reads no value, or two, fails with a message that names its type and
//! has the path of the value. A failed read returns the input to the start
//! of the value of the innermost running call before the next operation, so
//! that the call can read the value as another type.

use super::{DecodeFormat, field_names, path_text, type_name};
use crate::stdlib::core;
use crate::vm::{Fail, Vm, fail};

/// A running call of a hand-written implementation of `Encode`.
#[derive(Clone, Copy)]
struct EncodeCall {
    /// The runtime definition of the implementing type, for messages.
    def: u32,
    /// The nesting level of the value that the call writes.
    level: usize,
    /// The values at that level before the call.
    values: u32,
}

/// An open level of an `Encoder`: the top level, or a container.
#[derive(Clone, Copy)]
pub(crate) enum LevelKind {
    Top,
    Seq,
    Map,
    /// A struct (`variant == false`, `id` is the definition), or the
    /// payload of variant `id`.
    Fields {
        id: u32,
        variant: bool,
    },
    Some,
}

#[derive(Clone, Copy)]
struct EncodeLevel {
    kind: LevelKind,
    /// The values that the level holds.
    values: u32,
    /// The current field of a struct or of a named payload.
    field: Option<usize>,
}

/// The protocol state of an `Encoder`.
pub(super) struct EncodeProtocol {
    top: EncodeLevel,
    /// The open containers, from the outermost.
    levels: Vec<EncodeLevel>,
    calls: Vec<EncodeCall>,
    /// Whether the encode ended, after which each use is an error.
    pub(super) ended: bool,
}

impl EncodeProtocol {
    /// The state of a new `Encoder`.
    pub(super) const NEW: EncodeProtocol = EncodeProtocol {
        top: EncodeLevel { kind: LevelKind::Top, values: 0, field: None },
        levels: Vec::new(),
        calls: Vec::new(),
        ended: false,
    };
}

/// The error for a use of an `Encoder` after its encode ended.
pub(super) fn encoder_ended<T>() -> Result<T, Fail> {
    fail("invalid", "the encoder is used after its encode ended")
}

/// The error for a use of a `Decoder` after its decode ended.
pub(super) fn decoder_ended<T>() -> Result<T, Fail> {
    fail("invalid", "the decoder is used after its decode ended")
}

impl EncodeProtocol {
    fn level(&self) -> usize {
        self.levels.len()
    }

    /// Returns level `i`: the top level, or an open container.
    fn at(&self, i: usize) -> &EncodeLevel {
        match i.checked_sub(1) {
            Some(i) => &self.levels[i],
            None => &self.top,
        }
    }

    fn current_mut(&mut self) -> &mut EncodeLevel {
        self.levels.last_mut().unwrap_or(&mut self.top)
    }

    /// Returns `true` if a new value at the current level belongs to no call
    /// that already wrote its value, as [`EncodeProtocol::check_value`]
    /// checks.
    pub(super) fn takes_value(&self) -> bool {
        let level = self.level();
        let values = self.at(level).values;
        match self.calls.last() {
            Some(call) if call.level == level => values == call.values,
            _ => level > 0 || values == 0,
        }
    }

    /// Checks that a new value at the current level belongs to no call that
    /// already wrote its value.
    pub(super) fn check_value(&self, vm: &Vm) -> Result<(), Fail> {
        let level = self.level();
        let values = self.at(level).values;
        match self.calls.last() {
            Some(call) if call.level == level && values > call.values => self.violation(vm, call, "wrote 2 values"),
            Some(call) if call.level == level => Ok(()),
            _ if level == 0 && values > 0 => fail("invalid", "$: the encoder already has its value"),
            _ => Ok(()),
        }
    }

    /// Notes a new value at the current level, which opens a container of
    /// `kind` if it is `Some`.
    pub(super) fn add_value(&mut self, open: Option<LevelKind>) {
        self.current_mut().values += 1;
        if let Some(kind) = open {
            self.levels.push(EncodeLevel { kind, values: 0, field: None });
        }
    }

    pub(super) fn field(&mut self, index: usize) {
        self.current_mut().field = Some(index);
    }

    pub(super) fn end(&mut self) {
        self.levels.pop();
    }

    /// Starts a call of the hand-written implementation for the type with
    /// runtime definition `def`, after [`EncodeProtocol::takes_value`].
    pub(super) fn enter(&mut self, def: u32) {
        let level = self.level();
        self.calls.push(EncodeCall { def, level, values: self.at(level).values });
    }

    /// Ends the innermost call if it wrote one value, and returns `false`
    /// if it did not.
    pub(super) fn leave(&mut self) -> bool {
        match self.calls.last() {
            Some(call) if self.level() == call.level && self.at(call.level).values > call.values => {
                self.calls.pop();
                true
            }
            _ => false,
        }
    }

    /// Returns the error of the innermost call, which wrote no value.
    pub(super) fn leave_error(&self, vm: &Vm) -> Result<(), Fail> {
        match self.calls.last() {
            Some(call) => self.violation(vm, call, "wrote no value"),
            None => crate::vm::internal("encode: no running implementation"),
        }
    }

    fn violation<T>(&self, vm: &Vm, call: &EncodeCall, what: &str) -> Result<T, Fail> {
        fail("invalid", format!("{}: Encode for {} {what}", self.path(vm, call), type_name(vm, call.def)))
    }

    /// Returns the path of the value of `call`, such as `$.items[2]`.
    fn path(&self, vm: &Vm, call: &EncodeCall) -> String {
        let mut s = String::from("$");
        for i in 0..=call.level.min(self.level()) {
            let level = self.at(i);
            let index = if i == call.level { call.values } else { level.values.saturating_sub(1) };
            match level.kind {
                LevelKind::Seq => s.push_str(&format!("[{index}]")),
                LevelKind::Fields { id, variant } => {
                    if variant && let Some(v) = vm.prog.variants.get(id as usize) {
                        crate::value::write_key_segment(&mut s, &v.name);
                    }
                    if let Some(name) = level.field.and_then(|f| field_names(vm, id, variant).get(f)) {
                        crate::value::write_key_segment(&mut s, name);
                    } else if variant {
                        s.push_str(&format!("[{index}]"));
                    }
                }
                LevelKind::Top | LevelKind::Map | LevelKind::Some => {}
            }
        }
        s
    }
}

/// How an operation of a `Decoder` changes the values of its levels.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DecodeOp {
    /// Reads a value, or opens one.
    Value,
    /// Reads whether an `Option` is `Some`. `None` is a value, and the
    /// payload of `Some` is the value that the next operation reads.
    Option,
    /// Reads no value: it moves in a container or closes it, or checks the
    /// end of the input.
    Other,
}

/// A running call of a hand-written implementation of `Decode`.
#[derive(Clone, Copy)]
pub(super) struct DecodeCall {
    /// The runtime definition of the implementing type, for messages.
    def: u32,
    /// The nesting level of the value that the call reads.
    level: usize,
    /// The values that the call read at that level.
    read: u32,
    /// The mark of the format at the start of the value.
    mark: usize,
    /// The errors that the decoder had made before the call.
    errors: u32,
}

/// The origin of the error that a call or a decode returns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorOrigin {
    /// The decoder made the error, which has its path.
    Decoder,
    /// The implementation made the error, which gets the path of its value.
    Implementation,
}

/// How a call of a hand-written `Decode`, or a decode, ends.
pub(super) enum Ending {
    /// With its result.
    Result,
    /// With its error, which gets this path.
    Locate(String),
    /// With this error instead of its result.
    Fail(core::Error),
}

impl Ending {
    /// The ending of a broken protocol with the message `message`.
    fn violation(message: String) -> Ending {
        Ending::Fail(core::Error { message, kind: VIOLATION_KIND.into() })
    }
}

/// The protocol state of a `Decoder`, with the path of the part that it
/// reads.
#[derive(Default)]
pub(super) struct DecodeProtocol {
    pub(super) path: super::DecodePath,
    /// The values read at the top level.
    top: u32,
    calls: Vec<DecodeCall>,
    /// The errors that the decoder made and keeps (see [`Decoder`](super::Decoder)).
    pub(super) errors: u32,
    /// The errors that the decoder made before the input last returned to
    /// the start of a value. Each later error is a failed read, after which
    /// the next operation returns to the start of the value of the innermost
    /// running call.
    settled: u32,
    /// The message of the error of a call that broke the protocol, which
    /// each later operation returns.
    violation: Option<String>,
    /// Whether the decode ended, after which each use is an error.
    ended: bool,
}

/// The kind of the error of a broken protocol.
pub(super) const VIOLATION_KIND: &str = "invalid";

impl DecodeProtocol {
    fn level(&self) -> usize {
        self.path.0.len()
    }

    /// Returns `true` if a read failed after the input last returned to the
    /// start of a value.
    #[inline]
    fn failed(&self) -> bool {
        self.errors > self.settled
    }

    /// Notes an error that the decoder made and keeps. It fails the read
    /// that made it.
    pub(super) fn add_error(&mut self) {
        self.errors += 1;
    }

    /// Returns the values read at `level` by the innermost call at that
    /// level, or at the top level. Other levels belong to synthesized and
    /// prelude implementations, which read one value for each part.
    #[inline]
    fn read_at(&mut self, level: usize) -> Option<&mut u32> {
        match self.calls.last_mut() {
            Some(call) if call.level == level => Some(&mut call.read),
            _ if level == 0 => Some(&mut self.top),
            _ => None,
        }
    }

    /// Returns `true` if operation `op` needs no work of the protocol before
    /// it runs: no failed read, no broken protocol, and no second value of a
    /// call.
    #[inline]
    pub(super) fn is_ready(&self, op: DecodeOp) -> bool {
        if self.ended || self.failed() || self.violation.is_some() {
            return false;
        }
        let level = self.level();
        let read = match self.calls.last() {
            Some(call) if call.level == level => call.read,
            _ if level == 0 => self.top,
            _ => 0,
        };
        op == DecodeOp::Other || read == 0
    }

    /// Prepares an operation `op`: returns to the start of the innermost
    /// value after a failure, and checks that a new value belongs to no call
    /// that already read its value. Returns the message of a broken
    /// protocol.
    #[inline]
    pub(super) fn before(&mut self, vm: &Vm, format: &mut dyn DecodeFormat, op: DecodeOp) -> Result<Result<(), String>, Fail> {
        if self.ended || self.failed() || self.violation.is_some() {
            return self.before_cold(vm, format, op);
        }
        Ok(self.check_value(vm, op))
    }

    #[cold]
    fn before_cold(&mut self, vm: &Vm, format: &mut dyn DecodeFormat, op: DecodeOp) -> Result<Result<(), String>, Fail> {
        if self.ended {
            return decoder_ended();
        }
        if let Some(v) = &self.violation {
            return Ok(Err(v.clone()));
        }
        let (mark, level) = match self.calls.last() {
            Some(c) => (Some(c.mark), c.level),
            None => (None, 0),
        };
        self.rewind(format, mark, level);
        Ok(self.check_value(vm, op))
    }

    /// Checks that a value of operation `op` belongs to no call that already
    /// read its value.
    #[inline]
    fn check_value(&mut self, vm: &Vm, op: DecodeOp) -> Result<(), String> {
        let level = self.level();
        if op != DecodeOp::Other && self.read_at(level).is_some_and(|n| *n > 0) {
            return Err(self.violate(vm, "read 2 values"));
        }
        Ok(())
    }

    /// Notes the end of a successful operation that began at `level`, and
    /// that read a value there if `read`.
    #[inline]
    pub(super) fn after(&mut self, level: usize, read: bool) {
        if read && let Some(n) = self.read_at(level) {
            *n += 1;
        }
    }

    /// Returns to mark `mark` of `format`, or to the start of the input if
    /// `mark` is `None`, at the start of a value at `level`.
    fn rewind(&mut self, format: &mut dyn DecodeFormat, mark: Option<usize>, level: usize) {
        match mark {
            Some(mark) => format.rewind(mark),
            None => format.rewind_to_start(),
        }
        self.path.0.truncate(level);
        if let Some(n) = self.read_at(level) {
            *n = 0;
        }
        self.settled = self.errors;
    }

    /// Records a broken protocol of the innermost call at the current level,
    /// or of the top level, and returns its message.
    fn violate(&mut self, vm: &Vm, what: &str) -> String {
        let level = self.level();
        let path = path_text(vm, &self.path.0);
        let message = match self.calls.last().filter(|c| c.level == level) {
            Some(call) => format!("{path}: Decode for {} {what}", type_name(vm, call.def)),
            None => format!("{path}: the decoder already read its value"),
        };
        self.violation = Some(message.clone());
        message
    }

    /// Starts a call of the hand-written implementation for the type with
    /// runtime definition `def`.
    pub(super) fn enter(&mut self, vm: &Vm, format: &mut dyn DecodeFormat, def: u32) -> Result<(), Fail> {
        // A broken protocol stays in the state, and each later operation
        // returns it, so the call starts in any case.
        let _ = self.before(vm, format, DecodeOp::Value)?;
        self.calls.push(DecodeCall { def, level: self.level(), read: 0, mark: format.mark(), errors: self.errors });
        Ok(())
    }

    /// Ends the innermost call, whose result is an error from `error` if
    /// that is `Some`. The decoder then keeps only the errors that it made
    /// before the call, and the caller adds the error that the call returns,
    /// if any, with [`DecodeProtocol::add_error`].
    pub(super) fn leave(&mut self, vm: &Vm, format: &mut dyn DecodeFormat, error: Option<ErrorOrigin>) -> Result<Ending, Fail> {
        if self.ended {
            return decoder_ended();
        }
        let Some(&call) = self.calls.last() else { return crate::vm::internal("decode: no running implementation") };
        let ending = if let Some(v) = &self.violation {
            Ending::violation(v.clone())
        } else if let Some(origin) = error {
            self.path.0.truncate(call.level);
            match origin {
                ErrorOrigin::Decoder => Ending::Result,
                ErrorOrigin::Implementation => Ending::Locate(self.path_text(vm)),
            }
        } else {
            if self.failed() {
                self.rewind(format, Some(call.mark), call.level);
            }
            if self.level() != call.level || self.calls.last().is_some_and(|c| c.read == 0) {
                self.path.0.truncate(call.level);
                Ending::violation(self.violate(vm, "read no value"))
            } else {
                Ending::Result
            }
        };
        self.calls.pop();
        format.release(call.mark);
        (self.errors, self.settled) = (call.errors, self.settled.min(call.errors));
        if error.is_none() && matches!(ending, Ending::Result) {
            // The value of the call is the value of the call or the level
            // that called it.
            self.after(call.level, true);
        }
        Ok(ending)
    }

    /// Returns how the decode ends, whose result is an error from `error` if
    /// that is `Some`. After [`Ending::Result`] for a value, the caller
    /// checks that no input follows it. Then it ends the decode with
    /// [`DecodeProtocol::close`].
    pub(super) fn end(&mut self, vm: &Vm, format: &mut dyn DecodeFormat, error: Option<ErrorOrigin>) -> Result<Ending, Fail> {
        if self.ended {
            return decoder_ended();
        }
        if !self.calls.is_empty() {
            return fail("invalid", "`Decoder.finish` ends a decode, so a `Decode` implementation cannot call it");
        }
        if let Some(v) = &self.violation {
            return Ok(Ending::violation(v.clone()));
        }
        match error {
            Some(ErrorOrigin::Decoder) => return Ok(Ending::Result),
            Some(ErrorOrigin::Implementation) => {
                self.path.0.clear();
                return Ok(Ending::Locate(self.path_text(vm)));
            }
            None => {}
        }
        if self.failed() {
            self.rewind(format, None, 0);
        }
        if self.top == 0 {
            return Ok(Ending::violation("$: the decoder read no value".into()));
        }
        Ok(Ending::Result)
    }

    /// Ends the decode, after which each use is an error.
    pub(super) fn close(&mut self) {
        self.ended = true;
    }

    /// Returns the path of the current part, such as `$.items[2]`.
    pub(super) fn path_text(&self, vm: &Vm) -> String {
        path_text(vm, &self.path.0)
    }
}
