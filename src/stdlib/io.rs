//! The `io` and `env` modules.

use super::stream::{self, IoFailure, Suspended, bytes_read, line_read, read_line, read_to_end_reserved, read_upto, strip_eol};
use crate::check::ModKind;
use crate::native::{HostError, IntoValue, Later, NativeModule, NativeObject, ObjectType, Val};
use crate::policy::Capability;
use crate::vm::iomem::Reservation;
use crate::vm::value::{Obj, Value, to_usize_saturating, u64_to_usize_saturating};
use crate::vm::{Ctl, StdinSource, Vm};
use std::collections::BTreeMap;
use tokio::io::AsyncBufRead;
use tokio::sync::Mutex;

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("io", ModKind::Std, include_str!("io.sand"));
    m.bind::<Stdin>();
    m.bind::<Stdout>();
    m.bind::<Stderr>();
    m.vm_function("new_stdin", |vm: &mut Vm| vm.new_object(Stdin));
    m.vm_function("new_stdout", |vm: &mut Vm| vm.new_object(Stdout));
    m.vm_function("new_stderr", |vm: &mut Vm| vm.new_object(Stderr));
    m.vm_function("Stdin.read", |vm: &mut Vm, _: Val<Stdin>, max: i64| {
        if max <= 0 {
            return Err(HostError::new("invalid", "read: max must be positive"));
        }
        read_stdin(vm, Read::Upto(to_usize_saturating(max)), bytes_read)
    });
    m.vm_function("Stdin.read_line", |vm: &mut Vm, _: Val<Stdin>| read_stdin(vm, Read::Line, input_line));
    m.vm_function("Stdin.read_all", |vm: &mut Vm, _: Val<Stdin>| read_stdin(vm, Read::All, bytes_read));
    output::<Stdout>(&mut m);
    output::<Stderr>(&mut m);
    m
}

/// The type of `io.stdin`. Reading requires the `Stdin` capability.
#[derive(crate::SandObject)]
#[sand(module = "io")]
struct Stdin;

/// The type of `io.stdout`. Writing requires the `Stdout` capability and counts
/// toward `max_output_bytes`.
#[derive(crate::SandObject)]
#[sand(module = "io")]
struct Stdout;

/// The type of `io.stderr`. Writing requires the `Stderr` capability and counts
/// toward `max_output_bytes`.
#[derive(crate::SandObject)]
#[sand(module = "io")]
struct Stderr;

impl NativeObject for Stdin {}
impl NativeObject for Stdout {}
impl NativeObject for Stderr {}

trait Output: ObjectType {
    /// The stream: stdout (1) or stderr (2).
    const STREAM: u8;
}

impl Output for Stdout {
    const STREAM: u8 = 1;
}

impl Output for Stderr {
    const STREAM: u8 = 2;
}

fn output<T: Output>(m: &mut NativeModule) {
    m.vm_function(&format!("{}.write", T::NAME), |vm: &mut Vm, _: Val<T>, data: Val<Vec<u8>>| write_out(vm, T::STREAM, data.raw()));
    m.vm_function(&format!("{}.write_str", T::NAME), |vm: &mut Vm, _: Val<T>, text: Val<String>| write_out(vm, T::STREAM, text.raw()));
    m.vm_function(&format!("{}.flush", T::NAME), |vm: &mut Vm, _: Val<T>| flush_out(vm, T::STREAM));
}

/// Writes the `str` or `bytes` value `v` to stdout (1) or stderr (2).
fn write_out(vm: &mut Vm, which: u8, v: Value) -> Result<(), HostError> {
    let len = match vm.heap.get(v) {
        Obj::Str(s) => s.len(),
        _ => vm.heap.bytes_of(v).len(),
    };
    vm.charge_bytes(len)?;
    vm.write_stream_value(which, v)
}

fn flush_out(vm: &mut Vm, which: u8) -> Result<(), HostError> {
    let sink = if which == 1 { &vm.host.stdout } else { &vm.host.stderr };
    Ok(sink.flush()?)
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

fn read_stdin<R: IntoValue + Send + 'static>(
    vm: &mut Vm,
    what: Read,
    done: fn(&mut Vm, Option<Vec<u8>>) -> Result<R, HostError>,
) -> Suspended<R> {
    if !vm.permit(&Capability::Stdin)? {
        return Err(HostError::new("permission_denied", "permission denied: read standard input"));
    }
    // A prompt written before reading must be visible (on a terminal).
    vm.flush_before_input()?;
    if what == Read::Line
        && let Some(data) = buffered_line(vm)
    {
        return Ok(Later::ready(done(vm, data)));
    }
    let stdin = vm.host.stdin.clone();
    // The input is buffered outside the heap until it is complete.
    let mut res = match what {
        Read::Upto(max) => vm.reserve_io(max.min(STDIN_CHUNK))?,
        _ => vm.io_reservation(),
    };
    let read = async move {
        let data = read_input(&stdin, what, &mut res).await.map_err(|e| IoFailure::new(e.kind, format!("stdin: {}", e.message)))?;
        Ok((data, res))
    };
    stream::suspend(read, move |vm, (data, res)| {
        drop(res);
        done(vm, data)
    })
}

fn input_line(vm: &mut Vm, line: Option<Vec<u8>>) -> Result<Option<String>, HostError> {
    line_read(vm, line, "stdin: input is not valid UTF-8")
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
        Read::Line => read_line(r, res).await,
        Read::All => {
            let mut data = Vec::new();
            read_to_end_reserved(r, &mut data, res, usize::MAX).await?;
            Ok(Some(data))
        }
        Read::Upto(max) => read_upto(r, max.min(STDIN_CHUNK)).await.map(Some),
    }
}

pub(crate) fn env_module() -> NativeModule {
    let mut m = NativeModule::std("env", ModKind::Std, include_str!("env.sand"));
    m.vm_function("get", |vm: &mut Vm, name: Val<String>| -> Result<Option<String>, Ctl> {
        let name = name.str(&vm.heap).to_string();
        if !vm.permit(&Capability::Env(name.clone()))? {
            return Ok(None);
        }
        Ok(match &vm.host.env {
            Some(vars) => vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()),
            // `std::env::var` may panic on such names.
            #[cfg(feature = "os")]
            None if name.is_empty() || name.contains(['=', '\0']) => None,
            #[cfg(feature = "os")]
            None => std::env::var(&name).ok(),
            // No process environment without operating-system access.
            #[cfg(not(feature = "os"))]
            None => None,
        })
    });
    m.vm_function("args", |vm: &mut Vm| vm.host.args.clone());
    m.vm_function("vars", |vm: &mut Vm| -> Result<BTreeMap<String, String>, Ctl> {
        if !vm.permit(&Capability::Env("*".into()))? {
            return Ok(BTreeMap::new());
        }
        Ok(match &vm.host.env {
            Some(vars) => vars.iter().cloned().collect(),
            #[cfg(not(feature = "os"))]
            None => BTreeMap::new(),
            #[cfg(feature = "os")]
            None => std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect(),
        })
    });
    m
}
