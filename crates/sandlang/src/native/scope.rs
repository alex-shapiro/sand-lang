//! Lifetime tokens that check heap handles in synchronous native functions.
//!
//! The garbage collector frees unrooted heap objects only while Sand code runs.
//! A [`CallContext`] function therefore gets a [`GcScope`], and only the
//! `CallContext` methods that can run Sand code or collect garbage take it:
//! [`CallContext::call`] and [`CallContext::step`]. Reads take a [`NoGcScope`].
//!
//! A [`BoundValue`] is a heap handle that borrows a `NoGcScope`. The borrow
//! checker rejects a `BoundValue` that the function uses after it passes
//! `gc.reborrow()` to a method that can collect:
//!
//! ```compile_fail,E0499
//! # use sandlang::native::NativeModule;
//! # use sandlang::native::context::CallContext;
//! # use sandlang::native::scope::GcScope;
//! # use sandlang::native::ty::VmVal;
//! # use sandlang::vm::Fail;
//! let mut module = NativeModule::new("calls");
//! module.function("stale", |context: &mut CallContext, mut gc: GcScope, make: VmVal<fn() -> String>| -> Result<i64, Fail> {
//!     let first = context.call(make.raw(), &[], gc.reborrow())?;
//!     let _second = context.call(make.raw(), &[], gc.reborrow())?;
//!     Ok(context.vm(gc.nogc()).str(first).len() as i64)
//! });
//! ```
//!
//! To keep a value across `call` or `step`, root it with [`CallContext::root`]
//! and read it back with [`CallContext::value`]. To pass a value to `call` or
//! to a `step` closure, call [`BoundValue::unbind`]. The compiler does not check
//! an unbound [`VmValue`].
//!
//! [`CallContext`]: super::context::CallContext
//! [`CallContext::call`]: super::context::CallContext::call
//! [`CallContext::step`]: super::context::CallContext::step
//! [`CallContext::root`]: super::context::CallContext::root
//! [`CallContext::value`]: super::context::CallContext::value

use crate::vm::Vm;
use crate::vm::value::VmValue;
use std::marker::PhantomData;
use std::ops::Deref;

/// An invariant lifetime marker.
type Invariant<'s> = PhantomData<fn(&'s ()) -> &'s ()>;

/// Permission to run Sand code and collect garbage inside a synchronous native call.
///
/// `'s` is the lifetime of the roots that the native call creates.
pub struct GcScope<'a, 's> {
    _gc: PhantomData<&'a mut ()>,
    _roots: Invariant<'s>,
}

/// Proof that the garbage collector does not run while it is borrowed.
#[derive(Clone, Copy)]
pub struct NoGcScope<'a, 's> {
    _gc: PhantomData<&'a ()>,
    _roots: Invariant<'s>,
}

impl<'a, 's> GcScope<'a, 's> {
    /// Returns the scope of a new native call.
    pub(crate) fn new() -> GcScope<'a, 's> {
        GcScope { _gc: PhantomData, _roots: PhantomData }
    }

    /// Lends the scope to a method that can collect garbage.
    ///
    /// Values bound to `self` are unusable until the returned scope ends.
    #[inline(always)]
    pub fn reborrow(&mut self) -> GcScope<'_, 's> {
        GcScope { _gc: PhantomData, _roots: PhantomData }
    }

    /// Returns a token for operations that cannot collect garbage.
    #[inline(always)]
    pub fn nogc(&self) -> NoGcScope<'_, 's> {
        NoGcScope { _gc: PhantomData, _roots: PhantomData }
    }

    /// Converts the scope into a token that lives as long as the scope.
    ///
    /// Use it to return a [`BoundValue`] from a function that takes a `GcScope`.
    #[inline(always)]
    pub fn into_nogc(self) -> NoGcScope<'a, 's> {
        NoGcScope { _gc: PhantomData, _roots: PhantomData }
    }
}

/// A [`VmValue`] that stays valid while its [`NoGcScope`] lasts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoundValue<'a> {
    value: VmValue,
    _gc: PhantomData<&'a ()>,
}

impl BoundValue<'_> {
    /// Detaches the value from its scope.
    ///
    /// Root the value or pass it to Sand before the garbage collector next runs.
    #[inline(always)]
    pub fn unbind(self) -> VmValue {
        self.value
    }
}

impl VmValue {
    /// Binds the value to `gc`.
    #[inline(always)]
    pub fn bind<'a>(self, _gc: NoGcScope<'a, '_>) -> BoundValue<'a> {
        BoundValue { value: self, _gc: PhantomData }
    }
}

/// A value rooted until its native call returns.
///
/// Read it with [`CallContext::value`](super::context::CallContext::value).
#[derive(Clone, Copy, Debug)]
pub struct ScopedValue<'s> {
    value: VmValue,
    _roots: Invariant<'s>,
}

impl<'s> ScopedValue<'s> {
    /// Wraps `value`, which the caller has rooted until the native call returns.
    #[inline(always)]
    pub(crate) fn new(value: VmValue, _gc: NoGcScope<'_, 's>) -> ScopedValue<'s> {
        ScopedValue { value, _roots: PhantomData }
    }

    /// Returns the rooted value bound to `gc`.
    #[inline(always)]
    pub(crate) fn get<'a>(self, gc: NoGcScope<'a, 's>) -> BoundValue<'a> {
        self.value.bind(gc)
    }
}

/// Read access to the [`Vm`] inside a [`NoGcScope`].
///
/// `VmReader` dereferences to [`Vm`]. Its own methods return heap handles as
/// [`BoundValue`]s.
#[derive(Clone, Copy)]
pub struct VmReader<'v, 'a> {
    vm: &'v Vm,
    _gc: PhantomData<&'a ()>,
}

impl<'v, 'a> VmReader<'v, 'a> {
    pub(crate) fn new(vm: &'v Vm, _gc: NoGcScope<'a, '_>) -> VmReader<'v, 'a> {
        VmReader { vm, _gc: PhantomData }
    }

    /// Returns the text of the string `value`.
    #[inline]
    pub fn str(&self, value: BoundValue<'_>) -> &'v str {
        self.vm.heap.str(value.value)
    }

    /// Returns field `index` of a tuple, struct, or enum variant.
    #[inline]
    pub fn field(&self, value: BoundValue<'_>, index: usize) -> Option<BoundValue<'a>> {
        Some(BoundValue { value: self.vm.heap.field(value.value, index)?, _gc: PhantomData })
    }

    /// Returns item `index` of a list.
    #[inline]
    pub fn list_item(&self, list: BoundValue<'_>, index: usize) -> Option<BoundValue<'a>> {
        Some(BoundValue { value: *self.vm.heap.list(list.value).get(index)?, _gc: PhantomData })
    }
}

impl Deref for VmReader<'_, '_> {
    type Target = Vm;

    fn deref(&self) -> &Vm {
        self.vm
    }
}
