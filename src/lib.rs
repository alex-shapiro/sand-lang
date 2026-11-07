//! Sand is a sandboxed, statically typed scripting language with a
//! register-based bytecode VM.
//!
//! Configure and run the interpreter with [`engine::Config`] and
//! [`engine::Engine`]. Register host functions and types with
//! [`native::NativeModule`]. Import each item from the module that defines it.
//!
//! See `docs/SPEC.md` for the language specification.
#![forbid(unsafe_code)]

extern crate self as sandlang;

pub mod check;
mod compile;
pub mod diag;
#[cfg(any(test, doctest))]
mod doc_examples;
pub mod docs;
pub mod engine;
pub mod files;
pub mod fmt;
pub mod hir;
pub mod lint;
mod modules;
pub mod native;
#[cfg(feature = "os")]
pub mod network;
pub mod platform;
pub mod policy;
pub mod session;
pub mod stdin_feed;
pub mod stdio;
pub mod stdlib;
pub mod syntax;
pub mod types;
mod unwind;
pub mod vm;

pub use sandlang_macros::{SandEnum, SandObject, SandStruct, SandValue};
