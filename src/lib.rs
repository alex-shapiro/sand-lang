//! Sand: a sandboxed, statically typed scripting language with a fast
//! register-based bytecode VM.
//!
//! See `docs/SPEC.md` for the language specification.
#![forbid(unsafe_code)]

pub mod check;
mod compile;
pub mod diag;
pub mod engine;
mod feed;
mod files;
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
mod stdio;
mod stdlib;
pub mod syntax;
pub mod types;
mod unwind;
pub mod vm;

pub use diag::{Diagnostic, Severity};
pub use engine::{
    Config, Engine, Error, InterruptHandle, LimitKind, Limits, MemorySink, Outcome, RequestFilter, RequestTarget, TestReport, TestResult,
    TraceFrame,
};
pub use feed::StdinFeed;
pub use files::MemFs;
pub use native::{Conversion, FromValue, HostError, IntoHostError, IntoValue, ListElem, NativeModule, SandType};
pub use policy::{Capability, CustomPolicy, Decision, Policy, PromptAnswer, Prompter, RulePolicy};
pub use session::Session;
pub use stdio::flush_stdio;
pub use vm::OutputSink;
pub use vm::heap::GcStats;
pub use vm::value::Value;
