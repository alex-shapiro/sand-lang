//! Sand: a sandboxed, statically typed scripting language with a fast
//! register-based bytecode VM.
//!
//! See `docs/SPEC.md` for the language specification.
#![forbid(unsafe_code)]

pub mod check;
mod compile;
pub mod diag;
pub mod engine;
pub mod fmt;
pub mod hir;
pub mod lint;
pub mod native;
pub mod policy;
mod stdlib;
pub mod syntax;
pub mod types;
pub mod vm;

pub use diag::{Diagnostic, Severity};
pub use engine::{Config, Engine, Error, InterruptHandle, LimitKind, Limits, MemorySink, Outcome, TestReport, TestResult, TraceFrame};
pub use native::{FromValue, IntoValue, NativeModule, SandType};
pub use policy::{Capability, Decision, Policy, PromptAnswer, Prompter, RulePolicy};
pub use vm::OutputSink;
pub use vm::value::Value;
