//! The file system scripts see: the `fs` module and the loading of script
//! modules go through an [`Fs`], the host's (with the `os` feature) or a
//! [`MemFs`].
//!
//! [`Fs::target`] first resolves an operation's path into a [`Target`]: the
//! normal form the policy checks and, on the host, the directory handles of
//! the walk, through which the operation then acts (see `unix.rs`). Callers
//! run host file system calls on worker threads (`Vm::offload`).

#[cfg(feature = "os")]
mod host;
mod mem;
#[cfg(all(feature = "os", unix))]
mod unix;
#[cfg(all(feature = "os", windows))]
mod windows;

// Elsewhere there is no way to open a path without following symlinks, so
// the host's file system cannot be confined to what the policy checked.
#[cfg(all(feature = "os", not(any(unix, windows))))]
compile_error!("the `os` feature needs Unix or Windows (files are opened without following symlinks)");

pub use mem::MemFs;
pub(crate) use mem::normalize as memory_path;

use crate::policy::PathSpace;
use crate::vm::iomem::Reservation;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// The bytes reserved for each directory entry read, in addition to its name.
const ENTRY_OVERHEAD: usize = 2 * std::mem::size_of::<std::ffi::OsString>();

/// The type of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "os"), allow(dead_code))]
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
    /// Unix time in nanoseconds.
    pub modified: Option<i64>,
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

/// File access mode for `open`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, crate::SandEnum)]
#[sand(module = "fs", name = "Mode")]
pub(crate) enum OpenMode {
    /// Opens an existing file for reading. Requires `FsRead`.
    Read,
    /// Creates or truncates a file for writing from the start. Requires `FsWrite`.
    Write,
    /// Opens or creates a file for appending at the end. Requires `FsWrite`.
    Append,
    /// Creates a new file for writing, or fails with kind `"already_exists"` if
    /// the path exists. Requires `FsWrite`.
    CreateNew,
    /// Opens an existing file for reading and writing in place. Requires `FsRead` and `FsWrite`.
    ReadWrite,
}

impl OpenMode {
    pub fn reads(self) -> bool {
        matches!(self, OpenMode::Read | OpenMode::ReadWrite)
    }

    pub fn writes(self) -> bool {
        self != OpenMode::Read
    }

    pub fn creates(self) -> bool {
        matches!(self, OpenMode::Write | OpenMode::Append | OpenMode::CreateNew)
    }

    pub fn truncates(self) -> bool {
        self == OpenMode::Write
    }
}

/// Whether an operation acts on what a final symlink points to (`Follow`)
/// or on the directory entry itself (`Entry`: removing or renaming a link).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Follow,
    Entry,
}

/// Work done resolving a path besides scanning it (for metering).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Work {
    /// Names resolved, and bytes of symlink targets read.
    pub steps: usize,
    pub bytes: usize,
    /// Directory entries read to find a stored name, and their bytes.
    pub scanned: usize,
    pub scanned_bytes: usize,
}

/// A path resolved by an [`Fs`] for an operation.
pub(crate) struct Target {
    /// The fully resolved absolute path evaluated by the policy.
    pub path: PathBuf,
    pub mode: Mode,
    #[cfg(feature = "os")]
    host: Option<host::Resolved>,
}

impl Target {
    /// True if the resolved path matches its on-disk name.
    pub fn exact(&self) -> bool {
        #[cfg(feature = "os")]
        if let Some(h) = &self.host {
            return h.exact();
        }
        true
    }

    /// Replaces the final path component with its on-disk name.
    pub fn fix_spelling(&mut self) {
        #[cfg(feature = "os")]
        if let Some(h) = &mut self.host
            && !h.exact()
        {
            h.fix_spelling();
            self.path = h.path();
        }
    }

    /// Work done resolving it (for metering).
    pub fn work(&self) -> Work {
        #[cfg(feature = "os")]
        if let Some(h) = &self.host {
            return h.work;
        }
        Work::default()
    }

    /// Moves the target out and leaves an empty one in its place.
    pub fn take(&mut self) -> Target {
        std::mem::replace(self, Target::memory(PathBuf::new(), self.mode))
    }

    fn memory(path: PathBuf, mode: Mode) -> Target {
        Target {
            path,
            mode,
            #[cfg(feature = "os")]
            host: None,
        }
    }

    #[cfg(feature = "os")]
    fn host(self) -> io::Result<(PathBuf, host::Resolved)> {
        match self.host {
            Some(h) => Ok((self.path, h)),
            None => Err(io::Error::other("not a host path")),
        }
    }
}

/// A file system scripts can see.
#[derive(Clone)]
pub(crate) enum Fs {
    /// The host's file system.
    #[cfg(feature = "os")]
    Host,
    Memory(MemFs),
}

impl Default for Fs {
    /// Host file system if the os feature is enabled, otherwise an in-memory file system.
    fn default() -> Fs {
        #[cfg(feature = "os")]
        return Fs::Host;
        #[cfg(not(feature = "os"))]
        return Fs::Memory(MemFs::new());
    }
}

impl Fs {
    /// The namespace of this file system's paths.
    pub fn space(&self) -> PathSpace {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => PathSpace::Host,
            Fs::Memory(_) => PathSpace::Memory,
        }
    }

    /// Returns the in-memory file system if active.
    pub fn memory(&self) -> Option<&MemFs> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => None,
            Fs::Memory(m) => Some(m),
        }
    }

    /// Base directory for relative paths.
    pub fn default_dir(&self) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => std::env::current_dir().unwrap_or_default(),
            Fs::Memory(_) => PathBuf::from("/"),
        }
    }

    /// Converts a path to an absolute path.
    pub fn absolute(&self, p: &Path) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()),
            Fs::Memory(_) => mem::normalize(p),
        }
    }

    /// Resolves a path for an operation.
    pub fn target(&self, p: &Path, mode: Mode) -> Target {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let h = host::resolve(p, mode);
                Target { path: h.path(), mode, host: Some(h) }
            }
            Fs::Memory(_) => Target::memory(mem::normalize(p), mode),
        }
    }

    /// Resolves a path to its normalized form.
    pub fn resolve(&self, p: &Path) -> PathBuf {
        let mut t = self.target(p, Mode::Follow);
        t.fix_spelling();
        t.path
    }

    /// The paths of the directories above the target that do not exist,
    /// outermost first.
    pub fn missing_ancestors(&self, t: &Target) -> Vec<PathBuf> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host.as_ref().map(host::Resolved::missing_ancestors).unwrap_or_default(),
            Fs::Memory(m) => {
                let mut missing: Vec<PathBuf> = t.path.ancestors().skip(1).take_while(|a| !m.exists(a)).map(Path::to_path_buf).collect();
                missing.reverse();
                missing
            }
        }
    }

    /// The size of the file at the target, if known without more I/O.
    pub fn size(&self, t: &Target) -> Option<u64> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host.as_ref().and_then(host::Resolved::size),
            Fs::Memory(m) => m.stat(&t.path).ok().map(|m| m.size),
        }
    }

    /// Reads a regular file into a buffer reserved in `res`.
    pub fn read(&self, t: Target, res: Reservation) -> io::Result<(Vec<u8>, Reservation)> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let (path, h) = t.host()?;
                host::read_reserved(h.open_read(&path)?, res)
            }
            Fs::Memory(m) => m.read_reserved(&t.path, res),
        }
    }

    /// Writes `data` to the file at `t`, creating or truncating it.
    pub fn write(&self, t: Target, data: &[u8]) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let (path, h) = t.host()?;
                host::write_all(h.open_write(&path, false)?, data)
            }
            Fs::Memory(m) => m.write_file(&t.path, data, false),
        }
    }

    /// Metadata of the target, recorded when it was resolved, so this does
    /// no I/O. In `Entry` mode, a final symlink is not followed.
    pub fn stat(&self, t: &Target) -> io::Result<EntryMeta> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host.as_ref().ok_or_else(|| io::Error::other("not a host path"))?.stat(),
            Fs::Memory(m) => m.stat(&t.path),
        }
    }

    /// The names in the directory `t`, sorted, reserved in `res`.
    pub fn list_dir(&self, t: Target, mut res: Reservation) -> io::Result<(Vec<String>, Reservation)> {
        let mut names = match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host()?.1.list_dir(&mut res)?,
            Fs::Memory(m) => m.list_dir(&t.path, &mut res)?,
        };
        names.sort();
        Ok((names, res))
    }

    pub fn create_dir(&self, t: Target) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host()?.1.create_dir(),
            Fs::Memory(m) => m.create_dir(&t.path),
        }
    }

    /// Creates `t` and the missing directories above it (those that
    /// [`Fs::missing_ancestors`] reported, which the policy checked).
    pub fn create_dir_all(&self, t: Target) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let (path, h) = t.host()?;
                h.create_dir_all(&path)
            }
            // Nothing changes between the policy check and the operation.
            Fs::Memory(m) => m.make_dir_all(&t.path),
        }
    }

    pub fn remove_file(&self, t: Target) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host()?.1.remove_file(),
            Fs::Memory(m) => m.remove_file(&t.path),
        }
    }

    /// Removes the empty directory `t`.
    pub fn remove_dir(&self, t: Target) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => t.host()?.1.remove_dir(),
            Fs::Memory(m) => m.remove_dir(&t.path),
        }
    }

    /// Walks the tree below the directory `t` depth first, without
    /// following symlinks (`t` itself must be a directory, not a symlink to
    /// one). Calls `visit` with each entry's normal path before descending
    /// into it. If `remove` is set, removes each entry after visiting it,
    /// and each directory, including `t`, once it is empty. The walk stops
    /// if `visit` returns `Err` or `Ok(Err)`. The memory for the names read
    /// is reserved in `res`.
    pub fn walk_tree<E>(
        &self,
        t: &Target,
        res: &mut Reservation,
        remove: bool,
        visit: impl FnMut(&Path) -> Result<io::Result<()>, E>,
    ) -> Result<io::Result<()>, E> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => match &t.host {
                Some(h) => h.walk_tree(res, remove, visit),
                None => Ok(Err(io::Error::other("not a host path"))),
            },
            Fs::Memory(m) => m.walk_tree(&t.path, res, remove, visit),
        }
    }

    pub fn rename(&self, from: Target, to: Target) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::Resolved::rename(from.host()?.1, to.host()?.1),
            Fs::Memory(m) => m.rename(&from.path, &to.path),
        }
    }

    /// Copies the contents of the regular file `from` to `to`.
    pub fn copy(&self, from: Target, to: Target) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let ((from_path, from), (to_path, to)) = (from.host()?, to.host()?);
                host::copy(from.open_read(&from_path)?, to.open_write(&to_path, false)?)
            }
            Fs::Memory(m) => m.copy(&from.path, &to.path),
        }
    }

    /// Opens the regular file at `t` for `mode` (a file that exists is
    /// opened only if it is a regular file).
    pub fn open(&self, t: Target, mode: OpenMode) -> io::Result<OpenedFile> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let (path, h) = t.host()?;
                Ok(OpenedFile::Host(h.open(&path, mode)?))
            }
            Fs::Memory(m) => Ok(OpenedFile::Memory(m.open(&t.path, mode)?, 0)),
        }
    }

    /// Reads the text of a main script (the path as the host gave it).
    pub fn read_main(&self, path: &Path) -> io::Result<String> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => std::fs::read_to_string(path),
            Fs::Memory(m) => String::from_utf8(m.read(path)?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8")),
        }
    }

    /// Reads a script module (at most `max + 1` bytes, so that oversized
    /// sources are detected without reading them whole).
    pub fn read_source(&self, t: Target, max: usize) -> io::Result<String> {
        use io::{Error, ErrorKind};
        let data = match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                use io::Read;
                let (path, h) = t.host()?;
                let mut data = Vec::new();
                h.open_read(&path)?.take(max as u64 + 1).read_to_end(&mut data)?;
                data
            }
            Fs::Memory(m) => m.read_source(&t.path, max)?,
        };
        if data.len() > max {
            return Err(Error::new(ErrorKind::InvalidData, "source too large"));
        }
        String::from_utf8(data).map_err(|_| Error::new(ErrorKind::InvalidData, "source is not valid UTF-8"))
    }
}

/// A file opened by a script (`fs.File`), on the host (whose system
/// keeps the position) or in a memory file system (with the position).
pub(crate) enum OpenedFile {
    #[cfg(feature = "os")]
    Host(std::fs::File),
    Memory(mem::OpenFile, u64),
}

impl OpenedFile {
    /// Reads at the position into `buf`, moving it on; 0 at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => loop {
                match io::Read::read(f, buf) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    r => return r,
                }
            },
            OpenedFile::Memory(f, pos) => {
                let n = f.read_at(*pos, buf);
                *pos += n as u64;
                Ok(n)
            }
        }
    }

    /// Writes all of `data` at the position (at the end in append mode),
    /// moving it on. Returns the bytes of zeros written to fill a gap
    /// before it (a memory file has no holes).
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<u64> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => io::Write::write_all(f, data).map(|()| 0),
            OpenedFile::Memory(f, pos) => {
                let len = f.meta().size;
                let start = if f.appends() { len } else { *pos };
                *pos = f.write_at(*pos, data)?;
                Ok(if data.is_empty() { 0 } else { start.saturating_sub(len) })
            }
        }
    }

    /// Moves to byte offset `to`, which may be past the end.
    pub fn seek(&mut self, to: u64) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => io::Seek::seek(f, io::SeekFrom::Start(to)).map(|_| ()),
            OpenedFile::Memory(_, pos) => {
                *pos = to;
                Ok(())
            }
        }
    }

    pub fn position(&mut self) -> io::Result<u64> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => io::Seek::stream_position(f),
            OpenedFile::Memory(_, pos) => Ok(*pos),
        }
    }

    /// The size and modification time of the file.
    pub fn meta(&self) -> io::Result<EntryMeta> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => {
                let m = f.metadata()?;
                let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                let modified = modified.map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
                Ok(EntryMeta { kind: EntryKind::File, size: m.len(), modified })
            }
            OpenedFile::Memory(f, _) => Ok(f.meta()),
        }
    }

    /// Truncates or extends (with zeros) the file to `len` bytes. Returns
    /// the bytes of zeros written (by a memory file).
    pub fn set_len(&mut self, len: u64) -> io::Result<u64> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => f.set_len(len).map(|()| 0),
            OpenedFile::Memory(f, _) => {
                let before = f.meta().size;
                f.set_len(len)?;
                Ok(len.saturating_sub(before))
            }
        }
    }

    /// Waits until the file's contents are stored durably.
    pub fn sync(&mut self) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            OpenedFile::Host(f) => f.sync_all(),
            OpenedFile::Memory(..) => Ok(()),
        }
    }
}

/// Whether `e` reports that the process (or the system) has too many files
/// open: closing unreachable ones may help.
pub(crate) fn too_many_open_files(e: &io::Error) -> bool {
    #[cfg(all(feature = "os", unix))]
    return matches!(e.raw_os_error(), Some(code) if code == rustix::io::Errno::MFILE.raw_os_error() || code == rustix::io::Errno::NFILE.raw_os_error());
    // ERROR_TOO_MANY_OPEN_FILES
    #[cfg(all(feature = "os", windows))]
    return e.raw_os_error() == Some(4);
    #[cfg(not(feature = "os"))]
    {
        let _ = e;
        false
    }
}

/// The plain form of a Windows "verbatim" path (`\\?\C:\x` is `C:\x`,
/// `\\?\UNC\server\share\x` is `\\server\share\x`), if it has one.
#[cfg(any(all(feature = "os", windows), test))]
pub(crate) fn plain_from_verbatim(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    let b = rest.as_bytes();
    // `C:` alone would be relative to the drive's current directory.
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\').then(|| rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths_become_plain() {
        assert_eq!(plain_from_verbatim(r"\\?\C:\Users\a.sand").as_deref(), Some(r"C:\Users\a.sand"));
        assert_eq!(plain_from_verbatim(r"\\?\d:\").as_deref(), Some(r"d:\"));
        assert_eq!(plain_from_verbatim(r"\\?\C:"), None);
        assert_eq!(plain_from_verbatim(r"\\?\UNC\server\share\x").as_deref(), Some(r"\\server\share\x"));
        // Not verbatim, or a verbatim form without a plain equivalent.
        assert_eq!(plain_from_verbatim(r"C:\Users"), None);
        assert_eq!(plain_from_verbatim("/home/user"), None);
        assert_eq!(plain_from_verbatim(r"\\?\Volume{0000}\x"), None);
        assert_eq!(plain_from_verbatim(r"\\?\GLOBALROOT\Device"), None);
    }
}
