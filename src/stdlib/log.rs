//! The `log` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::{Obj, Value};
use crate::vm::{NResult, Vm, fail};
use std::fmt;
use std::sync::Arc;

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
}

/// Receives the records of the `log` module (see [`crate::Config::log_sink`]).
///
/// The engine calls the sink on the thread that runs the script, and the
/// script waits until the call returns.
pub trait LogSink: Send + Sync {
    fn log(&self, record: &LogRecord);
}

impl<F: Fn(&LogRecord) + Send + Sync> LogSink for F {
    fn log(&self, record: &LogRecord) {
        self(record)
    }
}

/// Where the records of an engine go, and the lowest level that goes there.
#[derive(Clone)]
pub(crate) struct LogConfig {
    /// `None`: standard error, as text.
    pub sink: Option<Arc<dyn LogSink>>,
    pub min: LogLevel,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig { sink: None, min: LogLevel::Debug }
    }
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

/// Sends a record to the log sink if both the script and the host enable
/// `level`.
fn record(vm: &mut Vm, level: LogLevel, a: &[Value]) -> NResult {
    if vm.host.log_level.is_none_or(|l| level < l) || level < vm.host.log.min {
        return Ok(Value::Unit);
    }
    let entries: Vec<(Value, Value)> = match vm.heap.get(a[1]) {
        Obj::Map(m) => m.entries.iter().flatten().copied().collect(),
        _ => Vec::new(),
    };
    match vm.host.log.sink.clone() {
        Some(sink) => {
            let len = vm.str(a[0]).len() + entries.iter().map(|&(k, v)| vm.str(k).len() + vm.str(v).len()).sum::<usize>();
            vm.reserve(len)?;
            vm.charge_bytes(len)?;
            vm.count_output(2, len)?;
            let message = vm.str(a[0]).to_string();
            let fields = entries.iter().map(|&(k, v)| (vm.str(k).to_string(), vm.str(v).to_string())).collect();
            sink.log(&LogRecord { level, message, fields });
        }
        None => {
            let mut line = format!("{level} {}", vm.str(a[0]));
            for (k, v) in entries {
                let value = vm.str(v);
                let quote = value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '=');
                let value = if quote { vm.debug(v)? } else { value.to_string() };
                line.push_str(&format!(" {}={value}", vm.str(k)));
                vm.reserve(line.len())?;
            }
            line.push('\n');
            vm.charge_bytes(line.len())?;
            vm.write_stream(2, line.as_bytes())?;
        }
    }
    Ok(Value::Unit)
}
