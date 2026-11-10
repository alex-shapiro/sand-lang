//! The `test` module (written in Sand, except `fail`).

use crate::check::ModKind;
use crate::native::NativeModule;
use crate::native::ty::Never;
use crate::vm::Fail;

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("test", ModKind::Std, include_str!("test.sand"));
    m.function("fail", |msg: &str| -> Result<Never, Fail> { crate::vm::fail("assert", msg) });
    m
}
