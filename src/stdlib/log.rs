//! The `log` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::display::escape_str_debug;
use crate::vm::value::{Obj, Value};
use crate::vm::{NResult, Vm};
use std::borrow::Cow;
use std::fmt;

/// The severity of a log record, from lowest to highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, crate::SandEnum)]
#[sand(module = "log", name = "Level")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Returns the uppercase level name (such as `"INFO"`).
    pub fn name(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A structured log record emitted by the `log` module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    pub level: LogLevel,
    pub message: String,
    /// Key-value fields in insertion order.
    pub fields: Vec<(String, String)>,
    /// Source location of the `log` call.
    pub location: Option<crate::TraceFrame>,
}

/// Formats the record as `LEVEL message key=value ...`, quoting empty messages,
/// messages with control characters, and empty keys or values or those
/// containing whitespace, control characters, `"`, or `=`.
impl fmt::Display for LogRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.level, quote_if(&self.message, self.message.is_empty() || self.message.contains(char::is_control)))?;
        for (key, value) in &self.fields {
            write!(f, " {}={}", quote_if(key, needs_quotes(key)), quote_if(value, needs_quotes(value)))?;
        }
        Ok(())
    }
}

fn needs_quotes(s: &str) -> bool {
    s.is_empty() || s.contains(|c: char| c.is_whitespace() || c.is_control() || c == '"' || c == '=')
}

fn quote_if(s: &str, quote: bool) -> Cow<'_, str> {
    if !quote {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 2);
    escape_str_debug(s, &mut out);
    Cow::Owned(out)
}

/// Consumer for records emitted by the `log` module (see [`crate::Config::log_sink`]).
///
/// The engine invokes [`LogSink::log`] synchronously on the thread executing the
/// script. Any `Fn(&LogRecord) + Send + Sync` implements `LogSink` for all
/// levels.
pub trait LogSink: Send + Sync {
    /// Returns true if the sink accepts records at `level`. The engine skips
    /// constructing [`LogRecord`] values for disabled levels.
    fn enabled(&self, _level: LogLevel) -> bool {
        true
    }

    fn log(&self, record: &LogRecord);
}

impl<F: Fn(&LogRecord) + Send + Sync> LogSink for F {
    fn log(&self, record: &LogRecord) {
        self(record)
    }
}

pub(crate) struct NullLogSink;

impl LogSink for NullLogSink {
    fn enabled(&self, _: LogLevel) -> bool {
        false
    }

    fn log(&self, _: &LogRecord) {}
}

pub fn register(r: &mut Registry) {
    r.source("log", ModKind::Std, include_str!("log.sand"));
    r.add_builtin("log.debug", |vm, a| emit_record(vm, LogLevel::Debug, a));
    r.add_builtin("log.info", |vm, a| emit_record(vm, LogLevel::Info, a));
    r.add_builtin("log.warn", |vm, a| emit_record(vm, LogLevel::Warn, a));
    r.add_builtin("log.error", |vm, a| emit_record(vm, LogLevel::Error, a));
    r.add_enum_type::<LogLevel>();
    r.add_builtin("log.set_level", |vm, a| {
        vm.host.log_level = Some(vm.enum_value(a[0])?);
        Ok(Value::Unit)
    });
    r.add_builtin("log.disable", |vm, _| {
        vm.host.log_level = None;
        Ok(Value::Unit)
    });
}

fn emit_record(vm: &mut Vm, level: LogLevel, a: &[Value]) -> NResult {
    if vm.host.log_level.is_none_or(|l| level < l) {
        return Ok(Value::Unit);
    }
    let sink = vm.host.log_sink.clone();
    let enabled = match &sink {
        Some(sink) => sink.enabled(level),
        None => vm.output_allowed(2)?,
    };
    if !enabled {
        return Ok(Value::Unit);
    }
    let fields: Vec<(Value, Value)> = match vm.heap.get(a[1]) {
        Obj::Map(m) => m.entries.iter().flatten().copied().collect(),
        _ => Vec::new(),
    };
    let len = vm.str(a[0]).len() + fields.iter().map(|&(k, v)| vm.str(k).len() + vm.str(v).len()).sum::<usize>();
    vm.reserve(len)?;
    vm.charge_bytes(len)?;
    let record = LogRecord {
        level,
        message: vm.str(a[0]).to_string(),
        fields: fields.iter().map(|&(k, v)| (vm.str(k).to_string(), vm.str(v).to_string())).collect(),
        location: vm.native_caller(),
    };
    match sink {
        Some(sink) => {
            vm.count_output(2, len)?;
            sink.log(&record);
        }
        None => {
            let line = format!("{record}\n");
            vm.reserve(line.len())?;
            vm.count_output(2, line.len())?;
            vm.write_counted(2, line.as_bytes())?;
        }
    }
    Ok(Value::Unit)
}
