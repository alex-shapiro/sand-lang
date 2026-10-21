//! The `fs` module.
//!
//! Every native resolves its paths with [`Fs::target`] (on the host, one
//! walk that resolves `..` and symlinks like `realpath`, so neither can
//! escape, holding the directories it passes), checks the policy on the
//! resolved path, then acts through the held directories without following
//! a symlink, so it reaches exactly the checked location even if the file
//! system changes in between. Host system calls run on worker threads
//! (`Vm::offload`), so the run's limits apply even if a call does not
//! return.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::files::{EntryKind, Fs, Mode, OpenMode, OpenedFile, Target, is_unchecked_symlink, too_many_open_files, unchecked_symlink};
use crate::policy::{Capability, Decision, MAX_PATH_BYTES, Policy};
use crate::vm::iomem::{Reservation, ResourceSlot};
use crate::vm::meter::{Allowance, Stopped};
use crate::vm::value::{NativeObject, Obj, Value, to_usize_saturating};
use crate::vm::{Ctl, CtlKind, NResult, Vm};
use std::io;
use std::path::{Path, PathBuf};

pub fn register(r: &mut Registry) {
    r.source("fs", ModKind::Std, include_str!("fs.sand"));
    register_file(r);

    r.add_builtin("fs.read_bytes", |vm, a| {
        let (shown, mut t) = target(vm, a[0], Mode::Follow)?;
        check(vm, &mut t, Access::Read, &shown)?;
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
        let (shown, mut t) = target(vm, a[0], Mode::Follow)?;
        check(vm, &mut t, Access::Read, &shown)?;
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
                    Err(_) => Err(Ctl::native_err("invalid", format!("{shown}: file is not valid UTF-8"))),
                }
            },
        )
    });
    r.add_builtin("fs.write_text", |vm, a| {
        let res = vm.reserve_io(vm.str(a[1]).len())?;
        vm.charge_bytes(vm.str(a[1]).len())?;
        let data = vm.str(a[1]).as_bytes().to_vec();
        write(vm, a[0], data, res)
    });
    r.add_builtin("fs.write_bytes", |vm, a| {
        let res = vm.reserve_io(vm.heap.bytes_of(a[1]).len())?;
        vm.charge_bytes(vm.heap.bytes_of(a[1]).len())?;
        let data = vm.heap.bytes_of(a[1]).to_vec();
        write(vm, a[0], data, res)
    });
    r.add_builtin("fs.exists", |vm, a| file_kind(vm, a[0], |k| k != EntryKind::Symlink));
    r.add_builtin("fs.is_file", |vm, a| file_kind(vm, a[0], |k| k == EntryKind::File));
    r.add_builtin("fs.is_dir", |vm, a| file_kind(vm, a[0], |k| k == EntryKind::Dir));
    r.add_builtin("fs.list_dir", |vm, a| {
        let (shown, mut t) = target(vm, a[0], Mode::Follow)?;
        check(vm, &mut t, Access::Read, &shown)?;
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
        let (shown, mut t) = target(vm, a[0], Mode::Follow)?;
        check(vm, &mut t, Access::Write, &shown)?;
        if !a[1].as_bool() {
            return run(vm, shown, move |fs| fs.create_dir(t), ok_unit);
        }
        // Every directory to be created must be writable, not just the last.
        // Those that exist are not created, whatever happens meanwhile.
        for dir in vm.host.fs.missing_ancestors(&t) {
            check_path(vm, Capability::FsWrite(dir), &shown, false)?;
        }
        run(vm, shown, move |fs| fs.create_dir_all(t), ok_unit)
    });
    r.add_builtin("fs.remove_file", |vm, a| {
        let (shown, mut t) = target(vm, a[0], Mode::Entry)?;
        check(vm, &mut t, Access::Write, &shown)?;
        run(vm, shown, move |fs| fs.remove_file(t), ok_unit)
    });
    r.add_builtin("fs.remove_empty_dir", |vm, a| {
        let (shown, mut t) = target(vm, a[0], Mode::Entry)?;
        check(vm, &mut t, Access::Write, &shown)?;
        run(vm, shown, move |fs| fs.remove_dir(t), ok_unit)
    });
    r.add_builtin("fs.remove_tree", remove_tree);
    r.add_builtin("fs.rename", |vm, a| {
        let (shown, mut from) = target(vm, a[0], Mode::Entry)?;
        let (shown_to, mut to) = target(vm, a[1], Mode::Entry)?;
        for (t, shown) in [(&mut from, &shown), (&mut to, &shown_to)] {
            check(vm, t, Access::Write, shown)?;
        }
        // A memory file system walks the moved tree (to check its paths' lengths).
        if let Some(m) = vm.host.fs.memory() {
            vm.charge(m.entries_below(&from.path) as u64)?;
        }
        // Moving a directory moves every entry below it, and replacing the
        // directory `to` removes every entry below that.
        let (fs, policy) = (vm.host.fs.clone(), vm.host.policy.clone());
        let mut res = vm.io_reservation();
        let renamed = vm.offload(move |allowance| -> Result<io::Result<()>, (Halt, bool)> {
            for (t, moved_to, is_to) in [(&from, Some(to.path.as_path()), false), (&to, None, true)] {
                if fs.stat(t).is_ok_and(|m| m.kind == EntryKind::Dir)
                    && let Err(e) = scan_tree(&fs, &policy, t, moved_to, &mut res, allowance).map_err(|h| (h, is_to))?
                {
                    return Ok(Err(e));
                }
            }
            Ok(fs.rename(from, to))
        })?;
        match renamed {
            Ok(r) => settle(vm, shown, r, ok_unit),
            Err((Halt::Stopped, _)) => unreachable!("`offload` reports a stopped walk"),
            Err((_, is_to)) => Err(protected(if is_to { &shown_to } else { &shown })),
        }
    });
    r.add_builtin("fs.copy", |vm, a| {
        let (shown, mut from) = target(vm, a[0], Mode::Follow)?;
        let (shown_to, mut to) = target(vm, a[1], Mode::Follow)?;
        for (t, access, shown) in [(&mut from, Access::Read, &shown), (&mut to, Access::Write, &shown_to)] {
            check(vm, t, access, shown)?;
        }
        if let Some(size) = vm.host.fs.size(&from) {
            vm.charge_bytes(usize::try_from(size).unwrap_or(usize::MAX))?;
        }
        run(vm, shown, move |fs| fs.copy(from, to), ok_unit)
    });
    r.add_builtin("fs.stat", |vm, a| {
        let (shown, mut t) = target(vm, a[0], Mode::Follow)?;
        check(vm, &mut t, Access::Read, &shown)?;
        let stat = match vm.host.fs.stat(&t) {
            Ok(m) if m.kind == EntryKind::Symlink => Err(unchecked_symlink(t.path.file_name().unwrap_or_default())),
            r => r,
        };
        settle(vm, shown, stat, |vm, m| {
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
fn target(vm: &mut Vm, p: Value, mode: Mode) -> Result<(String, Target), Ctl> {
    let shown = vm.str(p);
    if shown.len() > MAX_PATH_BYTES {
        return Err(Ctl::native_err("invalid", format!("path too long ({} bytes; the limit is {MAX_PATH_BYTES})", shown.len())));
    }
    if shown.contains('\0') {
        return Err(Ctl::native_err("invalid", "path contains a NUL character"));
    }
    let shown = shown.to_string();
    let joined = vm.host.cwd.join(&shown);
    // Lexical normalization, the walk and the policy's checks, each linear
    // in the length.
    vm.charge(4 * joined.components().count() as u64)?;
    vm.charge_bytes(joined.as_os_str().len().saturating_mul(4))?;
    let (fs, lexical) = (vm.host.fs.clone(), crate::policy::lexical(&joined));
    let t = vm.offload(move |_| fs.target(&lexical, mode))?;
    charge_work(vm, &t)?;
    Ok((shown, t))
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

fn denied(cap: &Capability, shown: &str) -> Ctl {
    let verb = match cap {
        Capability::FsRead(_) => "read",
        _ => "write",
    };
    Ctl::native_err("permission_denied", format!("permission denied: {verb} {shown}"))
}

/// Checks `access` to `t` against the policy, resolving on-disk case on
/// case-insensitive file systems only if required by the policy.
fn check(vm: &mut Vm, t: &mut Target, access: Access, shown: &str) -> Result<(), Ctl> {
    let cap = |p: &Path| match access {
        Access::Read => Capability::FsRead(p.to_path_buf()),
        Access::Write => Capability::FsWrite(p.to_path_buf()),
    };
    if !t.exact() {
        let space = vm.host.fs.space();
        let decided = if vm.has_answers() { None } else { vm.host.policy.decision_for_any_spelling(&cap(&t.path), space) };
        match decided {
            Some(Decision::Allow) => return Ok(()),
            Some(_) => return Err(denied(&cap(&t.path), shown)),
            None => {
                let before = t.work();
                let mut owned = t.take();
                *t = vm.offload(move |_| {
                    owned.fix_spelling();
                    owned
                })?;
                let after = t.work();
                vm.charge(4 * after.scanned.saturating_sub(before.scanned) as u64)?;
                vm.charge_bytes(after.scanned_bytes.saturating_sub(before.scanned_bytes).saturating_mul(4))?;
            }
        }
    }
    check_path(vm, cap(&t.path), shown, t.mode == Mode::Entry)
}

fn check_path(vm: &mut Vm, cap: Capability, shown: &str, entry: bool) -> Result<(), Ctl> {
    if vm.permit_path(&cap, entry)? { Ok(()) } else { Err(denied(&cap, shown)) }
}

/// Why a walk of a directory tree on a worker thread stopped early.
enum Halt {
    /// A charge to its [`Allowance`] failed.
    Stopped,
    /// The policy needs to ask the user about this capability.
    Prompt(Capability),
    /// The policy protects an entry of the tree (see [`scan_tree`]).
    Protected,
    /// The policy denies writing this entry.
    Denied(PathBuf),
}

impl From<Stopped> for Halt {
    fn from(_: Stopped) -> Halt {
        Halt::Stopped
    }
}

/// Charges for visiting the entry `path` of a tree, which includes reading
/// the entry, checking it against the policy and acting on it.
fn charge_entry(allowance: &mut Allowance, path: &Path) -> Result<(), Stopped> {
    allowance.charge(4 + SYSCALL_FUEL)?;
    allowance.charge_bytes(path.as_os_str().len().saturating_mul(4))
}

/// Fails with [`Halt::Protected`] if the policy protects an entry of the
/// directory tree at `t` (see [`Policy::protects_normalized_in`]). If the
/// tree is being moved to `to`, the entry's new path is checked as well. A
/// tree with a protected entry may not be moved or removed as a whole. The
/// tree is scanned only if a deny rule may match a path in it.
fn scan_tree(
    fs: &Fs,
    policy: &Policy,
    t: &Target,
    to: Option<&Path>,
    res: &mut Reservation,
    allowance: &mut Allowance,
) -> Result<io::Result<()>, Halt> {
    let space = fs.space();
    if !policy.denies_under_normalized_in(&t.path, space) && to.is_none_or(|to| !policy.denies_under_normalized_in(to, space)) {
        return Ok(Ok(()));
    }
    fs.walk_tree(t, res, false, |path| {
        charge_entry(allowance, path)?;
        let moved = to.map(|to| to.join(path.strip_prefix(&t.path).unwrap_or(path)));
        if policy.protects_normalized_in(path, space) || moved.is_some_and(|m| policy.protects_normalized_in(&m, space)) {
            return Err(Halt::Protected);
        }
        Ok(Ok(()))
    })
}

fn protected(shown: &str) -> Ctl {
    Ctl::native_err("permission_denied", format!("permission denied: {shown} contains paths protected by the policy"))
}

/// Runs the file system operation `op` on a worker thread (see
/// `Vm::offload`) and converts its result with [`settle`].
fn run<T: Send + 'static>(
    vm: &mut Vm,
    shown: String,
    op: impl FnOnce(&Fs) -> io::Result<T> + Send + 'static,
    finish: impl FnOnce(&mut Vm, T) -> NResult,
) -> NResult {
    let fs = vm.host.fs.clone();
    let r = vm.offload(move |_| op(&fs))?;
    settle(vm, shown, r, finish)
}

/// Converts an I/O result into an engine result.
fn settle<T>(vm: &mut Vm, shown: String, r: io::Result<T>, finish: impl FnOnce(&mut Vm, T) -> NResult) -> NResult {
    match r {
        Ok(v) => finish(vm, v),
        Err(e) => Err(Ctl::native_err(io_kind(&e), format!("{shown}: {e}"))),
    }
}

fn ok_unit<T>(vm: &mut Vm, _: T) -> NResult {
    Ok(vm.ok(Value::Unit))
}

fn ok_bool(vm: &mut Vm, b: bool) -> NResult {
    Ok(vm.ok(Value::Bool(b)))
}

/// `remove_tree(path)`: removes the directory `path` (not a symlink to
/// one) and everything below it. If the policy protects an entry of the
/// tree ([`scan_tree`]), nothing is removed. Each entry is also checked
/// against the policy just before it is removed.
fn remove_tree(vm: &mut Vm, a: &[Value]) -> NResult {
    let (shown, mut t) = target(vm, a[0], Mode::Entry)?;
    check(vm, &mut t, Access::Write, &shown)?;
    let (fs, permits, root) = (vm.host.fs.clone(), vm.permits(), t.path.clone());
    let mut res = vm.io_reservation();
    let removed = vm.offload(move |allowance| -> Result<io::Result<()>, Halt> {
        if fs.stat(&t).is_ok_and(|m| m.kind == EntryKind::Dir)
            && let Err(e) = scan_tree(&fs, &permits.policy, &t, None, &mut res, allowance)?
        {
            return Ok(Err(e));
        }
        fs.walk_tree(&t, &mut res, true, |path| {
            charge_entry(allowance, path)?;
            let cap = Capability::FsWrite(path.to_path_buf());
            match permits.permit_path(&cap, true) {
                Ok(true) => Ok(Ok(())),
                Ok(false) => Err(Halt::Denied(path.to_path_buf())),
                Err(()) => Err(Halt::Prompt(cap)),
            }
        })
    })?;
    match removed {
        Ok(r) => settle(vm, shown, r, ok_unit),
        Err(Halt::Protected) => Err(protected(&shown)),
        // Named as the script would name it: below the path it gave.
        Err(Halt::Denied(path)) => {
            let below = path.strip_prefix(&root).unwrap_or(&path);
            Err(Ctl::native_err("permission_denied", format!("permission denied: write {}", Path::new(&shown).join(below).display())))
        }
        Err(Halt::Prompt(cap)) => Err(Ctl::need_permission(cap)),
        Err(Halt::Stopped) => unreachable!("`offload` reports a stopped walk"),
    }
}

/// Writes `data` (whose copy is reserved in `res` until the write is done).
fn write(vm: &mut Vm, path: Value, data: Vec<u8>, res: Reservation) -> NResult {
    let (shown, mut t) = target(vm, path, Mode::Follow)?;
    check(vm, &mut t, Access::Write, &shown)?;
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
    let (shown, mut t) = target(vm, p, Mode::Follow)?;
    check(vm, &mut t, Access::Read, &shown)?;
    let kind = match vm.host.fs.stat(&t) {
        Ok(m) => Ok(test(m.kind)),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) || is_unchecked_symlink(&e) => Ok(false),
        Err(e) => Err(e),
    };
    settle(vm, shown, kind, ok_bool)
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
    /// `None` if an operation on a worker still holds the file because the
    /// VM stopped waiting for it.
    file: Option<OpenedFile>,
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

impl Drop for FileObj {
    fn drop(&mut self) {
        // Closing a file can block, so a file that is garbage collected while open is closed on a worker.
        #[cfg(feature = "os")]
        if let Some(st) = &mut self.open
            && let Some(file @ OpenedFile::Host(_)) = st.file.take()
        {
            crate::platform::offload(move || drop(file));
        }
    }
}

impl OpenState {
    /// Runs `op` on the file on a worker thread (see `Vm::offload`).
    fn io<T: Send + 'static>(&mut self, vm: &Vm, op: impl FnOnce(&mut OpenedFile) -> T + Send + 'static) -> Result<T, Ctl> {
        let mut file = self.file.take().ok_or_else(|| Ctl::internal("a file without its file"))?;
        let (file, t) = vm.offload(move |_| {
            let t = op(&mut file);
            (file, t)
        })?;
        self.file = Some(file);
        Ok(t)
    }

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
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(old + FILE_BUFFER, 0);
        let (mut buf, r) = self.io(vm, move |f| {
            let r = f.read(&mut buf[old..]);
            (buf, r)
        })?;
        buf.truncate(old + *r.as_ref().unwrap_or(&0));
        self.buf = buf;
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

    /// Discards the data read ahead, before a write or a seek, and returns
    /// its length. The caller must then move the file's position back by
    /// that length (see [`unbuffer`]).
    fn take_unread(&mut self) -> u64 {
        let unread = self.unread() as u64;
        self.buf.clear();
        self.at = 0;
        unread
    }
}

/// Moves the position of `f` back by the `unread` bytes that were read
/// ahead, to the position the script expects.
fn unbuffer(f: &mut OpenedFile, unread: u64) -> io::Result<()> {
    if unread > 0 {
        let pos = f.position()?;
        f.seek(pos.saturating_sub(unread))?;
    }
    Ok(())
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
    let f = vm.heap.native_mut::<FileObj>(v).ok_or_else(|| Ctl::internal("not a file"))?;
    if f.open.as_ref().is_none_or(|o| o.file.is_none()) {
        let msg = format!("{}: file closed", f.shown);
        return Err(Ctl::native_err("closed", msg));
    }
    let mode = f.mode;
    let denied = match needs {
        Needs::Read if !mode.reads() => Some("the file was not opened for reading"),
        Needs::Write if !mode.writes() => Some("the file was not opened for writing"),
        _ => None,
    };
    if let Some(why) = denied {
        let msg = format!("{}: {why}", f.shown);
        return Err(Ctl::native_err("invalid", msg));
    }
    let before = f.size();
    let Some(mut st) = f.open.take() else {
        let msg = format!("{}: file closed", f.shown);
        return Err(Ctl::native_err("closed", msg));
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
            Err(Ctl::native_err(io_kind(&e), msg))
        }
    }
}

fn register_file(r: &mut Registry) {
    r.add_enum_type::<OpenMode>();
    r.add_builtin("fs.open", open);
    r.add_method("fs.File.read", |vm, a| {
        let max = a[1].as_int();
        if max <= 0 {
            return Err(Ctl::native_err("invalid", "read: max must be positive"));
        }
        let max = to_usize_saturating(max);
        with_file(vm, a[0], Needs::Read, |vm, st| {
            vm.charge(1)?;
            if st.unread() == 0 && max >= FILE_BUFFER {
                // A large read goes straight into its value: at most what
                // is left of the file (as far as it is known).
                vm.charge(2 * SYSCALL_FUEL)?;
                let left = match st.io(vm, |f| f.meta().and_then(|m| Ok(m.size.saturating_sub(f.position()?))))? {
                    Ok(left) => usize::try_from(left).unwrap_or(usize::MAX),
                    Err(e) => return Ok(Err(e)),
                };
                // A file that grows meanwhile is read on by the next call.
                let n = max.min(MAX_READ).min(left.max(1));
                if !fits(vm, n)? {
                    return Ok(Err(too_large()));
                }
                let (mut data, r) = st.io(vm, move |f| {
                    let mut data = vec![0; n];
                    let r = f.read(&mut data);
                    (data, r)
                })?;
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
                fn bytes(vm: &Vm, v: Value) -> &[u8] {
                    match vm.heap.get(v) {
                        Obj::Str(s) => s.as_bytes(),
                        _ => vm.heap.bytes_of(v),
                    }
                }
                let len = bytes(vm, data).len();
                vm.charge(SYSCALL_FUEL)?;
                vm.charge_bytes(len)?;
                // Reserve memory for the copy of the data that the worker writes.
                let Some(res) = vm.try_reserve_io(len)? else { return Ok(Err(too_large())) };
                let bytes = bytes(vm, data).to_vec();
                let unread = st.take_unread();
                let r = st.io(vm, move |f| {
                    let r = unbuffer(f, unread).and_then(|()| f.write_all(&bytes));
                    drop(res);
                    r
                })?;
                // A write past the end of a memory file fills the gap.
                let filled = r.as_ref().map_or(0, |&n| n);
                vm.charge_bytes(usize::try_from(filled).unwrap_or(usize::MAX))?;
                Ok(r.map(|_| Value::Unit))
            })
        });
    }
    r.add_method("fs.File.close", |vm, a| {
        let f = vm.heap.native_mut::<FileObj>(a[0]).ok_or_else(|| Ctl::internal("not a file"))?;
        let before = f.size();
        let open = f.open.take();
        let after = f.size();
        vm.heap.account(before, after);
        // Closing can block, for example while data is flushed to a network
        // file system. Dropping the state releases its resource slot.
        if let Some(mut st) = open
            && let Some(file) = st.file.take()
        {
            vm.offload(move |_| drop(file))?;
        }
        Ok(Value::Unit)
    });
    r.add_builtin("fs.File.seek", |vm, a| {
        let Ok(pos) = u64::try_from(a[1].as_int()) else {
            return Err(Ctl::native_err("invalid", format!("seek: negative position {}", a[1].as_int())));
        };
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            st.take_unread();
            Ok(st.io(vm, move |f| f.seek(pos))?.map(|()| Value::Unit))
        })
    });
    r.add_builtin("fs.File.position", |vm, a| {
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            let unread = st.unread() as u64;
            let pos = st.io(vm, |f| f.position())?;
            Ok(pos.map(|p| Value::Int(i64::try_from(p.saturating_sub(unread)).unwrap_or(i64::MAX))))
        })
    });
    r.add_builtin("fs.file_stat", |vm, a| {
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            let m = match st.io(vm, |f| f.meta())? {
                Ok(m) => m,
                Err(e) => return Ok(Err(e)),
            };
            let modified = vm.option(m.modified.map(Value::Int));
            Ok(Ok(vm.heap.new_tuple(&[Value::Int(i64::try_from(m.size).unwrap_or(i64::MAX)), modified])))
        })
    });
    r.add_builtin("fs.File.set_len", |vm, a| {
        let Ok(len) = u64::try_from(a[1].as_int()) else {
            return Err(Ctl::native_err("invalid", format!("set_len: negative length {}", a[1].as_int())));
        };
        with_file(vm, a[0], Needs::Write, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            let unread = st.take_unread();
            let r = st.io(vm, move |f| unbuffer(f, unread).and_then(|()| f.set_len(len)))?;
            let filled = r.as_ref().map_or(0, |&n| n);
            vm.charge_bytes(usize::try_from(filled).unwrap_or(usize::MAX))?;
            Ok(r.map(|_| Value::Unit))
        })
    });
    r.add_builtin("fs.File.sync", |vm, a| {
        with_file(vm, a[0], Needs::Nothing, |vm, st| {
            vm.charge(SYSCALL_FUEL)?;
            Ok(st.io(vm, |f| f.sync())?.map(|()| Value::Unit))
        })
    });
    r.add_builtin("fs.File.path", |vm, a| {
        let shown = vm.heap.native::<FileObj>(a[0]).ok_or_else(|| Ctl::internal("not a file"))?.shown.clone();
        vm.charge_bytes(shown.len())?;
        Ok(vm.new_str(shown))
    });
}

fn open(vm: &mut Vm, a: &[Value]) -> NResult {
    let mode: OpenMode = vm.enum_value(a[1])?;
    let (shown, mut t) = target(vm, a[0], Mode::Follow)?;
    for (needed, access) in [(mode.reads(), Access::Read), (mode.writes(), Access::Write)] {
        if needed {
            check(vm, &mut t, access, &shown)?;
        }
    }
    let slot = vm.resource_slot()?;
    vm.charge(SYSCALL_FUEL)?;
    let fs = vm.host.fs.clone();
    let file = match vm.offload(move |_| fs.open(t, mode))? {
        Ok(f) => f,
        // Closing unreachable files may make room.
        Err(e) if too_many_open_files(&e) && !vm.collected => return Err(CtlKind::Collect.into()),
        Err(e) => return Err(Ctl::native_err(io_kind(&e), format!("{shown}: {e}"))),
    };
    let obj = FileObj { open: Some(OpenState { file: Some(file), _slot: slot, buf: Vec::new(), at: 0, counted: 0 }), shown, mode };
    let v = vm.heap.alloc(Obj::Native(Box::new(obj)));
    Ok(vm.ok(v))
}

/// `read_all` (`Result<bytes>`) or, with `text`, `read_text` (`Result<str>`):
/// what was read ahead, then the rest of the file, whose size is reserved
/// first (so that a file too large fails before it is read).
fn read_rest(vm: &mut Vm, file: Value, text: bool) -> NResult {
    with_file(vm, file, Needs::Read, |vm, st| {
        vm.charge(SYSCALL_FUEL)?;
        let size = match st.io(vm, |f| f.meta().and_then(|m| Ok(m.size.saturating_sub(f.position()?))))? {
            Ok(size) => size,
            Err(e) => return Ok(Err(e)),
        };
        let size = usize::try_from(size).unwrap_or(usize::MAX).saturating_add(st.unread());
        let too_large = || io::Error::new(io::ErrorKind::OutOfMemory, "the rest of the file does not fit in the memory limit");
        let Some(mut res) = vm.try_reserve_io(size)? else { return Ok(Err(too_large())) };
        let mut data = Vec::new();
        if let Err(e) = res.reserve_buf(&mut data, size.min(64 << 20)) {
            return Ok(Err(e.into()));
        }
        data.extend_from_slice(&st.buf[st.at..]);
        st.take_unread();
        st.shrink();
        let mut file = st.file.take().ok_or_else(|| Ctl::internal("a file without its file"))?;
        let (file, read) = vm.offload(move |allowance| {
            let r = read_to_end(&mut file, &mut data, &mut res, allowance);
            (file, r.map(|r| r.map(|()| (data, res))))
        })?;
        st.file = Some(file);
        let (data, res) = match read {
            Ok(Ok(read)) => read,
            Ok(Err(e)) => return Ok(Err(e)),
            Err(Stopped) => unreachable!("`offload` reports a stopped read"),
        };
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

/// Reads the rest of `f` into `data`, reserving its growth in `res`.
fn read_to_end(
    f: &mut OpenedFile,
    data: &mut Vec<u8>,
    res: &mut Reservation,
    allowance: &mut Allowance,
) -> Result<io::Result<()>, Stopped> {
    let mut chunk = [0u8; 8192];
    loop {
        allowance.charge(SYSCALL_FUEL)?;
        let start = data.len();
        let n = if data.capacity() > start {
            data.resize(data.capacity(), 0);
            let n = f.read(&mut data[start..]);
            data.truncate(start + *n.as_ref().unwrap_or(&0));
            n
        } else {
            f.read(&mut chunk).and_then(|n| {
                res.reserve_buf(data, n)?;
                data.extend_from_slice(&chunk[..n]);
                Ok(n)
            })
        };
        match n {
            Ok(0) => return Ok(Ok(())),
            Ok(_) => {}
            Err(e) => return Ok(Err(e)),
        }
    }
}
