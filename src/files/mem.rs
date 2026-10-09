//! An in-memory file system: [`MemFs`].

use super::{EntryKind, EntryMeta};
use crate::vm::iomem::Reservation;
use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Bytes counted per file or directory besides its name and contents.
const NODE_OVERHEAD: usize = 64;

/// An in-memory file system for scripts: the `fs` module and script imports
/// of engines configured with [`Config::memory_fs`](crate::Config::memory_fs)
/// (and of every engine built without the `os` feature) see it instead of
/// the host's file system.
///
/// A `MemFs` is a handle: clones share the same files, so the host can
/// prepare files before a run and read what the script wrote afterwards,
/// and several engines can share one file system. Paths are absolute
/// (relative paths are taken from the root, `/`), use `/` as the separator,
/// and name UTF-8 entries; there are no symbolic links. The policy decides
/// access as for the host's file system, on these paths.
///
/// Its contents (with a small overhead per entry) are limited to a capacity
/// ([`MemFs::with_capacity`]); writes beyond it fail with an error of kind
/// `limit`. The capacity is separate from the engines' memory limits.
///
/// ```
/// use sandlang::MemFs;
/// let fs = MemFs::new();
/// fs.write("/data/hello.txt", "hi").unwrap();
/// assert_eq!(fs.read("/data/hello.txt").unwrap(), b"hi");
/// assert_eq!(fs.files(), vec![("/data/hello.txt".into(), b"hi".to_vec())]);
/// ```
#[derive(Clone)]
pub struct MemFs {
    tree: Arc<Mutex<Tree>>,
}

impl Default for MemFs {
    fn default() -> Self {
        MemFs::new()
    }
}

impl std::fmt::Debug for MemFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = self.lock();
        f.debug_struct("MemFs").field("used", &t.used).field("capacity", &t.capacity).finish_non_exhaustive()
    }
}

struct Tree {
    root: Dir,
    /// Bytes of contents, names and per-entry overhead.
    used: usize,
    capacity: usize,
}

enum Node {
    File(File),
    Dir(Dir),
}

struct File {
    data: Vec<u8>,
    /// Unix time in nanoseconds.
    modified: i64,
}

#[derive(Default)]
struct Dir {
    entries: BTreeMap<String, Node>,
    modified: i64,
}

/// Paths are at most this many bytes long (as script paths are on the
/// host): every entry can be named, and the tree is not deeper than this.
const MAX_PATH: usize = crate::policy::MAX_PATH_BYTES;

/// The length of the path `/a/b` named by `parts`.
fn path_len(parts: &[&str]) -> usize {
    parts.iter().map(|p| p.len() + 1).sum()
}

fn check_path_len(len: usize) -> io::Result<()> {
    if len > MAX_PATH { Err(error(ErrorKind::InvalidFilename, &format!("path too long (the limit is {MAX_PATH} bytes)"))) } else { Ok(()) }
}

impl Drop for Dir {
    /// Drops the tree without recursion (it may be deep): every directory
    /// below is dropped empty.
    fn drop(&mut self) {
        let mut stack = vec![std::mem::take(&mut self.entries)];
        while let Some(entries) = stack.pop() {
            for (_, node) in entries {
                if let Node::Dir(mut d) = node {
                    stack.push(std::mem::take(&mut d.entries));
                }
            }
        }
    }
}

/// Walks the tree below `dir` without recursion, calling `visit` with each
/// entry's path relative to `dir` (as components) and the entry.
fn walk<'a>(dir: &'a Dir, mut visit: impl FnMut(&[&'a str], &'a Node)) {
    let mut names: Vec<&'a str> = Vec::new();
    let mut stack = vec![dir.entries.iter()];
    while let Some(iter) = stack.last_mut() {
        let Some((name, node)) = iter.next() else {
            stack.pop();
            names.pop();
            continue;
        };
        names.push(name);
        visit(&names, node);
        match node {
            Node::Dir(d) => stack.push(d.entries.iter()),
            Node::File(_) => {
                names.pop();
            }
        }
    }
}

impl Node {
    fn meta(&self) -> EntryMeta {
        match self {
            Node::File(f) => EntryMeta { kind: EntryKind::File, size: f.data.len() as u64, modified: Some(f.modified) },
            Node::Dir(d) => EntryMeta { kind: EntryKind::Dir, size: 0, modified: Some(d.modified) },
        }
    }

    /// The bytes this node and everything below it count.
    fn footprint(&self, name: &str) -> usize {
        let own = |name: &str, node: &Node| NODE_OVERHEAD + name.len() + if let Node::File(f) = node { f.data.len() } else { 0 };
        let mut total = own(name, self);
        if let Node::Dir(d) = self {
            walk(d, |names, node| total += own(names[names.len() - 1], node));
        }
        total
    }

    /// The length of the longest path below this node, relative to it.
    fn depth_len(&self) -> usize {
        let mut longest = 0;
        if let Node::Dir(d) = self {
            walk(d, |names, _| longest = longest.max(path_len(names)));
        }
        longest
    }

    /// The number of entries below this node.
    fn entries_below(&self) -> usize {
        let mut n = 0;
        if let Node::Dir(d) = self {
            walk(d, |_, _| n += 1);
        }
        n
    }
}

fn now() -> i64 {
    let since = crate::platform::SystemTime::now().duration_since(crate::platform::UNIX_EPOCH);
    since.map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)).unwrap_or(0)
}

fn error(kind: ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message.to_string())
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, format!("{} is not a regular file", path.display()))
}

fn not_found() -> io::Error {
    error(ErrorKind::NotFound, "no such file or directory")
}

/// The names along `path`, from the root (`.` and `..` resolved lexically;
/// `..` at the root stays there).
fn parts(path: &Path) -> io::Result<Vec<&str>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name.to_str().ok_or_else(|| error(ErrorKind::InvalidInput, "path is not valid UTF-8"))?),
            Component::Prefix(_) => {
                return Err(error(ErrorKind::InvalidInput, "drive and UNC prefixes do not exist in a memory file system"));
            }
        }
    }
    Ok(out)
}

/// `path` as an absolute, lexically normal path of a memory file system.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

impl Dir {
    fn get(&self, parts: &[&str]) -> io::Result<&Node> {
        let (last, dirs) = parts.split_last().ok_or_else(|| error(ErrorKind::InvalidInput, "the root directory"))?;
        let mut dir = self;
        for name in dirs {
            dir = match dir.entries.get(*name) {
                Some(Node::Dir(d)) => d,
                Some(Node::File(_)) => return Err(error(ErrorKind::NotADirectory, "not a directory")),
                None => return Err(not_found()),
            };
        }
        dir.entries.get(*last).ok_or_else(not_found)
    }

    fn dir(&self, parts: &[&str]) -> io::Result<&Dir> {
        if parts.is_empty() {
            return Ok(self);
        }
        match self.get(parts)? {
            Node::Dir(d) => Ok(d),
            Node::File(_) => Err(error(ErrorKind::NotADirectory, "not a directory")),
        }
    }

    fn dir_mut(&mut self, parts: &[&str]) -> io::Result<&mut Dir> {
        let mut dir = self;
        for name in parts {
            dir = match dir.entries.get_mut(*name) {
                Some(Node::Dir(d)) => d,
                Some(Node::File(_)) => return Err(error(ErrorKind::NotADirectory, "not a directory")),
                None => return Err(not_found()),
            };
        }
        Ok(dir)
    }

    /// The directory containing the entry `parts` (which must not be the
    /// root), and the entry's name.
    fn parent_mut<'p>(&mut self, parts: &[&'p str]) -> io::Result<(&mut Dir, &'p str)> {
        let (last, dirs) = parts.split_last().ok_or_else(|| error(ErrorKind::PermissionDenied, "the root directory cannot be changed"))?;
        Ok((self.dir_mut(dirs)?, last))
    }
}

impl Tree {
    /// Accounts for `n` more bytes, if they fit.
    fn take(&mut self, n: usize) -> io::Result<()> {
        match self.used.checked_add(n) {
            Some(total) if total <= self.capacity => {
                self.used = total;
                Ok(())
            }
            _ => Err(error(ErrorKind::QuotaExceeded, &format!("file system full (its capacity is {} bytes)", self.capacity))),
        }
    }

    fn give_back(&mut self, n: usize) {
        self.used = self.used.saturating_sub(n);
    }

    fn write(&mut self, parts: &[&str], data: &[u8], append: bool) -> io::Result<()> {
        if parts.is_empty() {
            return Err(error(ErrorKind::IsADirectory, "is a directory"));
        }
        check_path_len(path_len(parts))?;
        let (dir, name) = self.root.parent_mut(parts)?;
        let old = match dir.entries.get(name) {
            Some(Node::Dir(_)) => return Err(error(ErrorKind::IsADirectory, "is a directory")),
            Some(Node::File(f)) => Some(f.data.len()),
            None => None,
        };
        let extra = match (old, append) {
            (Some(_), true) => data.len(),
            (Some(len), false) => data.len().saturating_sub(len),
            (None, _) => NODE_OVERHEAD + name.len() + data.len(),
        };
        self.take(extra)?;
        let (dir, name) = self.root.parent_mut(parts)?;
        let t = now();
        match dir.entries.get_mut(name) {
            Some(Node::File(f)) => {
                let freed = if append {
                    f.data.extend_from_slice(data);
                    0
                } else {
                    let freed = f.data.len().saturating_sub(data.len());
                    f.data = data.to_vec();
                    freed
                };
                f.modified = t;
                self.give_back(freed);
            }
            _ => {
                dir.entries.insert(name.to_string(), Node::File(File { data: data.to_vec(), modified: t }));
                dir.modified = t;
            }
        }
        Ok(())
    }

    fn create_dir(&mut self, parts: &[&str]) -> io::Result<()> {
        let (dir, name) = self.root.parent_mut(parts).map_err(|e| match e.kind() {
            ErrorKind::PermissionDenied => error(ErrorKind::AlreadyExists, "already exists"),
            _ => e,
        })?;
        if dir.entries.contains_key(name) {
            return Err(error(ErrorKind::AlreadyExists, "already exists"));
        }
        check_path_len(path_len(parts))?;
        self.take(NODE_OVERHEAD + name.len())?;
        let (dir, name) = self.root.parent_mut(parts)?;
        let t = now();
        dir.entries.insert(name.to_string(), Node::Dir(Dir { entries: BTreeMap::new(), modified: t }));
        dir.modified = t;
        Ok(())
    }

    fn create_dir_all(&mut self, parts: &[&str]) -> io::Result<()> {
        for i in 1..=parts.len() {
            match self.root.get(&parts[..i]) {
                Ok(Node::Dir(_)) => {}
                // As `mkdir -p`: a file in place of the directory exists already,
                // one in place of a directory above it is not a directory.
                Ok(Node::File(_)) if i == parts.len() => return Err(error(ErrorKind::AlreadyExists, "already exists")),
                Ok(Node::File(_)) => return Err(error(ErrorKind::NotADirectory, "not a directory")),
                Err(e) if e.kind() == ErrorKind::NotFound => self.create_dir(&parts[..i])?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Removes the entry `parts` if `check` accepts it.
    fn remove(&mut self, parts: &[&str], check: impl FnOnce(&Node) -> io::Result<()>) -> io::Result<()> {
        let (dir, name) = self.root.parent_mut(parts)?;
        check(dir.entries.get(name).ok_or_else(not_found)?)?;
        let node = dir.entries.remove(name).ok_or_else(not_found)?;
        dir.modified = now();
        let freed = node.footprint(name);
        self.give_back(freed);
        Ok(())
    }

    fn rename(&mut self, from: &[&str], to: &[&str]) -> io::Result<()> {
        if from.is_empty() {
            return Err(error(ErrorKind::PermissionDenied, "the root directory cannot be moved"));
        }
        let source = self.root.get(from)?;
        let source_is_dir = matches!(source, Node::Dir(_));
        if to.len() > from.len() && to.starts_with(from) {
            return Err(if source_is_dir {
                error(ErrorKind::InvalidInput, "cannot move a directory into itself")
            } else {
                error(ErrorKind::NotADirectory, "not a directory")
            });
        }
        if from == to {
            return Ok(());
        }
        // Every path below the moved entry stays within the limit.
        check_path_len(path_len(to).saturating_add(source.depth_len()))?;
        if to.is_empty() {
            return Err(error(ErrorKind::PermissionDenied, "the root directory cannot be replaced"));
        }
        // The destination's directory must exist; what is there may be replaced.
        let replaced = match self.root.dir(&to[..to.len() - 1])?.entries.get(to[to.len() - 1]) {
            None => 0,
            Some(Node::File(_)) if source_is_dir => return Err(error(ErrorKind::NotADirectory, "not a directory")),
            Some(Node::Dir(_)) if !source_is_dir => return Err(error(ErrorKind::IsADirectory, "is a directory")),
            Some(Node::Dir(d)) if !d.entries.is_empty() => return Err(error(ErrorKind::DirectoryNotEmpty, "directory not empty")),
            Some(node) => node.footprint(to[to.len() - 1]),
        };
        let (from_name, to_name) = (from[from.len() - 1], to[to.len() - 1]);
        // The replaced entry is given back first.
        let freed = replaced + from_name.len();
        if to_name.len() > freed {
            self.take(to_name.len() - freed)?;
        }
        let t = now();
        let (dir, name) = self.root.parent_mut(from)?;
        let node = dir.entries.remove(name).ok_or_else(not_found)?;
        dir.modified = t;
        let (dir, name) = self.root.parent_mut(to)?;
        dir.entries.insert(name.to_string(), node);
        dir.modified = t;
        self.give_back(freed.saturating_sub(to_name.len()));
        Ok(())
    }
}

impl MemFs {
    /// The capacity of [`MemFs::new`]: 64 MiB.
    pub const DEFAULT_CAPACITY: usize = 64 << 20;

    /// An empty file system of [`MemFs::DEFAULT_CAPACITY`] bytes.
    pub fn new() -> MemFs {
        MemFs::with_capacity(MemFs::DEFAULT_CAPACITY)
    }

    /// An empty file system holding at most `bytes` bytes of contents and
    /// names (each file and directory counts 64 bytes more).
    pub fn with_capacity(bytes: usize) -> MemFs {
        let tree = Tree { root: Dir { entries: BTreeMap::new(), modified: now() }, used: 0, capacity: bytes };
        MemFs { tree: Arc::new(Mutex::new(tree)) }
    }

    fn lock(&self) -> MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// The bytes in use (contents, names and per-entry overhead).
    pub fn used(&self) -> usize {
        self.lock().used
    }

    /// Writes a file, creating its directory (and theirs) if need be, and
    /// replacing the file if it exists.
    pub fn write(&self, path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
        let parts = parts(path.as_ref())?;
        let mut t = self.lock();
        if let Some((_, dirs)) = parts.split_last() {
            t.create_dir_all(dirs)?;
        }
        t.write(&parts, contents.as_ref(), false)
    }

    /// The contents of a file.
    pub fn read(&self, path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let parts = parts(path.as_ref())?;
        match self.lock().root.get(&parts)? {
            Node::File(f) => Ok(f.data.clone()),
            Node::Dir(_) => Err(error(ErrorKind::IsADirectory, "is a directory")),
        }
    }

    /// Creates a directory and any missing directories above it.
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let parts = parts(path.as_ref())?;
        self.lock().create_dir_all(&parts)
    }

    /// Removes a file, or a directory with everything in it.
    pub fn remove(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let parts = parts(path.as_ref())?;
        self.lock().remove(&parts, |_| Ok(()))
    }

    /// Removes everything.
    pub fn clear(&self) {
        let mut t = self.lock();
        t.root.entries.clear();
        t.used = 0;
    }

    /// Every file with its contents, by path (in lexicographic order of
    /// their components).
    pub fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        self.for_each_file(|path, data| out.push((path.to_path_buf(), data.to_vec())));
        out
    }

    /// Calls `f` with every file and its contents, in the order of
    /// [`MemFs::files`], without copying them. (`f` must not use this file
    /// system.)
    pub fn for_each_file(&self, mut f: impl FnMut(&Path, &[u8])) {
        let t = self.lock();
        walk(&t.root, |names, node| {
            if let Node::File(file) = node {
                let path: PathBuf = std::iter::once("/").chain(names.iter().copied()).collect();
                f(&path, &file.data);
            }
        });
    }

    // ------------------------------------------------------------------
    // Script operations (see `files::Fs`); paths are normalized.

    pub(crate) fn read_reserved(&self, path: &Path, mut res: Reservation) -> io::Result<(Vec<u8>, Reservation)> {
        let parts = parts(path)?;
        let t = self.lock();
        match t.root.get(&parts) {
            Ok(Node::File(f)) => {
                res.ensure(f.data.len())?;
                Ok((f.data.clone(), res))
            }
            Ok(Node::Dir(_)) => Err(not_regular(path)),
            Err(_) if parts.is_empty() => Err(not_regular(path)),
            Err(e) => Err(e),
        }
    }

    /// The contents of a script module, if it has at most `max` bytes.
    pub(crate) fn read_source(&self, path: &Path, max: usize) -> io::Result<Vec<u8>> {
        let parts = parts(path)?;
        match self.lock().root.get(&parts)? {
            Node::File(f) if f.data.len() > max => Err(error(ErrorKind::InvalidData, "source too large")),
            Node::File(f) => Ok(f.data.clone()),
            Node::Dir(_) => Err(not_regular(path)),
        }
    }

    /// The number of entries below `path` (the work of moving it).
    pub(crate) fn entries_below(&self, path: &Path) -> usize {
        let Ok(parts) = parts(path) else { return 0 };
        self.lock().root.get(&parts).map_or(0, Node::entries_below)
    }

    pub(crate) fn write_file(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()> {
        self.lock().write(&parts(path)?, data, append)
    }

    pub(crate) fn stat(&self, path: &Path) -> io::Result<EntryMeta> {
        let parts = parts(path)?;
        let t = self.lock();
        if parts.is_empty() {
            return Ok(EntryMeta { kind: EntryKind::Dir, size: 0, modified: Some(t.root.modified) });
        }
        t.root.get(&parts).map(Node::meta)
    }

    /// The names in a directory, in order.
    pub(crate) fn list_dir(&self, path: &Path, res: &mut Reservation) -> io::Result<Vec<String>> {
        let parts = parts(path)?;
        let t = self.lock();
        let dir = t.root.dir(&parts)?;
        let mut names = Vec::with_capacity(dir.entries.len());
        for name in dir.entries.keys() {
            res.grow(name.len() + 2 * std::mem::size_of::<String>())?;
            names.push(name.clone());
        }
        Ok(names)
    }

    pub(crate) fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.lock().create_dir(&parts(path)?)
    }

    pub(crate) fn make_dir_all(&self, path: &Path) -> io::Result<()> {
        self.lock().create_dir_all(&parts(path)?)
    }

    pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.lock().remove(&parts(path)?, |n| match n {
            Node::File(_) => Ok(()),
            Node::Dir(_) => Err(error(ErrorKind::IsADirectory, "is a directory")),
        })
    }

    pub(crate) fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.lock().remove(&parts(path)?, |n| match n {
            Node::Dir(d) if d.entries.is_empty() => Ok(()),
            Node::Dir(_) => Err(error(ErrorKind::DirectoryNotEmpty, "directory not empty")),
            Node::File(_) => Err(error(ErrorKind::NotADirectory, "not a directory")),
        })
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.lock().rename(&parts(from)?, &parts(to)?)
    }

    /// Copies the contents of the regular file `from` to `to`.
    pub(crate) fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (parts(from)?, parts(to)?);
        let mut t = self.lock();
        let data = match t.root.get(&from) {
            Ok(Node::File(f)) => f.data.clone(),
            Ok(Node::Dir(_)) => return Err(not_regular(Path::new("source"))),
            Err(_) if from.is_empty() => return Err(not_regular(Path::new("/"))),
            Err(e) => return Err(e),
        };
        t.write(&to, &data, false)
    }

    /// Whether anything is at `path`.
    pub(crate) fn exists(&self, path: &Path) -> bool {
        parts(path).is_ok_and(|p| p.is_empty() || self.lock().root.get(&p).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(e: io::Result<impl std::fmt::Debug>) -> ErrorKind {
        e.unwrap_err().kind()
    }

    #[test]
    fn files_and_directories() {
        let fs = MemFs::new();
        fs.write("/a/b/c.txt", "hello").unwrap();
        assert_eq!(fs.read("/a/b/c.txt").unwrap(), b"hello");
        assert_eq!(fs.read("a/./b/../b/c.txt").unwrap(), b"hello", "relative to the root, lexically normalized");
        assert_eq!(kind(fs.read("/a/b")), ErrorKind::IsADirectory);
        assert_eq!(kind(fs.read("/a/b/c.txt/d")), ErrorKind::NotADirectory);
        assert_eq!(kind(fs.read("/nope")), ErrorKind::NotFound);
        fs.write_file(Path::new("/a/b/c.txt"), b" world", true).unwrap();
        assert_eq!(fs.read("/a/b/c.txt").unwrap(), b"hello world");
        assert_eq!(
            kind(fs.write_file(Path::new("/x/y.txt"), b"", false)),
            ErrorKind::NotFound,
            "scripts do not create directories implicitly"
        );
        assert_eq!(kind(fs.create_dir(Path::new("/a"))), ErrorKind::AlreadyExists);
        assert_eq!(kind(fs.remove_dir(Path::new("/a"))), ErrorKind::DirectoryNotEmpty);
        assert_eq!(kind(fs.remove_file(Path::new("/a"))), ErrorKind::IsADirectory);
        let mut res = Reservation::new(&crate::vm::iomem::IoMemory::new(None));
        assert_eq!(fs.list_dir(Path::new("/a/b"), &mut res).unwrap(), vec!["c.txt"]);
        assert_eq!(fs.stat(Path::new("/")).unwrap().kind, EntryKind::Dir);
        assert_eq!(fs.stat(Path::new("/a/b/c.txt")).unwrap().size, 11);
    }

    #[test]
    fn rename_follows_posix_rules() {
        let fs = MemFs::new();
        fs.write("/d/f", "1").unwrap();
        fs.write("/e/g", "2").unwrap();
        fs.create_dir_all("/empty").unwrap();
        assert_eq!(kind(fs.rename(Path::new("/d"), Path::new("/d/inner"))), ErrorKind::InvalidInput);
        assert_eq!(kind(fs.rename(Path::new("/d"), Path::new("/e"))), ErrorKind::DirectoryNotEmpty);
        assert_eq!(kind(fs.rename(Path::new("/d/f"), Path::new("/e"))), ErrorKind::IsADirectory);
        assert_eq!(kind(fs.rename(Path::new("/d"), Path::new("/e/g"))), ErrorKind::NotADirectory);
        fs.rename(Path::new("/d"), Path::new("/empty")).unwrap();
        assert_eq!(fs.read("/empty/f").unwrap(), b"1");
        fs.rename(Path::new("/empty/f"), Path::new("/e/g")).unwrap();
        assert_eq!(fs.files(), vec![(PathBuf::from("/e/g"), b"1".to_vec())]);
        assert_eq!(kind(fs.rename(Path::new("/"), Path::new("/x"))), ErrorKind::PermissionDenied);
    }

    #[test]
    fn edge_cases_match_the_host() {
        let fs = MemFs::with_capacity(200);
        fs.write("/f", "x").unwrap();
        assert_eq!(kind(fs.rename(Path::new("/f"), Path::new("/f/g"))), ErrorKind::NotADirectory);
        assert_eq!(kind(fs.write_file(Path::new("/"), b"", false)), ErrorKind::IsADirectory);
        assert_eq!(
            kind(fs.read_reserved(Path::new("/"), Reservation::new(&crate::vm::iomem::IoMemory::new(None)))),
            ErrorKind::InvalidInput
        );
        // Replacing a file frees its space before the new name is charged.
        fs.write("/a-much-longer-name", vec![0; 200 - 2 * NODE_OVERHEAD - 2 - 18]).unwrap();
        assert_eq!(fs.used(), 200);
        fs.rename(Path::new("/f"), Path::new("/a-much-longer-name")).unwrap();
        assert_eq!(fs.used(), NODE_OVERHEAD + 18 + 1);
        // Paths are limited, so the tree's depth is too.
        let long = "/d".repeat(MAX_PATH / 2 + 1);
        assert_eq!(kind(MemFs::new().create_dir_all(&long)), ErrorKind::InvalidFilename);
    }

    #[test]
    fn capacity_is_enforced_and_accounted_exactly() {
        let fs = MemFs::with_capacity(1000);
        fs.write("/f", vec![0; 500]).unwrap();
        assert_eq!(fs.used(), NODE_OVERHEAD + 1 + 500);
        assert_eq!(kind(fs.write("/g", vec![0; 500])), ErrorKind::QuotaExceeded);
        fs.write("/f", vec![0; 10]).unwrap();
        assert_eq!(fs.used(), NODE_OVERHEAD + 1 + 10);
        fs.create_dir_all("/d/e").unwrap();
        fs.rename(Path::new("/f"), Path::new("/d/e/longer-name")).unwrap();
        fs.copy(Path::new("/d/e/longer-name"), Path::new("/d/copy")).unwrap();
        fs.remove("/d").unwrap();
        assert_eq!(fs.used(), 0, "everything given back");
        assert!(fs.files().is_empty());
    }
}
