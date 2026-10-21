//! Sand: a sandboxed, statically typed scripting language with a fast
//! register-based bytecode VM.
//!
//! See `docs/SPEC.md` for the language specification.
#![forbid(unsafe_code)]

pub mod check;
mod compile;
pub mod diag;
pub mod docs;
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
pub use native::{Conversion, FromValue, HostError, IntoHostError, IntoValue, ListElem, NativeModule, SandType, TypeContext};
pub use policy::{Capability, CustomPolicy, Decision, Policy, PromptAnswer, Prompter, RulePolicy};
pub use session::Session;
pub use stdio::flush_stdio;
pub use stdlib::log::{LogLevel, LogRecord, LogSink};
pub use vm::OutputSink;
pub use vm::heap::GcStats;
pub use vm::value::{Packing, Value, ValueType};

pub use jiff;

/// Rust types for `time` module values. `Instant`, `PlainDateTime`,
/// `PlainDate`, and `PlainTime` are [`jiff::Timestamp`],
/// [`jiff::civil::DateTime`], [`jiff::civil::Date`], and [`jiff::civil::Time`].
pub mod time {
    pub use crate::stdlib::time::{PlainMonthDay, PlainYearMonth, Span, TimeZone, ZonedDateTime};
}
