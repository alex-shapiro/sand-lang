//! The shared bytecode programs.

use super::asm::assemble;
use super::isa::Program;
use std::sync::Arc;

pub const DATASET: &str = include_str!("../../programs/dataset.sasm");
pub const B1: &str = include_str!("../../programs/b1.sasm");
pub const B4: &str = include_str!("../../programs/b4.sasm");
pub const B6: &str = include_str!("../../programs/b6.sasm");
pub const B8: &str = include_str!("../../programs/b8.sasm");
pub const NODE: &str = include_str!("../../programs/node.sasm");
/// `Encode` and `Decode` for `Address` and `Status`.
pub const CUSTOM: &str = include_str!("../../programs/custom.sasm");
/// An `Address.decode` that allocates garbage before it decodes.
pub const GARBAGE_CALLBACK: &str = include_str!("../../programs/garbage_callback.sasm");
/// `Encode` and `Decode` for `Node`.
pub const NODE_CUSTOM: &str = include_str!("../../programs/node_custom.sasm");

/// Program variants, which differ only in their custom codec methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Structural,
    Custom,
    GarbageCallback,
    NodeCustom,
}

pub fn program(variant: Variant) -> Arc<Program> {
    let extra = match variant {
        Variant::Structural => "",
        Variant::Custom => CUSTOM,
        Variant::GarbageCallback => GARBAGE_CALLBACK,
        Variant::NodeCustom => NODE_CUSTOM,
    };
    let source = [DATASET, B1, B4, B6, B8, NODE, extra].join("\n");
    Arc::new(assemble(&source).unwrap_or_else(|e| panic!("{e}")))
}
