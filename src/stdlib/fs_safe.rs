//! File system access that never follows a symbolic link.
//!
//! The `fs` natives check the policy on a resolved path (symlinks already
//! followed, see [`crate::policy::normalize_path`]) and then perform the
//! operation with these functions. Every directory on the way is opened
//! without following symlinks (`openat` with `O_NOFOLLOW`, one component at a
//! time, or a single `O_NOFOLLOW_ANY` open on macOS) and the final operation
//! is relative to the parent directory handle and does not follow a final
//! symlink either. So whatever changes between the check and the operation,
//! the operation reaches exactly the checked location or fails: a symlink
//! that appears in the path fails with [`unchecked_symlink`].
//!
//! On Windows, every component is likewise opened relative to its parent's
//! handle without following reparse points (symlinks, junctions); see the
//! Windows `imp` module for details and for `rename`, which is path-based
//! there but holds the directories on both paths open so that they cannot
//! be replaced. On other platforms the operations fall back to path-based
//! `std::fs` calls, which do follow symlinks swapped in after the check.
//!
//! Paths must be absolute and lexically normalised (no `.`/`..`).

use crate::vm::iomem::Reservation;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// Bytes counted per directory entry besides its name.
const ENTRY_OVERHEAD: usize = 2 * std::mem::size_of::<std::ffi::OsString>();

/// The type of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// Metadata of a directory entry (a final symlink is not followed).
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryMeta {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// The error for a symlink met where the checked path had none.
#[derive(Debug)]
struct UncheckedSymlink(String);

impl std::fmt::Display for UncheckedSymlink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not following symbolic link {:?}: it was not checked by the policy", self.0)
    }
}

impl std::error::Error for UncheckedSymlink {}

pub(crate) fn unchecked_symlink(name: &OsStr) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, UncheckedSymlink(name.to_string_lossy().into_owned()))
}

/// Whether `e` reports a symlink that was not followed.
pub(crate) fn is_unchecked_symlink(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<UncheckedSymlink>())
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display()))
}

/// The final component of `path` (`.` for the root).
#[cfg(not(windows))]
fn final_name(path: &Path) -> &OsStr {
    path.file_name().unwrap_or(OsStr::new("."))
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::fs::File;
    use std::os::fd::{AsFd, OwnedFd};
    use std::path::Component;
    use std::time::Duration;

    /// Flags for directories that are only traversed (O_PATH needs no read permission).
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    const TRAVERSE: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    const TRAVERSE: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

    /// Converts an error of a no-follow operation on `name` in `dir`: a
    /// symlink (reported as ELOOP, EMLINK on FreeBSD, or ENOTDIR with
    /// O_DIRECTORY) becomes [`unchecked_symlink`].
    fn nofollow_error(e: Errno, dir: impl AsFd, name: &OsStr) -> io::Error {
        if matches!(e, Errno::LOOP | Errno::MLINK | Errno::NOTDIR)
            && rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|st| FileType::from_raw_mode(st.st_mode as _).is_symlink())
        {
            return unchecked_symlink(name);
        }
        e.into()
    }

    /// Opens a directory in one call, failing if any component is a symlink
    /// (`O_NOFOLLOW_ANY` excludes `O_NOFOLLOW`).
    #[cfg(target_vendor = "apple")]
    const NO_SYMLINKS: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC).union(OFlags::NOFOLLOW_ANY);

    /// `O_EXEC` (macOS 13+; with `O_DIRECTORY` it is `O_SEARCH`, which rustix
    /// does not name): opens a directory for lookups only, needing search but
    /// not read permission.
    #[cfg(target_vendor = "apple")]
    const SEARCH: OFlags = OFlags::from_bits_retain(0x4000_0000);

    /// Whether `O_NOFOLLOW_ANY` (macOS 11+) is honoured: `/var` is a symlink.
    /// If not, directories are walked component by component.
    #[cfg(target_vendor = "apple")]
    pub(super) fn nofollow_any() -> bool {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| rustix::fs::open("/var", NO_SYMLINKS, Mode::empty()).err() == Some(Errno::LOOP))
    }

    /// Opens the directory `path` without following symlinks in any component.
    fn open_dir(path: &Path) -> io::Result<OwnedFd> {
        #[cfg(target_vendor = "apple")]
        if nofollow_any() {
            let opened = match rustix::fs::open(path, NO_SYMLINKS | SEARCH, Mode::empty()) {
                // A kernel without O_SEARCH; read permission is needed then.
                Err(Errno::INVAL) => rustix::fs::open(path, NO_SYMLINKS, Mode::empty()),
                r => r,
            };
            return match opened {
                // Find the offending component.
                Err(Errno::LOOP) => walk(path),
                r => Ok(r?),
            };
        }
        walk(path)
    }

    /// Opens `path` one component at a time.
    pub(super) fn walk(path: &Path) -> io::Result<OwnedFd> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not an absolute path", path.display())));
        }
        let mut dir = rustix::fs::open("/", TRAVERSE, Mode::empty())?;
        for c in components {
            let Component::Normal(name) = c else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a normalised path", path.display())));
            };
            dir = rustix::fs::openat(&dir, name, TRAVERSE, Mode::empty()).map_err(|e| nofollow_error(e, &dir, name))?;
        }
        Ok(dir)
    }

    /// The parent directory of `path` and the final name.
    fn parent(path: &Path) -> io::Result<(OwnedFd, &OsStr)> {
        Ok((open_dir(path.parent().unwrap_or(path))?, final_name(path)))
    }

    /// Opens the regular file `path` with `flags`.
    fn open_file(path: &Path, flags: OFlags) -> io::Result<File> {
        let (dir, name) = parent(path)?;
        // Non-blocking, so that opening a FIFO cannot hang; it is rejected below.
        let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let fd = rustix::fs::openat(&dir, name, flags, Mode::from_raw_mode(0o666)).map_err(|e| nofollow_error(e, &dir, name))?;
        if !FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode as _).is_file() {
            return Err(not_regular(path));
        }
        Ok(File::from(fd))
    }

    pub(crate) fn open_read(path: &Path) -> io::Result<File> {
        open_file(path, OFlags::RDONLY)
    }

    /// Opens a regular file for writing, creating it if needed; truncates it
    /// unless `append`.
    pub(crate) fn open_write(path: &Path, append: bool) -> io::Result<File> {
        let mode = if append { OFlags::APPEND } else { OFlags::TRUNC };
        open_file(path, OFlags::WRONLY | OFlags::CREATE | mode)
    }

    pub(crate) fn stat(path: &Path) -> io::Result<EntryMeta> {
        let (dir, name) = parent(path)?;
        let st = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let t = FileType::from_raw_mode(st.st_mode as _);
        let kind = if t.is_file() {
            EntryKind::File
        } else if t.is_dir() {
            EntryKind::Dir
        } else if t.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        let (secs, nanos) = (st.st_mtime as i64, st.st_mtime_nsec as u32);
        let modified = if secs >= 0 {
            SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs as u64, nanos))
        } else {
            SystemTime::UNIX_EPOCH
                .checked_sub(Duration::from_secs(secs.unsigned_abs()))
                .and_then(|t| t.checked_add(Duration::from_nanos(nanos as u64)))
        };
        Ok(EntryMeta { kind, size: st.st_size as u64, modified })
    }

    /// The names of the entries of the directory `path` (unsorted, without
    /// `.` and `..`), reserving their memory in `res`.
    pub(crate) fn list_dir(path: &Path, res: &mut Reservation) -> io::Result<Vec<std::ffi::OsString>> {
        use std::os::unix::ffi::OsStrExt;
        let (dir, name) = parent(path)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&dir, name, flags, Mode::empty()).map_err(|e| nofollow_error(e, &dir, name))?;
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::new(fd)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                res.grow(name.len() + ENTRY_OVERHEAD)?;
                names.push(OsStr::from_bytes(name).to_os_string());
            }
        }
        Ok(names)
    }

    pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
        let (dir, name) = parent(path)?;
        Ok(rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o777))?)
    }

    /// Creates `path` and missing parents; an existing directory is fine.
    /// The first `keep` components below the root must exist already (they
    /// were not checked as directories that may be created).
    pub(crate) fn create_dir_all(path: &Path, keep: usize) -> io::Result<()> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not an absolute path", path.display())));
        }
        let mut dir = rustix::fs::open("/", TRAVERSE, Mode::empty())?;
        let mut components = components.enumerate().peekable();
        while let Some((i, c)) = components.next() {
            let Component::Normal(name) = c else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a normalised path", path.display())));
            };
            let opened = match rustix::fs::openat(&dir, name, TRAVERSE, Mode::empty()) {
                Err(Errno::NOENT) if i < keep => Err(Errno::NOENT),
                Err(Errno::NOENT) => {
                    match rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(e.into()),
                    }
                    rustix::fs::openat(&dir, name, TRAVERSE, Mode::empty())
                }
                r => r,
            };
            dir = match opened {
                Ok(d) => d,
                Err(e) => match nofollow_error(e, &dir, name) {
                    // Like `std::fs::create_dir_all`: something other than a directory is in the way.
                    e if components.peek().is_none() && e.kind() == io::ErrorKind::NotADirectory => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("{} exists and is not a directory", path.display()),
                        ));
                    }
                    e => return Err(e),
                },
            };
        }
        Ok(())
    }

    /// Removes the file or symlink `path` (not a symlink's target).
    pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
        let (dir, name) = parent(path)?;
        Ok(rustix::fs::unlinkat(&dir, name, AtFlags::empty())?)
    }

    pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
        let (dir, name) = parent(path)?;
        Ok(rustix::fs::unlinkat(&dir, name, AtFlags::REMOVEDIR)?)
    }

    /// Renames the entry `from` (a final symlink itself) to `to`.
    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        let (from_dir, from_name) = parent(from)?;
        let (to_dir, to_name) = parent(to)?;
        Ok(rustix::fs::renameat(&from_dir, from_name, &to_dir, to_name)?)
    }
}

#[cfg(windows)]
mod imp {
    //! Every component is opened relative to the handle of its parent
    //! directory (`NtCreateFile` with a `RootDirectory`, through the `fs_at`
    //! crate) with `FILE_OPEN_REPARSE_POINT`, so a symlink, junction or other
    //! name-surrogate reparse point is opened itself, never followed, and is
    //! refused (or, for entry operations, acted on itself). Creating and
    //! removing entries and listing directories also go through handles.
    //! Only `rename` has no handle-based form in a safe API: it is path-based,
    //! with every directory leading to both entries held open (and checked
    //! against the handle-relative walk) so that none of them can be renamed,
    //! deleted or replaced meanwhile.
    //!
    //! Names that Win32 path functions would alter or treat specially (a
    //! trailing `.` or space, `:` for alternate data streams, reserved
    //! device names such as `CON`) are refused: handle-relative opens take
    //! them literally, so they would name entries that the policy's
    //! (Win32-based) normalisation does not see.
    use super::*;
    use fs_at::os::windows::{FileExt, OpenOptionsExt};
    use same_file::Handle;
    use std::fs::{File, Metadata};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::{Component, PathBuf};

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

    /// Splits the absolute, normalised `path` into its root (`C:\`,
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
                _ => return Err(invalid(path, "is not a normalised path")),
            }
        }
        if !root.is_absolute() {
            return Err(invalid(path, "is not an absolute path"));
        }
        Ok((root, names))
    }

    /// Refuses names that Win32 path functions would alter or treat
    /// specially, which handle-relative opens would take literally.
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

    /// Options to open an entry relative to a directory handle with `access`.
    /// `FILE_OPEN_REPARSE_POINT` is always set: a symlink or junction is
    /// opened itself, never followed, and callers check what they opened.
    /// (`follow(true)` only stops `fs_at` from probing for links with
    /// `FSCTL_GET_REPARSE_POINT`, which fails on file systems without
    /// reparse points, such as FAT.)
    fn options(access: u32, create_options: u32) -> fs_at::OpenOptions {
        let mut o = fs_at::OpenOptions::default();
        o.follow(true)
            .desired_access(access)
            .create_options(create_options | FILE_OPEN_REPARSE_POINT)
            .object_attributes(OBJ_CASE_INSENSITIVE);
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
    pub(super) fn open_dir(path: &Path) -> io::Result<File> {
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

    pub(crate) fn open_read(path: &Path) -> io::Result<File> {
        let (dir, name) = parent(path)?;
        let f = open_in(&dir, name, &options(FILE_GENERIC_READ, 0)).map_err(|e| nofollow_error(e, &dir, name))?;
        regular(path, name, f)
    }

    /// Opens a regular file for writing, creating it if needed; truncates it
    /// unless `append`.
    pub(crate) fn open_write(path: &Path, append: bool) -> io::Result<File> {
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

    fn entry_meta(m: &Metadata) -> EntryMeta {
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
        EntryMeta { kind, size: m.len(), modified: m.modified().ok() }
    }

    pub(crate) fn stat(path: &Path) -> io::Result<EntryMeta> {
        if path.parent().is_none() {
            return Ok(entry_meta(&open_dir(path)?.metadata()?));
        }
        let (dir, name) = parent(path)?;
        Ok(entry_meta(&lstat(&dir, name)?))
    }

    /// The names of the entries of the directory `path` (unsorted, without
    /// `.` and `..`), reserving their memory in `res`.
    pub(crate) fn list_dir(path: &Path, res: &mut Reservation) -> io::Result<Vec<std::ffi::OsString>> {
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

    pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
        let (dir, name) = parent(path)?;
        mkdir(&dir, name).map(drop)
    }

    /// Creates `path` and missing parents; an existing directory is fine.
    /// The first `keep` components below the root must exist already (they
    /// were not checked as directories that may be created).
    pub(crate) fn create_dir_all(path: &Path, keep: usize) -> io::Result<()> {
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
                // Like `std::fs::create_dir_all`: something other than a directory is in the way.
                Err(e) if i + 1 == last && e.kind() == io::ErrorKind::NotADirectory => {
                    return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", path.display())));
                }
                Err(e) => return Err(e),
            };
        }
        Ok(())
    }

    /// Opens the entry `path` itself (not followed) for deletion and deletes
    /// it if `accept` approves of its metadata.
    fn remove(path: &Path, accept: impl FnOnce(&Metadata) -> io::Result<()>) -> io::Result<()> {
        let (dir, name) = parent(path)?;
        let entry = open_in(&dir, name, &options(DELETE | FILE_READ_ATTRIBUTES, 0))?;
        accept(&entry.metadata()?)?;
        entry.delete_by_handle().map_err(|(_, e)| e)
    }

    /// Removes the file or symlink `path` (not a symlink's target).
    pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
        remove(path, |m| {
            if m.is_dir() {
                return Err(io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory", path.display())));
            }
            Ok(())
        })
    }

    pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
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
    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
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
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::*;
    use std::fs::File;

    // Path-based fallbacks: a symlink swapped in after the policy check is
    // followed. Final symlinks are still refused where Unix refuses them.

    fn refuse_symlink(path: &Path) -> io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => Err(unchecked_symlink(final_name(path))),
            _ => Ok(()),
        }
    }

    fn regular(path: &Path, f: File) -> io::Result<File> {
        if f.metadata()?.is_file() { Ok(f) } else { Err(not_regular(path)) }
    }

    pub(crate) fn open_read(path: &Path) -> io::Result<File> {
        refuse_symlink(path)?;
        regular(path, File::open(path)?)
    }

    pub(crate) fn open_write(path: &Path, append: bool) -> io::Result<File> {
        refuse_symlink(path)?;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true);
        if append {
            o.append(true);
        } else {
            o.truncate(true);
        }
        regular(path, o.open(path)?)
    }

    pub(crate) fn stat(path: &Path) -> io::Result<EntryMeta> {
        let m = std::fs::symlink_metadata(path)?;
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
        Ok(EntryMeta { kind, size: m.len(), modified: m.modified().ok() })
    }

    pub(crate) fn list_dir(path: &Path, res: &mut Reservation) -> io::Result<Vec<std::ffi::OsString>> {
        refuse_symlink(path)?;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let name = entry?.file_name();
            res.grow(name.len() + ENTRY_OVERHEAD)?;
            names.push(name);
        }
        Ok(names)
    }

    pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    pub(crate) fn create_dir_all(path: &Path, keep: usize) -> io::Result<()> {
        use std::path::Component;
        let root = path.components().take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir)).count();
        let kept: std::path::PathBuf = path.components().take(root + keep).collect();
        if !kept.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} does not exist", kept.display())));
        }
        std::fs::create_dir_all(path)
    }

    pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

pub(crate) use imp::*;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sand-fs-safe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("real/file.txt"), "inside").unwrap();
        std::fs::write(dir.join("outside/file.txt"), "outside").unwrap();
        symlink(dir.join("outside"), dir.join("link")).unwrap();
        symlink(dir.join("outside/file.txt"), dir.join("real/link.txt")).unwrap();
        crate::policy::normalize_path(&dir)
    }

    #[test]
    fn symlinks_are_never_followed() {
        let dir = temp("nofollow");
        let mut s = String::new();
        open_read(&dir.join("real/file.txt")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "inside");
        // A directory symlink anywhere in the path, or a final symlink.
        for p in ["link/file.txt", "real/link.txt"] {
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
        assert!(!dir.join("outside/new.txt").exists() && !dir.join("outside/sub").exists() && !dir.join("outside/a").exists());
        assert_eq!(std::fs::read_to_string(dir.join("outside/file.txt")).unwrap(), "outside");
        // Final symlinks are entries in their own right for stat, remove and rename.
        assert_eq!(stat(&dir.join("real/link.txt")).unwrap().kind, EntryKind::Symlink);
        rename(&dir.join("real/link.txt"), &dir.join("real/moved.txt")).unwrap();
        remove_file(&dir.join("real/moved.txt")).unwrap();
        assert!(dir.join("outside/file.txt").exists());
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
        let read = open_read(&dir.join("real/wx/f.txt")).and_then(|mut f| f.read_to_string(&mut s));
        let write = open_write(&dir.join("real/wx/g.txt"), false);
        let st = stat(&dir.join("real/wx/f.txt"));
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
        assert!(imp::nofollow_any());
    }

    #[test]
    fn walking_refuses_symlinked_directories() {
        let dir = temp("walk");
        assert!(imp::walk(&dir.join("real")).is_ok());
        assert!(is_unchecked_symlink(&imp::walk(&dir.join("link")).unwrap_err()));
        assert!(is_unchecked_symlink(&imp::walk(&dir.join("link/deeper")).unwrap_err()));
        assert_eq!(imp::walk(&dir.join("real/file.txt")).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        assert_eq!(imp::walk(&dir.join("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regular_files_and_directories() {
        let dir = temp("ops");
        let mut f = open_write(&dir.join("real/new.txt"), false).unwrap();
        f.write_all(b"one").unwrap();
        drop(f);
        open_write(&dir.join("real/new.txt"), true).unwrap().write_all(b"two").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real/new.txt")).unwrap(), "onetwo");
        let m = stat(&dir.join("real/new.txt")).unwrap();
        assert_eq!((m.kind, m.size), (EntryKind::File, 6));
        assert!(m.modified.is_some());
        assert_eq!(stat(&dir).unwrap().kind, EntryKind::Dir);
        assert_eq!(stat(Path::new("/")).unwrap().kind, EntryKind::Dir);
        create_dir_all(&dir.join("real/a/b/c"), 0).unwrap();
        create_dir_all(&dir.join("real/a/b/c"), 0).unwrap();
        assert_eq!(create_dir_all(&dir.join("real/file.txt"), 0).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(create_dir_all(&dir.join("real/file.txt/sub"), 0).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        // Components that must exist are not created.
        let depth = dir.join("real/x").components().count() - 1;
        assert_eq!(create_dir_all(&dir.join("real/x/y"), depth).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!dir.join("real/x").exists());
        create_dir_all(&dir.join("real/x/y"), depth - 1).unwrap();
        create_dir(&dir.join("real/a/d")).unwrap();
        assert_eq!(create_dir(&dir.join("real/a/d")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let mem = crate::vm::iomem::IoMemory::new(None);
        let mut names = list_dir(&dir.join("real/a"), &mut Reservation::new(&mem)).unwrap();
        names.sort();
        assert_eq!(names, ["b", "d"]);
        assert_eq!(open_read(&dir.join("real/a")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(open_read(&dir.join("real/missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
        remove_dir(&dir.join("real/a/d")).unwrap();
        rename(&dir.join("real/a"), &dir.join("real/z")).unwrap();
        assert!(dir.join("real/z/b/c").is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
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
        assert!(imp::open_dir(&dir.join("real")).is_ok());
        assert!(is_unchecked_symlink(&imp::open_dir(&dir.join("link")).unwrap_err()));
        assert!(is_unchecked_symlink(&imp::open_dir(&dir.join("link/deeper")).unwrap_err()));
        assert_eq!(imp::open_dir(&dir.join("real/file.txt")).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        assert_eq!(imp::open_dir(&dir.join("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
        // Names are looked up case-insensitively, as by Win32 path functions.
        assert!(imp::open_dir(&dir.join("REAL")).is_ok());
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
