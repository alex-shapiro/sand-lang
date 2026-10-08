//! The `fs` module.
//!
//! Every native resolves its paths against the engine's working directory
//! (canonicalising existing prefixes, so `..` and symlinks cannot escape) and
//! checks the policy on the resolved path *before* touching the file system.
//! The I/O itself runs asynchronously on tokio.

use super::{io_kind, resolve_path};
use crate::LimitKind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::{Capability, normalize_path};
use crate::vm::value::{Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, Vm, panic};
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;

pub fn register(r: &mut Registry) {
    r.source("fs", ModKind::Std, include_str!("fs.sand"));

    r.add_builtin("fs.read_bytes", |vm, a| {
        let (shown, path) = target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
            return Ok(e);
        }
        let budget = budget(vm);
        run(shown, read_limited(path, budget), |vm, data| {
            let data = within_budget(vm, data)?;
            let b = vm.heap.new_bytes(data);
            Ok(vm.ok(b))
        })
    });
    r.add_builtin("fs.read_text", |vm, a| {
        let (shown, path) = target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
            return Ok(e);
        }
        let budget = budget(vm);
        run(shown.clone(), read_limited(path, budget), move |vm, data| match String::from_utf8(within_budget(vm, data)?) {
            Ok(text) => {
                let s = vm.new_str(text);
                Ok(vm.ok(s))
            }
            Err(_) => Ok(vm.err_value("invalid", &format!("{shown}: file is not valid UTF-8"))),
        })
    });
    r.add_builtin("fs.write_text", |vm, a| {
        let data = vm.str(a[1]).as_bytes().to_vec();
        write(vm, a[0], data, false)
    });
    r.add_builtin("fs.write_bytes", |vm, a| {
        let data = vm.heap.bytes_of(a[1]).to_vec();
        write(vm, a[0], data, false)
    });
    r.add_builtin("fs.append_text", |vm, a| {
        let data = vm.str(a[1]).as_bytes().to_vec();
        write(vm, a[0], data, true)
    });
    r.add_builtin("fs.exists", |vm, a| {
        let (shown, path) = target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
            return Ok(e);
        }
        run(shown, tokio::fs::try_exists(path), ok_bool)
    });
    r.add_builtin("fs.is_file", |vm, a| file_kind(vm, a[0], |m| m.is_file()));
    r.add_builtin("fs.is_dir", |vm, a| file_kind(vm, a[0], |m| m.is_dir()));
    r.add_builtin("fs.list_dir", |vm, a| {
        let (shown, path) = target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
            return Ok(e);
        }
        run(shown, list_dir(path), |vm, names| {
            let items = names.into_iter().map(|n| vm.new_str(n)).collect();
            let list = vm.heap.new_list(items);
            Ok(vm.ok(list))
        })
    });
    r.add_builtin("fs.create_dir", |vm, a| {
        let (shown, path) = target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsWrite(path.clone()))? {
            return Ok(e);
        }
        if a[1].as_bool() { run(shown, tokio::fs::create_dir_all(path), ok_unit) } else { run(shown, tokio::fs::create_dir(path), ok_unit) }
    });
    r.add_builtin("fs.remove_file", |vm, a| {
        let (shown, path) = entry_target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsWrite(path.clone()))? {
            return Ok(e);
        }
        run(shown, tokio::fs::remove_file(path), ok_unit)
    });
    r.add_builtin("fs.remove_empty_dir", |vm, a| {
        let (shown, path) = entry_target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsWrite(path.clone()))? {
            return Ok(e);
        }
        run(shown, tokio::fs::remove_dir(path), ok_unit)
    });
    r.add_builtin("fs.entry_type", |vm, a| {
        let (shown, path) = entry_target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
            return Ok(e);
        }
        run(shown, tokio::fs::symlink_metadata(path), |vm, m| {
            let t = m.file_type();
            let kind = if t.is_symlink() {
                "symlink"
            } else if t.is_dir() {
                "dir"
            } else if t.is_file() {
                "file"
            } else {
                "other"
            };
            let s = vm.new_str(kind);
            Ok(vm.ok(s))
        })
    });
    r.add_builtin("fs.rename", |vm, a| {
        let (shown, from) = entry_target(vm, a[0]);
        let (_, to) = entry_target(vm, a[1]);
        for cap in [Capability::FsWrite(from.clone()), Capability::FsWrite(to.clone())] {
            if let Some(e) = refuse(vm, cap)? {
                return Ok(e);
            }
        }
        run(shown, tokio::fs::rename(from, to), ok_unit)
    });
    r.add_builtin("fs.copy", |vm, a| {
        let (shown, from) = target(vm, a[0]);
        let (_, to) = target(vm, a[1]);
        for cap in [Capability::FsRead(from.clone()), Capability::FsWrite(to.clone())] {
            if let Some(e) = refuse(vm, cap)? {
                return Ok(e);
            }
        }
        run(shown, tokio::fs::copy(from, to), ok_unit)
    });
    r.add_builtin("fs.metadata", |vm, a| {
        let (shown, path) = target(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
            return Ok(e);
        }
        run(shown, tokio::fs::metadata(path), |vm, m| {
            let modified = match m.modified() {
                Ok(t) => {
                    let nanos = Value::Int(unix_nanos(t));
                    let dt = new_struct(vm, "time", "DateTime", &[("unix_nanos", nanos)])?;
                    vm.some(dt)
                }
                Err(_) => vm.none(),
            };
            let size = Value::Int(i64::try_from(m.len()).unwrap_or(i64::MAX));
            let md = new_struct(
                vm,
                "fs",
                "Metadata",
                &[("size", size), ("is_file", Value::Bool(m.is_file())), ("is_dir", Value::Bool(m.is_dir())), ("modified", modified)],
            )?;
            Ok(vm.ok(md))
        })
    });

    // Pure path helpers.
    r.add_builtin("fs.join", |vm, a| {
        let mut path = PathBuf::new();
        for &part in vm.heap.list(a[0]) {
            path.push(vm.str(part));
        }
        let s = path.to_string_lossy().into_owned();
        Ok(vm.new_str(s))
    });
    r.add_builtin("fs.basename", |vm, a| {
        let s = Path::new(vm.str(a[0])).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(vm.new_str(s))
    });
    r.add_builtin("fs.dirname", |vm, a| {
        let s = Path::new(vm.str(a[0])).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(vm.new_str(s))
    });
    r.add_builtin("fs.extension", |vm, a| {
        let ext = Path::new(vm.str(a[0])).extension().map(|e| e.to_string_lossy().into_owned());
        let v = ext.map(|e| vm.new_str(e));
        Ok(vm.option(v))
    });
}

/// The script-supplied path (for messages) and its resolved form.
fn target(vm: &Vm, p: Value) -> (String, PathBuf) {
    let shown = vm.str(p).to_string();
    let path = resolve_path(vm, &shown);
    (shown, path)
}

/// Like [`target`], but a final symlink is not followed: for operations on
/// the directory entry itself (removing or renaming a link must not affect,
/// or require permission for, its target).
fn entry_target(vm: &Vm, p: Value) -> (String, PathBuf) {
    let shown = vm.str(p).to_string();
    let mut lexical = PathBuf::new();
    for c in vm.host.cwd.join(&shown).components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let path = match (lexical.parent(), lexical.file_name()) {
        (Some(parent), Some(name)) => normalize_path(parent).join(name),
        _ => normalize_path(&lexical),
    };
    (shown, path)
}

/// Checks `cap` against the policy: `Some(Err(Error))` to return if it is
/// denied, `None` if the effect may proceed.
fn refuse(vm: &mut Vm, cap: Capability) -> Result<Option<Value>, Ctl> {
    if vm.permit(&cap)? {
        return Ok(None);
    }
    Ok(Some(vm.err_value("permission_denied", &format!("permission denied: {cap}"))))
}

/// The number of bytes that may still be allocated, to bound reads before
/// they happen.
fn budget(vm: &Vm) -> u64 {
    match vm.heap.limit {
        Some(limit) => limit.saturating_sub(vm.heap.bytes) as u64,
        None => 1 << 40,
    }
}

/// Runs `op` on the I/O runtime; `finish` converts its result on the VM
/// thread. I/O errors become `Err(Error)` values mentioning `shown`.
fn run<T: Send + 'static>(
    shown: String,
    op: impl Future<Output = io::Result<T>> + Send + 'static,
    finish: impl FnOnce(&mut Vm, T) -> NResult + Send + 'static,
) -> NResult {
    Err(Ctl::Async(Box::pin(async move {
        let result = op.await;
        Box::new(move |vm: &mut Vm| match result {
            Ok(v) => finish(vm, v),
            Err(e) => Ok(vm.err_value(io_kind(&e), &format!("{shown}: {e}"))),
        }) as Finisher
    })))
}

fn ok_unit<T>(vm: &mut Vm, _: T) -> NResult {
    Ok(vm.ok(Value::Unit))
}

fn ok_bool(vm: &mut Vm, b: bool) -> NResult {
    Ok(vm.ok(Value::Bool(b)))
}

/// Reads a file, giving up (`None`) once it exceeds `budget` bytes.
async fn read_limited(path: PathBuf, budget: u64) -> io::Result<Option<Vec<u8>>> {
    let file = tokio::fs::File::open(&path).await?;
    let len = file.metadata().await?.len();
    if len > budget {
        return Ok(None);
    }
    let mut data = Vec::with_capacity(len as usize);
    file.take(budget.saturating_add(1)).read_to_end(&mut data).await?;
    Ok((data.len() as u64 <= budget).then_some(data))
}

/// The data of a bounded read, or the memory limit error.
fn within_budget(vm: &Vm, data: Option<Vec<u8>>) -> Result<Vec<u8>, Ctl> {
    let data = data.ok_or(Ctl::Limit(LimitKind::Memory))?;
    vm.reserve(data.len())?;
    Ok(data)
}

fn write(vm: &mut Vm, path: Value, data: Vec<u8>, append: bool) -> NResult {
    let (shown, path) = target(vm, path);
    if let Some(e) = refuse(vm, Capability::FsWrite(path.clone()))? {
        return Ok(e);
    }
    if append {
        run(
            shown,
            async move {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new().append(true).create(true).open(path).await?;
                f.write_all(&data).await?;
                f.flush().await
            },
            ok_unit,
        )
    } else {
        run(shown, tokio::fs::write(path, data), ok_unit)
    }
}

/// `is_file` / `is_dir`: `false` when nothing exists at the path.
fn file_kind(vm: &mut Vm, p: Value, test: fn(&std::fs::Metadata) -> bool) -> NResult {
    let (shown, path) = target(vm, p);
    if let Some(e) = refuse(vm, Capability::FsRead(path.clone()))? {
        return Ok(e);
    }
    let op = async move {
        match tokio::fs::metadata(path).await {
            Ok(m) => Ok(test(&m)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    };
    run(shown, op, ok_bool)
}

async fn list_dir(path: PathBuf) -> io::Result<Vec<String>> {
    let mut dir = tokio::fs::read_dir(path).await?;
    let mut names = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn unix_nanos(t: SystemTime) -> i64 {
    let clamp = |n: u128| i64::try_from(n).unwrap_or(i64::MAX);
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => clamp(d.as_nanos()),
        Err(e) => -clamp(e.duration().as_nanos()),
    }
}

/// Builds an instance of the struct `module.name`, placing `fields` by name.
fn new_struct(vm: &mut Vm, module: &str, name: &str, fields: &[(&str, Value)]) -> NResult {
    let Some(def) = vm.prog.types.iter().position(|t| t.name == name && t.module == module) else {
        return panic(format!("internal error: {module}.{name} is not loaded"));
    };
    let mut values = Vec::with_capacity(fields.len());
    for field in &vm.prog.types[def].fields {
        match fields.iter().find(|(n, _)| n == field) {
            Some(&(_, v)) => values.push(v),
            None => return panic(format!("internal error: no value for {module}.{name}.{field}")),
        }
    }
    Ok(vm.heap.alloc(Obj::Struct(def as u32, values.into_boxed_slice())))
}
