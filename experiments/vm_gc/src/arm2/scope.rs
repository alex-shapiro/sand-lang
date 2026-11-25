//! Lifetime tokens and the scoped host API of Arm 2.
//!
//! Arm 2 runs on the Arm 1 heap and interpreter. Host code sees heap handles
//! only as [`Value<'gc>`], bound to a [`NoGcScope`]. A function that may
//! collect garbage takes a [`GcScope`], so the borrow checker rejects a bound
//! handle that the caller uses after it passes `gc.reborrow()`:
//!
//! ```compile_fail,E0502
//! use vm_gc::arm2::scope::{GcScope, Value, Vm};
//! fn stale<'a, 's>(vm: &mut Vm, mut gc: GcScope<'a, 's>) -> vm_gc::shared::Result<()> {
//!     let s = vm.new_str("a string longer than inline", gc.reborrow())?.unbind().bind(gc.nogc());
//!     let _other = vm.new_str("another string longer than inline", gc.reborrow())?;
//!     vm.str(&s, gc.nogc())?; // error: `s` borrows `gc`, which `reborrow` borrowed mutably
//!     Ok(())
//! }
//! ```
//!
//! To keep a handle across a collection, root it with [`Value::scope`] and
//! read it back with [`Scoped::get`]. To pass a handle into a function that
//! may collect, call [`Value::unbind`] at the call site; the callee calls
//! [`Value::bind`] on entry. The compiler does not check that a callee binds
//! its arguments, or that unbound values stay rooted.

use crate::arm1::codec::{DecoderObj, EncoderObj};
use crate::arm1::heap::{HostObject, Obj, Value as RawValue};
use crate::arm1::vm::{self as raw, Args, AsyncRoot};
use crate::shared::format::{FormatReader, StrSpan, charge_input};
use crate::shared::isa::Program;
use crate::shared::{Fail, Result};
use std::future::Future;
use std::marker::PhantomData;
use std::mem::size_of;
use std::pin::Pin;
use std::sync::Arc;

/// An invariant lifetime marker.
type Invariant<'s> = PhantomData<fn(&'s ()) -> &'s ()>;

/// Permission to collect garbage. `'s` is the lifetime of the roots that
/// the current native call may create.
pub struct GcScope<'a, 's> {
    _gc: PhantomData<&'a mut ()>,
    _roots: Invariant<'s>,
}

/// Proof that no garbage collection runs while it is borrowed.
#[derive(Clone, Copy)]
pub struct NoGcScope<'a, 's> {
    _gc: PhantomData<&'a ()>,
    _roots: Invariant<'s>,
}

impl<'a, 's> GcScope<'a, 's> {
    /// Lends the scope to a callee that may collect. Handles bound to `self`
    /// become unusable until the returned scope ends.
    #[inline(always)]
    pub fn reborrow(&mut self) -> GcScope<'_, 's> {
        GcScope { _gc: PhantomData, _roots: PhantomData }
    }

    /// Returns a token for reads that cannot collect.
    #[inline(always)]
    pub fn nogc(&self) -> NoGcScope<'_, 's> {
        NoGcScope { _gc: PhantomData, _roots: PhantomData }
    }

    #[inline(always)]
    pub fn into_nogc(self) -> NoGcScope<'a, 's> {
        NoGcScope { _gc: PhantomData, _roots: PhantomData }
    }
}

/// Runs `f` with the root scope of one native call.
#[inline(always)]
pub(super) fn enter<R>(f: impl for<'a, 's> FnOnce(GcScope<'a, 's>) -> R) -> R {
    f(GcScope { _gc: PhantomData, _roots: PhantomData })
}

/// An immediate value or a heap handle that is valid while `'gc` lasts.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(transparent)]
pub struct Value<'gc> {
    raw: RawValue,
    _gc: PhantomData<&'gc ()>,
}

impl<'gc> Value<'gc> {
    pub const UNIT: Value<'static> = Value::new_raw(RawValue::Unit);
    pub const NONE: Value<'static> = Value::new_raw(RawValue::None);

    #[inline(always)]
    const fn new_raw(raw: RawValue) -> Value<'gc> {
        Value { raw, _gc: PhantomData }
    }

    #[inline(always)]
    pub fn int(i: i64) -> Value<'static> {
        Value::new_raw(RawValue::Int(i))
    }

    #[inline(always)]
    pub fn bool(b: bool) -> Value<'static> {
        Value::new_raw(RawValue::Bool(b))
    }

    #[inline(always)]
    pub fn float(f: f64) -> Value<'static> {
        Value::new_raw(RawValue::Float(f))
    }

    pub fn as_int(self) -> Option<i64> {
        match self.raw {
            RawValue::Int(i) => Some(i),
            _ => None,
        }
    }

    pub fn as_float(self) -> Option<f64> {
        match self.raw {
            RawValue::Float(f) => Some(f),
            _ => None,
        }
    }

    pub fn as_bool(self) -> Option<bool> {
        match self.raw {
            RawValue::Bool(b) => Some(b),
            _ => None,
        }
    }

    pub fn is_unit(self) -> bool {
        self.raw == RawValue::Unit
    }

    /// Detaches the value from its scope, to pass it into a function that may collect.
    #[inline(always)]
    pub fn unbind(self) -> Value<'static> {
        Value::new_raw(self.raw)
    }

    /// Attaches the value to `gc`.
    #[inline(always)]
    pub fn bind<'b>(self, _gc: NoGcScope<'b, '_>) -> Value<'b> {
        Value::new_raw(self.raw)
    }

    /// Reports the value to the collector, for [`HostObject::trace`].
    #[inline]
    pub fn trace(self, visit: &mut dyn FnMut(RawValue)) {
        visit(self.raw);
    }

    /// Roots the value until the current native call returns.
    #[inline(always)]
    pub fn scope<'s>(self, vm: &mut Vm, _gc: GcScope<'_, 's>) -> Result<Scoped<'s>> {
        vm.raw.push_root(self.raw)?;
        Ok(Scoped { raw: self.raw, _roots: PhantomData })
    }

    #[inline(always)]
    pub(super) fn raw(self) -> RawValue {
        self.raw
    }

    #[inline(always)]
    pub(super) fn from_raw(raw: RawValue, _gc: NoGcScope<'gc, '_>) -> Value<'gc> {
        Value::new_raw(raw)
    }
}

/// A rooted value that stays valid across collections until its native call returns.
///
/// The Arm 1 collector never moves objects, so a rooted handle never
/// changes and `Scoped` keeps a copy of it next to the proof of rooting.
#[derive(Clone, Copy, Debug)]
pub struct Scoped<'s> {
    raw: RawValue,
    _roots: Invariant<'s>,
}

impl<'s> Scoped<'s> {
    #[inline(always)]
    pub fn get<'a>(self, _vm: &Vm, _gc: NoGcScope<'a, 's>) -> Value<'a> {
        Value::new_raw(self.raw)
    }

    /// Returns the text of the rooted string.
    #[inline(always)]
    pub fn str<'r>(&'r self, vm: &'r Vm, _gc: NoGcScope<'_, 's>) -> Result<&'r str> {
        vm.raw.heap.str(&self.raw).ok_or_else(|| raw::type_error("str"))
    }
}

/// Copies up to 8 values into a raw argument array.
#[inline(always)]
fn raw_args(args: &[Value<'_>]) -> Result<([RawValue; 8], usize)> {
    let mut raw = [RawValue::Unit; 8];
    let slots = raw.get_mut(..args.len()).ok_or_else(|| Fail::error("too many callback arguments"))?;
    for (slot, v) in slots.iter_mut().zip(args) {
        *slot = v.raw;
    }
    Ok((raw, args.len()))
}

/// The scoped host API of an Arm 2 engine.
pub struct Vm<'v> {
    raw: &'v mut raw::Vm,
}

impl<'v> Vm<'v> {
    #[inline(always)]
    pub(super) fn new(raw: &'v mut raw::Vm) -> Vm<'v> {
        Vm { raw }
    }

    #[inline(always)]
    pub fn arg<'a>(&self, args: Args, i: usize, _gc: NoGcScope<'a, '_>) -> Value<'a> {
        Value::new_raw(self.raw.arg(args, i))
    }

    pub fn program(&self) -> &Arc<Program> {
        &self.raw.program
    }

    #[inline(always)]
    pub fn charge(&mut self, units: u64) -> Result<()> {
        self.raw.charge(units)
    }

    /// Charges fuel for the input bytes from `charged` to `pos`.
    #[inline(always)]
    pub fn charge_input(&mut self, charged: &mut usize, pos: usize) -> Result<()> {
        charge_input(&mut self.raw.fuel, charged, pos)
    }

    #[inline(always)]
    pub fn check_stack(&self) -> Result<()> {
        self.raw.check_stack()
    }

    #[inline(always)]
    pub fn str<'r>(&'r self, v: &'r Value<'_>, _gc: NoGcScope<'_, '_>) -> Result<&'r str> {
        self.raw.heap.str(&v.raw).ok_or_else(|| raw::type_error("str"))
    }

    #[inline(always)]
    pub fn field<'a>(&self, v: Value<'_>, idx: usize, _gc: NoGcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.field(v.raw, idx).map(Value::new_raw)
    }

    #[inline(always)]
    pub fn list_len(&self, list: Value<'_>, _gc: NoGcScope<'_, '_>) -> Result<usize> {
        self.raw.list_len(list.raw)
    }

    #[inline(always)]
    pub fn list_get<'a>(&self, list: Value<'_>, i: usize, _gc: NoGcScope<'a, '_>) -> Result<Option<Value<'a>>> {
        Ok(self.raw.list_get(list.raw, i)?.map(Value::new_raw))
    }

    pub fn host<T: HostObject>(&self, v: Value<'_>, _gc: NoGcScope<'_, '_>) -> Result<&T> {
        self.raw.host(v.raw)
    }

    pub fn host_mut<T: HostObject>(&mut self, v: Value<'_>, _gc: NoGcScope<'_, '_>) -> Result<&mut T> {
        self.raw.host_mut(v.raw)
    }

    pub fn new_str<'a>(&mut self, s: &str, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.new_str(s).map(Value::new_raw)
    }

    pub fn new_list<'a>(&mut self, capacity: usize, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.new_list(capacity).map(Value::new_raw)
    }

    /// Appends `v` to `list`. The caller must root `list` and `v`.
    pub fn list_push(&mut self, list: Value<'_>, v: Value<'_>, _gc: GcScope<'_, '_>) -> Result<()> {
        self.raw.list_push(list.raw, v.raw)
    }

    pub fn new_host<'a>(&mut self, obj: Box<dyn HostObject>, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.new_host(obj).map(Value::new_raw)
    }

    /// Calls `callee` synchronously. The callee cannot wait on async natives.
    pub fn call<'a>(&mut self, callee: Value<'_>, args: &[Value<'_>], _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let (raw, n) = raw_args(args)?;
        self.raw.call(callee.raw, &raw[..n]).map(Value::new_raw)
    }

    /// Calls function `func` of the program synchronously.
    pub fn call_function<'a>(&mut self, func: u16, args: &[Value<'_>], _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let (raw, n) = raw_args(args)?;
        self.raw.call_function(func, RawValue::Unit, &raw[..n]).map(Value::new_raw)
    }

    pub fn collect(&mut self, _gc: GcScope<'_, '_>) {
        self.raw.collect();
    }

    /// Roots `v` for an async native until [`Vm::take_async_root`].
    pub fn async_root(&mut self, v: Value<'_>, _gc: GcScope<'_, '_>) -> Result<AsyncRoot> {
        self.raw.async_root(v.raw)
    }

    pub fn take_async_root<'a>(&mut self, root: AsyncRoot, _gc: NoGcScope<'a, '_>) -> Value<'a> {
        Value::new_raw(self.raw.take_async_root(root))
    }

    // ---------------------------------------------------------------------
    // Codec core
    // ---------------------------------------------------------------------

    #[inline(always)]
    pub(super) fn reserve(&mut self, bytes: usize, _gc: GcScope<'_, '_>) -> Result<()> {
        self.raw.reserve(bytes)
    }

    #[inline(always)]
    pub(super) fn release(&mut self, bytes: usize) {
        self.raw.release(bytes)
    }

    #[inline(always)]
    pub(super) fn root_mark(&self) -> usize {
        self.raw.root_mark()
    }

    /// Truncates the root stack to `mark`. Invalidates every [`Scoped`] root
    /// and root index created after `mark`; the compiler does not check this.
    #[inline(always)]
    pub(super) fn truncate_roots(&mut self, mark: usize) {
        self.raw.truncate_roots(mark)
    }

    #[inline(always)]
    pub(super) fn push_root(&mut self, v: Value<'_>, _gc: GcScope<'_, '_>) -> Result<usize> {
        self.raw.push_root(v.raw)
    }

    #[inline(always)]
    pub(super) fn root<'a>(&self, index: usize, _gc: NoGcScope<'a, '_>) -> Value<'a> {
        Value::new_raw(self.raw.root(index))
    }

    #[inline(always)]
    pub(super) fn set_root(&mut self, index: usize, v: Value<'_>) {
        self.raw.set_root(index, v.raw)
    }

    #[inline(always)]
    pub(super) fn obj(&self, v: Value<'_>, _gc: NoGcScope<'_, '_>) -> Option<&Obj> {
        match v.raw {
            RawValue::Obj(h) => Some(self.raw.heap.get(h)),
            _ => None,
        }
    }

    /// Returns payload field `i` of a variant object.
    #[inline(always)]
    pub(super) fn variant_field<'a>(&self, v: Value<'_>, i: usize, gc: NoGcScope<'a, '_>) -> Result<Value<'a>> {
        match self.obj(v, gc) {
            Some(Obj::Variant { fields, .. }) => Ok(Value::new_raw(fields[i])),
            _ => Err(raw::type_error("variant")),
        }
    }

    /// Returns entry `i` of a map object.
    #[inline(always)]
    pub(super) fn map_entry<'a>(&self, v: Value<'_>, i: usize, gc: NoGcScope<'a, '_>) -> Result<Option<(Value<'a>, Value<'a>)>> {
        match self.obj(v, gc) {
            Some(Obj::Map(entries)) => Ok(entries.get(i).map(|&(k, v)| (Value::new_raw(k), Value::new_raw(v)))),
            _ => Err(raw::type_error("map")),
        }
    }

    pub(super) fn new_str_from_reserved<'a>(&mut self, s: String, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.new_str_from_reserved(s).map(Value::new_raw)
    }

    /// Creates a string from `span` of the string `input` or of `scratch`.
    #[inline]
    pub(super) fn new_str_from_span<'a>(
        &mut self,
        input: Value<'_>,
        span: StrSpan,
        scratch: &str,
        _gc: GcScope<'a, '_>,
    ) -> Result<Value<'a>> {
        let text = span.resolve(self.raw.heap.str(&input.raw).ok_or_else(|| raw::type_error("str"))?, scratch);
        if let Some(short) = crate::shared::ShortStr::new(text) {
            return Ok(Value::new_raw(RawValue::Short(short)));
        }
        self.raw.reserve_object(text.len())?;
        let text: Box<str> = span.resolve(self.raw.heap.str(&input.raw).expect("input"), scratch).into();
        Ok(Value::new_raw(self.raw.insert(Obj::Str(text))))
    }

    /// Moves the values above `mark` on the root stack into a new list.
    pub(super) fn new_list_from_roots<'a>(&mut self, mark: usize, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let n = self.raw.root_mark() - mark;
        self.raw.reserve_object(n * size_of::<RawValue>())?;
        let items = self.raw.roots_from(mark).to_vec();
        self.raw.truncate_roots(mark);
        Ok(Value::new_raw(self.raw.insert(Obj::List(items))))
    }

    /// Moves the key-value pairs above `mark` on the root stack into a new map.
    pub(super) fn new_map_from_roots<'a>(&mut self, mark: usize, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let n = (self.raw.root_mark() - mark) / 2;
        self.raw.reserve_object(n * size_of::<(RawValue, RawValue)>())?;
        let entries = self.raw.roots_from(mark).as_chunks::<2>().0.iter().map(|&[k, v]| (k, v)).collect();
        self.raw.truncate_roots(mark);
        Ok(Value::new_raw(self.raw.insert(Obj::Map(entries))))
    }

    /// Moves the values above `mark` on the root stack into a new struct of
    /// type `ty`, or into a variant with tag `tag`.
    pub(super) fn new_record_from_roots<'a>(&mut self, mark: usize, record: Record, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let n = self.raw.root_mark() - mark;
        self.raw.reserve_object(n * size_of::<RawValue>())?;
        let fields = self.raw.roots_from(mark).into();
        self.raw.truncate_roots(mark);
        Ok(Value::new_raw(self.raw.insert(match record {
            Record::Struct(ty) => Obj::Struct { ty, fields },
            Record::Variant(tag) => Obj::Variant { tag, fields },
        })))
    }

    pub(super) fn new_encoder<'a>(&mut self, obj: EncoderObj, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.reserve_object(size_of::<EncoderObj>())?;
        Ok(Value::new_raw(self.raw.insert(Obj::Encoder(Box::new(obj)))))
    }

    pub(super) fn encoder_obj(&mut self, v: Value<'_>, _gc: NoGcScope<'_, '_>) -> Result<&mut EncoderObj> {
        let RawValue::Obj(h) = v.raw else { return Err(raw::type_error("Encoder")) };
        match self.raw.heap.get_mut(h) {
            Obj::Encoder(e) => Ok(e),
            _ => Err(raw::type_error("Encoder")),
        }
    }

    pub(super) fn new_decoder<'a>(&mut self, obj: DecoderObj, _gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        self.raw.reserve_object(size_of::<DecoderObj>())?;
        Ok(Value::new_raw(self.raw.insert(Obj::Decoder(Box::new(obj)))))
    }

    pub(super) fn decoder_obj(&mut self, v: Value<'_>, _gc: NoGcScope<'_, '_>) -> Result<&mut DecoderObj> {
        let RawValue::Obj(h) = v.raw else { return Err(raw::type_error("Decoder")) };
        match self.raw.heap.get_mut(h) {
            Obj::Decoder(d) => Ok(d),
            _ => Err(raw::type_error("Decoder")),
        }
    }

    /// Runs `f` on the text of the rooted string `input`.
    #[inline(always)]
    pub(super) fn with_input<'s, T>(&mut self, input: Scoped<'s>, _gc: NoGcScope<'_, 's>, f: impl FnOnce(&str) -> T) -> Result<T> {
        let src = self.raw.heap.str(&input.raw).ok_or_else(|| raw::type_error("str"))?;
        Ok(f(src))
    }

    /// Runs the nested-encode entry that `encoder` stores.
    pub(super) fn run_encode_walk(&mut self, encoder: Value<'_>, v: Value<'_>, gc: GcScope<'_, '_>) -> Result<()> {
        let walk = self.encoder_obj(encoder, gc.nogc())?.walk;
        walk(self.raw, encoder.raw, v.raw)
    }

    /// Runs the nested-decode entry that `decoder` stores.
    pub(super) fn run_decode_walk<'a>(&mut self, decoder: Value<'_>, ty: u16, gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let walk = self.decoder_obj(decoder, gc.nogc())?.walk;
        walk(self.raw, decoder.raw, ty).map(Value::new_raw)
    }

    /// Returns the reader and input text of `decoder`, for `f` to scan.
    pub(super) fn with_decoder_input<T>(
        &mut self,
        decoder: Value<'_>,
        gc: NoGcScope<'_, '_>,
        f: impl FnOnce(&dyn FormatReader, &str, &mut usize, &mut String) -> Result<T>,
    ) -> Result<T> {
        let d = self.decoder_obj(decoder, gc)?;
        let (input, reader, mut pos, mut scratch) = (d.input, d.reader, d.pos, std::mem::take(&mut d.scratch));
        let src = self.raw.heap.str(&input).ok_or_else(|| raw::type_error("str"))?;
        let result = f(reader, src, &mut pos, &mut scratch);
        let d = self.decoder_obj(decoder, gc)?;
        d.pos = pos;
        d.scratch = scratch;
        let mut charged = d.charged;
        let charge = charge_input(&mut self.raw.fuel, &mut charged, pos);
        self.decoder_obj(decoder, gc)?.charged = charged;
        charge?;
        result
    }

    /// Converts a span that `with_decoder_input` returned for `decoder`.
    pub(super) fn decoder_span_to_str<'a>(&mut self, decoder: Value<'_>, span: StrSpan, mut gc: GcScope<'a, '_>) -> Result<Value<'a>> {
        let d = self.decoder_obj(decoder, gc.nogc())?;
        let input = Value::new_raw(d.input);
        let scratch = std::mem::take(&mut d.scratch);
        let result = self.new_str_from_span(input, span, &scratch, gc.reborrow()).map(Value::unbind);
        self.decoder_obj(decoder, gc.nogc())?.scratch = scratch;
        Ok(result?.bind(gc.into_nogc()))
    }
}

/// The object that [`Vm::new_record_from_roots`] builds.
#[derive(Clone, Copy)]
pub(super) enum Record {
    Struct(u16),
    /// A variant with a payload.
    Variant(u16),
}

/// Continues an async native call after its future completes.
pub type AsyncResume = Box<dyn for<'v, 'w, 'a, 's> FnOnce(&'w mut Vm<'v>, GcScope<'a, 's>) -> Result<Value<'a>> + Send>;
/// The future of an async native call.
pub type AsyncCall = Pin<Box<dyn Future<Output = AsyncResume> + Send>>;

/// A named group of native functions that receive the scoped host API.
#[derive(Default)]
pub struct NativeModule {
    raw: raw::NativeModule,
}

impl NativeModule {
    pub fn function<F>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: for<'v, 'w, 'a, 's> Fn(&'w mut Vm<'v>, Args, GcScope<'a, 's>) -> Result<Value<'a>> + Send + Sync + 'static,
    {
        self.raw.function(name, move |vm, args| enter(|gc| f(&mut Vm::new(vm), args, gc).map(Value::raw)));
        self
    }

    pub fn async_function<F>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: for<'v, 'w, 'a, 's> Fn(&'w mut Vm<'v>, Args, GcScope<'a, 's>) -> Result<AsyncCall> + Send + Sync + 'static,
    {
        self.raw.async_function(name, move |vm, args| {
            let call = enter(|gc| f(&mut Vm::new(vm), args, gc))?;
            Ok(Box::pin(async move {
                let resume = call.await;
                let resume: raw::AsyncResume = Box::new(move |vm| enter(|gc| resume(&mut Vm::new(vm), gc).map(Value::raw)));
                resume
            }))
        });
        self
    }

    pub(super) fn into_raw(self) -> raw::NativeModule {
        self.raw
    }
}
