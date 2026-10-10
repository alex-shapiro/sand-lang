//! The host's file system on Windows.
//!
//! Paths are canonicalised by the standard library (`canonicalize`, which
//! follows symlinks and junctions and names components as stored) on the
//! longest prefix that resolves; then every component is opened relative to
//! the handle of its parent directory with `FILE_OPEN_REPARSE_POINT`, so a
//! symlink, junction or other name-surrogate reparse point is opened
//! itself, never followed, and is refused (or, for entry operations, acted
//! on itself). Creating and removing entries and listing directories also
//! go through handles. Only `rename` has no handle-based form in a safe
//! API: it is path-based, with every directory leading to both entries held
//! open (and checked against the handle-relative walk) so that none of them
//! can be renamed, deleted or replaced meanwhile.
//!
//! This takes two walks (the canonicalisation and the handle walk): the
//! single walk of `unix.rs` would need to read reparse points and the final
//! path of a handle (`GetFinalPathNameByHandleW`), which the standard
//! library and `fs_at` do not offer without `unsafe`.
//!
//! Names that Win32 path functions would alter or treat specially (a
//! trailing `.` or space, `:` for alternate data streams, reserved device
//! names such as `CON`) are refused: handle-relative opens take them
//! literally, so they would name entries that the (Win32-based)
//! canonicalisation does not see.

use super::{EntryKind, EntryMeta, Mode, Work, plain_from_verbatim, unchecked_symlink};
use crate::policy::{MAX_LINK_SCAN, fold_name, lexical};
use crate::vm::iomem::Reservation;
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes counted per directory entry besides its name.
const ENTRY_OVERHEAD: usize = 2 * std::mem::size_of::<std::ffi::OsString>();

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display()))
}

/// A host path resolved for an operation: its canonical form, on which
/// the operation then walks again without following reparse points.
pub(crate) struct Resolved {
    path: PathBuf,
    missing: Vec<PathBuf>,
    pub work: Work,
}

/// Resolves the absolute path `p` (with `mode` `Entry`, a final symlink or
/// junction is not followed).
pub(crate) fn resolve(p: &Path, mode: Mode) -> Resolved {
    let lexical = lexical(p);
    let mut work = Work::default();
    let path = match mode {
        Mode::Follow => canonicalize_existing(lexical),
        Mode::Entry => {
            let e = entry_path_on_host(&lexical);
            work.scanned = e.scanned;
            work.scanned_bytes = e.scanned_bytes;
            e.path
        }
    };
    let mut missing: Vec<PathBuf> =
        path.ancestors().skip(1).take_while(|a| std::fs::symlink_metadata(a).is_err()).map(Path::to_path_buf).collect();
    missing.reverse();
    Resolved { path, missing, work }
}

impl Resolved {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Paths are canonicalised whole (see the module documentation).
    pub fn exact(&self) -> bool {
        true
    }

    pub fn fix_spelling(&mut self) {}

    pub fn size(&self) -> Option<u64> {
        stat(&self.path).ok().map(|m| m.size)
    }

    pub fn missing_ancestors(&self) -> Vec<PathBuf> {
        self.missing.clone()
    }

    pub fn open_read(self, _: &Path) -> io::Result<std::fs::File> {
        open_read(&self.path)
    }

    pub fn open_write(self, _: &Path, append: bool) -> io::Result<std::fs::File> {
        open_write(&self.path, append)
    }

    pub fn stat(&self) -> io::Result<EntryMeta> {
        let m = stat(&self.path)?;
        Ok(EntryMeta { kind: m.kind, size: m.size, modified: m.modified.map(unix_nanos) })
    }

    pub fn list_dir(self, res: &mut Reservation) -> io::Result<Vec<String>> {
        Ok(list_dir(&self.path, res)?.into_iter().map(|n| n.to_string_lossy().into_owned()).collect())
    }

    pub fn create_dir(self) -> io::Result<()> {
        create_dir(&self.path)
    }

    /// Creates the path and the missing directories above it; the others
    /// must exist (they were not checked as directories that may be created).
    pub fn create_dir_all(self, _: &Path) -> io::Result<()> {
        let names = self.path.components().filter(|c| matches!(c, Component::Normal(_))).count();
        create_dir_all(&self.path, names.saturating_sub(self.missing.len() + 1))
    }

    pub fn remove_file(self) -> io::Result<()> {
        remove_file(&self.path)
    }

    pub fn remove_dir(self) -> io::Result<()> {
        remove_dir(&self.path)
    }

    pub fn rename(from: Resolved, to: Resolved) -> io::Result<()> {
        rename(&from.path, &to.path)
    }
}

fn unix_nanos(t: SystemTime) -> i64 {
    let clamp = |n: u128| i64::try_from(n).unwrap_or(i64::MAX);
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => clamp(d.as_nanos()),
        Err(e) => -clamp(e.duration().as_nanos()),
    }
}

/// Metadata of a directory entry (a final symlink is not followed).
#[derive(Clone, Copy, Debug)]
struct Meta {
    kind: EntryKind,
    size: u64,
    modified: Option<SystemTime>,
}

/// Canonicalises the longest prefix of the lexically normal path `p` that
/// resolves, keeping the rest as it is. If a prefix resolves, so do the
/// shorter ones, so the prefix is found by binary search: a path of n
/// components takes O(log n) `canonicalize` calls rather than n.
fn canonicalize_existing(p: PathBuf) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return simplify_verbatim(c);
    }
    let parts: Vec<&std::ffi::OsStr> = p.components().map(|c| c.as_os_str()).collect();
    // The root (`/`, `C:\`) is not split: `C:` alone is relative.
    let root = p.components().take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir)).count();
    let prefix = |k: usize| parts[..k].iter().collect::<PathBuf>();
    // Invariant: `prefix(hi)` does not resolve; `best` is the canonical
    // `prefix(lo)` once one was found.
    let (mut lo, mut hi, mut best) = (root, parts.len(), None);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match prefix(mid).canonicalize() {
            Ok(c) => {
                lo = mid;
                best = Some(c);
            }
            Err(_) => hi = mid,
        }
    }
    let best = match best {
        Some(b) => b,
        None if root > 0 => match prefix(root).canonicalize() {
            Ok(b) => b,
            Err(_) => return p,
        },
        None => return p,
    };
    let mut r = simplify_verbatim(best);
    for part in &parts[lo..] {
        r.push(part);
    }
    r
}

/// A path with the symlinks before its final name resolved (see
/// `policy::normalize_entry_path`) and the directory entries read to find a
/// symlink's stored name.
struct EntryPath {
    path: PathBuf,
    scanned: usize,
    scanned_bytes: usize,
}

fn entry_path_on_host(p: &Path) -> EntryPath {
    let lexical = lexical(p);
    let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
        return EntryPath { path: normalize_path(&lexical), scanned: 0, scanned_bytes: 0 };
    };
    let parent = normalize_path(parent);
    let entry = parent.join(name);
    let path = match std::fs::symlink_metadata(&entry) {
        Ok(m) if m.file_type().is_symlink() => {
            let (stored, scanned, scanned_bytes) = stored_link_name(&parent, name, MAX_LINK_SCAN);
            let path = stored.map_or(entry, |n| parent.join(n));
            return EntryPath { path, scanned, scanned_bytes };
        }
        Ok(_) => match entry.canonicalize().map(simplify_verbatim) {
            // (If it became a symlink meanwhile, its target is ignored.)
            Ok(c) if c.parent() == Some(parent.as_path()) => c,
            _ => entry,
        },
        Err(_) => entry,
    };
    EntryPath { path, scanned: 0, scanned_bytes: 0 }
}

/// The stored name of the symlink `parent/name` if it is spelled
/// differently, with the number of entries read and the bytes of
/// their names. The stored name is the entry whose name folds like
/// `name` (see [`fold_name`]) and that is the same directory entry
/// as `parent/name`; an exact match is preferred.
fn stored_link_name(parent: &Path, name: &std::ffi::OsStr, limit: usize) -> (Option<std::ffi::OsString>, usize, usize) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return (None, 0, 0);
    };
    let link = parent.join(name);
    let folded = fold_name(&name.to_string_lossy());
    let (mut found, mut scanned, mut bytes) = (None, 0, 0);
    for entry in entries {
        if scanned == limit {
            return (None, scanned, bytes);
        }
        scanned += 1;
        let Ok(entry) = entry else { continue };
        let candidate = entry.file_name();
        bytes += candidate.len();
        if candidate == name {
            return (None, scanned, bytes);
        }
        if found.is_none() && fold_name(&candidate.to_string_lossy()) == folded && same_entry(&link, &parent.join(&candidate)) {
            found = Some(candidate);
        }
    }
    (found, scanned, bytes)
}

/// Whether `a` and `b` name the same directory entry (final symlinks not
/// followed).
fn same_entry(a: &Path, b: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_READ_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT.
    let open = |p: &Path| {
        std::fs::OpenOptions::new().access_mode(0x80).custom_flags(0x0200_0000 | 0x0020_0000).open(p).and_then(same_file::Handle::from_file)
    };
    matches!((open(a), open(b)), (Ok(a), Ok(b)) if a == b)
}

/// Windows canonical paths are "verbatim" (`\\?\C:\x`, `\\?\UNC\server\share\x`).
/// Every path is converted to the plain form (`C:\x`, `\\server\share\x`), so
/// that all normalized paths share one form whether or not they exist (and
/// read naturally in prompts and errors). The standard library re-adds the
/// prefix where needed (long paths).
fn simplify_verbatim(p: PathBuf) -> PathBuf {
    if let Some(s) = p.to_str()
        && let Some(plain) = plain_from_verbatim(s)
    {
        return PathBuf::from(plain);
    }
    p
}

/// `p` normalized lexically, with the longest prefix that resolves canonicalised.
fn normalize_path(p: &Path) -> PathBuf {
    canonicalize_existing(lexical(p))
}

use fs_at::os::windows::{FileExt, OpenOptionsExt};
use same_file::Handle;
use std::fs::{File, Metadata};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt as _;

// Access rights, `NtCreateFile` create options and `CreateFileW` flags
// (winnt.h, ntifs.h, winbase.h).
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const DELETE: u32 = 0x1_0000;
/// READ_CONTROL | FILE_READ_DATA (FILE_LIST_DIRECTORY) | FILE_READ_ATTRIBUTES | FILE_READ_EA | SYNCHRONIZE.
const FILE_GENERIC_READ: u32 = 0x12_0089;
/// READ_CONTROL | FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA | SYNCHRONIZE.
const FILE_GENERIC_WRITE: u32 = 0x12_0116;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_OPEN_REPARSE_POINT: u32 = 0x20_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const ERROR_DIRECTORY: i32 = 267;

fn invalid(path: &Path, what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} {what}", path.display()))
}

/// The error for a file system that changed while an operation checked it.
fn changed(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{} changed during the operation", path.display()))
}

/// Splits the absolute, normalized `path` into its root (`C:\`,
/// `\\server\share\`) and the names below it.
fn split(path: &Path) -> io::Result<(PathBuf, Vec<&OsStr>)> {
    let mut root = PathBuf::new();
    let mut names = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir if names.is_empty() => root.push(c),
            Component::Normal(name) if root.is_absolute() => {
                check_name(path, name)?;
                names.push(name);
            }
            _ if !root.is_absolute() => return Err(invalid(path, "is not an absolute path")),
            _ => return Err(invalid(path, "is not a normalized path")),
        }
    }
    if !root.is_absolute() {
        return Err(invalid(path, "is not an absolute path"));
    }
    Ok((root, names))
}

/// Checks for names that Win32 path functions treat specially.
fn check_name(path: &Path, name: &OsStr) -> io::Result<()> {
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.contains(&0) {
        // (`NtCreateFile` names would end there.)
        return Err(invalid(path, "contains a NUL character"));
    }
    if wide.contains(&u16::from(b':')) {
        return Err(invalid(path, "names an alternate data stream (`:` in a file name)"));
    }
    if matches!(wide.last(), Some(&c) if c == u16::from(b'.') || c == u16::from(b' ')) {
        return Err(invalid(path, "has a file name ending in `.` or a space"));
    }
    let name = name.to_string_lossy();
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_uppercase();
    let numbered = |prefix: &str| {
        let mut rest = stem.strip_prefix(prefix).unwrap_or_default().chars();
        matches!((rest.next(), rest.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
    };
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT");
    if reserved {
        return Err(invalid(path, "names a reserved device"));
    }
    Ok(())
}

/// Options to open an entry relative to a directory handle.
fn options(access: u32, create_options: u32) -> fs_at::OpenOptions {
    let mut o = fs_at::OpenOptions::default();
    o.follow(true).desired_access(access).create_options(create_options | FILE_OPEN_REPARSE_POINT).object_attributes(OBJ_CASE_INSENSITIVE);
    o
}

/// Opens `name` in `dir` with `o`.
fn open_in(dir: &File, name: &OsStr, o: &fs_at::OpenOptions) -> io::Result<File> {
    o.open_at(dir, name).map_err(|e| {
        // `fs_at` reports ERROR_DIRECTORY (with FILE_DIRECTORY_FILE, the
        // entry is not a directory) as an `AlreadyExists` error wrapping it.
        let inner = e.get_ref().and_then(|i| i.downcast_ref::<io::Error>()).and_then(io::Error::raw_os_error);
        if inner == Some(ERROR_DIRECTORY) { io::Error::from_raw_os_error(ERROR_DIRECTORY) } else { e }
    })
}

/// The metadata of the entry `name` in `dir` itself (not followed).
fn lstat(dir: &File, name: &OsStr) -> io::Result<Metadata> {
    open_in(dir, name, &options(FILE_READ_ATTRIBUTES, 0))?.metadata()
}

/// Converts an error of opening `name` in `dir`: if a symlink is there
/// (e.g. a file symlink where a directory was expected),
/// [`unchecked_symlink`].
fn nofollow_error(e: io::Error, dir: &File, name: &OsStr) -> io::Error {
    if e.kind() != io::ErrorKind::NotFound && lstat(dir, name).is_ok_and(|m| m.file_type().is_symlink()) {
        return unchecked_symlink(name);
    }
    e
}

fn open_root(root: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new().access_mode(FILE_GENERIC_READ).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(root)
}

/// Opens the directory `name` in `dir`, refusing a symlink or junction.
fn open_subdir(dir: &File, name: &OsStr) -> io::Result<File> {
    let sub = open_in(dir, name, &options(FILE_GENERIC_READ, FILE_DIRECTORY_FILE)).map_err(|e| nofollow_error(e, dir, name))?;
    if sub.metadata()?.file_type().is_symlink() {
        return Err(unchecked_symlink(name));
    }
    Ok(sub)
}

/// Opens the directory `path` without following symlinks in any component.
fn open_dir(path: &Path) -> io::Result<File> {
    let (root, names) = split(path)?;
    let mut dir = open_root(&root)?;
    for name in names {
        dir = open_subdir(&dir, name)?;
    }
    Ok(dir)
}

/// The parent directory of `path` and the final name.
fn parent(path: &Path) -> io::Result<(File, &OsStr)> {
    let (root, mut names) = split(path)?;
    let name = names.pop().ok_or_else(|| invalid(path, "is a root directory"))?;
    let mut dir = open_root(&root)?;
    for name in names {
        dir = open_subdir(&dir, name)?;
    }
    Ok((dir, name))
}

/// `f` (opened as `name` of `path`) if it is a regular file.
fn regular(path: &Path, name: &OsStr, f: File) -> io::Result<File> {
    let t = f.metadata()?.file_type();
    if t.is_symlink() {
        Err(unchecked_symlink(name))
    } else if t.is_file() {
        Ok(f)
    } else {
        Err(not_regular(path))
    }
}

fn open_read(path: &Path) -> io::Result<File> {
    let (dir, name) = parent(path)?;
    let f = open_in(&dir, name, &options(FILE_GENERIC_READ, 0)).map_err(|e| nofollow_error(e, &dir, name))?;
    regular(path, name, f)
}

/// Opens a regular file for writing, creating it if needed; truncates it
/// unless `append`.
fn open_write(path: &Path, append: bool) -> io::Result<File> {
    let (dir, name) = parent(path)?;
    // Without FILE_WRITE_DATA, every write appends. Truncation is done
    // after the checks: truncating on open would turn a symlink into an
    // empty regular file.
    let write = if append { FILE_GENERIC_WRITE & !FILE_WRITE_DATA } else { FILE_GENERIC_WRITE };
    let mut o = options(write | FILE_READ_ATTRIBUTES, FILE_NON_DIRECTORY_FILE);
    o.create(true);
    let opened = open_in(&dir, name, &o).map_err(|e| match nofollow_error(e, &dir, name) {
        // FILE_NON_DIRECTORY_FILE fails with "access denied" for a directory.
        e if e.kind() == io::ErrorKind::PermissionDenied && lstat(&dir, name).is_ok_and(|m| m.is_dir()) => {
            io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory", path.display()))
        }
        e => e,
    })?;
    let f = regular(path, name, opened)?;
    if !append {
        f.set_len(0)?;
    }
    Ok(f)
}

fn entry_meta(m: &Metadata) -> Meta {
    let t = m.file_type();
    let kind = if t.is_symlink() {
        EntryKind::Symlink
    } else if t.is_dir() {
        EntryKind::Dir
    } else if t.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Meta { kind, size: m.len(), modified: m.modified().ok() }
}

fn stat(path: &Path) -> io::Result<Meta> {
    if path.parent().is_none() {
        return Ok(entry_meta(&open_dir(path)?.metadata()?));
    }
    let (dir, name) = parent(path)?;
    Ok(entry_meta(&lstat(&dir, name)?))
}

/// The names of the entries of the directory `path` (unsorted, without
/// `.` and `..`), reserving their memory in `res`.
fn list_dir(path: &Path, res: &mut Reservation) -> io::Result<Vec<std::ffi::OsString>> {
    let mut dir = open_dir(path)?;
    let mut names = Vec::new();
    for entry in fs_at::read_dir(&mut dir)? {
        let entry = entry?;
        let name = entry.name();
        if name != OsStr::new(".") && name != OsStr::new("..") {
            res.grow(name.len() + ENTRY_OVERHEAD)?;
            names.push(name.to_os_string());
        }
    }
    Ok(names)
}

/// Creates the directory `name` in `dir` and opens it.
fn mkdir(dir: &File, name: &OsStr) -> io::Result<File> {
    // FILE_CREATE with FILE_DIRECTORY_FILE creates a directory and never
    // opens an existing entry (or follows a symlink).
    let mut o = options(FILE_GENERIC_READ, FILE_DIRECTORY_FILE);
    o.create_new(true);
    open_in(dir, name, &o)
}

fn create_dir(path: &Path) -> io::Result<()> {
    let (dir, name) = parent(path)?;
    mkdir(&dir, name).map(drop)
}

/// Creates `path` and missing parents; an existing directory is fine.
/// The first `keep` components below the root must exist already.
fn create_dir_all(path: &Path, keep: usize) -> io::Result<()> {
    let (root, names) = split(path)?;
    let mut dir = open_root(&root)?;
    let last = names.len();
    for (i, name) in names.into_iter().enumerate() {
        let opened = match open_subdir(&dir, name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && i >= keep => match mkdir(&dir, name) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => open_subdir(&dir, name),
                r => r,
            },
            r => r,
        };
        dir = match opened {
            Ok(d) => d,
            Err(e) if i + 1 == last && e.kind() == io::ErrorKind::NotADirectory => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", path.display())));
            }
            Err(e) => return Err(e),
        };
    }
    Ok(())
}

/// Opens the entry `path` itself for deletion and deletes it if allowed.
fn remove(path: &Path, accept: impl FnOnce(&Metadata) -> io::Result<()>) -> io::Result<()> {
    let (dir, name) = parent(path)?;
    let entry = open_in(&dir, name, &options(DELETE | FILE_READ_ATTRIBUTES, 0))?;
    accept(&entry.metadata()?)?;
    entry.delete_by_handle().map_err(|(_, e)| e)
}

/// Removes the file or symlink `path` (not a symlink's target).
fn remove_file(path: &Path) -> io::Result<()> {
    remove(path, |m| {
        if m.is_dir() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory", path.display())));
        }
        Ok(())
    })
}

fn remove_dir(path: &Path) -> io::Result<()> {
    remove(path, |m| {
        if !m.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", path.display())));
        }
        Ok(())
    })
}

/// Opens the directory `path` by path, without `FILE_SHARE_DELETE`: while
/// the handle is open, the directory cannot be renamed or deleted.
fn lock(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .access_mode(FILE_GENERIC_READ)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Holds the directories from `root` along `names` open so that none of
/// them can be renamed, deleted or replaced: a path through them keeps
/// naming the same directories. Each is opened by path and must be the
/// directory that the handle-relative, no-follow walk finds there.
fn lock_dirs(root: &Path, names: &[&OsStr]) -> io::Result<Vec<Handle>> {
    let mut path = root.to_path_buf();
    let mut held = vec![Handle::from_file(lock(&path)?)?];
    for &name in names {
        let dir = held.last().map(Handle::as_file).ok_or_else(|| changed(&path))?;
        let genuine = Handle::from_file(open_subdir(dir, name)?)?;
        path.push(name);
        let locked = Handle::from_file(lock(&path)?)?;
        if locked != genuine {
            return Err(changed(&path));
        }
        held.push(locked);
    }
    Ok(held)
}

/// Renames the entry `from` (a final symlink itself) to `to`.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let (from_root, mut from_names) = split(from)?;
    let (to_root, mut to_names) = split(to)?;
    if from_names.pop().is_none() || to_names.pop().is_none() {
        return Err(invalid(from, "cannot be renamed (a root directory is involved)"));
    }
    let from_dirs = lock_dirs(&from_root, &from_names)?;
    let to_dirs = lock_dirs(&to_root, &to_names)?;
    // The held directories cannot be replaced, and those that contain
    // another held directory cannot become junctions (only an empty
    // directory can). The two parents could, so they are checked right
    // before and after the path-based rename.
    let parents = [from_dirs.last(), to_dirs.last()];
    let unchanged = || -> io::Result<()> {
        for (dir, path) in parents.iter().zip([from, to]) {
            if dir.ok_or_else(|| changed(path))?.as_file().metadata()?.file_type().is_symlink() {
                return Err(changed(path));
            }
        }
        Ok(())
    };
    unchanged()?;
    std::fs::rename(from, to)?;
    unchanged()
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::super::is_unchecked_symlink;
    use super::*;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    /// Creates the junction `link` to the directory `target` (junctions need
    /// no privilege, unlike symlinks).
    fn junction(link: &Path, target: &Path) {
        junction::create(target, link).unwrap();
    }

    /// The Unix fixture, with a junction `link` to `outside` and, if the
    /// process may create symlinks, a file symlink `real/link.txt` (whether
    /// it could is returned).
    fn temp(tag: &str) -> (PathBuf, bool) {
        let dir = std::env::temp_dir().join(format!("sand-fs-safe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("real/file.txt"), "inside").unwrap();
        std::fs::write(dir.join("outside/file.txt"), "outside").unwrap();
        let dir = crate::policy::normalize_path(&dir);
        junction(&dir.join("link"), &dir.join("outside"));
        let file_link = std::os::windows::fs::symlink_file(dir.join("outside/file.txt"), dir.join("real/link.txt")).is_ok();
        (dir, file_link)
    }

    /// The number of names below the root.
    fn depth(p: &Path) -> usize {
        p.components().filter(|c| matches!(c, std::path::Component::Normal(_))).count()
    }

    #[test]
    fn junctions_and_symlinks_are_never_followed() {
        let (dir, file_link) = temp("nofollow");
        let mut s = String::new();
        open_read(&dir.join("real/file.txt")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "inside");
        let mut links = vec!["link/file.txt"];
        if file_link {
            links.push("real/link.txt");
        }
        for p in links {
            let e = open_read(&dir.join(p)).unwrap_err();
            assert!(is_unchecked_symlink(&e), "{p}: {e}");
            let e = open_write(&dir.join(p), false).unwrap_err();
            assert!(is_unchecked_symlink(&e), "{p}: {e}");
        }
        assert!(is_unchecked_symlink(&open_write(&dir.join("link/new.txt"), true).unwrap_err()));
        let mem = crate::vm::iomem::IoMemory::new(None);
        assert!(is_unchecked_symlink(&list_dir(&dir.join("link"), &mut Reservation::new(&mem)).unwrap_err()));
        assert!(is_unchecked_symlink(&create_dir(&dir.join("link/sub")).unwrap_err()));
        assert!(is_unchecked_symlink(&create_dir_all(&dir.join("link/a/b"), 0).unwrap_err()));
        assert!(is_unchecked_symlink(&stat(&dir.join("link/file.txt")).unwrap_err()));
        assert!(is_unchecked_symlink(&remove_file(&dir.join("link/file.txt")).unwrap_err()));
        assert!(is_unchecked_symlink(&rename(&dir.join("link/file.txt"), &dir.join("stolen.txt")).unwrap_err()));
        assert!(is_unchecked_symlink(&rename(&dir.join("real/file.txt"), &dir.join("link/stolen.txt")).unwrap_err()));
        assert!(!dir.join("outside/new.txt").exists() && !dir.join("outside/sub").exists() && !dir.join("outside/a").exists());
        assert!(!dir.join("outside/stolen.txt").exists() && dir.join("real/file.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.join("outside/file.txt")).unwrap(), "outside");
        // Links are entries in their own right for stat, remove and rename.
        assert_eq!(stat(&dir.join("link")).unwrap().kind, EntryKind::Symlink);
        if file_link {
            assert_eq!(stat(&dir.join("real/link.txt")).unwrap().kind, EntryKind::Symlink);
            rename(&dir.join("real/link.txt"), &dir.join("real/moved.txt")).unwrap();
            remove_file(&dir.join("real/moved.txt")).unwrap();
        }
        assert_eq!(remove_dir(&dir.join("link")).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        rename(&dir.join("link"), &dir.join("real/moved_link")).unwrap();
        remove_file(&dir.join("real/moved_link")).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("real/moved_link")).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("outside/file.txt")).unwrap(), "outside");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn walking_refuses_junctions() {
        let (dir, _) = temp("walk");
        assert!(open_dir(&dir.join("real")).is_ok());
        assert!(is_unchecked_symlink(&open_dir(&dir.join("link")).unwrap_err()));
        assert!(is_unchecked_symlink(&open_dir(&dir.join("link/deeper")).unwrap_err()));
        assert_eq!(open_dir(&dir.join("real/file.txt")).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        assert_eq!(open_dir(&dir.join("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(open_dir(&dir.join("REAL")).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regular_files_and_directories() {
        let (dir, _) = temp("ops");
        let mut f = open_write(&dir.join("real/new.txt"), false).unwrap();
        f.write_all(b"one").unwrap();
        drop(f);
        open_write(&dir.join("real/new.txt"), true).unwrap().write_all(b"two").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/new.txt")).unwrap(), "onetwo");
        open_write(&dir.join("real/NEW.txt"), true).unwrap().write_all(b"!").unwrap();
        let m = stat(&dir.join("real/new.txt")).unwrap();
        assert_eq!((m.kind, m.size), (EntryKind::File, 7));
        assert!(m.modified.is_some());
        // Writing truncates.
        open_write(&dir.join("real/new.txt"), false).unwrap().write_all(b"3").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/new.txt")).unwrap(), "3");
        assert_eq!(stat(&dir).unwrap().kind, EntryKind::Dir);
        let root = dir.ancestors().last().unwrap();
        assert_eq!(stat(root).unwrap().kind, EntryKind::Dir);
        create_dir_all(&dir.join("real/a/b/c"), 0).unwrap();
        create_dir_all(&dir.join("real/a/b/c"), 0).unwrap();
        assert_eq!(create_dir_all(&dir.join("real/file.txt"), 0).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(create_dir_all(&dir.join("real/file.txt/sub"), 0).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        // Components that must exist are not created.
        let depth = depth(&dir.join("real/x"));
        assert_eq!(create_dir_all(&dir.join("real/x/y"), depth).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!dir.join("real/x").exists());
        create_dir_all(&dir.join("real/x/y"), depth - 1).unwrap();
        create_dir(&dir.join("real/a/d")).unwrap();
        assert_eq!(create_dir(&dir.join("real/a/d")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let mem = crate::vm::iomem::IoMemory::new(None);
        let mut names = list_dir(&dir.join("real/a"), &mut Reservation::new(&mem)).unwrap();
        names.sort();
        assert_eq!(names, ["b", "d"]);
        assert!(!list_dir(root, &mut Reservation::new(&mem)).unwrap().is_empty());
        assert_eq!(open_read(&dir.join("real/a")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(open_write(&dir.join("real/a"), false).unwrap_err().kind(), io::ErrorKind::IsADirectory);
        assert_eq!(open_read(&dir.join("real/missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(remove_file(&dir.join("real/a/d")).unwrap_err().kind(), io::ErrorKind::IsADirectory);
        assert_eq!(remove_dir(&dir.join("real/file.txt")).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        assert_eq!(remove_dir(&dir.join("real/a")).unwrap_err().kind(), io::ErrorKind::DirectoryNotEmpty);
        remove_dir(&dir.join("real/a/d")).unwrap();
        rename(&dir.join("real/a"), &dir.join("real/z")).unwrap();
        assert!(dir.join("real/z/b/c").is_dir());
        // Across directories, replacing a file.
        rename(&dir.join("real/new.txt"), &dir.join("real/z/b/file.txt")).unwrap();
        rename(&dir.join("real/z/b/file.txt"), &dir.join("real/file.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/file.txt")).unwrap(), "3");
        let e = rename(&dir.join("real/file.txt"), &dir.join("real/missing/f.txt")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        remove_file(&dir.join("real/file.txt")).unwrap();
        assert!(!dir.join("real/file.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Handle-relative opens take names literally, so names that Win32
    /// path functions would alter (and the policy would not see) are refused.
    #[test]
    fn names_win32_treats_specially_are_refused() {
        let (dir, _) = temp("names");
        for name in ["x.", "x ", "file.txt:stream", "file.txt::$DATA", "CON", "nul.txt", "Com1", "LPT9.log", "aux ", "file.txt\0x"] {
            let e = open_write(&dir.join("real").join(name), false).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{name}: {e}");
            assert_eq!(open_read(&dir.join(name).join("x.txt")).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{name}");
            assert_eq!(create_dir(&dir.join("real").join(name)).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{name}");
        }
        for name in ["console", "COM10", "x.y", "LPT"] {
            open_write(&dir.join("real").join(name), false).unwrap();
        }
        assert_eq!(std::fs::read_to_string(dir.join("real/file.txt")).unwrap(), "inside");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
