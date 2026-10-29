//! Sand: a sandboxed, statically typed scripting language with a fast
//! register-based bytecode VM.
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
pub use native::{
    Conversion, EnumType, EnumVariant, FromValue, HostError, IntoHostError, IntoValue, NamedType, NativeModule, NativeObject, ObjectType,
    SandType, StructField, StructType,
};
pub use policy::{Capability, CustomPolicy, Decision, PromptAnswer, Prompter, RulePolicy};
pub use sandlang_macros::{SandEnum, SandObject, SandStruct, SandValue};
pub use session::Session;
pub use stdio::flush_stdio;
pub use stdlib::log::{LogLevel, LogRecord, LogSink};
pub use vm::OutputSink;
pub use vm::heap::GcStats;
pub use vm::value::{Packing, Value, ValueType};

pub use indexmap;
pub use jiff;

/// Rust representation of `json.Json` values.
pub mod json {
    pub use crate::stdlib::json::Json;
}

/// Rust types for `time` module values. `Instant`, `PlainDateTime`,
/// `PlainDate`, and `PlainTime` are [`jiff::Timestamp`],
/// [`jiff::civil::DateTime`], [`jiff::civil::Date`], and [`jiff::civil::Time`].
pub mod time {
    pub use crate::stdlib::time::enums::{Disambiguation, RoundingMode, Unit};
    pub use crate::stdlib::time::{PlainMonthDay, PlainYearMonth, Span, TimeZone, ZonedDateTime};
}
