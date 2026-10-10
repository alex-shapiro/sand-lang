//! The file system scripts see: the `fs` module's operations and the
//! loading of script modules (imports and main files) go through an [`Fs`],
//! either the host's file system (with the `os` feature) or a [`MemFs`].
//!
//! A path given to an operation is first resolved by the same `Fs` into a
//! [`Target`] ([`Fs::target`]): its normal form, which the policy checks in
//! the file system's [`PathSpace`], and on the host the directory handles
//! of the walk that found it, through which the operation then acts (see
//! `unix.rs`). Operations run on the VM thread: on the host they are system
//! calls, which count towards the run's CPU time like the rest of its work.

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
    /// The normal form, which the policy decides on: absolute, without
    /// `.` and `..`, and on the host with every symlink before the final
    /// name resolved (and the final one unless the mode is `Entry`), and
    /// existing directories named as stored.
    pub path: PathBuf,
    #[cfg(feature = "os")]
    host: Option<host::Resolved>,
}

impl Target {
    /// Whether the final name is known to be spelled as stored. On a case-
    /// or normalization-insensitive file system an existing entry can be
    /// named differently; the policy's deny rules equate every such
    /// spelling (`policy::fold_name`), so only decisions that compare it
    /// exactly need [`Target::fix_spelling`].
    pub fn exact(&self) -> bool {
        #[cfg(feature = "os")]
        if let Some(h) = &self.host {
            return h.exact();
        }
        true
    }

    /// Names an existing final entry as stored.
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

    fn memory(path: PathBuf) -> Target {
        Target {
            path,
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

    /// Resolves the absolute path `p` for an operation (lexically in memory).
    pub fn target(&self, p: &Path, mode: Mode) -> Target {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let h = host::resolve(p, mode);
                Target { path: h.path(), host: Some(h) }
            }
            Fs::Memory(_) => {
                let _ = mode;
                Target::memory(mem::normalize(p))
            }
        }
    }

    /// The normal form of the absolute path `p` that the policy decides on
    /// (see [`crate::policy::normalize_path`]; lexical in memory).
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

    /// Writes (or appends) `data` to the file at `t`, creating it.
    pub fn write(&self, t: Target, data: &[u8], append: bool) -> io::Result<()> {
        match self {
            #[cfg(feature = "os")]
            Fs::Host => {
                let (path, h) = t.host()?;
                host::write_all(h.open_write(&path, append)?, data)
            }
            Fs::Memory(m) => m.write_file(&t.path, data, append),
        }
    }

    /// Metadata of the target (a final symlink is not followed in `Entry`
    /// mode).
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
