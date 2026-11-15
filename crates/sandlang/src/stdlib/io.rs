//! The `io` and `env` modules.

use super::stream::{self, IoFailure, Suspended, decode_utf8_line, read_line, read_to_end_reserved, read_upto, strip_eol};
use crate::check::ModKind;
use crate::native::convert::HostError;
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
        read_stdin_bytes(vm, StdinReadMode::UpTo(to_usize_saturating(max)))
    });
    m.function("Stdin.read_line", |vm: &mut Vm, _: VmVal<Stdin>| read_stdin_line(vm));
    m.function("Stdin.read_all", |vm: &mut Vm, _: VmVal<Stdin>| read_stdin_bytes(vm, StdinReadMode::All));
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

/// Read mode for standard input.
#[derive(Clone, Copy, PartialEq)]
enum StdinReadMode {
    /// Single line without its line ending.
    Line,
    /// Remaining bytes until end of input.
    All,
    /// Maximum byte count to read from the next available chunk.
    UpTo(usize),
}

const INVALID_STDIN_UTF8: &str = "stdin: input is not valid UTF-8";

/// Reads bytes from standard input for `Stdin.read(max)` and `Stdin.read_all()`.
fn read_stdin_bytes(vm: &mut Vm, mode: StdinReadMode) -> Suspended<Vec<u8>> {
    prepare_stdin_read(vm)?;
    let (wait_kind, read_future) = start_stdin_read(vm, mode)?;
    stream::suspend(wait_kind, async move { Ok(read_future.await?.unwrap_or_default()) })
}

/// Reads a UTF-8 line from standard input for `Stdin.read_line()`.
fn read_stdin_line(vm: &mut Vm) -> Suspended<Option<String>> {
    prepare_stdin_read(vm)?;
    if let Some(line) = buffered_line(vm) {
        return Ok(Later::ready(decode_utf8_line(vm, line, INVALID_STDIN_UTF8)));
    }
    let (wait_kind, line_future) = start_stdin_read(vm, StdinReadMode::Line)?;
    stream::suspend_line(wait_kind, line_future, INVALID_STDIN_UTF8)
}

/// Checks the `Stdin` capability. Flushes buffered output before reading.
fn prepare_stdin_read(vm: &mut Vm) -> Result<(), HostError> {
    if !vm.permit(&Capability::Stdin)? {
        return Err(HostError::new("permission_denied", "permission denied: read standard input"));
    }
    Ok(vm.flush_before_input()?)
}

/// Result of an off-heap standard input read.
type StdinReadOutput = Result<Option<Vec<u8>>, IoFailure>;

/// Reserves I/O memory for a standard input read. Returns its [`WaitKind`] and read future.
fn start_stdin_read(
    vm: &mut Vm,
    mode: StdinReadMode,
) -> Result<(WaitKind, impl Future<Output = StdinReadOutput> + Send + 'static), HostError> {
    let stdin = vm.host.stdin.clone();
    // Count the off-heap read buffer against the I/O memory limit during the read.
    let mut reservation = match mode {
        StdinReadMode::UpTo(max) => vm.reserve_io(max.min(STDIN_CHUNK))?,
        _ => vm.io_reservation(),
    };
    let read_future = async move {
        let data = read_input(stdin.as_deref(), mode, &mut reservation)
            .await
            .map_err(|e| IoFailure::new(e.kind, format!("stdin: {}", e.message)))?;
        drop(reservation);
        Ok(data)
    };
    let wait_kind = vm.host.stdin.as_ref().map_or(WaitKind::Io, |channel| channel.wait_kind);
    Ok((wait_kind, read_future))
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

/// Reads from `stdin` according to `mode`. Returns `Ok(None)` at end of input in [`StdinReadMode::Line`] mode.
async fn read_input(
    stdin: Option<&StdinChannel>,
    mode: StdinReadMode,
    reservation: &mut Reservation,
) -> Result<Option<Vec<u8>>, IoFailure> {
    let stdin = stdin.ok_or_else(|| IoFailure::new("io", "the engine has no standard input"))?;
    read_from(&mut stdin.begin_read().await, mode, reservation).await
}

async fn read_from<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    mode: StdinReadMode,
    reservation: &mut Reservation,
) -> Result<Option<Vec<u8>>, IoFailure> {
    match mode {
        StdinReadMode::Line => read_line(reader, reservation).await,
        StdinReadMode::All => {
            let mut data = Vec::new();
            read_to_end_reserved(reader, &mut data, reservation, usize::MAX).await?;
            Ok(Some(data))
        }
        StdinReadMode::UpTo(max) => read_upto(reader, max.min(STDIN_CHUNK)).await.map(Some),
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
