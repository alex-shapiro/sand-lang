//! The `io` and `env` modules.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::Capability;
use crate::vm::value::{Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, StdinSource, Vm};
use std::io::{self, BufRead, Read};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::sync::Mutex;

pub fn register(r: &mut Registry) {
    r.source("io", ModKind::Std, include_str!("io.sand"));
    r.source("env", ModKind::Std, include_str!("env.sand"));
    register_io(r);
    register_env(r);
}

fn register_io(r: &mut Registry) {
    r.add_builtin("io.read_line", |vm, _| read_stdin(vm, true));
    r.add_builtin("io.read_all", |vm, _| read_stdin(vm, false));
    r.add_builtin("io.write", |vm, a| {
        let data = vm.str(a[0]).as_bytes().to_vec();
        vm.write_stream(1, &data)?;
        Ok(Value::Unit)
    });
    r.add_builtin("io.ewrite", |vm, a| {
        let data = vm.str(a[0]).as_bytes().to_vec();
        vm.write_stream(2, &data)?;
        Ok(Value::Unit)
    });
    r.add_builtin("io.flush", |vm, _| {
        for sink in [&vm.host.stdout, &vm.host.stderr] {
            let _ = sink.lock().unwrap_or_else(|e| e.into_inner()).flush();
        }
        Ok(Value::Unit)
    });
}

/// `read_line` (`Result<str?>`) or `read_all` (`Result<str>`).
fn read_stdin(vm: &mut Vm, line: bool) -> NResult {
    if !vm.permit(&Capability::Stdin)? {
        return Ok(vm.err_value("permission_denied", "permission denied: read standard input"));
    }
    let stdin = vm.host.stdin.clone();
    // Read at most one byte more than can be allocated; the finisher then
    // fails with the memory limit instead of exhausting host memory.
    let limit = match vm.heap.limit {
        Some(l) => l.saturating_sub(vm.heap.bytes) as u64 + 1,
        None => 1 << 40,
    };
    Err(Ctl::Async(Box::pin(async move {
        let result = if line { read_line(&stdin, limit).await } else { read_all(&stdin, limit).await.map(Some) };
        Box::new(move |vm: &mut Vm| -> NResult {
            let data = match result {
                Ok(Some(data)) => data,
                Ok(None) => {
                    let none = vm.none();
                    return Ok(vm.ok(none));
                }
                Err(e) => return Ok(vm.err_value(io_kind(&e), &format!("stdin: {e}"))),
            };
            vm.reserve(data.len())?;
            let Ok(text) = String::from_utf8(data) else {
                return Ok(vm.err_value("invalid", "stdin: input is not valid UTF-8"));
            };
            let s = vm.new_str(text);
            let v = if line { vm.some(s) } else { s };
            Ok(vm.ok(v))
        }) as Finisher
    })))
}

/// The next line without its terminator, `None` at end of input.
async fn read_line(stdin: &Mutex<StdinSource>, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut source = stdin.lock().await;
    let mut buf = Vec::new();
    let n = match &mut *source {
        StdinSource::None => 0,
        StdinSource::Bytes(c) => Read::take(c, limit).read_until(b'\n', &mut buf)?,
        StdinSource::Inherit(r) => r.take(limit).read_until(b'\n', &mut buf).await?,
    };
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    Ok(Some(buf))
}

async fn read_all(stdin: &Mutex<StdinSource>, limit: u64) -> io::Result<Vec<u8>> {
    let mut source = stdin.lock().await;
    let mut buf = Vec::new();
    match &mut *source {
        StdinSource::None => 0,
        StdinSource::Bytes(c) => Read::take(c, limit).read_to_end(&mut buf)?,
        StdinSource::Inherit(r) => r.take(limit).read_to_end(&mut buf).await?,
    };
    Ok(buf)
}

fn register_env(r: &mut Registry) {
    r.add_builtin("env.get", |vm, a| {
        let name = vm.str(a[0]).to_string();
        if !vm.permit(&Capability::Env(name.clone()))? {
            return Ok(vm.none());
        }
        let value = match &vm.host.env {
            Some(vars) => vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()),
            // `std::env::var` may panic on such names.
            None if name.is_empty() || name.contains(['=', '\0']) => None,
            None => std::env::var(&name).ok(),
        };
        let v = value.map(|v| vm.new_str(v));
        Ok(vm.option(v))
    });
    r.add_builtin("env.args", |vm, _| {
        let args = vm.host.args.clone();
        let items = args.into_iter().map(|a| vm.new_str(a)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("env.vars", |vm, _| {
        let map = vm.heap.alloc(Obj::Map(Box::default()));
        if !vm.permit(&Capability::Env("*".into()))? {
            return Ok(map);
        }
        let vars = match &vm.host.env {
            Some(vars) => vars.clone(),
            None => {
                let mut vars: Vec<(String, String)> =
                    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
                vars.sort();
                vars
            }
        };
        for (k, v) in vars {
            let (k, v) = (vm.new_str(k), vm.new_str(v));
            vm.map_insert(map, k, v).map_err(Ctl::Panic)?;
        }
        Ok(map)
    });
}
