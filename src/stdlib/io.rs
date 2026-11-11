//! The `io` and `env` modules.

use super::stream::{self, IoFailure, Suspended, bytes_read, line_read, read_line, read_to_end_reserved, read_upto, strip_eol};
use crate::check::ModKind;
use crate::native::convert::{HostError, IntoVmValue};
use crate::native::func::Later;
use crate::native::ty::VmVal;
use crate::native::{NativeModule, NativeObject, ObjectType};
use crate::policy::Capability;
use crate::stdin_feed::StdinChannel;
use crate::vm::iomem::Reservation;
use crate::vm::value::{Obj, VmValue, to_usize_saturating};
use crate::vm::{Ctl, Vm, WaitKind};
use std::collections::BTreeMap;
use tokio::io::AsyncBufRead;

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("io", ModKind::Std, include_str!("io.sand"));
    m.bind::<Stdin>();
    m.bind::<Stdout>();
    m.bind::<Stderr>();
    m.function("new_stdin", |vm: &mut Vm| vm.new_object(Stdin));
    m.function("new_stdout", |vm: &mut Vm| vm.new_object(Stdout));
    m.function("new_stderr", |vm: &mut Vm| vm.new_object(Stderr));
    m.function("Stdin.read", |vm: &mut Vm, _: VmVal<Stdin>, max: i64| {
        if max <= 0 {
            return Err(HostError::new("invalid", "read: max must be positive"));
        }
        read_stdin(vm, Read::Upto(to_usize_saturating(max)), bytes_read)
    });
    m.function("Stdin.read_line", |vm: &mut Vm, _: VmVal<Stdin>| read_stdin(vm, Read::Line, input_line));
    m.function("Stdin.read_all", |vm: &mut Vm, _: VmVal<Stdin>| read_stdin(vm, Read::All, bytes_read));
    output::<Stdout>(&mut m);
    output::<Stderr>(&mut m);
    m
}

/// A handle to the standard input stream.
#[derive(crate::SandObject)]
#[sand(module = "io")]
struct Stdin;

/// A handle to the standard output stream.
#[derive(crate::SandObject)]
#[sand(module = "io")]
struct Stdout;

/// A handle to the standard error stream.
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
    m.function(&format!("{}.write", T::NAME), |vm: &mut Vm, _: VmVal<T>, data: VmVal<Vec<u8>>| write_out(vm, T::STREAM, data.raw()));
    m.function(&format!("{}.write_str", T::NAME), |vm: &mut Vm, _: VmVal<T>, text: VmVal<String>| write_out(vm, T::STREAM, text.raw()));
    m.function(&format!("{}.flush", T::NAME), |vm: &mut Vm, _: VmVal<T>| flush_out(vm, T::STREAM));
}

/// Writes the `str` or `bytes` value `v` to stdout (1) or stderr (2).
fn write_out(vm: &mut Vm, which: u8, v: VmValue) -> Result<(), HostError> {
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

fn read_stdin<R: IntoVmValue + 'static>(
    vm: &mut Vm,
    what: Read,
    done: fn(&mut Vm, Option<Vec<u8>>) -> stream::FinishResult<R>,
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
        let data =
            read_input(stdin.as_deref(), what, &mut res).await.map_err(|e| IoFailure::new(e.kind, format!("stdin: {}", e.message)))?;
        Ok((data, res))
    };
    let wait_kind = vm.host.stdin.as_ref().map_or(WaitKind::Io, |channel| channel.wait_kind);
    stream::suspend(wait_kind, read, move |vm, (data, res)| {
        drop(res);
        done(vm, data)
    })
}

fn input_line(vm: &mut Vm, line: Option<Vec<u8>>) -> stream::FinishResult<Option<String>> {
    line_read(vm, line, "stdin: input is not valid UTF-8")
}

/// The most `read` returns at once from standard input.
const STDIN_CHUNK: usize = 64 << 10;

/// Takes a buffered line from standard input without waiting.
///
/// Returns `Some(Some(line))` without the trailing `\n` or `\r\n`,
/// `Some(None)` at end-of-file, or `None` if a complete line is not buffered,
/// another read is active, or the line exceeds the memory limit.
fn buffered_line(vm: &Vm) -> Option<Option<Vec<u8>>> {
    let mut read = vm.host.stdin.as_ref()?.try_begin_read()?;
    let (buf, at_end) = read.available();
    let len = match buf.iter().position(|&b| b == b'\n') {
        Some(nl) => nl + 1,
        None if at_end => buf.len(),
        None => return None,
    };
    if len == 0 {
        return Some(None);
    }
    if vm.heap.would_exceed(len) {
        return None;
    }
    let mut line = buf[..len].to_vec();
    std::pin::Pin::new(&mut read).consume(len);
    strip_eol(&mut line);
    Some(Some(line))
}

/// Reads `what` from `stdin`, returning `Ok(None)` if `Read::Line` reaches
/// end-of-file.
async fn read_input(stdin: Option<&StdinChannel>, what: Read, res: &mut Reservation) -> Result<Option<Vec<u8>>, IoFailure> {
    let stdin = stdin.ok_or_else(|| IoFailure::new("io", "the engine has no standard input"))?;
    read_from(&mut stdin.begin_read().await, what, res).await
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
    m.function("get", |vm: &mut Vm, name: VmVal<String>| -> Result<Option<String>, Ctl> {
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
    m.function("args", |vm: &mut Vm| vm.host.args.clone());
    m.function("vars", |vm: &mut Vm| -> Result<BTreeMap<String, String>, Ctl> {
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
