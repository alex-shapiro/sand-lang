//! The `fs` module.
//!
//! Every native resolves its paths with [`Fs::target`] (on the host, one
//! walk that resolves `..` and symlinks like `realpath`, so neither can
//! escape, holding the directories it passes), checks the policy on the
//! resolved path, then acts through the held directories without following
//! a symlink, so it reaches exactly the checked location even if the file
//! system changes in between.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::files::{EntryKind, Fs, Mode, OpenMode, OpenedFile, Target, is_unchecked_symlink, too_many_open_files, unchecked_symlink};
use crate::policy::{Capability, Decision, MAX_PATH_BYTES};
use crate::vm::iomem::{Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value, to_usize_saturating};
use crate::vm::{Ctl, CtlKind, NResult, Vm};
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
    register_file(r);

    r.add_builtin("fs.read_bytes", |vm, a| {
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = check(vm, &mut t, Access::Read, &shown)? {
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
        if let Some(e) = check(vm, &mut t, Access::Read, &shown)? {
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
        let res = super::or_return!(vm.reserve_io(vm.str(a[1]).len()));
        vm.charge_bytes(vm.str(a[1]).len())?;
        let data = vm.str(a[1]).as_bytes().to_vec();
        write(vm, a[0], data, res)
    });
    r.add_builtin("fs.write_bytes", |vm, a| {
        let res = super::or_return!(vm.reserve_io(vm.heap.bytes_of(a[1]).len()));
        vm.charge_bytes(vm.heap.bytes_of(a[1]).len())?;
        let data = vm.heap.bytes_of(a[1]).to_vec();
        write(vm, a[0], data, res)
    });
    r.add_builtin("fs.exists", |vm, a| file_kind(vm, a[0], |k| k != EntryKind::Symlink));
    r.add_builtin("fs.is_file", |vm, a| file_kind(vm, a[0], |k| k == EntryKind::File));
    r.add_builtin("fs.is_dir", |vm, a| file_kind(vm, a[0], |k| k == EntryKind::Dir));
    r.add_builtin("fs.list_dir", |vm, a| {
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = check(vm, &mut t, Access::Read, &shown)? {
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
        if let Some(e) = check(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        if !a[1].as_bool() {
            return run(vm, shown, move |fs| fs.create_dir(t), ok_unit);
        }
        // Every directory to be created must be writable, not just the last.
        // Those that exist are not created, whatever happens meanwhile.
        for dir in vm.host.fs.missing_ancestors(&t) {
            if let Some(e) = deny_path(vm, Capability::FsWrite(dir), &shown)? {
                return Ok(e);
            }
        }
        run(vm, shown, move |fs| fs.create_dir_all(t), ok_unit)
    });
    r.add_builtin("fs.remove_file", |vm, a| {
        let (shown, mut t) = entry_target!(vm, a[0]);
        if let Some(e) = check(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.remove_file(t), ok_unit)
    });
    r.add_builtin("fs.remove_empty_dir", |vm, a| {
        let (shown, mut t) = entry_target!(vm, a[0]);
        if let Some(e) = check(vm, &mut t, Access::Write, &shown)? {
            return Ok(e);
        }
        run(vm, shown, move |fs| fs.remove_dir(t), ok_unit)
    });
    r.add_builtin("fs.remove_tree", remove_tree);
    r.add_builtin("fs.rename", |vm, a| {
        let (shown, mut from) = entry_target!(vm, a[0]);
        let (shown_to, mut to) = entry_target!(vm, a[1]);
        for (t, shown) in [(&mut from, &shown), (&mut to, &shown_to)] {
            if let Some(e) = check(vm, t, Access::Write, shown)? {
                return Ok(e);
            }
        }
        // Moving a directory moves its tree to below `to`, and replacing a
        // directory `to` replaces its tree.
        let moves_tree = is_dir(vm, &from);
        for (shown, t, tree) in [(&shown, &from, moves_tree), (&shown_to, &to, moves_tree || is_dir(vm, &to))] {
            if tree && let Some(e) = protected_tree(vm, shown, &t.path) {
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
            if let Some(e) = check(vm, t, access, shown)? {
                return Ok(e);
            }
        }
        if let Some(size) = vm.host.fs.size(&from) {
            vm.charge_bytes(usize::try_from(size).unwrap_or(usize::MAX))?;
        }
        run(vm, shown, move |fs| fs.copy(from, to), ok_unit)
    });
    r.add_builtin("fs.stat", |vm, a| {
        let (shown, mut t) = target!(vm, a[0]);
        if let Some(e) = check(vm, &mut t, Access::Read, &shown)? {
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
            let t = vm.heap.new_tuple(&[size, is_file, is_dir, modified]);
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
/// Overlong paths are denied (kind `"invalid"`) before resolution, which
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
fn check(vm: &mut Vm, t: &mut Target, access: Access, shown: &str) -> Result<Option<Value>, Ctl> {
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
    deny_path(vm, cap(&t.path), shown)
}

/// Checks `cap` (whose path is normalized) against the policy.
fn deny_path(vm: &mut Vm, cap: Capability, shown: &str) -> Result<Option<Value>, Ctl> {
    if vm.permit_normalized(&cap)? {
        return Ok(None);
    }
    Ok(Some(denied(vm, &cap, shown)))
}

/// The `Err(permission_denied)` value if a deny rule may match something in
/// the tree at `path` (which is about to be moved, replaced or removed).
fn protected_tree(vm: &mut Vm, shown: &str, path: &Path) -> Option<Value> {
    let protected = vm.host.policy.denies_under_normalized_in(path, vm.host.fs.space());
    protected.then(|| vm.err_value("permission_denied", &format!("permission denied: {shown} contains paths protected by the policy")))
}

/// Whether `t` is a directory (not a symlink to one).
fn is_dir(vm: &Vm, t: &Target) -> bool {
    vm.host.fs.stat(t).is_ok_and(|m| m.kind == EntryKind::Dir)
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

/// `remove_tree(path)`: removes the directory `path` (not a symlink to
/// one) and everything below it. The whole tree must be writable (no deny
/// rule may match below it), and each entry is checked against the policy before it
/// is removed; the walk opens each directory once (`Fs::remove_tree`).
fn remove_tree(vm: &mut Vm, a: &[Value]) -> NResult {
    let (shown, mut t) = entry_target!(vm, a[0]);
    if let Some(e) = check(vm, &mut t, Access::Write, &shown)? {
        return Ok(e);
    }
    if is_dir(vm, &t)
        && let Some(e) = protected_tree(vm, &shown, &t.path)
    {
        return Ok(e);
    }
    let fs = vm.host.fs.clone();
    let root = t.path.clone();
    let mut res = vm.io_reservation();
    let removed = fs.remove_tree(t, &mut res, |path| -> Result<io::Result<()>, Ctl> {
        // Reading the entry, checking it and removing it.
        vm.charge(4 + SYSCALL_FUEL)?;
        vm.charge_bytes(path.as_os_str().len().saturating_mul(4))?;
        let cap = Capability::FsWrite(path.to_path_buf());
        if vm.permit_normalized(&cap)? {
            return Ok(Ok(()));
        }
        // Named as the script would name it: below the path it gave.
        let below = path.strip_prefix(&root).unwrap_or(path);
        let message = format!("permission denied: write {}", Path::new(&shown).join(below).display());
        Ok(Err(io::Error::new(io::ErrorKind::PermissionDenied, message)))
    });
    drop(res);
    Ok(match removed? {
        Ok(()) => vm.ok(Value::Unit),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && e.to_string().starts_with("permission denied: write") => {
            vm.err_value("permission_denied", &e.to_string())
        }
        Err(e) => vm.err_value(io_kind(&e), &format!("{shown}: {e}")),
    })
}

/// Writes `data` (whose copy is reserved in `res` until the write is done).
fn write(vm: &mut Vm, path: Value, data: Vec<u8>, res: Reservation) -> NResult {
    let (shown, mut t) = target!(vm, path);
    if let Some(e) = check(vm, &mut t, Access::Write, &shown)? {
        return Ok(e);
    }
    let op = move |fs: &Fs| {
        let r = fs.write(t, &data);
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
    if let Some(e) = check(vm, &mut t, Access::Read, &shown)? {
        return Ok(e);
    }
    let op = move |fs: &Fs| match fs.stat(&t) {
        Ok(m) => Ok(test(m.kind)),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) || is_unchecked_symlink(&e) => Ok(false),
        Err(e) => Err(e),
    };
    run(vm, shown, op, ok_bool)
}

// ----------------------------------------------------------------------
// Open files (`fs.File`)

/// The size of a file's read buffer (allocated by its first buffered read).
const FILE_BUFFER: usize = 16 << 10;

/// The most a single `read` of a file returns.
const MAX_READ: usize = 1 << 20;

/// Fuel charged per system call (about a microsecond).
const SYSCALL_FUEL: u64 = 16;

struct FileObj {
    /// `None` once closed.
    open: Option<OpenState>,
    /// The path as the script gave it (for messages and `path`).
    shown: String,
    mode: OpenMode,
}

struct OpenState {
    file: OpenedFile,
    /// Counts the file against `max_open_resources` while it is open.
    _slot: ResourceSlot,
    /// Read ahead of the position: `buf[at..]` has not been read yet. Its
    /// capacity is counted in the object's size.
    buf: Vec<u8>,
    at: usize,
    /// The capacity of `buf` that the heap counts (until the operation in
    /// progress accounts for its growth).
    counted: usize,
}

impl NativeObject for FileObj {
    fn type_name(&self) -> &'static str {
        "File"
    }
    fn size(&self) -> usize {
        64 + self.shown.len() + self.open.as_ref().map_or(0, |o| o.buf.capacity())
    }
    fn is_resource(&self) -> bool {
        self.open.is_some()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl OpenState {
    fn unread(&self) -> usize {
        self.buf.len() - self.at
    }

    fn consume(&mut self, n: usize) {
        self.at += n;
        if self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        }
    }

    /// Makes room for `FILE_BUFFER` more bytes after the unread ones; `Ok(Err)`
    /// if the buffer's growth does not fit in memory even after a collection.
    fn make_room(&mut self, vm: &Vm) -> Result<io::Result<()>, Ctl> {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        let need = self.buf.len() + FILE_BUFFER;
        if need > self.buf.capacity() {
            let cap = need.max(self.buf.capacity().saturating_mul(2));
            // Growth earlier in the same operation is not counted yet.
            if !fits(vm, cap.saturating_sub(self.counted))? {
                return Ok(Err(too_large()));
            }
            self.buf.reserve_exact(cap - self.buf.len());
        }
        Ok(Ok(()))
    }

    /// Reads up to `FILE_BUFFER` more bytes into the buffer (after
    /// `make_room`); 0 at the end of the file.
    fn fill(&mut self, vm: &Vm) -> Result<io::Result<usize>, Ctl> {
        vm.charge(SYSCALL_FUEL)?;
        let old = self.buf.len();
        self.buf.resize(old + FILE_BUFFER, 0);
        let r = self.file.read(&mut self.buf[old..]);
        self.buf.truncate(old + *r.as_ref().unwrap_or(&0));
        Ok(r)
    }

    /// Gives back a buffer grown by a long line.
    fn shrink(&mut self) {
        if self.buf.capacity() > 4 * FILE_BUFFER && self.unread() <= FILE_BUFFER {
            self.buf.drain(..self.at);
            self.at = 0;
            self.buf.shrink_to(FILE_BUFFER);
        }
    }

    /// Drops what was read ahead, moving the file back to the position the
    /// script sees (before writing or seeking).
    fn unbuffer(&mut self) -> io::Result<()> {
        let unread = self.unread() as u64;
        if unread > 0 {
            let pos = self.file.position()?;
            self.file.seek(pos.saturating_sub(unread))?;
        }
        self.buf.clear();
        self.at = 0;
        Ok(())
    }

    /// The position the script sees.
    fn position(&mut self) -> io::Result<u64> {
        Ok(self.file.position()?.saturating_sub(self.unread() as u64))
    }
}

/// Whether `bytes` more fit in the memory limit, asking for a collection
/// first if they do not.
fn fits(vm: &Vm, bytes: usize) -> Result<bool, Ctl> {
    if !vm.heap.would_exceed(bytes) {
        return Ok(true);
    }
    if !vm.collected {
        return Err(CtlKind::Collect.into());
    }
    Ok(false)
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::OutOfMemory, "the data does not fit in the memory limit")
}

/// What an operation on a file needs.
#[derive(Clone, Copy, PartialEq)]
enum Needs {
    Read,
    Write,
    Nothing,
}

/// Runs `op` on the open file `v`, taken out of its object meanwhile (so
/// that `op` may read other heap values and the VM). An `io::Error` becomes
/// an `Err(Error)` value naming the file's path; a closed file, or one not
/// opened for what `needs`, is an error too.
fn with_file(vm: &mut Vm, v: Value, needs: Needs, op: impl FnOnce(&mut Vm, &mut OpenState) -> Result<io::Result<Value>, Ctl>) -> NResult {
    let f = vm.heap.native_mut::<FileObj>(v).ok_or_else(|| Ctl::panic("not a file"))?;
    if f.open.is_none() {
        let msg = format!("{}: file closed", f.shown);
        return Ok(vm.err_value("closed", &msg));
    }
    let mode = f.mode;
    let denied = match needs {
        Needs::Read if !mode.reads() => Some("the file was not opened for reading"),
        Needs::Write if !mode.writes() => Some("the file was not opened for writing"),
        _ => None,
    };
    if let Some(why) = denied {
        let msg = format!("{}: {why}", f.shown);
        return Ok(vm.err_value("invalid", &msg));
    }
    let before = f.size();
    let Some(mut st) = f.open.take() else {
        let msg = format!("{}: file closed", f.shown);
        return Ok(vm.err_value("closed", &msg));
    };
    st.counted = st.buf.capacity();
    let r = op(vm, &mut st);
    let f = vm.heap.native_mut::<FileObj>(v).expect("the file is still there");
    f.open = Some(st);
    let after = f.size();
    vm.heap.account(before, after);
    match r? {
        Ok(value) => Ok(vm.ok(value)),
        Err(e) => {
            let msg = format!("{}: {e}", vm.heap.native::<FileObj>(v).map_or("", |f| f.shown.as_str()));
            Ok(vm.err_value(io_kind(&e), &msg))
        }
    }
}

fn register_file(r: &mut Registry) {
    r.add_builtin("fs.open_file", open_file);
    r.add_method("fs.File.read", |vm, a| {
        let max = a[1].as_int();
        if max <= 0 {
            return Ok(vm.err_value("invalid", "read: max must be positive"));
        }
        let max = to_usize_saturating(max);
        with_file(vm, a[0], Needs::Read, |vm, st| {
            vm.charge(1)?;
            if st.unread() == 0 && max >= FILE_BUFFER {
                // A large read goes straight into its value: at most what
                // is left of the file (as far as it is known).
                vm.charge(2 * SYSCALL_FUEL)?;
                let left = match (st.file.meta(), st.file.position()) {
                    (Ok(m), Ok(pos)) => usize::try_from(m.size.saturating_sub(pos)).unwrap_or(usize::MAX),
                    (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
                };
                // A file that grows meanwhile is read on by the next call.
                let n = max.min(MAX_READ).min(left.max(1));
                if !fits(vm, n)? {
                    return Ok(Err(too_large()));
                }
                let mut data = vec![0; n];
                let r = st.file.read(&mut data);
                return Ok(match r {
                    Ok(got) => {
                        data.truncate(got);
                        vm.charge_bytes(got)?;
                        Ok(vm.heap.new_bytes(data))
                    }
                    Err(e) => Err(e),
                });
            }
            if st.unread() == 0 {
                if let Err(e) = st.make_room(vm)? {
                    return Ok(Err(e));
                }
                if let Err(e) = st.fill(vm)? {
                    return Ok(Err(e));
                }
            }
            let n = st.unread().min(max);
            vm.reserve(n)?;
            vm.charge_bytes(n)?;
            let data = st.buf[st.at..st.at + n].to_vec();
            st.consume(n);
            Ok(Ok(vm.heap.new_bytes(data)))
        })
    });
    r.add_method("fs.File.read_line", |vm, a| {
        with_file(vm, a[0], Needs::Read, |vm, st| {
            vm.charge(1)?;
            // The line stays in the buffer until it is complete, so that
            // nothing is consumed if it does not fit (the call is retried
            // after a collection, or fails with the memory limit).
            let mut scanned = 0;
            let end = loop {
                let unread = &st.buf[st.at..];
                if let Some(i) = unread[scanned..].iter().position(|&b| b == b'\n') {
                    vm.charge_bytes(i + 1)?;
                    break st.at + scanned + i + 1;
                }
                vm.charge_bytes(unread.len() - scanned)?;
                scanned = unread.len();
                if let Err(e) = st.make_room(vm)? {
                    // The line does not fit: what was read of it is dropped
                    // (as for connections), giving the memory back.
                    st.buf = Vec::new();
                    st.at = 0;
                    return Ok(Err(e));
                }
                match st.fill(vm)? {
                    Ok(0) if st.unread() == 0 => return Ok(Ok(vm.none())),
                    Ok(0) => break st.buf.len(),
                    Ok(_) => {}
                    Err(e) => return Ok(Err(e)),
                }
            };
            let mut line = &st.buf[st.at..end];
            if let Some(l) = line.strip_suffix(b"\n") {
                line = l.strip_suffix(b"\r").unwrap_or(l);
            }
            vm.reserve(line.len())?;
            vm.charge_slow_bytes(line.len())?;
            let line = match std::str::from_utf8(line) {
                Ok(text) => text.to_string(),
                Err(_) => {
                    st.consume(end - st.at);
                    return Ok(Err(io::Error::new(io::ErrorKind::InvalidData, "line is not valid UTF-8")));
                }
            };
            st.consume(end - st.at);
            st.shrink();
            let s = vm.new_str(line);
            Ok(Ok(vm.some(s)))
        })
    });
    r.add_method("fs.File.read_all", |vm, a| read_rest(vm, a[0], false));
    r.add_method("fs.File.read_text", |vm, a| read_rest(vm, a[0], true));
    for key in ["fs.File.write", "fs.File.write_str"] {
        r.add_method(key, |vm, a| {
            let data = a[1];
            with_file(vm, a[0], Needs::Write, move |vm, st| {
                let bytes = match vm.heap.get(data) {
                    Obj::Str(s) => s.as_bytes(),
                    _ => vm.heap.bytes_of(data),
                };
                vm.charge(SYSCALL_FUEL)?;
                vm.charge_bytes(bytes.len())?;
                let r = st.unbuffer().and_then(|()| st.file.write_all(bytes));
                // A write past the end of a memory file fills the gap.
                let filled = r.as_ref().map_or(0, |&n| n);
                vm.charge_bytes(usize::try_from(filled).unwrap_or(usize::MAX))?;
                Ok(r.map(|_| Value::Unit))
            })
        });
    }
    r.add_method("fs.File.close", |vm, a| {
        let f = vm.heap.native_mut::<FileObj>(a[0]).ok_or_else(|| Ctl::panic("not a file"))?;
        let before = f.size();
        // Dropping the file closes it and releases its slot.
        f.open = None;
        let after = f.size();
        vm.heap.account(before, after);
        Ok(Value::Unit)
    });
    r.add_builtin("fs.File.seek", |vm, a| {
        let Ok(pos) = u64::try_from(a[1].as_int()) else {
            return Ok(vm.err_value("invalid", &format!("seek: negative position {}", a[1].as_int())));
        };
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            st.buf.clear();
            st.at = 0;
            Ok(st.file.seek(pos).map(|()| Value::Unit))
        })
    });
    r.add_builtin("fs.File.position", |vm, a| {
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            Ok(st.position().map(|p| Value::Int(i64::try_from(p).unwrap_or(i64::MAX))))
        })
    });
    r.add_builtin("fs.file_stat", |vm, a| {
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            let m = match st.file.meta() {
                Ok(m) => m,
                Err(e) => return Ok(Err(e)),
            };
            let modified = vm.option(m.modified.map(Value::Int));
            Ok(Ok(vm.heap.new_tuple(&[Value::Int(i64::try_from(m.size).unwrap_or(i64::MAX)), modified])))
        })
    });
    r.add_builtin("fs.File.set_len", |vm, a| {
        let Ok(len) = u64::try_from(a[1].as_int()) else {
            return Ok(vm.err_value("invalid", &format!("set_len: negative length {}", a[1].as_int())));
        };
        with_file(vm, a[0], Needs::Write, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            let r = st.unbuffer().and_then(|()| st.file.set_len(len));
            let filled = r.as_ref().map_or(0, |&n| n);
            vm.charge_bytes(usize::try_from(filled).unwrap_or(usize::MAX))?;
            Ok(r.map(|_| Value::Unit))
        })
    });
    r.add_builtin("fs.File.sync", |vm, a| {
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            Ok(st.file.sync().map(|()| Value::Unit))
        })
    });
    r.add_builtin("fs.File.path", |vm, a| {
        let shown = vm.heap.native::<FileObj>(a[0]).ok_or_else(|| Ctl::panic("not a file"))?.shown.clone();
        vm.charge_bytes(shown.len())?;
        Ok(vm.new_str(shown))
    });
}

/// `fs.open_file(path, mode)`: `mode` is the index of an `fs.Mode` variant.
fn open_file(vm: &mut Vm, a: &[Value]) -> NResult {
    const MODES: [OpenMode; 5] = [OpenMode::Read, OpenMode::Write, OpenMode::Append, OpenMode::CreateNew, OpenMode::ReadWrite];
    let mode = *MODES.get(to_usize_saturating(a[1].as_int())).ok_or_else(|| Ctl::panic("invalid file mode"))?;
    let (shown, mut t) = target!(vm, a[0]);
    for (needed, access) in [(mode.reads(), Access::Read), (mode.writes(), Access::Write)] {
        if needed && let Some(e) = check(vm, &mut t, access, &shown)? {
            return Ok(e);
        }
    }
    let slot = super::or_return!(vm.resource_slot());
    vm.charge(SYSCALL_FUEL)?;
    let file = match vm.host.fs.open(t, mode) {
        Ok(f) => f,
        // Closing unreachable files may make room.
        Err(e) if too_many_open_files(&e) && !vm.collected => return Err(CtlKind::Collect.into()),
        Err(e) => return Ok(vm.err_value(io_kind(&e), &format!("{shown}: {e}"))),
    };
    let obj = FileObj { open: Some(OpenState { file, _slot: slot, buf: Vec::new(), at: 0, counted: 0 }), shown, mode };
    let v = vm.heap.alloc(Obj::Native(Box::new(obj)));
    Ok(vm.ok(v))
}

/// `read_all` (`Result<bytes>`) or, with `text`, `read_text` (`Result<str>`):
/// what was read ahead, then the rest of the file, whose size is reserved
/// first (so that a file too large fails before it is read).
fn read_rest(vm: &mut Vm, file: Value, text: bool) -> NResult {
    with_file(vm, file, Needs::Read, |vm, st| {
        vm.charge(SYSCALL_FUEL)?;
        let size = match (st.file.meta(), st.file.position()) {
            (Ok(m), Ok(pos)) => m.size.saturating_sub(pos),
            (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
        };
        let size = usize::try_from(size).unwrap_or(usize::MAX).saturating_add(st.unread());
        let too_large = || io::Error::new(io::ErrorKind::OutOfMemory, "the rest of the file does not fit in the memory limit");
        let Ok(mut res) = vm.reserve_io(size)? else { return Ok(Err(too_large())) };
        let mut data = Vec::new();
        if let Err(e) = res.reserve_buf(&mut data, size.min(64 << 20)) {
            return Ok(Err(e.into()));
        }
        data.extend_from_slice(&st.buf[st.at..]);
        st.buf.clear();
        st.at = 0;
        st.shrink();
        let mut chunk = [0u8; 8192];
        loop {
            vm.charge(SYSCALL_FUEL)?;
            let start = data.len();
            let n = if data.capacity() > start {
                data.resize(data.capacity(), 0);
                let n = st.file.read(&mut data[start..]);
                data.truncate(start + *n.as_ref().unwrap_or(&0));
                n
            } else {
                st.file.read(&mut chunk).and_then(|n| {
                    res.reserve_buf(&mut data, n)?;
                    data.extend_from_slice(&chunk[..n]);
                    Ok(n)
                })
            };
            match n {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Ok(Err(e)),
            }
        }
        // What was read is consumed: from here on, a failure is not a retry.
        drop(res);
        if vm.heap.would_exceed(data.len()) {
            return Ok(Err(too_large()));
        }
        vm.charge_bytes(data.len())?;
        if !text {
            return Ok(Ok(vm.heap.new_bytes(data)));
        }
        vm.charge_slow_bytes(data.len())?;
        Ok(match String::from_utf8(data) {
            Ok(s) => Ok(vm.new_str(s)),
            Err(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8")),
        })
    })
}
