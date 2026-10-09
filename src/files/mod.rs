//! The file system scripts see: the `fs` module's operations and the
//! loading of script modules (imports and main files) go through an [`Fs`],
//! either the host's file system (with the `os` feature) or a [`MemFs`].
//!
//! Paths given to the operations are normalized by the same `Fs`
//! ([`Fs::resolve`], [`Fs::entry`]) and checked against the policy in its
//! [`PathSpace`] before the operation runs. Host operations block, so they
//! run off the VM thread; memory operations complete at once.

#[cfg(feature = "os")]
mod host;
mod mem;
#[cfg(feature = "os")]
mod safe;

pub use mem::MemFs;
pub(crate) use mem::normalize as memory_path;

use crate::policy::{EntryPath, PathSpace};
use crate::vm::iomem::Reservation;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

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

/// A file system scripts can see.
#[derive(Clone)]
pub(crate) enum Fs {
    /// The host's file system.
    #[cfg(feature = "os")]
    Host,
    Memory(MemFs),
}

impl Default for Fs {
    /// The host's file system with the `os` feature, otherwise an empty
    /// memory file system.
    fn default() -> Fs {
        #[cfg(feature = "os")]
        return Fs::Host;
        #[cfg(not(feature = "os"))]
        return Fs::Memory(MemFs::new());
    }
}

impl Fs {
    /// The namespace of this file system's paths (for policy checks).
    pub fn space(&self) -> PathSpace {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => PathSpace::Host,
            Fs::Memory(_) => PathSpace::Memory,
        }
    }

    /// The memory file system, if this is one.
    pub fn memory(&self) -> Option<&MemFs> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => None,
            Fs::Memory(m) => Some(m),
        }
    }

    /// Whether operations block (and must run off the VM thread).
    pub fn is_blocking(&self) -> bool {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => true,
            Fs::Memory(_) => false,
        }
    }

    /// The directory of relative paths when none is configured: the
    /// process's working directory, or the root of a memory file system.
    pub fn default_dir(&self) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => std::env::current_dir().unwrap_or_default(),
            Fs::Memory(_) => PathBuf::from("/"),
        }
    }

    /// `p` made absolute (against the process's working directory, or the
    /// root of a memory file system), not otherwise normalized.
    pub fn absolute(&self, p: &Path) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()),
            Fs::Memory(_) => mem::normalize(p),
        }
    }

    /// The normal form of the absolute path `p` that the policy decides on
    /// (see [`crate::policy::normalize_path`]; lexical in memory).
    pub fn resolve(&self, p: &Path) -> PathBuf {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => crate::policy::normalize_path(p),
            Fs::Memory(_) => mem::normalize(p),
        }
    }

    /// Like [`Fs::resolve`] for the directory entry at `p` itself (a final
    /// symlink is not resolved).
    pub fn entry(&self, p: &Path) -> EntryPath {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => crate::policy::entry_path(p),
            Fs::Memory(_) => EntryPath { path: mem::normalize(p), scanned: 0, scanned_bytes: 0 },
        }
    }

    /// The ancestors of the normalized `path` that do not exist, outermost first.
    pub fn missing_ancestors(&self, path: &Path) -> Vec<PathBuf> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::missing_ancestors(path),
            Fs::Memory(m) => {
                let mut missing: Vec<PathBuf> = path.ancestors().skip(1).take_while(|a| !m.exists(a)).map(Path::to_path_buf).collect();
                missing.reverse();
                missing
            }
        }
    }

    /// Reads a regular file into a buffer reserved in `res`.
    pub fn read(&self, path: &Path, res: Reservation) -> io::Result<(Vec<u8>, Reservation)> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::read_reserved(path, res),
            Fs::Memory(m) => m.read_reserved(path, res),
        }
    }

    /// Writes (or appends) `data` to the file at `path`, creating it.
    pub fn write(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::write(path, data, append),
            Fs::Memory(m) => m.write_file(path, data, append),
        }
    }

    pub fn stat(&self, path: &Path) -> io::Result<EntryMeta> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::stat(path),
            Fs::Memory(m) => m.stat(path),
        }
    }

    /// The names in the directory `path`, sorted, reserved in `res`.
    pub fn list_dir(&self, path: &Path, mut res: Reservation) -> io::Result<(Vec<String>, Reservation)> {
        let mut names = match self {
            #[cfg(feature = "os")]
            Fs::Host => host::list_dir(path, &mut res)?,
            Fs::Memory(m) => m.list_dir(path, &mut res)?,
        };
        names.sort();
        Ok((names, res))
    }

    pub fn create_dir(&self, path: &Path) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => safe::create_dir(path),
            Fs::Memory(m) => m.create_dir(path),
        }
    }

    /// Creates `path` and its missing ancestors, of which the first `keep`
    /// components must exist (they were not checked by the policy).
    pub fn create_dir_all(&self, path: &Path, keep: usize) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => safe::create_dir_all(path, keep),
            Fs::Memory(m) => {
                // Nothing changes between the policy check and the operation.
                let _ = keep;
                m.make_dir_all(path)
            }
        }
    }

    pub fn remove_file(&self, path: &Path) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => safe::remove_file(path),
            Fs::Memory(m) => m.remove_file(path),
        }
    }

    /// Removes the empty directory `path`.
    pub fn remove_dir(&self, path: &Path) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => safe::remove_dir(path),
            Fs::Memory(m) => m.remove_dir(path),
        }
    }

    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => safe::rename(from, to),
            Fs::Memory(m) => m.rename(from, to),
        }
    }

    /// Copies the contents of the regular file `from` to `to`.
    pub fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::copy(from, to),
            Fs::Memory(m) => m.copy(from, to),
        }
    }

    /// Runs `op` on this file system, off the VM thread if operations
    /// block (see [`Fs::is_blocking`]).
    pub async fn run_blocking<T: Send + 'static>(self, op: impl FnOnce(&Fs) -> io::Result<T> + Send + 'static) -> io::Result<T> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => host::blocking(move || op(&Fs::Host)).await,
            Fs::Memory(_) => op(&self),
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
    /// sources are detected without reading them whole) without following
    /// symlinks.
    pub fn read_source(&self, path: &Path, max: usize) -> io::Result<String> {
        use io::{Error, ErrorKind};
        let data = match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                use io::Read;
                let mut data = Vec::new();
                safe::open_read(path)?.take(max as u64 + 1).read_to_end(&mut data)?;
                data
            }
            Fs::Memory(m) => m.read_source(path, max)?,
        };
        if data.len() > max {
            return Err(Error::new(ErrorKind::InvalidData, "source too large"));
        }
        String::from_utf8(data).map_err(|_| Error::new(ErrorKind::InvalidData, "source is not valid UTF-8"))
    }
}

/// Whether the host's file system worker is running an operation (the
/// scheduler then waits for it briefly instead of parking).
pub(crate) fn worker_busy() -> bool {
    #[cfg(feature = "os")]
    return host::FsWorker::busy();
    #[cfg(not(feature = "os"))]
    return false;
}
