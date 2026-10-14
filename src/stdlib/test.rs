//! The `test` module (written in Sand, except `fail`).

use crate::check::ModKind;
use crate::engine::Registry;

pub fn register(r: &mut Registry) {
    r.source("test", ModKind::Std, include_str!("test.sand"));
    r.add_builtin("test.fail", |vm, a| crate::vm::fail("assert", vm.str(a[0])));
}
