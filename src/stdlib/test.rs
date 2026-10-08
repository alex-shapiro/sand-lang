//! The `test` module (written entirely in Sand).

use crate::check::ModKind;
use crate::engine::Registry;

pub fn register(r: &mut Registry) {
    r.source("test", ModKind::Std, include_str!("test.sand"));
}
