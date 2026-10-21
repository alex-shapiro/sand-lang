//! The `log` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::display::escape_str_debug;
use crate::vm::value::{Obj, Value};
use crate::vm::{NResult, Vm, fail};
use std::borrow::Cow;
use std::fmt;

/// The level of a log record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    const ALL: [LogLevel; 4] = [LogLevel::Debug, LogLevel::Info, LogLevel::Warn, LogLevel::Error];

    /// The upper-case name (`"DEBUG"`).
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

/// A record that a script wrote with the `log` module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    pub level: LogLevel,
    pub message: String,
    /// The `fields` argument, in insertion order.
    pub fields: Vec<(String, String)>,
    /// The script location of the `log` call, if known.
    pub location: Option<crate::TraceFrame>,
}

/// The line that the engine writes to standard error without a host sink:
/// `LEVEL message key=value ...`. A message with control characters is
/// quoted, as is a key or value that is empty or contains whitespace,
/// control characters, `"` or `=`.
impl fmt::Display for LogRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.level, quote_if(&self.message, self.message.contains(char::is_control)))?;
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

/// Receives the records of the `log` module (see [`crate::Config::log_sink`]).
///
/// The engine calls the sink on the thread that runs the script, and the
/// script waits until the call returns. Any `Fn(&LogRecord) + Send + Sync`
/// is a sink that accepts all levels.
pub trait LogSink: Send + Sync {
    /// Whether the sink accepts records at `level`. The engine calls it for
    /// each record and does not build the records that it rejects.
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

/// A sink that rejects all records.
pub(crate) struct NullLogSink;

impl LogSink for NullLogSink {
    fn enabled(&self, _: LogLevel) -> bool {
        false
    }

    fn log(&self, _: &LogRecord) {}
}

pub fn register(r: &mut Registry) {
    r.source("log", ModKind::Std, include_str!("log.sand"));
    r.add_builtin("log.debug", |vm, a| record(vm, LogLevel::Debug, a));
    r.add_builtin("log.info", |vm, a| record(vm, LogLevel::Info, a));
    r.add_builtin("log.warn", |vm, a| record(vm, LogLevel::Warn, a));
    r.add_builtin("log.error", |vm, a| record(vm, LogLevel::Error, a));
    r.add_builtin("log.set_level", |vm, a| {
        let name = vm.str(a[0]);
        if name.eq_ignore_ascii_case("off") {
            vm.host.log_level = None;
            return Ok(Value::Unit);
        }
        let Some(&level) = LogLevel::ALL.iter().find(|l| l.name().eq_ignore_ascii_case(name)) else {
            return fail(
                "invalid",
                format!("unknown log level {:?} (expected debug, info, warn, error or off)", super::truncate(name, 64)),
            );
        };
        vm.host.log_level = Some(level);
        Ok(Value::Unit)
    });
}

/// Sends a record to the log sink if both the script and the sink enable
/// `level`. Without a host sink, writes the record's text to stderr if the
/// policy allows it.
fn record(vm: &mut Vm, level: LogLevel, a: &[Value]) -> NResult {
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
