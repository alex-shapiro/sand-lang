//! The `log` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::{Obj, Value};
use crate::vm::{NResult, Vm, panic};

/// Level names, indexed by `Host::log_level` (`"off"` is one past the last).
const LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARN", "ERROR"];

pub fn register(r: &mut Registry) {
    r.source("log", ModKind::Std, include_str!("log.sand"));
    r.add_builtin("log.debug", |vm, a| record(vm, 0, a));
    r.add_builtin("log.info", |vm, a| record(vm, 1, a));
    r.add_builtin("log.warn", |vm, a| record(vm, 2, a));
    r.add_builtin("log.error", |vm, a| record(vm, 3, a));
    r.add_builtin("log.set_level", |vm, a| {
        let name = vm.str(a[0]);
        let Some(level) = LEVELS.iter().chain(&["OFF"]).position(|l| l.eq_ignore_ascii_case(name)) else {
            return panic(format!("unknown log level {name:?} (expected debug, info, warn, error or off)"));
        };
        vm.host.log_level = level as u8;
        Ok(Value::Unit)
    });
}

/// Writes `LEVEL message key=value ...` to stderr if `level` is enabled.
fn record(vm: &mut Vm, level: u8, a: &[Value]) -> NResult {
    if level < vm.host.log_level {
        return Ok(Value::Unit);
    }
    let mut line = format!("{} {}", LEVELS[level as usize], vm.str(a[0]));
    let fields: Vec<(Value, Value)> = match vm.heap.get(a[1]) {
        Obj::Map(m) => m.entries.iter().flatten().copied().collect(),
        _ => Vec::new(),
    };
    for (k, v) in fields {
        let value = vm.str(v);
        let quote = value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '=');
        let value = if quote { vm.debug(v)? } else { value.to_string() };
        line.push_str(&format!(" {}={value}", vm.str(k)));
    }
    line.push('\n');
    vm.write_stream(2, line.as_bytes())?;
    Ok(Value::Unit)
}
