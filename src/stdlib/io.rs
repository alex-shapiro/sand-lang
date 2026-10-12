//! The `io` and `env` modules.

use super::stream::{IoFailure, read_line_bytes, read_to_end_reserved, strip_eol};
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::Capability;
use crate::vm::iomem::Reservation;
use crate::vm::value::{NativeObject, Obj, Value, to_usize_saturating, u64_to_usize_saturating};
use crate::vm::{Ctl, Finisher, NResult, NativeFn, StdinSource, Vm};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::Mutex;

pub fn register(r: &mut Registry) {
    r.source("io", ModKind::Std, include_str!("io.sand"));
    r.source("env", ModKind::Std, include_str!("env.sand"));
    register_io(r);
    register_env(r);
}

fn register_io(r: &mut Registry) {
    r.add_builtin("io.new_stdin", |vm, _| Ok(vm.heap.alloc(Obj::Native(Box::new(StdStream::Stdin)))));
    r.add_builtin("io.new_stdout", |vm, _| Ok(vm.heap.alloc(Obj::Native(Box::new(StdStream::Stdout)))));
    r.add_builtin("io.new_stderr", |vm, _| Ok(vm.heap.alloc(Obj::Native(Box::new(StdStream::Stderr)))));
    r.add_method("io.Stdin.read", |vm, a| {
        let max = a[1].as_int();
        if max <= 0 {
            return Ok(vm.err_value("invalid", "read: max must be positive"));
        }
        read_stdin(vm, Read::Upto(to_usize_saturating(max)))
    });
    r.add_method("io.Stdin.read_line", |vm, _| read_stdin(vm, Read::Line));
    r.add_method("io.Stdin.read_all", |vm, _| read_stdin(vm, Read::All));
    for (key, which) in [("io.Stdout", 1), ("io.Stderr", 2)] {
        let write: NativeFn = if which == 1 { |vm, a| write_out(vm, 1, a[1]) } else { |vm, a| write_out(vm, 2, a[1]) };
        r.add_method(format!("{key}.write"), write);
        r.add_method(format!("{key}.write_str"), write);
        r.add_method(format!("{key}.flush"), if which == 1 { |vm, _| flush_out(vm, 1) } else { |vm, _| flush_out(vm, 2) });
    }
}

/// The standard streams' objects (`io.stdin`, `io.stdout`, `io.stderr`).
enum StdStream {
    Stdin,
    Stdout,
    Stderr,
}

impl NativeObject for StdStream {
    fn type_name(&self) -> &'static str {
        match self {
            StdStream::Stdin => "Stdin",
            StdStream::Stdout => "Stdout",
            StdStream::Stderr => "Stderr",
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Writes the `str` or `bytes` value `v` to stdout (1) or stderr (2).
fn write_out(vm: &mut Vm, which: u8, v: Value) -> NResult {
    let len = match vm.heap.get(v) {
        Obj::Str(s) => s.len(),
        _ => vm.heap.bytes_of(v).len(),
    };
    vm.charge_bytes(len)?;
    vm.write_stream_value(which, v)
}

fn flush_out(vm: &mut Vm, which: u8) -> NResult {
    let sink = if which == 1 { &vm.host.stdout } else { &vm.host.stderr };
    Ok(match sink.flush() {
        Ok(()) => vm.ok(Value::Unit),
        Err(e) => vm.err_value(super::io_kind(&e), &e.to_string()),
    })
}

/// What a read of standard input reads.
#[derive(Clone, Copy, PartialEq)]
enum Read {
    /// A line (`Result<str?>`).
    Line,
    /// The rest (`Result<bytes>`).
    All,
    /// At most this many bytes, once some are available (`Result<bytes>`).
    Upto(usize),
}

fn read_stdin(vm: &mut Vm, what: Read) -> NResult {
    if !vm.permit(&Capability::Stdin)? {
        return Ok(vm.err_value("permission_denied", "permission denied: read standard input"));
    }
    // A prompt written before reading must be visible (on a terminal).
    vm.flush_before_input()?;
    if what == Read::Line
        && let Some(data) = buffered_line(vm)
    {
        return match data {
            Some(data) => line_result(vm, data),
            None => {
                let none = vm.none();
                Ok(vm.ok(none))
            }
        };
    }
    let stdin = vm.host.stdin.clone();
    // The input is buffered outside the heap until it is complete.
    let mut res = match what {
        Read::Upto(max) => match vm.reserve_io(max.min(STDIN_CHUNK))? {
            Ok(r) => r,
            Err(e) => return Ok(e),
        },
        _ => vm.io_reservation(),
    };
    Err(Ctl::suspend(Box::pin(async move {
        let result = read_input(&stdin, what, &mut res).await;
        Box::new(move |vm: &mut Vm| -> NResult {
            drop(res);
            let data = match result {
                Ok(Some(data)) => data,
                Ok(None) => {
                    let none = vm.none();
                    return Ok(vm.ok(none));
                }
                Err(e) => return IoFailure::new(e.kind, format!("stdin: {}", e.message)).into_value(vm),
            };
            vm.reserve(data.len())?;
            if what != Read::Line {
                vm.charge_bytes(data.len())?;
                let b = vm.heap.new_bytes(data);
                return Ok(vm.ok(b));
            }
            line_result(vm, data)
        }) as Finisher
    })))
}

/// `Ok(Some(line))`, or kind `"invalid"` if `line` is not UTF-8.
fn line_result(vm: &mut Vm, line: Vec<u8>) -> NResult {
    vm.charge_slow_bytes(line.len())?;
    let Ok(text) = String::from_utf8(line) else {
        return Ok(vm.err_value("invalid", "stdin: input is not valid UTF-8"));
    };
    let s = vm.new_str(text);
    let v = vm.some(s);
    Ok(vm.ok(v))
}

/// The most `read` returns at once from standard input.
const STDIN_CHUNK: usize = 64 << 10;

/// The next line of standard input without its terminator (`Some(None)` at
/// the end), if it is in a buffer already; `None`, consuming nothing, if the
/// read must wait, another is under way, or the line does not fit in memory.
fn buffered_line(vm: &Vm) -> Option<Option<Vec<u8>>> {
    let mut source = vm.host.stdin.try_lock().ok()?;
    let _op = match &mut *source {
        StdinSource::Fed(r) => Some(r.begin()),
        _ => None,
    };
    let (buf, at_end) = match &mut *source {
        StdinSource::None => return Some(None),
        StdinSource::Bytes(c) => {
            let at = u64_to_usize_saturating(c.position()).min(c.get_ref().len());
            (&c.get_ref()[at..], true)
        }
        #[cfg(feature = "os")]
        StdinSource::Inherit(r) => (r.buffer(), false),
        StdinSource::Fed(r) => r.available(),
    };
    let take = match buf.iter().position(|&b| b == b'\n') {
        Some(nl) => nl + 1,
        None if at_end => buf.len(),
        None => return None,
    };
    if take == 0 {
        return Some(None);
    }
    if vm.heap.would_exceed(take) {
        return None;
    }
    let mut data = buf[..take].to_vec();
    match &mut *source {
        StdinSource::Bytes(c) => c.set_position(c.position() + take as u64),
        #[cfg(feature = "os")]
        StdinSource::Inherit(r) => std::pin::Pin::new(r).consume(take),
        StdinSource::Fed(r) => std::pin::Pin::new(r).consume(take),
        StdinSource::None => {}
    }
    strip_eol(&mut data);
    Some(Some(data))
}

/// Reads `what` from standard input: `None` only for a line at the end of
/// the input.
async fn read_input(stdin: &Mutex<StdinSource>, what: Read, res: &mut Reservation) -> Result<Option<Vec<u8>>, IoFailure> {
    let mut source = stdin.lock().await;
    match &mut *source {
        StdinSource::None => Ok((what != Read::Line).then(Vec::new)),
        StdinSource::Bytes(c) => read_from(c, what, res).await,
        #[cfg(feature = "os")]
        StdinSource::Inherit(r) => read_from(r, what, res).await,
        StdinSource::Fed(r) => {
            let _op = r.begin();
            read_from(r, what, res).await
        }
    }
}

async fn read_from<R: AsyncBufRead + Unpin>(r: &mut R, what: Read, res: &mut Reservation) -> Result<Option<Vec<u8>>, IoFailure> {
    match what {
        Read::Line => {
            let Some(mut data) = read_line_bytes(r, usize::MAX, res).await? else { return Ok(None) };
            strip_eol(&mut data);
            Ok(Some(data))
        }
        Read::All => {
            let mut data = Vec::new();
            read_to_end_reserved(r, &mut data, res, usize::MAX).await?;
            Ok(Some(data))
        }
        Read::Upto(max) => {
            let available = r.fill_buf().await?;
            let data = available[..available.len().min(max).min(STDIN_CHUNK)].to_vec();
            r.consume(data.len());
            Ok(Some(data))
        }
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
            #[cfg(feature = "os")]
            None if name.is_empty() || name.contains(['=', '\0']) => None,
            #[cfg(feature = "os")]
            None => std::env::var(&name).ok(),
            // No process environment without operating-system access.
            #[cfg(not(feature = "os"))]
            None => None,
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
            #[cfg(not(feature = "os"))]
            None => Vec::new(),
            #[cfg(feature = "os")]
            None => {
                let mut vars: Vec<(String, String)> =
                    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
                vars.sort();
                vars
            }
        };
        for (k, v) in vars {
            let (k, v) = (vm.new_str(k), vm.new_str(v));
            vm.map_insert(map, k, v)?;
        }
        Ok(map)
    });
}
