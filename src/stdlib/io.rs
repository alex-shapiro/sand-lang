//! The `io` and `env` modules.

use super::stream::{self, IoFailure, Suspended, bytes_read, line_read, read_line, read_to_end_reserved, read_upto, strip_eol};
use crate::check::ModKind;
use crate::journal::OpKind;
use crate::native::convert::{HostError, IntoValue};
use crate::native::func::{Call, Later};
use crate::native::ty::Val;
use crate::native::{NativeModule, NativeObject, ObjectType};
use crate::policy::Capability;
use crate::stdlib::core::Error;
use crate::vm::iomem::Reservation;
use crate::vm::journal::{Begun, BuildFn, Effects, Identity, OpSpec, Outcome, Parking, Payload, Ready, Whole};
use crate::vm::value::{Obj, Value, to_usize_saturating, u64_to_usize_saturating};
use crate::vm::{AfterEffect, Ctl, StdinSource, Vm};
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
    m.vm_function("Stdin.read", |vm: &mut Vm, call: Call, _: Val<Stdin>, max: i64| {
        if max <= 0 {
            return Err(HostError::new("invalid", "read: max must be positive"));
        }
        read_stdin(vm, call, Read::Upto(to_usize_saturating(max)), bytes_read)
    });
    m.vm_function("Stdin.read_line", |vm: &mut Vm, call: Call, _: Val<Stdin>| read_stdin(vm, call, Read::Line, input_line));
    m.vm_function("Stdin.read_all", |vm: &mut Vm, call: Call, _: Val<Stdin>| read_stdin(vm, call, Read::All, bytes_read));
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
    call: Call,
    what: Read,
    done: fn(&Vm, Option<Vec<u8>>) -> Result<R, HostError>,
) -> Suspended<R> {
    if !vm.permit(&Capability::Stdin)? {
        return Err(HostError::new("permission_denied", "permission denied: read standard input"));
    }
    // A prompt written before reading must be visible (on a terminal).
    vm.flush_before_input()?;
    // Input that the host sends as the run goes is an input of the run's
    // journal; input fixed by the configuration is not.
    let external = !vm.host.stdin.try_lock().is_ok_and(|s| matches!(*s, StdinSource::None | StdinSource::Bytes(_)));
    let (code, label) = match what {
        Read::Line => (0, "read_line".to_string()),
        Read::All => (1, "read_all".to_string()),
        Read::Upto(n) => (2 + n as u64, format!("read {n}")),
    };
    let identity = Identity::default().add(&code.to_le_bytes()).finish();
    let describe = |_: &Vm| Ok((identity, label.clone()));
    if what == Read::Line {
        let line = if external { vm.ready_input(describe, |vm: &mut Vm| buffered_line(vm))? } else { buffered_line(vm) };
        match line {
            Ready::Pending => {}
            Ready::End => return Ok(Later::ready(done(vm, None))),
            Ready::Data(data) => return Ok(Later::ready(done(vm, Some(data)))),
        }
    }
    let stdin = vm.host.stdin.clone();
    // The input is buffered outside the heap until it is complete.
    let claim = vm.claim(call, None, if let Read::Upto(max) = what { max.min(STDIN_CHUNK) } else { 0 })?;
    let read = async move {
        let (_, mut res) = claim.take().map_err(|e| IoFailure::new("limit", e))?;
        let data = read_input(&stdin, what, &mut res).await.map_err(|e| IoFailure::new(e.kind, format!("stdin: {}", e.message)))?;
        Ok((data, res))
    };
    if !external {
        return stream::suspend(read, move |fx, (data, res)| {
            drop(res);
            done(fx, data)
        });
    }
    let build: BuildFn = Box::new(move |fx: &mut AfterEffect<'_>, payload: &[u8]| -> crate::vm::NResult {
        let read = StdinRead::from_payload(payload).ok_or_else(|| Ctl::internal("a journaled read of standard input is malformed"))?;
        read.map_err(HostError::from).and_then(|data| done(fx, data)).into_value(fx)
    });
    let spec = OpSpec { kind: OpKind::Stdin, effects: Effects::Pure, parking: Parking::Never, pre_fuel: 0, timer: None };
    Err(match vm.begin_op(spec, describe)? {
        Begun::Replayed(r) => r.wait(build),
        Begun::Live(live) => live.run(
            async move {
                let read: StdinRead = read.await.map(|(data, _)| data).map_err(Error::from);
                Box::new(Whole(read.to_payload())) as Box<dyn Outcome>
            },
            build,
        ),
    }
    .into())
}

fn input_line(vm: &Vm, line: Option<Vec<u8>>) -> Result<Option<String>, HostError> {
    line_read(vm, line, "stdin: input is not valid UTF-8")
}

/// The outcome of a read of standard input as the journal records it:
/// the data, `None` for a line at the end of the input, or the error.
type StdinRead = Result<Option<Vec<u8>>, Error>;

/// The most `read` returns at once from standard input.
const STDIN_CHUNK: usize = 64 << 10;

/// The next line of standard input without its terminator, if it is in a
/// buffer already. Gives [`Ready::Pending`], and consumes nothing, if the
/// read must wait, another is under way, or the line does not fit in memory.
fn buffered_line(vm: &Vm) -> Ready {
    let Ok(mut source) = vm.host.stdin.try_lock() else { return Ready::Pending };
    let _op = match &mut *source {
        StdinSource::Fed(r) => Some(r.begin()),
        _ => None,
    };
    let (buf, at_end) = match &mut *source {
        StdinSource::None => return Ready::End,
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
        None => return Ready::Pending,
    };
    if take == 0 {
        return Ready::End;
    }
    if vm.heap.would_exceed(take) {
        return Ready::Pending;
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
    Ready::Data(data)
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
    m.vm_function("get", |vm: &mut Vm, name: Val<String>| -> Result<Val<Option<String>>, Ctl> {
        let name = name.str(&vm.heap).to_string();
        if !vm.permit(&Capability::Env(name.clone()))? {
            return Ok(Val::from_raw(vm.none()));
        }
        let label = format!("env.get {name}");
        let lookup = move |vm: &mut Vm| match &vm.host.env {
            Some(vars) => vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()).into_value(&mut AfterEffect::new(vm)),
            None => process_var(&name).into_value(&mut AfterEffect::new(vm)),
        };
        Ok(Val::from_raw(process_input(vm, &label, lookup)?))
    });
    m.vm_function("args", |vm: &mut Vm| vm.host.args.clone());
    m.vm_function("vars", |vm: &mut Vm| -> Result<Val<BTreeMap<String, String>>, Ctl> {
        if !vm.permit(&Capability::Env("*".into()))? {
            return Ok(Val::from_raw(BTreeMap::<String, String>::new().into_value(&mut AfterEffect::new(vm))?));
        }
        let lookup = |vm: &mut Vm| match &vm.host.env {
            Some(vars) => vars.iter().cloned().collect::<BTreeMap<_, _>>().into_value(&mut AfterEffect::new(vm)),
            None => process_vars().into_value(&mut AfterEffect::new(vm)),
        };
        Ok(Val::from_raw(process_input(vm, "env.vars", lookup)?))
    });
    m
}

/// Runs `lookup`, which reads the process environment unless the
/// configuration fixes it. A journaled run records what the process
/// environment gives.
fn process_input(vm: &mut Vm, label: &str, lookup: impl FnOnce(&mut Vm) -> crate::vm::NResult) -> crate::vm::NResult {
    if vm.host.env.is_some() {
        return lookup(vm);
    }
    vm.input(|_| Ok((Identity::default().add(label.as_bytes()).finish(), label.to_string())), lookup)
}

/// The process's environment variable `name`.
fn process_var(name: &str) -> Option<String> {
    #[cfg(feature = "os")]
    {
        // `std::env::var` may panic on such names.
        if name.is_empty() || name.contains(['=', '\0']) {
            return None;
        }
        std::env::var(name).ok()
    }
    // No process environment without operating-system access.
    #[cfg(not(feature = "os"))]
    {
        let _ = name;
        None
    }
}

/// The process's environment variables.
fn process_vars() -> BTreeMap<String, String> {
    #[cfg(feature = "os")]
    {
        std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect()
    }
    #[cfg(not(feature = "os"))]
    {
        BTreeMap::new()
    }
}
