//! Sand: a sandboxed, statically typed scripting language with a fast
//! register-based bytecode VM.
//!
//! See `docs/SPEC.md` for the language specification.
//!
//! An embedder starts with [`engine::Config`] and [`engine::Engine`], and
//! registers Rust functions with [`native::NativeModule`]. Each item has one
//! path: the path of the module that defines it.
#![forbid(unsafe_code)]

extern crate self as sandlang;

pub mod check;
mod compile;
pub mod diag;
#[cfg(any(test, doctest))]
mod doc_examples;
pub mod docs;
pub mod engine;
pub mod feed;
pub mod files;
pub mod fmt;
pub mod hir;
pub mod journal;
pub mod lint;
mod modules;
pub mod native;
#[cfg(feature = "os")]
pub mod network;
pub mod platform;
pub mod policy;
pub mod session;
pub mod stdio;
pub mod stdlib;
pub mod syntax;
pub mod types;
mod unwind;
pub mod vm;

pub use sandlang_macros::{SandEnum, SandObject, SandStruct, SandValue};
