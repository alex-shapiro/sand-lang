//! The `io` and `env` modules.

use super::net::{NetError, read_line_bytes, read_to_end_reserved, strip_eol};
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::Capability;
use crate::vm::iomem::Reservation;
use crate::vm::value::{Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, StdinSource, Vm};
use tokio::io::AsyncBufRead;
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
        let data = copy_out(vm, a[0])?;
        vm.write_stream(1, &data)?;
        Ok(Value::Unit)
    });
    r.add_builtin("io.ewrite", |vm, a| {
        let data = copy_out(vm, a[0])?;
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
    // The input is buffered outside the heap until it is complete.
    let mut res = vm.io_reservation();
    Err(Ctl::Async(Box::pin(async move {
        let result = read_input(&stdin, line, &mut res).await;
        Box::new(move |vm: &mut Vm| -> NResult {
            drop(res);
            let data = match result {
                Ok(Some(data)) => data,
                Ok(None) => {
                    let none = vm.none();
                    return Ok(vm.ok(none));
                }
                Err(e) => return NetError::new(e.kind, format!("stdin: {}", e.message)).into_value(vm),
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

/// The next line without its terminator (`None` at end of input), or all
/// remaining input.
async fn read_input(stdin: &Mutex<StdinSource>, line: bool, res: &mut Reservation) -> Result<Option<Vec<u8>>, NetError> {
    let mut source = stdin.lock().await;
    match &mut *source {
        StdinSource::None => Ok((!line).then(Vec::new)),
        StdinSource::Bytes(c) => read_from(c, line, res).await,
        StdinSource::Inherit(r) => read_from(r, line, res).await,
    }
}

async fn read_from<R: AsyncBufRead + Unpin>(r: &mut R, line: bool, res: &mut Reservation) -> Result<Option<Vec<u8>>, NetError> {
    if line {
        let Some(mut data) = read_line_bytes(r, usize::MAX, res).await? else { return Ok(None) };
        strip_eol(&mut data);
        Ok(Some(data))
    } else {
        let mut data = Vec::new();
        read_to_end_reserved(r, &mut data, res, usize::MAX).await?;
        Ok(Some(data))
    }
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
            vm.map_insert(map, k, v).map_err(crate::vm::meter::ctl_from_message)?;
        }
        Ok(map)
    });
}

/// A copy of the string `s` to write (the stream needs `&mut Vm`), checked
/// against the memory limit and charged.
fn copy_out(vm: &Vm, s: Value) -> Result<Vec<u8>, Ctl> {
    let n = vm.str(s).len();
    vm.reserve(n)?;
    vm.charge_bytes(n)?;
    Ok(vm.str(s).as_bytes().to_vec())
}
