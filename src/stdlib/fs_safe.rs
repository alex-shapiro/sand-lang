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
//! Paths must be absolute and lexically normalised (no `.`/`..`).
//!
//! On non-Unix platforms the operations fall back to path-based `std::fs`
//! calls, which do follow symlinks swapped in after the check.

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
    pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
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

#[cfg(not(unix))]
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
        let kept: std::path::PathBuf = path.components().take(keep + 1).collect();
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
