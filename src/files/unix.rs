//! The host's file system on Unix: one walk per operation.
//!
//! [`resolve`] walks an absolute, lexically normal path from the root,
//! opening directories without following symlinks (`O_NOFOLLOW`, or several
//! components at once with `O_NOFOLLOW_ANY` on macOS and `openat2` with
//! `RESOLVE_NO_SYMLINKS` on Linux). A symlink met on the way is read and its
//! target spliced into the rest of the path, as `realpath` does. The walk
//! ends holding the directory containing the final entry ([`Resolved`]),
//! with the canonical path the policy decides on: the names walked, or on
//! macOS the kernel's path of the held directory (`F_GETPATH`), which spells
//! directories as stored and resolves firmlinks.
//!
//! The operation then acts on the final name relative to the held
//! directory, never following a final symlink, so it reaches exactly the
//! checked entry: the directories are held, and a symlink that replaces the
//! final entry fails with [`unchecked_symlink`].
//!
//! On macOS, and for a symlink entry itself, an existing final entry may be
//! stored under another spelling (`data/SECRETS` for `data/secrets`);
//! [`Resolved::fix_spelling`] looks it up on demand.

use super::{EntryKind, EntryMeta, Mode, OpenMode, Work, unchecked_symlink};
use crate::policy::{MAX_LINK_SCAN, fold_name};
use crate::vm::iomem::Reservation;
use rustix::fs::{AtFlags, FileType, Mode as Perm, OFlags, Stat};
use rustix::io::Errno;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// At most this many symlinks are followed in one path (Linux's limit).
const MAX_LINKS: usize = 40;

/// Bytes counted per directory entry besides its name.
const ENTRY_OVERHEAD: usize = 2 * std::mem::size_of::<OsString>();

/// Flags for directories that are only traversed (`O_PATH` needs no read
/// permission; `O_SEARCH` is added on macOS by [`open_dir_at`]).
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
const TRAVERSE: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
const TRAVERSE: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

/// The most directories a recursive removal holds open at once.
const OPEN_LEVELS: usize = 16;

/// Flags for opening the final entry as a file (non-blocking, so that
/// opening a FIFO cannot hang; it is rejected afterwards).
const FILE: OFlags = OFlags::NOFOLLOW.union(OFlags::CLOEXEC).union(OFlags::NOCTTY).union(OFlags::NONBLOCK);

/// Opens the directory `path` relative to `dir` with `flags`; on macOS
/// with `O_SEARCH` (lookups only: no read permission needed, and much
/// cheaper than a readable open where the system checks every open).
#[cfg(target_vendor = "apple")]
fn open_dir_at(dir: BorrowedFd<'_>, path: &OsStr, flags: OFlags) -> Result<OwnedFd, Errno> {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// `O_EXEC`, which with `O_DIRECTORY` is `O_SEARCH` (macOS 13+; rustix does not name it).
    const SEARCH: OFlags = OFlags::from_bits_retain(0x4000_0000);
    static NO_SEARCH: AtomicBool = AtomicBool::new(false);
    if !NO_SEARCH.load(Ordering::Relaxed) {
        match rustix::fs::openat(dir, path, flags | SEARCH, Perm::empty()) {
            // An older kernel without O_SEARCH: read permission is needed then.
            Err(Errno::INVAL) => NO_SEARCH.store(true, Ordering::Relaxed),
            r => return r,
        }
    }
    rustix::fs::openat(dir, path, flags, Perm::empty())
}

#[cfg(not(target_vendor = "apple"))]
fn open_dir_at(dir: BorrowedFd<'_>, path: &OsStr, flags: OFlags) -> Result<OwnedFd, Errno> {
    rustix::fs::openat(dir, path, flags, Perm::empty())
}

/// Opens the directory `path` (several components) relative to `dir`
/// without following a symlink in any of them: `None` if the system cannot.
#[cfg(target_vendor = "apple")]
fn open_dirs_at(dir: BorrowedFd<'_>, path: &OsStr) -> Option<Result<OwnedFd, Errno>> {
    // O_NOFOLLOW_ANY excludes O_NOFOLLOW.
    const FLAGS: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC).union(OFlags::NOFOLLOW_ANY);
    nofollow_any().then(|| open_dir_at(dir, path, FLAGS))
}

/// Whether `O_NOFOLLOW_ANY` (macOS 11+) is honoured: `/var` is a symlink.
#[cfg(target_vendor = "apple")]
pub(super) fn nofollow_any() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW_ANY;
        rustix::fs::open("/var", flags, Perm::empty()).err() == Some(Errno::LOOP)
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_dirs_at(dir: BorrowedFd<'_>, path: &OsStr) -> Option<Result<OwnedFd, Errno>> {
    use rustix::fs::ResolveFlags;
    use std::sync::atomic::{AtomicBool, Ordering};
    // `openat2` (Linux 5.6+) may be missing or filtered (seccomp).
    static UNSUPPORTED: AtomicBool = AtomicBool::new(false);
    if UNSUPPORTED.load(Ordering::Relaxed) {
        return None;
    }
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    match rustix::fs::openat2(dir, path, flags, Perm::empty(), ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS) {
        Err(Errno::NOSYS | Errno::PERM | Errno::INVAL) => {
            UNSUPPORTED.store(true, Ordering::Relaxed);
            None
        }
        r => Some(r),
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn open_dirs_at(_: BorrowedFd<'_>, _: &OsStr) -> Option<Result<OwnedFd, Errno>> {
    None
}

/// The longest path opened at once (`PATH_MAX` is 1024 bytes on macOS).
const MAX_CHUNK_BYTES: usize = if cfg!(target_vendor = "apple") { 1000 } else { 4000 };

/// Converts an error of a no-follow operation on `name` in `dir`: a
/// symlink (reported as ELOOP, EMLINK on FreeBSD, or ENOTDIR with
/// O_DIRECTORY) becomes [`unchecked_symlink`].
fn nofollow_error(e: Errno, dir: impl AsFd, name: &OsStr) -> io::Error {
    if matches!(e, Errno::LOOP | Errno::MLINK | Errno::NOTDIR)
        && rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|st| FileType::from_raw_mode(st.st_mode as _).is_symlink())
    {
        return unchecked_symlink(name);
    }
    e.into()
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display()))
}

fn file_type(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode as _)
}

/// Reads the entries of the directory `dir` (which may be open for
/// lookups only: `Dir::read_from` would reopen it with the same flags).
fn read_dir(dir: &OwnedFd) -> io::Result<rustix::fs::Dir> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::Dir::new(rustix::fs::openat(dir, ".", flags, Perm::empty())?)?)
}

/// Whether two `stat` results are of the same file.
fn same_file(a: &Stat, b: &Stat) -> bool {
    (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino)
}

/// Why a walk stopped before the final entry's directory.
enum Stop {
    /// A component could not be opened (`NOENT`: it does not exist).
    Errno(Errno),
    /// The symlink of this name (in the original path) does not resolve:
    /// its target, or a component after it, does not exist. Operations
    /// deny it, as one that would follow a symlink the policy did not see.
    Dangling(OsString),
}

/// A host path after [`resolve`]: the directory containing the final
/// entry, held open, and the names below it.
pub(crate) struct Resolved {
    /// The deepest directory reached: the one containing the final entry,
    /// unless the walk stopped early (`None` if not even the root opened).
    dir: Option<OwnedFd>,
    /// The canonical path of `dir`.
    dir_path: PathBuf,
    /// The names below `dir`: the final name; with a `stop`, every name from
    /// the one that could not be opened; empty if the target is `dir` itself.
    rest: Vec<OsString>,
    stop: Option<Stop>,
    /// The final entry (a final symlink not followed), if it exists.
    entry: Option<Stat>,
    /// Whether the final name is spelled as stored (see [`Resolved::fix_spelling`]).
    exact: bool,
    /// The final entry opened for reading while its spelling was fixed.
    file: Option<OwnedFd>,
    mode: Mode,
    pub work: Work,
}

/// The directory a walk is in: the root, or an open directory.
struct Here(Option<OwnedFd>);

impl Here {
    fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_ref().map_or(rustix::fs::CWD, |f| f.as_fd())
    }

    /// `rel` as a path from here (absolute at the root, where there is no
    /// directory to be relative to).
    fn path(&self, rel: &OsStr) -> OsString {
        if self.0.is_some() {
            return rel.to_os_string();
        }
        let mut p = OsString::with_capacity(rel.len() + 1);
        p.push("/");
        p.push(rel);
        p
    }

    fn open(self) -> io::Result<OwnedFd> {
        match self.0 {
            Some(fd) => Ok(fd),
            None => Ok(open_dir_at(rustix::fs::CWD, OsStr::new("/"), TRAVERSE)?),
        }
    }
}

/// Where the outermost symlink being followed is, to return there if its
/// target does not resolve.
struct Site {
    here: Here,
    names: Vec<OsString>,
    /// The link's name and the original names after it.
    rest: Vec<OsString>,
    /// The link is resolved once the queue is this short again.
    resume: usize,
}

struct Walk {
    here: Here,
    /// The names from the root to `here`.
    names: Vec<OsString>,
    /// The names still to resolve (`..` only from symlink targets).
    queue: VecDeque<OsString>,
    links: usize,
    site: Option<Site>,
    /// The number of leading names of `queue` known not to open at once,
    /// and whether opening just the first was tried since.
    fail: Option<usize>,
    tried_one: bool,
    work: Work,
}

/// Resolves the absolute, lexically normal `path` (`..` and `.` are not
/// expected; a relative path is made absolute against the process's
/// working directory). With `mode` `Entry`, a final symlink is not followed.
pub(crate) fn resolve(path: &Path, mode: Mode) -> Resolved {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::path::absolute(path).unwrap_or_else(|_| Path::new("/").join(path));
        &absolute
    };
    let mut queue = VecDeque::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => queue.push_back(n.to_os_string()),
            Component::ParentDir => queue.push_back(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let walk =
        Walk { here: Here(None), names: Vec::new(), queue, links: 0, site: None, fail: None, tried_one: false, work: Work::default() };
    walk.run(mode)
}

impl Walk {
    fn run(mut self, mode: Mode) -> Resolved {
        loop {
            let Some(first) = self.queue.front() else {
                return self.finish_here(mode);
            };
            if first.as_bytes() == b".." {
                if let Err(e) = self.up() {
                    return self.fail_with(e, mode);
                }
                continue;
            }
            if self.queue.len() == 1 {
                if let Some(r) = self.last(mode) {
                    return r;
                }
                continue;
            }
            if let Some(r) = self.step(mode) {
                return r;
            }
        }
    }

    /// Opens one or more directories from the front of the queue.
    fn step(&mut self, mode: Mode) -> Option<Resolved> {
        // Names before the final one, up to a `..`.
        let run = self.queue.iter().take(self.queue.len() - 1).take_while(|n| n.as_bytes() != b"..").count();
        let k = match self.fail {
            None => {
                let mut bytes = 0;
                self.queue
                    .iter()
                    .take(run)
                    .take_while(|n| {
                        bytes += n.len() + 1;
                        bytes <= MAX_CHUNK_BYTES
                    })
                    .count()
                    .max(1)
            }
            // Most symlinks are near the root (`/tmp`, `/var` on macOS):
            // try the first name alone, then halve.
            Some(_) if !self.tried_one => 1,
            Some(f) => (f / 2).max(1),
        };
        let opened = if k == 1 {
            None
        } else {
            let mut chunk = OsString::new();
            for (i, n) in self.queue.iter().take(k).enumerate() {
                if i > 0 {
                    chunk.push("/");
                }
                chunk.push(n);
            }
            open_dirs_at(self.here.fd(), &self.here.path(&chunk))
        };
        let (k, opened) = match opened {
            Some(r) => (k, r),
            // One name at a time.
            None => {
                let name = self.here.path(&self.queue[0]);
                (1, open_dir_at(self.here.fd(), &name, TRAVERSE))
            }
        };
        match opened {
            Ok(fd) => {
                self.here = Here(Some(fd));
                self.names.extend(self.queue.drain(..k));
                self.work.steps += k;
                if k == 1 && self.fail.is_some() {
                    self.tried_one = true;
                }
                self.fail = self.fail.map(|f| f.saturating_sub(k)).filter(|&f| f > 0);
                self.left_site();
                None
            }
            Err(_) if k > 1 => {
                self.fail = Some(k);
                None
            }
            Err(e) => self.not_a_dir(e, mode),
        }
    }

    /// The first name of the queue (not the final one) cannot be opened as
    /// a directory: follows it if it is a symlink, or stops.
    fn not_a_dir(&mut self, e: Errno, mode: Mode) -> Option<Resolved> {
        self.fail = None;
        self.tried_one = false;
        if e != Errno::NOENT {
            let name = self.here.path(&self.queue[0]);
            if let Ok(target) = rustix::fs::readlinkat(self.here.fd(), &name, Vec::new()) {
                return self.follow(target.into_bytes(), mode);
            }
        }
        if let Some(site) = &self.site {
            // A symlink whose target is not a directory resolves (the next
            // name then fails to open in it); one whose target is missing
            // does not.
            if e == Errno::NOTDIR && self.queue.len() == site.resume + 1 {
                self.site = None;
            } else {
                return Some(self.revert(mode));
            }
        }
        Some(self.stop(Stop::Errno(e), mode))
    }

    /// The final name.
    fn last(&mut self, mode: Mode) -> Option<Resolved> {
        let name = self.here.path(&self.queue[0]);
        match rustix::fs::statat(self.here.fd(), &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if mode == Mode::Follow && file_type(&st).is_symlink() => {
                match rustix::fs::readlinkat(self.here.fd(), &name, Vec::new()) {
                    Ok(target) => self.follow(target.into_bytes(), mode),
                    Err(e) => Some(self.fail_with(e, mode)),
                }
            }
            Ok(st) => {
                self.site = None;
                self.work.steps += 1;
                let exact = !may_be_spelled_differently(file_type(&st), mode);
                Some(self.finish(Some(st), exact, mode))
            }
            Err(e) if self.site.is_some() => {
                let _ = e;
                Some(self.revert(mode))
            }
            Err(Errno::NOENT) => Some(self.finish(None, true, mode)),
            Err(e) => Some(self.stop(Stop::Errno(e), mode)),
        }
    }

    /// Replaces the symlink at the front of the queue by its target.
    fn follow(&mut self, target: Vec<u8>, mode: Mode) -> Option<Resolved> {
        self.links += 1;
        self.work.steps += 1;
        self.work.bytes += target.len();
        if self.site.is_none() {
            let here = match &self.here.0 {
                Some(fd) => match fd.try_clone() {
                    Ok(fd) => Here(Some(fd)),
                    Err(e) => return Some(self.stop(Stop::Errno(Errno::from_io_error(&e).unwrap_or(Errno::IO)), mode)),
                },
                None => Here(None),
            };
            let rest = self.queue.iter().cloned().collect();
            self.site = Some(Site { here, names: self.names.clone(), rest, resume: self.queue.len() - 1 });
        }
        if self.links > MAX_LINKS || target.is_empty() {
            return Some(self.revert(mode));
        }
        self.queue.pop_front();
        let target = PathBuf::from(OsString::from_vec(target));
        let mut names = Vec::new();
        for c in target.components() {
            match c {
                Component::RootDir => {
                    self.here = Here(None);
                    self.names.clear();
                }
                Component::Normal(n) => names.push(n.to_os_string()),
                Component::ParentDir => names.push(OsString::from("..")),
                Component::CurDir | Component::Prefix(_) => {}
            }
        }
        for n in names.into_iter().rev() {
            self.queue.push_front(n);
        }
        self.fail = None;
        self.tried_one = false;
        None
    }

    /// `..` (from a symlink's target): the parent of the directory reached,
    /// physically (the root's parent is the root).
    fn up(&mut self) -> Result<(), Errno> {
        self.work.steps += 1;
        if self.names.pop().is_some() {
            let fd = open_dir_at(self.here.fd(), OsStr::new(".."), TRAVERSE)?;
            self.here = Here(Some(fd));
        }
        self.queue.pop_front();
        self.left_site();
        Ok(())
    }

    /// Forgets the symlink being followed once its target is resolved.
    fn left_site(&mut self) {
        if self.site.as_ref().is_some_and(|s| self.queue.len() <= s.resume) {
            self.site = None;
        }
    }

    fn fail_with(&mut self, e: Errno, mode: Mode) -> Resolved {
        if self.site.is_some() { self.revert(mode) } else { self.stop(Stop::Errno(e), mode) }
    }

    /// Stops at the symlink being followed, whose target does not resolve.
    fn revert(&mut self, mode: Mode) -> Resolved {
        let Some(site) = self.site.take() else {
            return self.stop(Stop::Errno(Errno::LOOP), mode);
        };
        self.here = site.here;
        self.names = site.names;
        self.queue = site.rest.into();
        let link = self.queue[0].clone();
        self.stop(Stop::Dangling(link), mode)
    }

    fn stop(&mut self, why: Stop, mode: Mode) -> Resolved {
        let mut r = self.finish(None, true, mode);
        r.stop = Some(why);
        r
    }

    /// The target is the directory reached itself (the root, or a symlink
    /// whose target ends in `..`).
    fn finish_here(&mut self, mode: Mode) -> Resolved {
        let mut r = self.finish(None, true, mode);
        r.entry = r.dir.as_ref().and_then(|d| rustix::fs::fstat(d).ok());
        r
    }

    fn finish(&mut self, entry: Option<Stat>, exact: bool, mode: Mode) -> Resolved {
        let here = std::mem::replace(&mut self.here, Here(None));
        let rest: Vec<OsString> = self.queue.drain(..).collect();
        let work = std::mem::take(&mut self.work);
        match here.open() {
            Ok(dir) => {
                let dir_path = dir_path(&dir, &self.names);
                Resolved { dir: Some(dir), dir_path, rest, stop: None, entry, exact, file: None, mode, work }
            }
            // The root cannot be opened.
            Err(e) => {
                let stop = Some(Stop::Errno(Errno::from_io_error(&e).unwrap_or(Errno::IO)));
                let rest = std::mem::take(&mut self.names).into_iter().chain(rest).collect();
                Resolved { dir: None, dir_path: PathBuf::from("/"), rest, stop, entry: None, exact: true, file: None, mode, work }
            }
        }
    }
}

/// Whether the stored name of an existing entry may differ from the name
/// that found it. On macOS volumes are usually case- and always
/// normalization-insensitive. Elsewhere names are kept as given (like
/// `realpath`), except that a symlink entry itself is looked up as stored
/// (case-insensitive directories exist on Linux too).
fn may_be_spelled_differently(t: FileType, mode: Mode) -> bool {
    cfg!(target_vendor = "apple") || (t.is_symlink() && mode == Mode::Entry)
}

/// The canonical path of the directory `dir`, reached through `names`.
#[cfg(target_vendor = "apple")]
fn dir_path(dir: &OwnedFd, names: &[OsString]) -> PathBuf {
    match rustix::fs::getpath(dir) {
        Ok(p) if p.as_bytes().first() == Some(&b'/') => PathBuf::from(OsString::from_vec(p.into_bytes())),
        // Longer than `MAXPATHLEN`, or deleted: the names walked.
        _ => names_path(names),
    }
}

#[cfg(not(target_vendor = "apple"))]
fn dir_path(_: &OwnedFd, names: &[OsString]) -> PathBuf {
    names_path(names)
}

fn names_path(names: &[OsString]) -> PathBuf {
    let mut p = PathBuf::from("/");
    p.extend(names);
    p
}

/// The stored name of the entry `name` (`entry`) of `dir` (whose path is
/// `dir_path`), from the kernel's path of the entry opened without
/// following it; and the entry opened for reading if it is a regular file.
#[cfg(target_vendor = "apple")]
fn stored_by_path(dir: &OwnedFd, dir_path: &Path, entry: &Stat, name: &OsStr) -> Option<(OsString, Option<OwnedFd>)> {
    let t = file_type(entry);
    let fd = if t.is_dir() {
        open_dir_at(dir.as_fd(), name, TRAVERSE).ok()?
    } else if t.is_file() {
        rustix::fs::openat(dir, name, OFlags::RDONLY | FILE, Perm::empty()).ok()?
    } else if t.is_symlink() {
        let flags = OFlags::RDONLY | OFlags::SYMLINK | OFlags::CLOEXEC | OFlags::NONBLOCK;
        rustix::fs::openat(dir, name, flags, Perm::empty()).ok()?
    } else {
        // Opening a device may have effects.
        return None;
    };
    let st = rustix::fs::fstat(&fd).ok()?;
    if !same_file(&st, entry) || file_type(&st) != t {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(rustix::fs::getpath(&fd).ok()?.into_bytes()));
    // A file with several hard links is named by the one last looked up.
    let stored = path.file_name()?;
    if path.parent() != Some(dir_path) || fold_name(&stored.to_string_lossy()) != fold_name(&name.to_string_lossy()) {
        return None;
    }
    Some((stored.to_os_string(), t.is_file().then_some(fd)))
}

/// The stored name of the entry `name` (`entry`) of `dir`: the directory
/// entry whose name folds the same and that is the same file, looked for
/// among at most [`MAX_LINK_SCAN`] entries (an exact match preferred);
/// `None` if `name` itself is there, or nothing is found.
fn stored_by_scan(dir: &OwnedFd, entry: &Stat, name: &OsStr, work: &mut Work) -> Option<OsString> {
    let entries = read_dir(dir).ok()?;
    let folded = fold_name(&name.to_string_lossy()).into_owned();
    let mut found = None;
    for (i, e) in entries.enumerate() {
        if i == MAX_LINK_SCAN {
            return None;
        }
        work.scanned += 1;
        let Ok(e) = e else { continue };
        let candidate = OsStr::from_bytes(e.file_name().to_bytes());
        work.scanned_bytes += candidate.len();
        if candidate == name {
            return None;
        }
        if found.is_none()
            && candidate != "."
            && candidate != ".."
            && fold_name(&candidate.to_string_lossy()) == folded
            && rustix::fs::statat(dir, candidate, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|st| same_file(&st, entry))
        {
            found = Some(candidate.to_os_string());
        }
    }
    found
}

impl Resolved {
    /// The canonical path.
    pub fn path(&self) -> PathBuf {
        let mut p = self.dir_path.clone();
        p.extend(&self.rest);
        p
    }

    /// Whether the final name is spelled as stored (or does not exist).
    pub fn exact(&self) -> bool {
        self.exact
    }

    /// The size of the final entry (`None` if there is none).
    pub fn size(&self) -> Option<u64> {
        self.entry.as_ref().map(|st| st.st_size as u64)
    }

    /// The paths of the directories that do not exist above the final
    /// entry, outermost first.
    pub fn missing_ancestors(&self) -> Vec<PathBuf> {
        if !matches!(self.stop, Some(Stop::Errno(Errno::NOENT))) {
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

    /// Names an existing final entry as stored (see the module
    /// documentation), unless it is already.
    pub fn fix_spelling(&mut self) {
        if self.exact {
            return;
        }
        self.exact = true;
        let (Some(entry), [name], Some(dir)) = (self.entry, &self.rest[..], &self.dir) else {
            return;
        };
        #[cfg(target_vendor = "apple")]
        if let Some((stored, file)) = stored_by_path(dir, &self.dir_path, &entry, name) {
            self.rest[0] = stored;
            self.file = file;
            return;
        }
        if let Some(stored) = stored_by_scan(dir, &entry, name, &mut self.work) {
            self.rest[0] = stored;
        }
    }

    /// Fails if the walk stopped early; otherwise the directory and the
    /// final name in it (`None` if the target is the directory itself).
    fn at(&self) -> io::Result<(&OwnedFd, Option<&OsStr>)> {
        match (&self.stop, &self.dir) {
            (Some(Stop::Errno(e)), _) => Err((*e).into()),
            (Some(Stop::Dangling(link)), _) => Err(unchecked_symlink(link)),
            (None, Some(dir)) => Ok((dir, self.rest.first().map(OsString::as_os_str))),
            (None, None) => Err(Errno::NOENT.into()),
        }
    }

    /// The directory and the final name, for operations on an entry of it.
    fn entry_at(&self) -> io::Result<(&OwnedFd, &OsStr)> {
        match self.at()? {
            (dir, Some(name)) => Ok((dir, name)),
            // The root directory.
            (_, None) => Err(Errno::INVAL.into()),
        }
    }

    /// Opens the regular file for reading.
    pub fn open_read(mut self, path: &Path) -> io::Result<File> {
        if let Some(fd) = self.file.take() {
            self.at()?;
            return Ok(File::from(fd));
        }
        let (dir, Some(name)) = self.at()? else {
            return Err(not_regular(path));
        };
        let fd = rustix::fs::openat(dir, name, OFlags::RDONLY | FILE, Perm::empty()).map_err(|e| nofollow_error(e, dir, name))?;
        if !file_type(&rustix::fs::fstat(&fd)?).is_file() {
            return Err(not_regular(path));
        }
        Ok(File::from(fd))
    }

    /// Opens a regular file for writing, creating it if needed; truncates it
    /// unless `append`.
    pub fn open_write(self, path: &Path, append: bool) -> io::Result<File> {
        self.open(path, if append { OpenMode::Append } else { OpenMode::Write })
    }

    /// Opens the regular file for `mode`.
    pub fn open(self, path: &Path, mode: OpenMode) -> io::Result<File> {
        if mode == OpenMode::Read {
            return self.open_read(path);
        }
        let (dir, Some(name)) = self.at()? else {
            return Err(Errno::ISDIR.into());
        };
        let flags = FILE
            | match mode {
                OpenMode::Read => OFlags::RDONLY,
                OpenMode::Write => OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
                OpenMode::Append => OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND,
                OpenMode::CreateNew => OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                OpenMode::ReadWrite => OFlags::RDWR,
            };
        let fd = rustix::fs::openat(dir, name, flags, Perm::from_raw_mode(0o666)).map_err(|e| nofollow_error(e, dir, name))?;
        if !file_type(&rustix::fs::fstat(&fd)?).is_file() {
            return Err(not_regular(path));
        }
        Ok(File::from(fd))
    }

    /// Metadata of the final entry, as the walk found it (a final symlink
    /// followed unless the mode is `Entry`).
    pub fn stat(&self) -> io::Result<EntryMeta> {
        let (_, name) = self.at()?;
        let st = self.entry.ok_or_else(|| io::Error::from(Errno::NOENT))?;
        let t = file_type(&st);
        let kind = if t.is_file() {
            EntryKind::File
        } else if t.is_dir() {
            EntryKind::Dir
        } else if t.is_symlink() {
            if self.mode == Mode::Follow {
                // The walk follows them.
                return Err(unchecked_symlink(name.unwrap_or_default()));
            }
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        // The field types differ between platforms.
        #[allow(clippy::unnecessary_cast)]
        let (secs, nanos) = (st.st_mtime as i64, st.st_mtime_nsec as i64);
        let modified = secs.checked_mul(1_000_000_000).and_then(|n| n.checked_add(nanos));
        Ok(EntryMeta { kind, size: st.st_size as u64, modified })
    }

    /// The names of the entries of the directory (unsorted, without `.` and
    /// `..`), reserving their memory in `res`.
    pub fn list_dir(self, res: &mut Reservation) -> io::Result<Vec<String>> {
        let entries = match self.at()? {
            (dir, None) => read_dir(dir)?,
            (dir, Some(name)) => {
                let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                rustix::fs::Dir::new(rustix::fs::openat(dir, name, flags, Perm::empty()).map_err(|e| nofollow_error(e, dir, name))?)?
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                res.grow(name.len() + ENTRY_OVERHEAD)?;
                names.push(OsStr::from_bytes(name).to_string_lossy().into_owned());
            }
        }
        Ok(names)
    }

    pub fn create_dir(self) -> io::Result<()> {
        let (dir, Some(name)) = self.at()? else {
            return Err(Errno::EXIST.into());
        };
        Ok(rustix::fs::mkdirat(dir, name, Perm::from_raw_mode(0o777))?)
    }

    /// Creates the final directory and the missing ones above it (those
    /// that were missing when the walk ended); an existing directory is fine.
    pub fn create_dir_all(self, path: &Path) -> io::Result<()> {
        let exists = || io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", path.display()));
        if let (Some(Stop::Errno(Errno::NOENT)), Some(dir)) = (&self.stop, &self.dir) {
            let mut dir = dir.try_clone()?;
            for (i, name) in self.rest.iter().enumerate() {
                match rustix::fs::mkdirat(&dir, name, Perm::from_raw_mode(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
                dir = match open_dir_at(dir.as_fd(), name, TRAVERSE) {
                    Ok(d) => d,
                    Err(e) => match nofollow_error(e, &dir, name) {
                        // Like `std::fs::create_dir_all`: something other than a directory is in the way.
                        e if i + 1 == self.rest.len() && e.kind() == io::ErrorKind::NotADirectory => return Err(exists()),
                        e => return Err(e),
                    },
                };
            }
            return Ok(());
        }
        let (dir, Some(name)) = self.at()? else {
            return Ok(());
        };
        match self.entry {
            Some(st) if file_type(&st).is_dir() => Ok(()),
            Some(_) => Err(exists()),
            None => match rustix::fs::mkdirat(dir, name, Perm::from_raw_mode(0o777)) {
                Ok(()) => Ok(()),
                // Created meanwhile.
                Err(Errno::EXIST) => match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(st) if file_type(&st).is_dir() => Ok(()),
                    Ok(st) if file_type(&st).is_symlink() => Err(unchecked_symlink(name)),
                    _ => Err(exists()),
                },
                Err(e) => Err(e.into()),
            },
        }
    }

    /// Removes the file or symlink (not a symlink's target).
    pub fn remove_file(self) -> io::Result<()> {
        let (dir, name) = self.entry_at()?;
        Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?)
    }

    pub fn remove_dir(self) -> io::Result<()> {
        let (dir, name) = self.entry_at()?;
        Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
    }

    /// Removes the directory entry and everything below it, opening each
    /// directory relative to its parent without following symlinks. `check`
    /// is asked about each entry below (by its stored path) before it is
    /// removed; its error stops the removal. The names read are reserved in
    /// `res`.
    pub fn remove_tree<E>(
        self,
        res: &mut Reservation,
        mut check: impl FnMut(&Path) -> Result<io::Result<()>, E>,
    ) -> Result<io::Result<()>, E> {
        macro_rules! io {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e.into())),
                }
            };
        }
        let (dir, name) = io!(self.entry_at());
        if !file_type(&io!(rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW))).is_dir() {
            return Ok(Err(Errno::NOTDIR.into()));
        }
        /// A directory being emptied: its path, the names left to remove
        /// and its own name in its parent, and while it is held open, its
        /// descriptor.
        struct Level {
            fd: Option<OwnedFd>,
            path: PathBuf,
            names: Vec<OsString>,
            name: OsString,
        }
        fn open(at: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            rustix::fs::openat(at, name, flags, Perm::empty()).map_err(|e| nofollow_error(e, at, name))
        }
        /// Opens level `i` again if it was closed, by name from its nearest
        /// open ancestor (the first level always is).
        fn reopen(levels: &mut [Level], i: usize) -> io::Result<()> {
            let from = (0..=i).rev().find(|&j| levels[j].fd.is_some()).expect("the first level stays open");
            let mut fd: Option<OwnedFd> = None;
            for k in from + 1..=i {
                let at = fd.as_ref().or(levels[from].fd.as_ref()).expect("open");
                fd = Some(open(at, &levels[k].name)?);
            }
            if fd.is_some() {
                levels[i].fd = fd;
            }
            Ok(())
        }
        let names = |fd: &OwnedFd, res: &mut Reservation| -> io::Result<Vec<OsString>> {
            let mut names = Vec::new();
            for entry in read_dir(fd)? {
                let entry = entry?;
                let n = entry.file_name().to_bytes();
                if n != b"." && n != b".." {
                    res.grow(n.len() + ENTRY_OVERHEAD)?;
                    names.push(OsStr::from_bytes(n).to_os_string());
                }
            }
            Ok(names)
        };
        let fd = io!(open(dir, name));
        let list = io!(names(&fd, res));
        let mut levels = vec![Level { fd: Some(fd), path: self.path(), names: list, name: name.to_os_string() }];
        while let Some(top) = levels.last_mut() {
            let Some(name) = top.names.pop() else {
                let done = levels.pop().expect("a level");
                drop(done.fd);
                let parent = match levels.len() {
                    0 => dir,
                    n => {
                        io!(reopen(&mut levels, n - 1));
                        levels[n - 1].fd.as_ref().expect("reopened")
                    }
                };
                io!(rustix::fs::unlinkat(parent, &done.name, AtFlags::REMOVEDIR));
                continue;
            };
            let path = top.path.join(&name);
            if let Err(e) = check(&path)? {
                return Ok(Err(e));
            }
            let at = top.fd.as_ref().expect("the innermost level is open");
            let st = io!(rustix::fs::statat(at, &name, AtFlags::SYMLINK_NOFOLLOW));
            if file_type(&st).is_dir() {
                let fd = io!(open(at, &name));
                let list = io!(names(&fd, res));
                levels.push(Level { fd: Some(fd), path, names: list, name });
                // However deep the tree, a bounded number of directories
                // stay open: the first, and the innermost ones.
                if levels.iter().filter(|l| l.fd.is_some()).count() > OPEN_LEVELS
                    && let Some(l) = levels[1..].iter_mut().find(|l| l.fd.is_some())
                {
                    l.fd = None;
                }
            } else {
                io!(rustix::fs::unlinkat(at, &name, AtFlags::empty()));
            }
        }
        Ok(Ok(()))
    }

    /// Renames the entry `from` (a final symlink itself) to `to`.
    pub fn rename(from: Resolved, to: Resolved) -> io::Result<()> {
        let ((from_dir, from_name), (to_dir, to_name)) = (from.entry_at()?, to.entry_at()?);
        Ok(rustix::fs::renameat(from_dir, from_name, to_dir, to_name)?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::is_unchecked_symlink;
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::symlink;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sand-fs-unix-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("real/file.txt"), "inside").unwrap();
        std::fs::write(dir.join("outside/file.txt"), "outside").unwrap();
        symlink(dir.join("outside"), dir.join("link")).unwrap();
        symlink(dir.join("outside/file.txt"), dir.join("real/link.txt")).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn follow(p: &Path) -> Resolved {
        resolve(p, Mode::Follow)
    }

    fn entry(p: &Path) -> Resolved {
        resolve(p, Mode::Entry)
    }

    #[test]
    fn symlinks_are_followed_by_the_walk_and_never_by_operations() {
        let dir = temp("follow");
        let mut s = String::new();
        follow(&dir.join("real/file.txt")).open_read(Path::new("x")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "inside");
        // The walk resolves symlinks: the policy sees where they lead.
        assert_eq!(follow(&dir.join("link/file.txt")).path(), dir.join("outside/file.txt"));
        assert_eq!(follow(&dir.join("real/link.txt")).path(), dir.join("outside/file.txt"));
        assert_eq!(follow(&dir.join("link/new.txt")).path(), dir.join("outside/new.txt"));
        // Entries themselves are not followed.
        assert_eq!(entry(&dir.join("real/link.txt")).path(), dir.join("real/link.txt"));
        assert_eq!(entry(&dir.join("link")).path(), dir.join("link"));
        assert_eq!(entry(&dir.join("link/file.txt")).path(), dir.join("outside/file.txt"));
        assert_eq!(entry(&dir.join("real/link.txt")).stat().unwrap().kind, EntryKind::Symlink);
        // A symlink that replaces the final entry after the walk is denied.
        let r = follow(&dir.join("real/file.txt"));
        std::fs::remove_file(dir.join("real/file.txt")).unwrap();
        symlink(dir.join("outside/file.txt"), dir.join("real/file.txt")).unwrap();
        assert!(is_unchecked_symlink(&r.open_read(Path::new("x")).unwrap_err()));
        let r = follow(&dir.join("real/file.txt"));
        assert_eq!(r.path(), dir.join("outside/file.txt"));
        std::fs::remove_file(dir.join("real/file.txt")).unwrap();
        let r = follow(&dir.join("real/new.txt"));
        symlink(dir.join("outside/new.txt"), dir.join("real/new.txt")).unwrap();
        assert!(is_unchecked_symlink(&r.open_write(Path::new("x"), false).unwrap_err()));
        assert!(!dir.join("outside/new.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Directories are held: renaming one after the walk and putting a
    /// symlink in its place does not redirect the operation.
    #[test]
    fn swapped_directories_do_not_redirect_operations() {
        let dir = temp("held");
        std::fs::create_dir(dir.join("real/sub")).unwrap();
        let r = follow(&dir.join("real/sub/new.txt"));
        assert_eq!(r.path(), dir.join("real/sub/new.txt"));
        std::fs::rename(dir.join("real/sub"), dir.join("real/moved")).unwrap();
        symlink(dir.join("outside"), dir.join("real/sub")).unwrap();
        r.open_write(Path::new("x"), false).unwrap().write_all(b"held").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/moved/new.txt")).unwrap(), "held");
        assert!(!dir.join("outside/new.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dangling_symlinks_are_not_resolved() {
        let dir = temp("dangling");
        symlink(dir.join("outside/missing.txt"), dir.join("real/dangling")).unwrap();
        symlink(dir.join("outside/missing/x"), dir.join("real/deeper")).unwrap();
        symlink("loop", dir.join("real/loop")).unwrap();
        for (p, rest) in [("real/dangling", ""), ("real/deeper", ""), ("real/deeper/a/b", "a/b"), ("real/loop", ""), ("real/loop/x", "x")] {
            let r = follow(&dir.join(p));
            let link = Path::new(p).components().take(2).collect::<PathBuf>();
            assert_eq!(r.path(), dir.join(&link).join(rest), "{p}");
            assert!(is_unchecked_symlink(&r.stat().unwrap_err()), "{p}");
        }
        assert!(is_unchecked_symlink(&follow(&dir.join("real/dangling")).open_write(Path::new("x"), false).unwrap_err()));
        assert!(is_unchecked_symlink(&follow(&dir.join("real/deeper/a")).create_dir_all(Path::new("x")).unwrap_err()));
        assert!(!dir.join("outside/missing.txt").exists() && !dir.join("outside/missing").exists());
        // A symlink to a file resolves; the next name then is not in a directory.
        let r = follow(&dir.join("real/link.txt/x"));
        assert_eq!(r.path(), dir.join("outside/file.txt/x"));
        assert_eq!(r.stat().unwrap_err().kind(), io::ErrorKind::NotADirectory);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relative_targets_and_parent_directories() {
        let dir = temp("relative");
        std::fs::create_dir_all(dir.join("real/a/b")).unwrap();
        symlink("../../outside", dir.join("real/a/up")).unwrap();
        symlink("..", dir.join("real/a/b/parent")).unwrap();
        symlink("/", dir.join("real/root")).unwrap();
        symlink("../../../../../../../../../../../../..", dir.join("real/a/b/far")).unwrap();
        assert_eq!(follow(&dir.join("real/a/up/file.txt")).path(), dir.join("outside/file.txt"));
        assert_eq!(follow(&dir.join("real/a/b/parent")).path(), dir.join("real/a"));
        assert_eq!(follow(&dir.join("real/a/b/parent/b/parent/up")).path(), dir.join("outside"));
        assert_eq!(follow(&dir.join("real/root")).path(), Path::new("/"));
        assert_eq!(follow(&dir.join("real/a/b/far")).path(), Path::new("/"));
        assert_eq!(follow(&dir.join("real/a/b/parent")).stat().unwrap().kind, EntryKind::Dir);
        let mem = crate::vm::iomem::IoMemory::new(None, usize::MAX);
        let names = follow(&dir.join("real/a/b/parent")).list_dir(&mut Reservation::new(&mem)).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(follow(Path::new("/")).path(), Path::new("/"));
        assert_eq!(follow(Path::new("/")).stat().unwrap().kind, EntryKind::Dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_components_and_errors() {
        let dir = temp("missing");
        let r = follow(&dir.join("real/x/y/z"));
        assert_eq!(r.path(), dir.join("real/x/y/z"));
        assert_eq!(r.missing_ancestors(), [dir.join("real/x"), dir.join("real/x/y")]);
        assert_eq!(r.stat().unwrap_err().kind(), io::ErrorKind::NotFound);
        follow(&dir.join("real/x/y/z")).create_dir_all(Path::new("x")).unwrap();
        assert!(dir.join("real/x/y/z").is_dir());
        follow(&dir.join("real/x/y/z")).create_dir_all(Path::new("x")).unwrap();
        let e = follow(&dir.join("real/file.txt")).create_dir_all(Path::new("x")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        let r = follow(&dir.join("real/file.txt/sub"));
        assert_eq!(r.path(), dir.join("real/file.txt/sub"));
        assert_eq!(r.create_dir_all(Path::new("x")).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        assert!(follow(&dir.join("real/x")).missing_ancestors().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regular_files_and_directories() {
        let dir = temp("ops");
        let mut f = follow(&dir.join("real/new.txt")).open_write(Path::new("x"), false).unwrap();
        f.write_all(b"one").unwrap();
        drop(f);
        follow(&dir.join("real/new.txt")).open_write(Path::new("x"), true).unwrap().write_all(b"two").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/new.txt")).unwrap(), "onetwo");
        let m = follow(&dir.join("real/new.txt")).stat().unwrap();
        assert_eq!((m.kind, m.size), (EntryKind::File, 6));
        assert!(m.modified.is_some());
        follow(&dir.join("real/a")).create_dir().unwrap();
        follow(&dir.join("real/a/b")).create_dir().unwrap();
        assert_eq!(follow(&dir.join("real/a/b")).create_dir().unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let mem = crate::vm::iomem::IoMemory::new(None, usize::MAX);
        assert_eq!(follow(&dir.join("real/a")).list_dir(&mut Reservation::new(&mem)).unwrap(), ["b"]);
        assert_eq!(follow(&dir.join("real/a")).open_read(Path::new("x")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(follow(&dir.join("real/missing")).open_read(Path::new("x")).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(entry(&dir.join("real/a")).remove_dir().unwrap_err().kind(), io::ErrorKind::DirectoryNotEmpty);
        entry(&dir.join("real/a/b")).remove_dir().unwrap();
        Resolved::rename(entry(&dir.join("real/a")), entry(&dir.join("real/z"))).unwrap();
        assert!(dir.join("real/z").is_dir());
        entry(&dir.join("real/new.txt")).remove_file().unwrap();
        assert!(!dir.join("real/new.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_only_directories_are_traversed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("search");
        std::fs::create_dir(dir.join("real/wx")).unwrap();
        std::fs::write(dir.join("real/wx/f.txt"), "f").unwrap();
        std::fs::set_permissions(dir.join("real/wx"), std::fs::Permissions::from_mode(0o300)).unwrap();
        let mut s = String::new();
        let read = follow(&dir.join("real/wx/f.txt")).open_read(Path::new("x")).and_then(|mut f| f.read_to_string(&mut s));
        let write = follow(&dir.join("real/wx/g.txt")).open_write(Path::new("x"), false);
        let st = follow(&dir.join("real/wx/f.txt")).stat();
        std::fs::set_permissions(dir.join("real/wx"), std::fs::Permissions::from_mode(0o700)).unwrap();
        read.unwrap();
        write.unwrap();
        assert_eq!(st.unwrap().kind, EntryKind::File);
        assert_eq!(s, "f");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn nofollow_any_is_supported() {
        assert!(nofollow_any());
    }

    /// Paths longer than the system's `PATH_MAX` are walked in pieces.
    #[test]
    fn long_paths() {
        let dir = temp("long");
        let mut deep = dir.join("real");
        let mut rel = PathBuf::new();
        for i in 0..60 {
            rel.push(format!("component-{i:02}-{}", "x".repeat(40)));
        }
        deep.push(&rel);
        std::fs::create_dir_all(&deep).unwrap_or_else(|_| {
            // `create_dir_all` uses whole paths: create it in pieces.
            let mut p = dir.join("real");
            for c in rel.components() {
                p.push(c);
                let _ = follow(&p).create_dir();
            }
        });
        assert!(deep.as_os_str().len() > 3000);
        let r = follow(&deep.join("f.txt"));
        assert_eq!(r.path(), deep.join("f.txt"));
        r.open_write(Path::new("x"), false).unwrap().write_all(b"deep").unwrap();
        let mut s = String::new();
        follow(&deep.join("f.txt")).open_read(Path::new("x")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "deep");
        // Through symlinks at the bottom, which path-based calls cannot reach.
        let held = follow(&deep.join("x")).dir.unwrap();
        rustix::fs::symlinkat("../../..", &held, "up").unwrap();
        rustix::fs::symlinkat(dir.join("outside"), &held, "out").unwrap();
        let above = deep.ancestors().nth(3).unwrap();
        assert_eq!(follow(&deep.join("up/x")).path(), above.join("x"));
        assert_eq!(follow(&deep.join("out/file.txt")).path(), dir.join("outside/file.txt"));
        assert_eq!(entry(&deep.join("up")).stat().unwrap().kind, EntryKind::Symlink);
        // `remove_dir_all` cannot remove it either.
        for name in ["f.txt", "up", "out"] {
            entry(&deep.join(name)).remove_file().unwrap();
        }
        for d in deep.ancestors().take(rel.components().count()) {
            entry(d).remove_dir().unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
