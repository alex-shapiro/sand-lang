//! The host's file system on Windows. As in `unix.rs`, each operation
//! resolves its path in a single walk.
//!
//! [`resolve`] walks an absolute, lexically normal path from its root
//! (`C:\`, `\\server\share\`). It opens each name relative to its parent's
//! handle without following it (`FILE_OPEN_REPARSE_POINT`). When it meets a
//! symlink or junction, it reads the target through the link's handle
//! (`winx`) and continues with the target in place of the link, as
//! `realpath` does. The walk keeps the directories it passes open. The
//! canonical path the policy decides on is the system's path of the
//! directory holding the final entry (`GetFinalPathNameByHandleW`), followed
//! by the final name. The system spells names as stored, and its path stays
//! correct even if a directory on the way was renamed during the walk.
//!
//! The operation then opens the final name relative to the held directory,
//! without following it, so it reaches exactly the entry that was checked.
//! `rename` is the exception, because safe Rust has no way to rename by
//! handle. It renames by path, and while it does, it holds every directory
//! on both paths open without `FILE_SHARE_DELETE`, so none of them can be
//! renamed, deleted or replaced. It fails if either parent became a
//! junction. That can still happen to a parent that is emptied between the
//! check and the rename.
//!
//! Names that Win32 path functions change are denied: a trailing `.` or
//! space, a `:` (which names an alternate data stream), and reserved device
//! names such as `CON`. Handle-relative opens take such names literally, so
//! they would reach entries that path-based functions, and the policy's
//! rules, would not.

use super::{ENTRY_OVERHEAD, EntryKind, EntryMeta, Mode, OpenMode, Work, plain_from_verbatim, unchecked_symlink};
use crate::policy::{MAX_LINK_SCAN, fold_name, lexical};
use crate::vm::iomem::Reservation;
use fs_at::os::windows::{FileExt, OpenOptionsExt};
use same_file::Handle;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{SystemTime, UNIX_EPOCH};

/// At most this many symlinks are followed in one path.
const MAX_LINKS: usize = 40;

// Access rights, `NtCreateFile` create options and `CreateFileW` flags
// (winnt.h, ntifs.h, winbase.h).
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const DELETE: u32 = 0x1_0000;
const SYNCHRONIZE: u32 = 0x10_0000;
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

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display()))
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_string())
}

/// The error for a directory that changed while an operation was checking it.
fn changed(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{} changed during the operation", path.display()))
}

/// Metadata of an entry. For a symlink or junction, it describes the link itself.
#[derive(Clone, Copy, Debug)]
struct Meta {
    kind: EntryKind,
    size: u64,
    modified: Option<SystemTime>,
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

fn unix_nanos(t: SystemTime) -> i64 {
    let clamp = |n: u128| i64::try_from(n).unwrap_or(i64::MAX);
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => clamp(d.as_nanos()),
        Err(e) => -clamp(e.duration().as_nanos()),
    }
}

/// Checks for names that Win32 path functions treat specially.
fn check_name(name: &OsStr) -> io::Result<()> {
    let shown = || name.to_string_lossy().into_owned();
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.contains(&0) {
        // `NtCreateFile` would end the name at the NUL.
        return Err(invalid(format_args!("{:?} contains a NUL character", shown())));
    }
    if wide.contains(&u16::from(b':')) {
        return Err(invalid(format_args!("{:?} names an alternate data stream (`:` in a file name)", shown())));
    }
    if matches!(wide.last(), Some(&c) if c == u16::from(b'.') || c == u16::from(b' ')) {
        return Err(invalid(format_args!("{:?} ends in `.` or a space", shown())));
    }
    let name = name.to_string_lossy();
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_uppercase();
    let numbered = |prefix: &str| {
        let mut rest = stem.strip_prefix(prefix).unwrap_or_default().chars();
        matches!((rest.next(), rest.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
    };
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT");
    if reserved {
        return Err(invalid(format_args!("{:?} names a reserved device", shown())));
    }
    Ok(())
}

/// Options to open an entry relative to a directory handle. A symlink or
/// junction is opened itself, never followed.
fn options(access: u32, create_options: u32) -> fs_at::OpenOptions {
    let mut o = fs_at::OpenOptions::default();
    o.follow(true).desired_access(access).create_options(create_options | FILE_OPEN_REPARSE_POINT).object_attributes(OBJ_CASE_INSENSITIVE);
    o
}

/// Opens `name` in `dir` with `o`.
fn open_in(dir: &File, name: &OsStr, o: &fs_at::OpenOptions) -> io::Result<File> {
    o.open_at(dir, name).map_err(|e| {
        // With FILE_DIRECTORY_FILE, opening something that is not a
        // directory fails with ERROR_DIRECTORY, which `fs_at` wraps in an
        // `AlreadyExists` error.
        let inner = e.get_ref().and_then(|i| i.downcast_ref::<io::Error>()).and_then(io::Error::raw_os_error);
        if inner == Some(ERROR_DIRECTORY) { io::Error::from_raw_os_error(ERROR_DIRECTORY) } else { e }
    })
}

/// Opens the entry `name` of `dir` to read its metadata.
fn open_entry(dir: &File, name: &OsStr) -> io::Result<File> {
    open_in(dir, name, &options(FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0))
}

/// The metadata of the entry `name` in `dir`.
fn lstat(dir: &File, name: &OsStr) -> io::Result<Metadata> {
    open_entry(dir, name)?.metadata()
}

/// Converts an error from opening `name` in `dir`. If the entry is a
/// symlink (for example, a file symlink where a directory was expected),
/// the error becomes [`unchecked_symlink`].
fn nofollow_error(e: io::Error, dir: &File, name: &OsStr) -> io::Error {
    if e.kind() != io::ErrorKind::NotFound && lstat(dir, name).is_ok_and(|m| m.file_type().is_symlink()) {
        return unchecked_symlink(name);
    }
    e
}

/// Opens the directory `name` in `dir` with `access`. A symlink or
/// junction is denied.
fn open_subdir(dir: &File, name: &OsStr, access: u32) -> io::Result<File> {
    let sub = open_in(dir, name, &options(access, FILE_DIRECTORY_FILE)).map_err(|e| nofollow_error(e, dir, name))?;
    if sub.metadata()?.file_type().is_symlink() {
        return Err(unchecked_symlink(name));
    }
    Ok(sub)
}

/// Opens the root directory `root` (`C:\`, `\\server\share\`) by path.
fn open_root(root: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(root)
}

/// Opens the directory `dir` again, with the access needed to list it.
fn listable(dir: &File) -> io::Result<File> {
    use winx::file::{AccessMode, Flags, ShareMode};
    winx::file::reopen_file(
        dir.as_handle(),
        AccessMode::FILE_LIST_DIRECTORY | AccessMode::FILE_READ_ATTRIBUTES | AccessMode::SYNCHRONIZE,
        ShareMode::FILE_SHARE_READ | ShareMode::FILE_SHARE_WRITE | ShareMode::FILE_SHARE_DELETE,
        Flags::FILE_FLAG_BACKUP_SEMANTICS | Flags::FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// The names of the entries of the directory `dir`, unsorted and without
/// `.` and `..`. Their memory is reserved in `res`.
fn read_names(dir: &File, res: &mut Reservation) -> io::Result<Vec<OsString>> {
    let mut dir = listable(dir)?;
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
    // With FILE_DIRECTORY_FILE, FILE_CREATE creates a directory and fails
    // on any existing entry, so it never follows a symlink.
    let mut o = options(FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_DIRECTORY_FILE);
    o.create_new(true);
    open_in(dir, name, &o)
}

/// Deletes the entry `name` of `dir` (a link itself, not its target) if
/// `accept` allows it.
fn delete(dir: &File, name: &OsStr, accept: impl FnOnce(&Metadata) -> io::Result<()>) -> io::Result<()> {
    let entry = open_in(dir, name, &options(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0))?;
    accept(&entry.metadata()?)?;
    entry.delete_by_handle().map_err(|(_, e)| e)
}

/// Returns `f`, opened as `name` of `path`, if it is a regular file.
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

/// Converts a "verbatim" path (`\\?\C:\x`, `\\?\UNC\server\share\x`) to
/// its plain form (`C:\x`, `\\server\share\x`), so that a normal path has
/// the same form whether or not it exists. Returns `None` if there is no
/// plain form.
fn plain(p: PathBuf) -> Option<PathBuf> {
    let s = p.to_str()?;
    if s.starts_with(r"\\?\") {
        return plain_from_verbatim(s).map(PathBuf::from);
    }
    Some(p)
}

/// The system's path of the open directory `dir`.
fn final_path(dir: &File) -> Option<PathBuf> {
    winx::file::get_file_path(dir).ok().and_then(plain)
}

/// Splits an absolute path into its root (`C:\`, `\\server\share\`) and
/// the names below it. Returns `None` if it has no drive or share root.
fn split_root(path: &Path) -> Option<(PathBuf, Vec<OsString>)> {
    let mut comps = path.components();
    let root = match comps.next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => PathBuf::from(format!("{}:\\", d as char)),
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let mut r = OsString::from(r"\\");
                r.push(server);
                r.push(r"\");
                r.push(share);
                r.push(r"\");
                PathBuf::from(r)
            }
            _ => return None,
        },
        _ => return None,
    };
    if comps.next() != Some(Component::RootDir) {
        return None;
    }
    let mut names = Vec::new();
    for c in comps {
        match c {
            Component::Normal(n) => names.push(n.to_os_string()),
            Component::ParentDir => names.push(OsString::from("..")),
            _ => {}
        }
    }
    Some((root, names))
}

/// Why a walk stopped before the final entry's directory.
enum Stop {
    /// A component could not be opened. `NotFound` means it does not exist.
    Io(io::ErrorKind, String),
    /// The symlink with this name in the original path does not resolve.
    /// Operations deny it, because they would follow a symlink the policy
    /// did not see.
    Dangling(OsString),
}

impl Stop {
    fn from(e: io::Error) -> Stop {
        Stop::Io(e.kind(), e.to_string())
    }

    fn error(&self) -> io::Error {
        match self {
            Stop::Io(kind, message) => io::Error::new(*kind, message.clone()),
            Stop::Dangling(link) => unchecked_symlink(link),
        }
    }
}

/// A host path after [`resolve`]: the directories leading to the final
/// entry, kept open, and the names below them.
pub(crate) struct Resolved {
    /// The root and the directories below it, down to the deepest one the
    /// walk reached. Empty if the root could not be opened.
    dirs: Vec<File>,
    root: PathBuf,
    /// The names of `dirs[1..]`.
    names: Vec<OsString>,
    /// The canonical path of the last of `dirs`.
    dir_path: PathBuf,
    /// The names below that directory. Normally this is the final name.
    /// If the walk stopped early, it is every name from the one that could
    /// not be opened. It is empty if the target is the directory itself.
    rest: Vec<OsString>,
    stop: Option<Stop>,
    /// The final entry's metadata and handle, if it exists (a final symlink
    /// is not followed). The handle gives its name as stored.
    entry: Option<Meta>,
    entry_file: Option<File>,
    exact: bool,
    mode: Mode,
    pub work: Work,
}

/// The state of the walk at the outermost symlink being followed, so that
/// the walk can return there if the target does not resolve.
struct Site {
    dirs: Vec<File>,
    root: PathBuf,
    names: Vec<OsString>,
    /// The link's name and the original names after it.
    rest: VecDeque<OsString>,
    /// The link is resolved once the queue is back to this length.
    resume: usize,
}

struct Walk {
    dirs: Vec<File>,
    root: PathBuf,
    names: Vec<OsString>,
    /// The names still to resolve. Only symlink targets add `..`.
    queue: VecDeque<OsString>,
    links: usize,
    site: Option<Site>,
    work: Work,
}

/// Resolves the path `p`, after making it absolute against the process's
/// working directory and normalizing it lexically. In `Entry` mode, a final
/// symlink or junction is not followed.
pub(crate) fn resolve(p: &Path, mode: Mode) -> Resolved {
    let absolute = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let lexical = lexical(&absolute);
    let Some((root, names)) = split_root(&lexical) else {
        let stop = Stop::Io(io::ErrorKind::InvalidInput, format!("{} is not a path on a drive or share", lexical.display()));
        return Resolved {
            dirs: Vec::new(),
            root: PathBuf::new(),
            names: Vec::new(),
            dir_path: lexical,
            rest: Vec::new(),
            stop: Some(stop),
            entry: None,
            entry_file: None,
            exact: true,
            mode,
            work: Work::default(),
        };
    };
    let mut walk = Walk { dirs: Vec::new(), root, names: Vec::new(), queue: names.into(), links: 0, site: None, work: Work::default() };
    match open_root(&walk.root) {
        Ok(d) => walk.dirs.push(d),
        Err(e) => return walk.stop(Stop::from(e), mode),
    }
    walk.run(mode)
}

impl Walk {
    fn here(&self) -> &File {
        self.dirs.last().expect("the walk holds the root")
    }

    fn run(mut self, mode: Mode) -> Resolved {
        loop {
            let Some(first) = self.queue.front() else {
                return self.finish_here(mode);
            };
            if first == ".." {
                // The root's parent is the root.
                if self.names.pop().is_some() {
                    self.dirs.pop();
                }
                self.work.steps += 1;
                self.queue.pop_front();
                self.left_site();
                continue;
            }
            if let Err(e) = check_name(first) {
                return self.fail_with(Stop::from(e), mode);
            }
            let name = first.clone();
            let last = self.queue.len() == 1;
            let opened = open_entry(self.here(), &name).and_then(|f| f.metadata().map(|m| (f, entry_meta(&m))));
            match opened {
                Ok((f, m)) if m.kind == EntryKind::Symlink && !(last && mode == Mode::Entry) => {
                    if let Some(r) = self.follow(&f, mode) {
                        return r;
                    }
                }
                Ok((f, m)) if last => {
                    self.site = None;
                    self.work.steps += 1;
                    return self.finish(Some((m, f)), mode);
                }
                Ok((_, m)) if m.kind == EntryKind::Dir => match open_subdir(self.here(), &name, FILE_READ_ATTRIBUTES | SYNCHRONIZE) {
                    Ok(d) => {
                        self.dirs.push(d);
                        self.names.push(name);
                        self.queue.pop_front();
                        self.work.steps += 1;
                        self.left_site();
                    }
                    Err(e) => return self.fail_with(Stop::from(e), mode),
                },
                Ok(_) => {
                    // A symlink to something other than a directory still
                    // resolves. The next name then fails to open in it.
                    if self.site.as_ref().is_some_and(|s| self.queue.len() == s.resume + 1) {
                        self.site = None;
                    }
                    let e = io::Error::new(io::ErrorKind::NotADirectory, format!("{:?} is not a directory", name));
                    return self.fail_with(Stop::from(e), mode);
                }
                Err(e) if last && e.kind() == io::ErrorKind::NotFound && self.site.is_none() => return self.finish(None, mode),
                Err(e) => return self.fail_with(Stop::from(e), mode),
            }
        }
    }

    /// Replaces the symlink at the front of the queue, opened as `link`,
    /// with its target.
    fn follow(&mut self, link: &File, mode: Mode) -> Option<Resolved> {
        self.links += 1;
        self.work.steps += 1;
        if self.site.is_none() {
            let dirs = match self.dirs.iter().map(File::try_clone).collect::<io::Result<Vec<_>>>() {
                Ok(d) => d,
                Err(e) => return Some(self.stop(Stop::from(e), mode)),
            };
            let resume = self.queue.len() - 1;
            self.site = Some(Site { dirs, root: self.root.clone(), names: self.names.clone(), rest: self.queue.clone(), resume });
        }
        if self.links > MAX_LINKS {
            return Some(self.revert(mode));
        }
        // Only junctions and symlinks can be read. Other reparse points are not followed.
        let Ok(target) = winx::file::read_link(link) else { return Some(self.revert(mode)) };
        self.work.bytes += target.as_os_str().len();
        // `winx` turns an absolute target from its NT form into `C:\x` or `UNC\server\share\x`.
        let target = match target.to_str().and_then(|s| s.strip_prefix(r"UNC\")) {
            Some(unc) => PathBuf::from(format!(r"\\{unc}")),
            None => target,
        };
        let mut names = Vec::new();
        match target.components().next() {
            Some(Component::Prefix(_)) => {
                let Some((root, below)) = split_root(&target) else { return Some(self.revert(mode)) };
                match open_root(&root) {
                    Ok(d) => self.dirs = vec![d],
                    Err(_) => return Some(self.revert(mode)),
                }
                self.root = root;
                self.names.clear();
                names = below;
            }
            first => {
                if first == Some(Component::RootDir) {
                    // The root of the current drive.
                    self.dirs.truncate(1);
                    self.names.clear();
                }
                for c in target.components() {
                    match c {
                        Component::Normal(n) => names.push(n.to_os_string()),
                        Component::ParentDir => names.push(OsString::from("..")),
                        _ => {}
                    }
                }
            }
        }
        self.queue.pop_front();
        for n in names.into_iter().rev() {
            self.queue.push_front(n);
        }
        None
    }

    /// Forgets the symlink being followed once its target is resolved.
    fn left_site(&mut self) {
        if self.site.as_ref().is_some_and(|s| self.queue.len() <= s.resume) {
            self.site = None;
        }
    }

    fn fail_with(&mut self, why: Stop, mode: Mode) -> Resolved {
        if self.site.is_some() { self.revert(mode) } else { self.stop(why, mode) }
    }

    /// Stops at the symlink being followed, whose target does not resolve.
    fn revert(&mut self, mode: Mode) -> Resolved {
        let Some(site) = self.site.take() else {
            let e = io::Error::new(io::ErrorKind::InvalidInput, "too many levels of symbolic links");
            return self.stop(Stop::from(e), mode);
        };
        self.dirs = site.dirs;
        self.root = site.root;
        self.names = site.names;
        self.queue = site.rest;
        let link = self.queue[0].clone();
        self.stop(Stop::Dangling(link), mode)
    }

    fn stop(&mut self, why: Stop, mode: Mode) -> Resolved {
        let mut r = self.finish(None, mode);
        r.stop = Some(why);
        r
    }

    /// Ends the walk when the target is the directory it reached.
    fn finish_here(&mut self, mode: Mode) -> Resolved {
        let mut r = self.finish(None, mode);
        r.entry = r.dirs.last().and_then(|d| d.metadata().ok()).map(|m| entry_meta(&m));
        r
    }

    fn finish(&mut self, entry: Option<(Meta, File)>, mode: Mode) -> Resolved {
        let dirs = std::mem::take(&mut self.dirs);
        let names = std::mem::take(&mut self.names);
        let walked = || {
            let mut p = self.root.clone();
            p.extend(&names);
            p
        };
        let dir_path = dirs.last().and_then(final_path).unwrap_or_else(walked);
        let (entry, entry_file) = entry.map_or((None, None), |(m, f)| (Some(m), Some(f)));
        Resolved {
            dirs,
            root: std::mem::take(&mut self.root),
            names,
            dir_path,
            rest: self.queue.drain(..).collect(),
            stop: None,
            exact: entry.is_none(),
            entry,
            entry_file,
            mode,
            work: std::mem::take(&mut self.work),
        }
    }
}

impl Resolved {
    /// The canonical path.
    pub fn path(&self) -> PathBuf {
        let mut p = self.dir_path.clone();
        p.extend(&self.rest);
        p
    }

    /// Whether the final name matches its on-disk casing.
    pub fn exact(&self) -> bool {
        self.exact
    }

    /// Replaces the final name with the name stored on disk, if the entry
    /// exists. For most entries that is the name returned for the
    /// entry's handle. For a link, the handle is labeled with the path
    /// used to open it, so the name is found by scanning the directory for an
    /// entry whose folded name matches.
    pub fn fix_spelling(&mut self) {
        if self.exact {
            return;
        }
        self.exact = true;
        let (Some(file), [name]) = (&self.entry_file, &self.rest[..]) else { return };
        let stored = match self.entry.map(|m| m.kind) {
            Some(EntryKind::Symlink) => self.dirs.last().and_then(|d| stored_by_scan(d, file, name, &mut self.work)),
            _ => final_path(file).and_then(|p| p.file_name().map(OsStr::to_os_string)),
        };
        if let Some(stored) = stored.filter(|s| fold_name(&s.to_string_lossy()) == fold_name(&name.to_string_lossy())) {
            self.rest[0] = stored;
        }
    }

    pub fn size(&self) -> Option<u64> {
        self.entry.map(|m| m.size)
    }

    /// The paths of the directories that do not exist above the final
    /// entry, outermost first.
    pub fn missing_ancestors(&self) -> Vec<PathBuf> {
        if !matches!(self.stop, Some(Stop::Io(io::ErrorKind::NotFound, _))) {
            return Vec::new();
        }
        let mut p = self.dir_path.clone();
        let mut missing = Vec::new();
        for name in &self.rest[..self.rest.len().saturating_sub(1)] {
            p.push(name);
            missing.push(p.clone());
        }
        missing
    }

    /// The directory holding the final entry and the final name, which is
    /// `None` if the target is the directory itself. Fails if the walk
    /// stopped early.
    fn at(&self) -> io::Result<(&File, Option<&OsStr>)> {
        match (&self.stop, self.dirs.last()) {
            (Some(why), _) => Err(why.error()),
            (None, Some(dir)) => Ok((dir, self.rest.first().map(OsString::as_os_str))),
            (None, None) => Err(io::ErrorKind::NotFound.into()),
        }
    }

    /// The directory and the final name, for operations on an entry in it.
    fn entry_at(&self) -> io::Result<(&File, &OsStr)> {
        match self.at()? {
            (dir, Some(name)) => Ok((dir, name)),
            (_, None) => Err(invalid(format_args!("{} is a root directory", self.path().display()))),
        }
    }

    pub fn open_read(self, path: &Path) -> io::Result<File> {
        self.open(path, OpenMode::Read)
    }

    pub fn open_write(self, path: &Path, append: bool) -> io::Result<File> {
        self.open(path, if append { OpenMode::Append } else { OpenMode::Write })
    }

    /// Opens the regular file for `mode`.
    pub fn open(self, path: &Path, mode: OpenMode) -> io::Result<File> {
        let (dir, Some(name)) = self.at()? else {
            return Err(not_regular(path));
        };
        if mode == OpenMode::Read {
            let f = open_in(dir, name, &options(FILE_GENERIC_READ, 0)).map_err(|e| nofollow_error(e, dir, name))?;
            return regular(path, name, f);
        }
        // Without FILE_WRITE_DATA, every write appends. Truncation happens
        // after the checks, because truncating on open would turn a symlink
        // into an empty regular file.
        let write = if mode == OpenMode::Append { FILE_GENERIC_WRITE & !FILE_WRITE_DATA } else { FILE_GENERIC_WRITE };
        let read = if mode.reads() { FILE_GENERIC_READ } else { FILE_READ_ATTRIBUTES };
        let mut o = options(write | read, FILE_NON_DIRECTORY_FILE);
        match mode {
            OpenMode::CreateNew => o.create_new(true),
            _ => o.create(mode.creates()),
        };
        let opened = open_in(dir, name, &o).map_err(|e| match nofollow_error(e, dir, name) {
            // FILE_NON_DIRECTORY_FILE fails with "access denied" for a directory.
            e if e.kind() == io::ErrorKind::PermissionDenied && lstat(dir, name).is_ok_and(|m| m.is_dir()) => {
                io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory", path.display()))
            }
            e => e,
        })?;
        let f = regular(path, name, opened)?;
        if mode.truncates() {
            f.set_len(0)?;
        }
        Ok(f)
    }

    /// Metadata of the final entry, as the walk found it. A final symlink is
    /// followed unless the mode is `Entry`.
    pub fn stat(&self) -> io::Result<EntryMeta> {
        let (_, name) = self.at()?;
        let m = self.entry.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if m.kind == EntryKind::Symlink && self.mode == Mode::Follow {
            return Err(unchecked_symlink(name.unwrap_or_default()));
        }
        Ok(EntryMeta { kind: m.kind, size: m.size, modified: m.modified.map(unix_nanos) })
    }

    /// The names of the entries of the directory, unsorted and without `.`
    /// and `..`. Their memory is reserved in `res`.
    pub fn list_dir(self, res: &mut Reservation) -> io::Result<Vec<String>> {
        let names = match self.at()? {
            (dir, None) => read_names(dir, res)?,
            (dir, Some(name)) => read_names(&open_subdir(dir, name, FILE_READ_ATTRIBUTES | SYNCHRONIZE)?, res)?,
        };
        Ok(names.into_iter().map(|n| n.to_string_lossy().into_owned()).collect())
    }

    pub fn create_dir(self) -> io::Result<()> {
        let (dir, name) = self.entry_at()?;
        mkdir(dir, name).map(drop)
    }

    /// Creates the final directory and the directories above it that were
    /// missing when the walk ended. It is not an error if the final
    /// directory exists.
    pub fn create_dir_all(self, path: &Path) -> io::Result<()> {
        let exists = || io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", path.display()));
        if let (Some(Stop::Io(io::ErrorKind::NotFound, _)), Some(dir)) = (&self.stop, self.dirs.last()) {
            let mut dir = dir.try_clone()?;
            for (i, name) in self.rest.iter().enumerate() {
                check_name(name)?;
                dir = match mkdir(&dir, name) {
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match open_subdir(&dir, name, FILE_READ_ATTRIBUTES | SYNCHRONIZE)
                    {
                        // As in `std::fs::create_dir_all`, an existing entry that is not a directory is an error.
                        Err(e) if i + 1 == self.rest.len() && e.raw_os_error() == Some(ERROR_DIRECTORY) => return Err(exists()),
                        r => r?,
                    },
                    r => r?,
                };
            }
            return Ok(());
        }
        let (dir, Some(name)) = self.at()? else {
            return Ok(());
        };
        match self.entry {
            Some(m) if m.kind == EntryKind::Dir => Ok(()),
            Some(_) => Err(exists()),
            None => match mkdir(dir, name) {
                Ok(_) => Ok(()),
                // Another process created it after the walk.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match lstat(dir, name) {
                    Ok(m) if m.file_type().is_symlink() => Err(unchecked_symlink(name)),
                    Ok(m) if m.is_dir() => Ok(()),
                    _ => Err(exists()),
                },
                Err(e) => Err(e),
            },
        }
    }

    /// Removes the file or symlink. A symlink's target is not removed.
    pub fn remove_file(self) -> io::Result<()> {
        let (dir, name) = self.entry_at()?;
        let shown = self.path();
        delete(dir, name, |m| {
            if m.is_dir() && !m.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory", shown.display())));
            }
            Ok(())
        })
    }

    pub fn remove_dir(self) -> io::Result<()> {
        let (dir, name) = self.entry_at()?;
        let shown = self.path();
        delete(dir, name, |m| {
            if !m.is_dir() || m.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", shown.display())));
            }
            Ok(())
        })
    }

    /// See [`super::Fs::walk_tree`]. Each directory is opened relative to
    /// its parent without following symlinks or junctions.
    pub fn walk_tree<E>(
        &self,
        res: &mut Reservation,
        remove: bool,
        mut visit: impl FnMut(&Path) -> Result<io::Result<()>, E>,
    ) -> Result<io::Result<()>, E> {
        macro_rules! io {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                }
            };
        }
        let (dir, name) = io!(self.entry_at());
        if lstat(dir, name).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
            return Ok(Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", self.path().display()))));
        }
        /// A directory being walked: its handle, its path, the names not
        /// yet visited, and its own name in its parent.
        struct Level {
            dir: File,
            path: PathBuf,
            names: Vec<OsString>,
            name: OsString,
        }
        let first = io!(open_subdir(dir, name, FILE_READ_ATTRIBUTES | SYNCHRONIZE));
        let names = io!(read_names(&first, res));
        let mut levels = vec![Level { dir: first, path: self.path(), names, name: name.to_os_string() }];
        while let Some(top) = levels.last_mut() {
            let Some(name) = top.names.pop() else {
                let done = levels.pop().expect("a level");
                drop(done.dir);
                if remove {
                    let parent = levels.last().map_or(dir, |l| &l.dir);
                    io!(delete(parent, &done.name, |_| Ok(())));
                }
                continue;
            };
            let path = top.path.join(&name);
            if let Err(e) = visit(&path)? {
                return Ok(Err(e));
            }
            let m = io!(lstat(&top.dir, &name));
            if m.is_dir() && !m.file_type().is_symlink() {
                let sub = io!(open_subdir(&top.dir, &name, FILE_READ_ATTRIBUTES | SYNCHRONIZE));
                let names = io!(read_names(&sub, res));
                levels.push(Level { dir: sub, path, names, name });
            } else if remove {
                io!(delete(&top.dir, &name, |_| Ok(())));
            }
        }
        Ok(Ok(()))
    }

    /// Renames the entry `from` to `to`. A final symlink is renamed itself.
    /// See the module documentation for how the paths are protected.
    pub fn rename(from: Resolved, to: Resolved) -> io::Result<()> {
        let (from_name, to_name) = (from.entry_at()?.1, to.entry_at()?.1);
        let from_held = from.hold()?;
        let to_held = to.hold()?;
        let (from_path, to_path) = (from.walked().join(from_name), to.walked().join(to_name));
        // A held directory cannot be replaced. Only an empty directory can
        // become a junction, so a held directory that contains another held
        // directory cannot. The two parents have no held directory inside
        // them, so they are checked right before and right after the rename.
        let unchanged = || -> io::Result<()> {
            for (held, path) in [(&from_held, &from_path), (&to_held, &to_path)] {
                let parent = held.last().ok_or_else(|| changed(path))?;
                if parent.as_file().metadata()?.file_type().is_symlink() {
                    return Err(changed(path));
                }
            }
            Ok(())
        };
        unchanged()?;
        std::fs::rename(&from_path, &to_path)?;
        unchanged()
    }

    /// The path the walk took to the last directory it holds.
    fn walked(&self) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(&self.names);
        p
    }

    /// Opens each directory of the walk again by path, without
    /// `FILE_SHARE_DELETE`, so that none can be renamed, deleted or replaced
    /// while the handles are open. Fails unless each one is the same
    /// directory the walk holds.
    fn hold(&self) -> io::Result<Vec<Handle>> {
        let lock = |p: &Path| {
            std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(p)
        };
        let mut path = self.root.clone();
        let mut held = Vec::with_capacity(self.dirs.len());
        for (i, walked) in self.dirs.iter().enumerate() {
            if i > 0 {
                path.push(&self.names[i - 1]);
            }
            let locked = Handle::from_file(lock(&path)?)?;
            if locked != Handle::from_file(walked.try_clone()?)? {
                return Err(changed(&path));
            }
            held.push(locked);
        }
        Ok(held)
    }
}

/// The stored name of the link `name` in `dir`, opened as `link`. It is
/// the name of the entry that folds to the same form and is the same link,
/// looked for among at most [`MAX_LINK_SCAN`] entries. Returns `None` if
/// `name` itself is in the directory or no match is found.
fn stored_by_scan(dir: &File, link: &File, name: &OsStr, work: &mut Work) -> Option<OsString> {
    let mut listed = listable(dir).ok()?;
    let link = Handle::from_file(link.try_clone().ok()?).ok()?;
    let folded = fold_name(&name.to_string_lossy()).into_owned();
    let mut found = None;
    for (i, entry) in fs_at::read_dir(&mut listed).ok()?.enumerate() {
        if i == MAX_LINK_SCAN {
            return None;
        }
        work.scanned += 1;
        let Ok(entry) = entry else { continue };
        let candidate = entry.name();
        work.scanned_bytes += candidate.len();
        if candidate == name {
            return None;
        }
        if found.is_none()
            && fold_name(&candidate.to_string_lossy()) == folded
            && open_entry(dir, candidate).and_then(Handle::from_file).is_ok_and(|h| h == link)
        {
            found = Some(candidate.to_os_string());
        }
    }
    found
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::super::is_unchecked_symlink;
    use super::*;
    use std::io::{Read, Write};

    /// Creates the junction `link` to the directory `target`. Unlike
    /// symlinks, junctions need no privilege.
    fn junction(link: &Path, target: &Path) {
        junction::create(target, link).unwrap();
    }

    /// The Unix fixture, with a junction `link` to `outside`. If the process
    /// may create symlinks, it also has a file symlink `real/link.txt`;
    /// the returned flag says whether it does.
    fn temp(tag: &str) -> (PathBuf, bool) {
        let dir = std::env::temp_dir().join(format!("sand-fs-win-{tag}-{}", std::process::id()));
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

    fn follow(p: &Path) -> Resolved {
        resolve(p, Mode::Follow)
    }

    fn entry(p: &Path) -> Resolved {
        resolve(p, Mode::Entry)
    }

    #[test]
    fn links_are_followed_by_the_walk_and_never_by_operations() {
        let (dir, file_link) = temp("follow");
        let mut s = String::new();
        follow(&dir.join("real/file.txt")).open_read(Path::new("x")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "inside");
        // The walk resolves junctions and symlinks, so the policy sees where they lead.
        assert_eq!(follow(&dir.join("link/file.txt")).path(), dir.join("outside/file.txt"));
        assert_eq!(follow(&dir.join("link/new.txt")).path(), dir.join("outside/new.txt"));
        assert_eq!(entry(&dir.join("link")).path(), dir.join("link"));
        assert_eq!(entry(&dir.join("link")).stat().unwrap().kind, EntryKind::Symlink);
        if file_link {
            assert_eq!(follow(&dir.join("real/link.txt")).path(), dir.join("outside/file.txt"));
            assert_eq!(entry(&dir.join("real/link.txt")).path(), dir.join("real/link.txt"));
        }
        // Names match their on-disk names.
        let mut r = follow(&dir.join("REAL/FILE.TXT"));
        r.fix_spelling();
        assert_eq!(r.path(), dir.join("real/file.txt"));
        // A junction that replaces the final entry after the walk is not followed.
        let r = follow(&dir.join("real/sub"));
        junction(&dir.join("real/sub"), &dir.join("outside"));
        assert!(r.list_dir(&mut Reservation::new(&crate::vm::iomem::IoMemory::new(None, usize::MAX))).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dangling_links_and_missing_components() {
        let (dir, _) = temp("dangling");
        junction(&dir.join("real/dangling"), &dir.join("outside/missing"));
        let r = follow(&dir.join("real/dangling/x"));
        assert_eq!(r.path(), dir.join("real/dangling/x"));
        assert!(is_unchecked_symlink(&r.stat().unwrap_err()));
        let r = follow(&dir.join("real/x/y/z"));
        assert_eq!(r.missing_ancestors(), [dir.join("real/x"), dir.join("real/x/y")]);
        follow(&dir.join("real/x/y/z")).create_dir_all(Path::new("x")).unwrap();
        assert!(dir.join("real/x/y/z").is_dir());
        assert!(!dir.join("outside/missing").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regular_files_and_directories() {
        let (dir, _) = temp("ops");
        follow(&dir.join("real/new.txt")).open_write(Path::new("x"), false).unwrap().write_all(b"one").unwrap();
        follow(&dir.join("real/new.txt")).open_write(Path::new("x"), true).unwrap().write_all(b"two").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/new.txt")).unwrap(), "onetwo");
        let m = follow(&dir.join("real/new.txt")).stat().unwrap();
        assert_eq!((m.kind, m.size), (EntryKind::File, 6));
        follow(&dir.join("real/a")).create_dir().unwrap();
        follow(&dir.join("real/a/b")).create_dir().unwrap();
        let mem = crate::vm::iomem::IoMemory::new(None, usize::MAX);
        assert_eq!(follow(&dir.join("real/a")).list_dir(&mut Reservation::new(&mem)).unwrap(), ["b"]);
        assert_eq!(entry(&dir.join("real/a")).remove_dir().unwrap_err().kind(), io::ErrorKind::DirectoryNotEmpty);
        entry(&dir.join("real/a/b")).remove_dir().unwrap();
        Resolved::rename(entry(&dir.join("real/a")), entry(&dir.join("real/z"))).unwrap();
        assert!(dir.join("real/z").is_dir());
        assert!(is_unchecked_symlink(&Resolved::rename(entry(&dir.join("link/file.txt")), entry(&dir.join("stolen.txt"))).unwrap_err()));
        entry(&dir.join("real/new.txt")).remove_file().unwrap();
        assert!(!dir.join("real/new.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Handle-relative opens take names literally, so names that Win32 path
    /// functions would change, and the policy would not see, are denied.
    #[test]
    fn names_win32_treats_specially_are_denied() {
        let (dir, _) = temp("names");
        for name in ["x.", "x ", "file.txt:stream", "file.txt::$DATA", "CON", "nul.txt", "Com1", "LPT9.log", "aux "] {
            let e = follow(&dir.join("real").join(name)).open_write(Path::new("x"), false).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{name}: {e}");
        }
        for name in ["console", "COM10", "x.y", "LPT"] {
            follow(&dir.join("real").join(name)).open_write(Path::new("x"), false).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
