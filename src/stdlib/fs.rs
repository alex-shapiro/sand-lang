//! The `fs` module.
//!
//! Every native resolves its paths against the engine's working directory
//! with the engine's file system ([`Fs::target`]: on the host, one walk
//! that resolves `..` and symlinks the way `realpath` does, so neither can
//! escape, and holds the directories it passes) and checks the policy on
//! the resolved path *before* the operation, which then acts through the
//! held directories without following a symlink, so it reaches exactly the
//! checked location even if the file system changes in between. Operations
//! run on the VM thread.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::files::{EntryKind, Fs, Mode, Target, is_unchecked_symlink, unchecked_symlink};
use crate::policy::{Capability, Decision, MAX_PATH_BYTES};
use crate::vm::iomem::Reservation;
use crate::vm::value::Value;
use crate::vm::{Ctl, NResult, Vm};
use std::io;
use std::path::{Path, PathBuf};

/// `target!(vm, value)`: the script-supplied path (for messages) and its
/// resolved form (see [`target`]); returns the `Err(Error)` value from the
/// native for an unusable path.
macro_rules! target {
    ($vm:expr, $p:expr) => {
        match target($vm, $p, Mode::Follow)? {
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
        match target($vm, $p, Mode::Entry)? {
            Ok(t) => t,
            Err(e) => return Ok(e),
        }
    };
}

pub fn register(r: &mut Registry) {
    r.source("fs", ModKind::Std, include_str!("fs.sand"));

    r.add_builtin("fs.read_bytes", |vm, a| {
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Read, &shown)? {
            return Ok(e);
        }
        let res = vm.io_reservation();
        run(
            vm,
            shown,
            move |fs| fs.read(t, res),
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
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Read, &shown)? {
            return Ok(e);
        }
        let res = vm.io_reservation();
        run(
            vm,
            shown.clone(),
            move |fs| fs.read(t, res),
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
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Read, &shown)? {
            return Ok(e);
        }
        let res = vm.io_reservation();
        run(
            vm,
            shown,
            move |fs| fs.list_dir(t, res),
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
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        if !a[1].as_bool() {
            return run(vm, shown, move |fs| fs.create_dir(t), ok_unit);
        }
        // Every directory to be created must be writable, not just the last.
        // (Those that exist are not created, whatever happens meanwhile.)
        for dir in vm.host.fs.missing_ancestors(&t) {
            if let Some(e) = refuse_path(vm, Capability::FsWrite(dir), &shown)? {
                return Ok(e);
            }
        }
        run(vm, shown, move |fs| fs.create_dir_all(t), ok_unit)
    });
    r.add_builtin("fs.remove_file", |vm, a| {
        let (shown, mut t) = entry_target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.remove_file(t), ok_unit)
    });
    r.add_builtin("fs.remove_empty_dir", |vm, a| {
        let (shown, mut t) = entry_target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.remove_dir(t), ok_unit)
    });
    r.add_builtin("fs.check_tree", |vm, a| {
        let (shown, mut t) = entry_target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        if let Some(e) = protected_tree(vm, &shown, &t.path) {
            return Ok(e);
        }
        Ok(vm.ok(Value::Unit))
    });
    r.add_builtin("fs.is_dir_entry", |vm, a| {
        let (shown, mut t) = entry_target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Read, &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.stat(&t), |vm, m| ok_bool(vm, m.kind == EntryKind::Dir))
    });
    r.add_builtin("fs.rename", |vm, a| {
        let (shown, mut from) = entry_target!(vm, a[0]);
        let (shown_to, mut to) = entry_target!(vm, a[1]);
        for (t, shown) in [(&mut from, &shown), (&mut to, &shown_to)] {
            if let Some(e) = refuse(vm, t, Access::Write, shown)? {
                return Ok(e);
            }
        }
        // Moving a directory moves everything below it, and replacing `to` replaces its tree.
        for (shown, t) in [(&shown, &from), (&shown_to, &to)] {
            if let Some(e) = protected_tree(vm, shown, &t.path) {
                return Ok(e);
            }
        }
        // A memory file system walks the moved tree (to check its paths' lengths).
        if let Some(m) = vm.host.fs.memory() {
            vm.charge(m.entries_below(&from.path) as u64)?;
        }
        run(vm, shown, move |fs| fs.rename(from, to), ok_unit)
    });
    r.add_builtin("fs.copy", |vm, a| {
        let (shown, mut from) = target!(vm, a[0]);
        let (shown_to, mut to) = target!(vm, a[1]);
        for (t, access, shown) in [(&mut from, Access::Read, &shown), (&mut to, Access::Write, &shown_to)] {
            if let Some(e) = refuse(vm, t, access, shown)? {
                return Ok(e);
            }
        }
        // Copying is charged for the bytes copied.
        if let Some(size) = vm.host.fs.size(&from) {
            vm.charge_bytes(usize::try_from(size).unwrap_or(usize::MAX))?;
        }
        run(vm, shown, move |fs| fs.copy(from, to), ok_unit)
    });
    r.add_builtin("fs.stat", |vm, a| {
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = refuse(vm, &mut t, Access::Read, &shown)? {
            return Ok(e);
        }
        let stat = move |fs: &Fs| match fs.stat(&t)? {
            m if m.kind == EntryKind::Symlink => Err(unchecked_symlink(t.path.file_name().unwrap_or_default())),
            m => Ok(m),
        };
        run(vm, shown, stat, |vm, m| {
            let modified = vm.option(m.modified.map(Value::Int));
            let size = Value::Int(i64::try_from(m.size).unwrap_or(i64::MAX));
            let (is_file, is_dir) = (Value::Bool(m.kind == EntryKind::File), Value::Bool(m.kind == EntryKind::Dir));
            let t = vm.heap.new_tuple(vec![size, is_file, is_dir, modified]);
            Ok(vm.ok(t))
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

/// The script-supplied path `p` and its resolved form (in `mode`).
/// Overlong paths are refused (kind `"invalid"`) before resolution, which
/// is charged per component, for the symlinks it follows and for the
/// directory entries it reads.
fn target(vm: &mut Vm, p: Value, mode: Mode) -> Result<Result<(String, Target), Value>, Ctl> {
    let shown = vm.str(p);
    if shown.len() > MAX_PATH_BYTES {
        return Ok(Err(vm.err_value("invalid", &format!("path too long ({} bytes; the limit is {MAX_PATH_BYTES})", shown.len()))));
    }
    if shown.contains('\0') {
        return Ok(Err(vm.err_value("invalid", "path contains a NUL character")));
    }
    let shown = shown.to_string();
    let joined = vm.host.cwd.join(&shown);
    // Lexical normalization, the walk and the policy's checks, each linear
    // in the length.
    vm.charge(4 * joined.components().count() as u64)?;
    vm.charge_bytes(joined.as_os_str().len().saturating_mul(4))?;
    let t = vm.host.fs.target(&crate::policy::lexical(&joined), mode);
    charge_work(vm, &t)?;
    Ok(Ok((shown, t)))
}

/// Charges for the symlinks followed and the entries scanned resolving `t`
/// (beyond the names of the path itself, charged before).
fn charge_work(vm: &Vm, t: &Target) -> Result<(), Ctl> {
    let w = t.work();
    vm.charge(4 * (w.steps.saturating_sub(t.path.components().count()) + w.scanned) as u64)?;
    vm.charge_bytes(w.bytes.saturating_add(w.scanned_bytes).saturating_mul(4))
}

/// The capability an operation needs on its target.
#[derive(Clone, Copy)]
enum Access {
    Read,
    Write,
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

/// Checks `access` to `t` against the policy: `Some(Err(Error))` to return
/// if it is denied, `None` if the effect may proceed. The final name is
/// looked up as stored first (`Target::fix_spelling`) only if the decision
/// may depend on it: on a case-insensitive file system that costs a system
/// call or two, and most decisions are the same for every spelling.
fn refuse(vm: &mut Vm, t: &mut Target, access: Access, shown: &str) -> Result<Option<Value>, Ctl> {
    let cap = |p: &Path| match access {
        Access::Read => Capability::FsRead(p.to_path_buf()),
        Access::Write => Capability::FsWrite(p.to_path_buf()),
    };
    if !t.exact() {
        let space = vm.host.fs.space();
        let decided = if vm.has_answers() { None } else { vm.host.policy.decision_for_any_spelling(&cap(&t.path), space) };
        match decided {
            Some(Decision::Allow) => return Ok(None),
            Some(_) => return Ok(Some(denied(vm, &cap(&t.path), shown))),
            None => {
                let before = t.work();
                t.fix_spelling();
                let after = t.work();
                vm.charge(4 * after.scanned.saturating_sub(before.scanned) as u64)?;
                vm.charge_bytes(after.scanned_bytes.saturating_sub(before.scanned_bytes).saturating_mul(4))?;
            }
        }
    }
    refuse_path(vm, cap(&t.path), shown)
}

/// Checks `cap` (whose path is normalized) against the policy.
fn refuse_path(vm: &mut Vm, cap: Capability, shown: &str) -> Result<Option<Value>, Ctl> {
    if vm.permit_normalized(&cap)? {
        return Ok(None);
    }
    Ok(Some(denied(vm, &cap, shown)))
}

/// The `Err(permission_denied)` value if a deny rule protects something in
/// the tree at `path` (which is about to be moved, replaced or removed).
fn protected_tree(vm: &mut Vm, shown: &str, path: &Path) -> Option<Value> {
    let protected = vm.host.policy.denies_under_normalized_in(path, vm.host.fs.space());
    protected.then(|| vm.err_value("permission_denied", &format!("permission denied: {shown} contains paths protected by the policy")))
}

/// Runs the file system operation `op`, then `finish` converts its result.
/// I/O errors become `Err(Error)` values mentioning `shown`. Operations run
/// on the VM thread: on the host they are system calls (a few microseconds
/// each), cheaper than handing them to another thread, and they count
/// towards the run's CPU time.
fn run<T>(vm: &mut Vm, shown: String, op: impl FnOnce(&Fs) -> io::Result<T>, finish: impl FnOnce(&mut Vm, T) -> NResult) -> NResult {
    let result = op(&vm.host.fs);
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
    let (shown, mut t) = target!(vm, path);
    if let Some(e) = refuse(vm, &mut t, Access::Write, &shown)? {
        return Ok(e);
    }
    let op = move |fs: &Fs| {
        let r = fs.write(t, &data, append);
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
    let (shown, mut t) = target!(vm, p);
    if let Some(e) = refuse(vm, &mut t, Access::Read, &shown)? {
        return Ok(e);
    }
    let op = move |fs: &Fs| match fs.stat(&t) {
        Ok(m) => Ok(test(m.kind)),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) || is_unchecked_symlink(&e) => Ok(false),
        Err(e) => Err(e),
    };
    run(vm, shown, op, ok_bool)
}
