//! The `test` module (written in Sand, except `fail`).

use crate::check::ModKind;
use crate::native::{NativeModule, Never, Val};
use crate::vm::{Ctl, Vm};

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("test", ModKind::Std, include_str!("test.sand"));
    m.vm_function("fail", |vm: &mut Vm, msg: Val<String>| -> Result<Never, Ctl> { crate::vm::fail("assert", msg.str(&vm.heap)) });
    m
}
