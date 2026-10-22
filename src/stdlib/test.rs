//! The `test` module (written in Sand, except `fail`).

use crate::check::ModKind;
use crate::native::{NativeModule, Never};
use crate::vm::Ctl;

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("test", ModKind::Std, include_str!("test.sand"));
    m.function("fail", |msg: &str| -> Result<Never, Ctl> { crate::vm::fail("assert", msg) });
    m
}
