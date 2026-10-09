//! The `fs` module.
//!
//! Every native resolves its paths against the engine's working directory
//! with the engine's file system ([`Fs`]: on the host, canonicalising
//! existing prefixes, so `..` and symlinks cannot escape) and checks the
//! policy on the resolved path *before* touching the file system. On the
//! host, the I/O itself runs off the VM thread through `files::safe`, which
//! never follows a symlink, so the operation reaches exactly the checked
//! location even if the file system changes in between. Operations on a
//! memory file system complete at once.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::files::{EntryKind, Fs, is_unchecked_symlink, unchecked_symlink};
use crate::policy::{Capability, MAX_PATH_BYTES};
use crate::vm::iomem::Reservation;
use crate::vm::value::{Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, Vm, panic};
use std::io;
use std::path::{Path, PathBuf};

/// `target!(vm, value)`: the script-supplied path (for messages) and its
/// resolved form (see [`target`]); returns the `Err(Error)` value from the
/// native for an unusable path.
macro_rules! target {
    ($vm:expr, $p:expr) => {
        match target($vm, $p, false)? {
            Ok(t) => t,
            Err(e) => return Ok(e),
        }
    };
}

/// Like `target!`, but a final symlink is not followed: for operations on
/// the directory entry itself (removing or renaming a link must not affect,
/// or require permission for, its target).
macro_rules! entry_target {
    ($vm:expr, $p:expr) => {
        match target($vm, $p, true)? {
            Ok(t) => t,
            Err(e) => return Ok(e),
        }
    };
}

pub fn register(r: &mut Registry) {
    r.source("fs", ModKind::Std, include_str!("fs.sand"));

    r.add_builtin("fs.read_bytes", |vm, a| {
        let (shown, path) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()), &shown)? {
            return Ok(e);
        }
        let res = vm.io_reservation();
        run(
            vm,
            shown,
            move |fs| fs.read(&path, res),
            |vm, (data, res)| {
                drop(res);
                vm.reserve(data.len())?;
                vm.charge_bytes(data.len())?;
                let b = vm.heap.new_bytes(data);
                Ok(vm.ok(b))
            },
        )
    });
    r.add_builtin("fs.read_text", |vm, a| {
        let (shown, path) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()), &shown)? {
            return Ok(e);
        }
        let res = vm.io_reservation();
        run(
            vm,
            shown.clone(),
            move |fs| fs.read(&path, res),
            move |vm, (data, res)| {
                drop(res);
                vm.reserve(data.len())?;
                // UTF-8 validation.
                vm.charge_slow_bytes(data.len())?;
                match String::from_utf8(data) {
                    Ok(text) => {
                        let s = vm.new_str(text);
                        Ok(vm.ok(s))
                    }
                    Err(_) => Ok(vm.err_value("invalid", &format!("{shown}: file is not valid UTF-8"))),
                }
            },
        )
    });
    r.add_builtin("fs.write_text", |vm, a| {
        let res = match vm.reserve_io(vm.str(a[1]).len()) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };
        vm.charge_bytes(vm.str(a[1]).len())?;
        let data = vm.str(a[1]).as_bytes().to_vec();
        write(vm, a[0], data, false, res)
    });
    r.add_builtin("fs.write_bytes", |vm, a| {
        let res = match vm.reserve_io(vm.heap.bytes_of(a[1]).len()) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };
        vm.charge_bytes(vm.heap.bytes_of(a[1]).len())?;
        let data = vm.heap.bytes_of(a[1]).to_vec();
        write(vm, a[0], data, false, res)
    });
    r.add_builtin("fs.append_text", |vm, a| {
        let res = match vm.reserve_io(vm.str(a[1]).len()) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };
        vm.charge_bytes(vm.str(a[1]).len())?;
        let data = vm.str(a[1]).as_bytes().to_vec();
        write(vm, a[0], data, true, res)
    });
    r.add_builtin("fs.exists", |vm, a| file_kind(vm, a[0], |k| k != EntryKind::Symlink));
    r.add_builtin("fs.is_file", |vm, a| file_kind(vm, a[0], |k| k == EntryKind::File));
    r.add_builtin("fs.is_dir", |vm, a| file_kind(vm, a[0], |k| k == EntryKind::Dir));
    r.add_builtin("fs.list_dir", |vm, a| {
        let (shown, path) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()), &shown)? {
            return Ok(e);
        }
        let res = vm.io_reservation();
        run(
            vm,
            shown,
            move |fs| fs.list_dir(&path, res),
            |vm, (names, res)| {
                drop(res);
                let bytes = names.iter().map(|n| n.len() + 32).sum();
                vm.reserve(bytes)?;
                // Reading, converting and sorting the names (done off the VM thread).
                let n = names.len() as u64;
                vm.charge(n * (1 + u64::from(n.max(1).ilog2())))?;
                vm.charge_bytes(bytes)?;
                let items = names.into_iter().map(|n| vm.new_str(n)).collect();
                let list = vm.heap.new_list(items);
                Ok(vm.ok(list))
            },
        )
    });
    r.add_builtin("fs.create_dir", |vm, a| {
        let (shown, path) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsWrite(path.clone()), &shown)? {
            return Ok(e);
        }
        if !a[1].as_bool() {
            return run(vm, shown, move |fs| fs.create_dir(&path), ok_unit);
        }
        // Every directory to be created must be writable, not just the last.
        let missing = vm.host.fs.missing_ancestors(&path);
        for dir in &missing {
            if let Some(e) = refuse(vm, Capability::FsWrite(dir.clone()), &shown)? {
                return Ok(e);
            }
        }
        // The others must exist already (they are not created).
        let names = path.components().filter(|c| matches!(c, std::path::Component::Normal(_))).count();
        let keep = names.saturating_sub(missing.len() + 1);
        run(vm, shown, move |fs| fs.create_dir_all(&path, keep), ok_unit)
    });
    r.add_builtin("fs.remove_file", |vm, a| {
        let (shown, path) = entry_target!(vm, a[0]);
        if let Some(e) = refuse_entry(vm, Capability::FsWrite(path.clone()), &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.remove_file(&path), ok_unit)
    });
    r.add_builtin("fs.remove_empty_dir", |vm, a| {
        let (shown, path) = entry_target!(vm, a[0]);
        if let Some(e) = refuse_entry(vm, Capability::FsWrite(path.clone()), &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.remove_dir(&path), ok_unit)
    });
    r.add_builtin("fs.check_tree", |vm, a| {
        let (shown, path) = entry_target!(vm, a[0]);
        if let Some(e) = refuse_entry(vm, Capability::FsWrite(path.clone()), &shown)? {
            return Ok(e);
        }
        if let Some(e) = protected_tree(vm, &shown, &path) {
            return Ok(e);
        }
        Ok(vm.ok(Value::Unit))
    });
    r.add_builtin("fs.entry_type", |vm, a| {
        let (shown, path) = entry_target!(vm, a[0]);
        if let Some(e) = refuse_entry(vm, Capability::FsRead(path.clone()), &shown)? {
            return Ok(e);
        }
        run(
            vm,
            shown,
            move |fs| fs.stat(&path),
            |vm, m| {
                let kind = match m.kind {
                    EntryKind::Symlink => "symlink",
                    EntryKind::Dir => "dir",
                    EntryKind::File => "file",
                    EntryKind::Other => "other",
                };
                let s = vm.new_str(kind);
                Ok(vm.ok(s))
            },
        )
    });
    r.add_builtin("fs.rename", |vm, a| {
        let (shown, from) = entry_target!(vm, a[0]);
        let (shown_to, to) = entry_target!(vm, a[1]);
        for (cap, shown) in [(Capability::FsWrite(from.clone()), &shown), (Capability::FsWrite(to.clone()), &shown_to)] {
            if let Some(e) = refuse_entry(vm, cap, shown)? {
                return Ok(e);
            }
        }
        // Moving a directory moves everything below it, and replacing `to` replaces its tree.
        for (shown, path) in [(&shown, &from), (&shown_to, &to)] {
            if let Some(e) = protected_tree(vm, shown, path) {
                return Ok(e);
            }
        }
        // A memory file system walks the moved tree (to check its paths' lengths).
        if let Some(m) = vm.host.fs.memory() {
            vm.charge(m.entries_below(&from) as u64)?;
        }
        run(vm, shown, move |fs| fs.rename(&from, &to), ok_unit)
    });
    r.add_builtin("fs.copy", |vm, a| {
        let (shown, from) = target!(vm, a[0]);
        let (shown_to, to) = target!(vm, a[1]);
        for (cap, shown) in [(Capability::FsRead(from.clone()), &shown), (Capability::FsWrite(to.clone()), &shown_to)] {
            if let Some(e) = refuse(vm, cap, shown)? {
                return Ok(e);
            }
        }
        // A memory file system copies on the VM thread.
        if !vm.host.fs.is_blocking()
            && let Ok(m) = vm.host.fs.stat(&from)
        {
            vm.charge_bytes(usize::try_from(m.size).unwrap_or(usize::MAX))?;
        }
        run(vm, shown, move |fs| fs.copy(&from, &to), ok_unit)
    });
    r.add_builtin("fs.metadata", |vm, a| {
        let (shown, path) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, Capability::FsRead(path.clone()), &shown)? {
            return Ok(e);
        }
        let stat = move |fs: &Fs| match fs.stat(&path)? {
            m if m.kind == EntryKind::Symlink => Err(unchecked_symlink(path.file_name().unwrap_or_default())),
            m => Ok(m),
        };
        run(vm, shown, stat, |vm, m| {
            let modified = match m.modified {
                Some(t) => {
                    let nanos = Value::Int(t);
                    let dt = new_struct(vm, "time", "DateTime", &[("unix_nanos", nanos)])?;
                    vm.some(dt)
                }
                None => vm.none(),
            };
            let size = Value::Int(i64::try_from(m.size).unwrap_or(i64::MAX));
            let (is_file, is_dir) = (Value::Bool(m.kind == EntryKind::File), Value::Bool(m.kind == EntryKind::Dir));
            let md = new_struct(vm, "fs", "Metadata", &[("size", size), ("is_file", is_file), ("is_dir", is_dir), ("modified", modified)])?;
            Ok(vm.ok(md))
        })
    });

    // Pure path helpers (charged for scanning and copying their input).
    r.add_builtin("fs.join", |vm, a| {
        let parts = vm.heap.list(a[0]);
        let total = parts.iter().map(|&p| vm.str(p).len() + 1).fold(0usize, usize::saturating_add);
        vm.charge(parts.len() as u64)?;
        vm.charge_bytes(total)?;
        vm.reserve(total)?;
        let mut path = PathBuf::new();
        for &part in vm.heap.list(a[0]) {
            path.push(vm.str(part));
        }
        let s = path.to_string_lossy().into_owned();
        Ok(vm.new_str(s))
    });
    r.add_builtin("fs.basename", |vm, a| {
        lexical_cost(vm, a[0])?;
        let s = Path::new(vm.str(a[0])).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(vm.new_str(s))
    });
    r.add_builtin("fs.dirname", |vm, a| {
        lexical_cost(vm, a[0])?;
        let s = Path::new(vm.str(a[0])).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(vm.new_str(s))
    });
    r.add_builtin("fs.extension", |vm, a| {
        lexical_cost(vm, a[0])?;
        let ext = Path::new(vm.str(a[0])).extension().map(|e| e.to_string_lossy().into_owned());
        let v = ext.map(|e| vm.new_str(e));
        Ok(vm.option(v))
    });
}

/// The script-supplied path `p` and its resolved form (the entry's own path
/// if `entry`). Overlong paths are refused (kind `"invalid"`) before
/// normalization, which is charged per component.
fn target(vm: &mut Vm, p: Value, entry: bool) -> Result<Result<(String, PathBuf), Value>, Ctl> {
    let shown = vm.str(p);
    if shown.len() > MAX_PATH_BYTES {
        return Ok(Err(vm.err_value("invalid", &format!("path too long ({} bytes; the limit is {MAX_PATH_BYTES})", shown.len()))));
    }
    if shown.contains('\0') {
        return Ok(Err(vm.err_value("invalid", "path contains a NUL character")));
    }
    let shown = shown.to_string();
    let joined = vm.host.cwd.join(&shown);
    // Lexical normalization, a few `canonicalize` calls (binary search) and
    // the policy's checks, each linear in the length.
    vm.charge(4 * joined.components().count() as u64)?;
    vm.charge_bytes(joined.as_os_str().len().saturating_mul(4))?;
    let path = if entry {
        let e = vm.host.fs.entry(&joined);
        vm.charge(4 * e.scanned as u64)?;
        vm.charge_bytes(e.scanned_bytes.saturating_mul(4))?;
        e.path
    } else {
        vm.host.fs.resolve(&joined)
    };
    Ok(Ok((shown, path)))
}

/// The error for a denied `cap`. It names the path as the script spelled it
/// (`shown`): the resolved path may reveal symlink targets or the stored
/// spelling of names the script may not access.
fn denied(vm: &mut Vm, cap: &Capability, shown: &str) -> Value {
    let verb = match cap {
        Capability::FsRead(_) => "read",
        _ => "write",
    };
    vm.err_value("permission_denied", &format!("permission denied: {verb} {shown}"))
}

/// Checks `cap` against the policy: `Some(Err(Error))` to return if it is
/// denied, `None` if the effect may proceed.
fn refuse(vm: &mut Vm, cap: Capability, shown: &str) -> Result<Option<Value>, Ctl> {
    if vm.permit_path(&cap, false)? {
        return Ok(None);
    }
    Ok(Some(denied(vm, &cap, shown)))
}

/// Like [`refuse`] for a capability on a directory entry itself.
fn refuse_entry(vm: &mut Vm, cap: Capability, shown: &str) -> Result<Option<Value>, Ctl> {
    if vm.permit_path(&cap, true)? {
        return Ok(None);
    }
    Ok(Some(denied(vm, &cap, shown)))
}

/// The `Err(permission_denied)` value if a deny rule protects something in
/// the tree at `path` (which is about to be moved, replaced or removed).
fn protected_tree(vm: &mut Vm, shown: &str, path: &Path) -> Option<Value> {
    let protected = vm.host.policy.denies_under_in(path, vm.host.fs.space());
    protected.then(|| vm.err_value("permission_denied", &format!("permission denied: {shown} contains paths protected by the policy")))
}

/// Runs the file system operation `op`; `finish` converts its result on the
/// VM thread. I/O errors become `Err(Error)` values mentioning `shown`.
/// Operations that block (on the host) suspend the calling task while they
/// run off the VM thread; the others complete at once.
fn run<T: Send + 'static>(
    vm: &mut Vm,
    shown: String,
    op: impl FnOnce(&Fs) -> io::Result<T> + Send + 'static,
    finish: impl FnOnce(&mut Vm, T) -> NResult + Send + 'static,
) -> NResult {
    let fs = vm.host.fs.clone();
    if !fs.is_blocking() {
        let result = op(&fs);
        return complete(vm, &shown, result, finish);
    }
    Err(Ctl::Async(Box::pin(async move {
        let result = fs.run_blocking(op).await;
        Box::new(move |vm: &mut Vm| complete(vm, &shown, result, finish)) as Finisher
    })))
}

fn complete<T>(vm: &mut Vm, shown: &str, result: io::Result<T>, finish: impl FnOnce(&mut Vm, T) -> NResult) -> NResult {
    match result {
        Ok(v) => finish(vm, v),
        Err(e) => Ok(vm.err_value(io_kind(&e), &format!("{shown}: {e}"))),
    }
}

fn ok_unit<T>(vm: &mut Vm, _: T) -> NResult {
    Ok(vm.ok(Value::Unit))
}

fn ok_bool(vm: &mut Vm, b: bool) -> NResult {
    Ok(vm.ok(Value::Bool(b)))
}

/// Writes `data` (whose copy is reserved in `res` until the write is done).
fn write(vm: &mut Vm, path: Value, data: Vec<u8>, append: bool, res: Reservation) -> NResult {
    let (shown, path) = target!(vm, path);
    if let Some(e) = refuse(vm, Capability::FsWrite(path.clone()), &shown)? {
        return Ok(e);
    }
    let op = move |fs: &Fs| {
        let r = fs.write(&path, &data, append);
        drop(res);
        r
    };
    run(vm, shown, op, ok_unit)
}

/// Charges for scanning the path `p` and copying (part of) it, and checks
/// the memory limit for the copy.
fn lexical_cost(vm: &Vm, p: Value) -> Result<(), Ctl> {
    let n = vm.str(p).len();
    vm.charge_bytes(n.saturating_mul(2))?;
    vm.reserve(n)
}

/// `exists` / `is_file` / `is_dir`: `false` when nothing (or a symlink that
/// was not resolved by the policy check) is at the path.
fn file_kind(vm: &mut Vm, p: Value, test: fn(EntryKind) -> bool) -> NResult {
    let (shown, path) = target!(vm, p);
    if let Some(e) = refuse(vm, Capability::FsRead(path.clone()), &shown)? {
        return Ok(e);
    }
    let op = move |fs: &Fs| match fs.stat(&path) {
        Ok(m) => Ok(test(m.kind)),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) || is_unchecked_symlink(&e) => Ok(false),
        Err(e) => Err(e),
    };
    run(vm, shown, op, ok_bool)
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
