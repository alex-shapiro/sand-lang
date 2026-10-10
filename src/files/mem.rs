//! An in-memory file system: [`MemFs`].

use super::{EntryKind, EntryMeta, OpenMode};
use crate::vm::iomem::Reservation;
use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Bytes counted per file or directory besides its name and contents.
const NODE_OVERHEAD: usize = 64;

/// An in-memory file system that scripts see instead of the host's, in
/// engines configured with [`Config::memory_fs`](crate::Config::memory_fs)
/// and in every engine built without the `os` feature.
///
/// A `MemFs` is a handle: clones share the same files, so the host can
/// prepare files before a run, read what the script wrote afterwards, and
/// share one file system between engines. Paths are absolute (relative
/// paths start at `/`), use `/` as the separator and are UTF-8; there are
/// no symbolic links. The policy decides access on these paths.
///
/// Contents and a small per-entry overhead are limited by a capacity
/// ([`MemFs::with_capacity`]), separate from the engines' memory limits;
/// writes beyond it fail with an error of kind `limit`.
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
        f.debug_struct("MemFs").field("used", &self.used()).field("capacity", &self.capacity()).finish_non_exhaustive()
    }
}

struct Tree {
    root: Dir,
    shared: Arc<Shared>,
}

/// What the tree and the files open in it share.
struct Shared {
    /// Bytes of contents, names and per-entry overhead.
    used: AtomicUsize,
    capacity: usize,
    /// Counts the writes and moves of files (see [`MemFs::generation`]).
    generation: AtomicU64,
}

impl Shared {
    /// Accounts for `n` more bytes, if they fit.
    fn take(&self, n: usize) -> io::Result<()> {
        let capacity = self.capacity;
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| used.checked_add(n).filter(|&t| t <= capacity))
            .map(|_| ())
            .map_err(|_| error(ErrorKind::QuotaExceeded, &format!("file system full (its capacity is {capacity} bytes)")))
    }

    fn give_back(&self, n: usize) {
        let _ = self.used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| Some(used.saturating_sub(n)));
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }
}

enum Node {
    File(Arc<FileCell>),
    Dir(Dir),
}

/// A file's contents, shared by its directory entry and the files opened
/// on it (`OpenFile`), which keep it when the entry is removed or replaced.
/// Its bytes count against the capacity until the last of them is dropped.
struct FileCell {
    content: Mutex<Content>,
    shared: Arc<Shared>,
}

struct Content {
    data: Vec<u8>,
    /// Unix time in nanoseconds.
    modified: i64,
    /// The generation of the file system when it was last written or moved.
    generation: u64,
}

impl FileCell {
    /// A new file holding `data`, whose bytes the caller has accounted for.
    fn new(shared: &Arc<Shared>, data: Vec<u8>, generation: u64) -> Arc<FileCell> {
        Arc::new(FileCell { content: Mutex::new(Content { data, modified: now(), generation }), shared: shared.clone() })
    }

    fn lock(&self) -> MutexGuard<'_, Content> {
        self.content.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces or extends the contents with `data` at `at` (at the end if
    /// `None`), filling a gap past the end with zeros; returns where the
    /// data ends. The growth must fit in the capacity.
    fn write_at(&self, at: Option<u64>, data: &[u8]) -> io::Result<u64> {
        let mut c = self.lock();
        let start = match at {
            Some(at) => usize::try_from(at).map_err(|_| error(ErrorKind::QuotaExceeded, "file too large"))?,
            None => c.data.len(),
        };
        let end = start.checked_add(data.len()).ok_or_else(|| error(ErrorKind::QuotaExceeded, "file too large"))?;
        if end > c.data.len() {
            self.shared.take(end - c.data.len())?;
            c.data.resize(end, 0);
        }
        c.data[start..end].copy_from_slice(data);
        c.modified = now();
        c.generation = self.shared.next_generation();
        Ok(end as u64)
    }

    /// Truncates or extends (with zeros) the contents to `len` bytes.
    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut c = self.lock();
        let len = usize::try_from(len).map_err(|_| error(ErrorKind::QuotaExceeded, "file too large"))?;
        if len > c.data.len() {
            self.shared.take(len - c.data.len())?;
        } else {
            self.shared.give_back(c.data.len() - len);
        }
        c.data.resize(len, 0);
        let keep = len.max(c.data.capacity() / 2);
        c.data.shrink_to(keep);
        c.modified = now();
        c.generation = self.shared.next_generation();
        Ok(())
    }
}

impl Drop for FileCell {
    fn drop(&mut self) {
        let len = self.content.get_mut().map_or(0, |c| c.data.len());
        self.shared.give_back(len);
    }
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
            Node::File(f) => {
                let c = f.lock();
                EntryMeta { kind: EntryKind::File, size: c.data.len() as u64, modified: Some(c.modified) }
            }
            Node::Dir(d) => EntryMeta { kind: EntryKind::Dir, size: 0, modified: Some(d.modified) },
        }
    }

    /// The bytes the entries of this node and everything below it count
    /// (their names and overhead: the contents of files are given back when
    /// their `FileCell`s are dropped).
    fn footprint(&self, name: &str) -> usize {
        let mut total = NODE_OVERHEAD + name.len();
        if let Node::Dir(d) = self {
            walk(d, |names, _| total += NODE_OVERHEAD + names[names.len() - 1].len());
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
    fn take(&self, n: usize) -> io::Result<()> {
        self.shared.take(n)
    }

    fn give_back(&self, n: usize) {
        self.shared.give_back(n)
    }

    /// Writes (or appends) a whole file at once: nothing changes if it does
    /// not fit.
    fn write(&mut self, parts: &[&str], data: &[u8], append: bool) -> io::Result<()> {
        if parts.is_empty() {
            return Err(error(ErrorKind::IsADirectory, "is a directory"));
        }
        check_path_len(path_len(parts))?;
        let (dir, name) = self.root.parent_mut(parts)?;
        let cell = match dir.entries.get(name) {
            Some(Node::Dir(_)) => return Err(error(ErrorKind::IsADirectory, "is a directory")),
            Some(Node::File(f)) => f.clone(),
            None => {
                self.take(NODE_OVERHEAD + name.len() + data.len())?;
                let generation = self.shared.next_generation();
                let cell = FileCell::new(&self.shared, data.to_vec(), generation);
                let (dir, name) = self.root.parent_mut(parts)?;
                dir.entries.insert(name.to_string(), Node::File(cell));
                dir.modified = now();
                return Ok(());
            }
        };
        let mut c = cell.lock();
        if append {
            self.take(data.len())?;
            c.data.extend_from_slice(data);
        } else {
            let old = c.data.len();
            self.take(data.len().saturating_sub(old))?;
            c.data = data.to_vec();
            self.give_back(old.saturating_sub(data.len()));
        }
        c.modified = now();
        c.generation = self.shared.next_generation();
        Ok(())
    }

    /// The file at `parts`, opened for `mode`: created if the mode creates
    /// files, emptied if it truncates them.
    fn open(&mut self, parts: &[&str], mode: OpenMode) -> io::Result<Arc<FileCell>> {
        if parts.is_empty() {
            return Err(error(ErrorKind::IsADirectory, "is a directory"));
        }
        let (dir, name) = self.root.parent_mut(parts)?;
        match dir.entries.get(name) {
            Some(Node::Dir(_)) => return Err(error(ErrorKind::IsADirectory, "is a directory")),
            Some(Node::File(_)) if mode == OpenMode::CreateNew => return Err(error(ErrorKind::AlreadyExists, "already exists")),
            Some(Node::File(f)) => {
                let f = f.clone();
                if mode.truncates() {
                    f.set_len(0)?;
                }
                return Ok(f);
            }
            None if !mode.creates() => return Err(not_found()),
            None => {}
        }
        check_path_len(path_len(parts))?;
        self.take(NODE_OVERHEAD + name.len())?;
        let generation = self.shared.next_generation();
        let cell = FileCell::new(&self.shared, Vec::new(), generation);
        let (dir, name) = self.root.parent_mut(parts)?;
        dir.entries.insert(name.to_string(), Node::File(cell.clone()));
        dir.modified = now();
        Ok(cell)
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
        // Moved files count as written.
        let generation = self.shared.next_generation();
        let mut node = node;
        let mut moved = vec![&mut node];
        while let Some(n) = moved.pop() {
            match n {
                Node::File(f) => f.lock().generation = generation,
                Node::Dir(d) => moved.extend(d.entries.values_mut()),
            }
        }
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
        let shared = Arc::new(Shared { used: AtomicUsize::new(0), capacity: bytes, generation: AtomicU64::new(0) });
        MemFs { tree: Arc::new(Mutex::new(Tree { root: Dir { entries: BTreeMap::new(), modified: now() }, shared })) }
    }

    fn lock(&self) -> MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.lock().shared.capacity
    }

    /// The bytes in use (contents, names and per-entry overhead), including
    /// the contents of removed files that scripts still have open.
    pub fn used(&self) -> usize {
        self.lock().shared.used.load(Ordering::Relaxed)
    }

    /// Writes a file, creating missing parent directories; replaces an existing file.
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
            Node::File(f) => Ok(f.lock().data.clone()),
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

    /// Removes everything; open files keep their contents, still counted, until closed.
    pub fn clear(&self) {
        let mut t = self.lock();
        let names: usize = t.root.entries.iter().map(|(name, node)| node.footprint(name)).sum();
        t.root.entries.clear();
        t.give_back(names);
    }

    /// Every file with its contents, by path (in lexicographic order of
    /// their components).
    pub fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        self.for_each_file(|path, data| out.push((path.to_path_buf(), data.to_vec())));
        out
    }

    /// A number that grows whenever a file is written (or moved), for
    /// [`MemFs::changed_files`].
    pub fn generation(&self) -> u64 {
        self.lock().shared.generation.load(Ordering::Relaxed)
    }

    /// The files written (or moved) since the file system was at
    /// `generation`, with their contents, in the order of [`MemFs::files`].
    pub fn changed_files(&self, generation: u64) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let t = self.lock();
        walk(&t.root, |names, node| {
            if let Node::File(file) = node {
                let c = file.lock();
                if c.generation > generation {
                    out.push((std::iter::once("/").chain(names.iter().copied()).collect(), c.data.clone()));
                }
            }
        });
        out
    }

    /// Calls `f` with every file and its contents, in the order of
    /// [`MemFs::files`], without copying them. `f` must not use this file
    /// system.
    pub fn for_each_file(&self, mut f: impl FnMut(&Path, &[u8])) {
        let t = self.lock();
        walk(&t.root, |names, node| {
            if let Node::File(file) = node {
                let path: PathBuf = std::iter::once("/").chain(names.iter().copied()).collect();
                f(&path, &file.lock().data);
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
                let c = f.lock();
                res.ensure(c.data.len())?;
                Ok((c.data.clone(), res))
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
            Node::File(f) => {
                let c = f.lock();
                if c.data.len() > max { Err(error(ErrorKind::InvalidData, "source too large")) } else { Ok(c.data.clone()) }
            }
            Node::Dir(_) => Err(not_regular(path)),
        }
    }

    /// Opens the regular file at `path` for `mode`.
    pub(crate) fn open(&self, path: &Path, mode: OpenMode) -> io::Result<OpenFile> {
        let parts = parts(path)?;
        let cell = self.lock().open(&parts, mode)?;
        Ok(OpenFile { cell, append: mode == OpenMode::Append })
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

    /// Removes the directory `path` and everything below it, entry by
    /// entry; `check` is asked about each entry below before it is removed
    /// (see `files::Fs::remove_tree`).
    pub(crate) fn remove_tree<E>(
        &self,
        path: &Path,
        mut check: impl FnMut(&Path) -> Result<io::Result<()>, E>,
    ) -> Result<io::Result<()>, E> {
        let names = |p: &Path| -> io::Result<Vec<String>> {
            let parts = parts(p)?;
            let t = self.lock();
            match t.root.get(&parts)? {
                Node::Dir(d) => Ok(d.entries.keys().cloned().collect()),
                Node::File(_) => Err(error(ErrorKind::NotADirectory, "not a directory")),
            }
        };
        let first = match names(path) {
            Ok(n) => n,
            Err(e) => return Ok(Err(e)),
        };
        let mut levels = vec![(path.to_path_buf(), first)];
        while let Some((dir, left)) = levels.last_mut() {
            let Some(name) = left.pop() else {
                let (done, _) = levels.pop().expect("a level");
                if let Err(e) = self.remove_dir(&done) {
                    return Ok(Err(e));
                }
                continue;
            };
            let child = dir.join(&name);
            if let Err(e) = check(&child)? {
                return Ok(Err(e));
            }
            let is_dir = matches!(self.stat(&child), Ok(m) if m.kind == EntryKind::Dir);
            let r = if is_dir { names(&child).map(|n| levels.push((child, n))) } else { self.remove_file(&child) };
            if let Err(e) = r {
                return Ok(Err(e));
            }
        }
        Ok(Ok(()))
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.lock().rename(&parts(from)?, &parts(to)?)
    }

    /// Copies the contents of the regular file `from` to `to`.
    pub(crate) fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (parts(from)?, parts(to)?);
        let mut t = self.lock();
        let data = match t.root.get(&from) {
            Ok(Node::File(f)) => f.lock().data.clone(),
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

/// A file of a memory file system opened by a script. It keeps its contents
/// when the file is removed, renamed or replaced.
pub(crate) struct OpenFile {
    cell: Arc<FileCell>,
    /// Every write goes to the end.
    append: bool,
}

impl OpenFile {
    /// Copies the bytes at `pos` into `buf`; returns how many.
    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> usize {
        let c = self.cell.lock();
        let start = usize::try_from(pos).unwrap_or(usize::MAX).min(c.data.len());
        let n = buf.len().min(c.data.len() - start);
        buf[..n].copy_from_slice(&c.data[start..start + n]);
        n
    }

    /// Writes `data` at `pos` (at the end in append mode); returns where it
    /// ends. Writing nothing changes nothing (also past the end).
    pub fn write_at(&self, pos: u64, data: &[u8]) -> io::Result<u64> {
        if data.is_empty() {
            return Ok(if self.append { self.cell.lock().data.len() as u64 } else { pos });
        }
        self.cell.write_at(if self.append { None } else { Some(pos) }, data)
    }

    /// Whether every write goes to the end.
    pub fn appends(&self) -> bool {
        self.append
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.cell.set_len(len)
    }

    pub fn meta(&self) -> EntryMeta {
        let c = self.cell.lock();
        EntryMeta { kind: EntryKind::File, size: c.data.len() as u64, modified: Some(c.modified) }
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
        let mut res = Reservation::new(&crate::vm::iomem::IoMemory::new(None, usize::MAX));
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
            kind(fs.read_reserved(Path::new("/"), Reservation::new(&crate::vm::iomem::IoMemory::new(None, usize::MAX)))),
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

    #[test]
    fn open_files_keep_their_contents_and_count_until_closed() {
        let fs = MemFs::with_capacity(1000);
        fs.write("/f", vec![1; 100]).unwrap();
        let f = fs.open(Path::new("/f"), OpenMode::ReadWrite).unwrap();
        fs.remove_file(Path::new("/f")).unwrap();
        // The contents are still there, and still counted.
        assert_eq!(fs.used(), 100);
        let mut buf = [0; 10];
        assert_eq!(f.read_at(95, &mut buf), 5);
        assert_eq!(f.write_at(100, &[2; 50]).unwrap(), 150);
        assert_eq!(fs.used(), 150);
        assert_eq!(f.write_at(0, &[0; 1001]).unwrap_err().kind(), ErrorKind::QuotaExceeded);
        drop(f);
        assert_eq!(fs.used(), 0);
        // Replacing a file keeps the old contents for those who opened it.
        fs.write("/g", "old").unwrap();
        let g = fs.open(Path::new("/g"), OpenMode::Read).unwrap();
        fs.write("/h", "new").unwrap();
        fs.rename(Path::new("/h"), Path::new("/g")).unwrap();
        let mut buf = [0; 3];
        g.read_at(0, &mut buf);
        assert_eq!(&buf, b"old");
        assert_eq!(fs.read("/g").unwrap(), b"new");
        // Modes: truncation, creation, exclusivity.
        assert_eq!(fs.open(Path::new("/g"), OpenMode::CreateNew).err().map(|e| e.kind()), Some(ErrorKind::AlreadyExists));
        assert_eq!(fs.open(Path::new("/none"), OpenMode::ReadWrite).err().map(|e| e.kind()), Some(ErrorKind::NotFound));
        fs.open(Path::new("/g"), OpenMode::Write).unwrap();
        assert_eq!(fs.read("/g").unwrap(), b"");
        let a = fs.open(Path::new("/new"), OpenMode::Append).unwrap();
        a.write_at(0, b"x").unwrap();
        a.write_at(0, b"y").unwrap();
        assert_eq!(fs.read("/new").unwrap(), b"xy");
        drop((g, a));
        fs.clear();
        assert_eq!(fs.used(), 0);
    }

    #[test]
    fn changed_files_since_a_generation() {
        let fs = MemFs::new();
        fs.write("/a/x", "1").unwrap();
        fs.write("/a/y", "2").unwrap();
        let start = fs.generation();
        assert_eq!(fs.changed_files(start), vec![]);
        fs.write("/a/x", "one").unwrap();
        fs.write_file(Path::new("/b"), b"!", true).unwrap();
        fs.rename(Path::new("/a"), Path::new("/c")).unwrap();
        let paths: Vec<PathBuf> = fs.changed_files(start).into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, [PathBuf::from("/b"), PathBuf::from("/c/x"), PathBuf::from("/c/y")]);
        assert_eq!(fs.changed_files(0).len(), 3);
    }
}
