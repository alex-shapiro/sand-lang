//! The host's file system (the `os` feature): operations through [`safe`],
//! which never follows a symlink, run off the VM thread.

use super::EntryMeta;
use super::safe;
use crate::vm::iomem::Reservation;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs a blocking file system operation `op` off the VM thread: on the
/// process's hot file system worker if idle, otherwise on tokio's blocking
/// pool.
pub(crate) async fn blocking<T: Send + 'static>(op: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job: Box<dyn FnOnce() + Send> = Box::new(move || {
        let _ = tx.send(op());
    });
    match FsWorker::get().try_run(job) {
        Ok(()) => rx.await.unwrap_or_else(|e| Err(io::Error::other(e))),
        Err(job) => {
            tokio::task::spawn_blocking(job).await.map_err(io::Error::other)?;
            rx.await.unwrap_or_else(|e| Err(io::Error::other(e)))
        }
    }
}

/// A thread that runs file system operations one at a time and stays awake
/// in between. It takes a job only when idle, so operations never queue
/// behind a slow one, and engines run operations in parallel.
pub(crate) struct FsWorker {
    jobs: std::sync::Mutex<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>>,
    busy: std::sync::atomic::AtomicBool,
}

impl FsWorker {
    const LINGER: std::time::Duration = std::time::Duration::from_micros(50);

    fn get() -> &'static FsWorker {
        static WORKER: std::sync::OnceLock<FsWorker> = std::sync::OnceLock::new();
        WORKER.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
            let spawned = std::thread::Builder::new().name("sand-fs".into()).spawn(move || {
                let worker = FsWorker::get();
                while let Ok(mut job) = rx.recv() {
                    loop {
                        job();
                        worker.busy.store(false, std::sync::atomic::Ordering::Release);
                        let until = std::time::Instant::now() + FsWorker::LINGER;
                        let next = loop {
                            match rx.try_recv() {
                                Ok(j) => break Some(j),
                                Err(_) if std::time::Instant::now() < until => std::hint::spin_loop(),
                                Err(_) => break None,
                            }
                        };
                        match next {
                            Some(j) => job = j,
                            None => break,
                        }
                    }
                }
            });
            let busy = std::sync::atomic::AtomicBool::new(spawned.is_err());
            FsWorker { jobs: std::sync::Mutex::new(tx), busy }
        })
    }

    /// Hands a job to the worker if it is idle; gives it back otherwise.
    fn try_run(&self, job: Box<dyn FnOnce() + Send>) -> Result<(), Box<dyn FnOnce() + Send>> {
        use std::sync::atomic::Ordering;
        if self.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            return Err(job);
        }
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.send(job).map_err(|e| {
            self.busy.store(false, Ordering::Release);
            e.0
        })
    }

    /// Whether the worker is running a job.
    pub(crate) fn busy() -> bool {
        FsWorker::get().busy.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Reads a regular file into a buffer reserved in `res`. The file's length
/// is reserved first, so a file too large for the memory limit fails before
/// it is read; the buffer then grows as data arrives (from a preallocation
/// of at most 64 MiB, in case the length is that of a sparse file).
pub(crate) fn read_reserved(path: &Path, mut res: Reservation) -> io::Result<(Vec<u8>, Reservation)> {
    let mut file = safe::open_read(path)?;
    let len = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    res.ensure(len)?;
    let mut data = Vec::new();
    res.reserve_buf(&mut data, len.min(64 << 20))?;
    let mut chunk = [0u8; 8192];
    loop {
        let start = data.len();
        let n = if data.capacity() > start {
            // Fill the reserved capacity.
            data.resize(data.capacity(), 0);
            let n = read_some(&mut file, &mut data[start..]);
            data.truncate(start + n.as_ref().map_or(0, |&n| n));
            n?
        } else {
            let n = read_some(&mut file, &mut chunk)?;
            res.reserve_buf(&mut data, n)?;
            data.extend_from_slice(&chunk[..n]);
            n
        };
        if n == 0 {
            return Ok((data, res));
        }
    }
}

fn read_some(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match r.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            r => return r,
        }
    }
}

/// Writes (or appends) `data` to the file at `path`.
pub(crate) fn write(path: &Path, data: &[u8], append: bool) -> io::Result<()> {
    let mut f = safe::open_write(path, append)?;
    f.write_all(data)?;
    f.flush()
}

/// Copies the contents of the regular file `from` to `to`. Permissions are
/// not copied: a new file gets the default mode, so a copy never becomes
/// executable (or setuid) because its source was.
pub(crate) fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let mut src = safe::open_read(from)?;
    let mut dst = safe::open_write(to, false)?;
    io::copy(&mut src, &mut dst)?;
    Ok(())
}

/// The names in the directory `path` (unsorted), reserved in `res`.
pub(crate) fn list_dir(path: &Path, res: &mut Reservation) -> io::Result<Vec<String>> {
    Ok(safe::list_dir(path, res)?.into_iter().map(|n| n.to_string_lossy().into_owned()).collect())
}

/// Metadata of the entry at `path` (a final symlink is not followed).
pub(crate) fn stat(path: &Path) -> io::Result<EntryMeta> {
    let m = safe::stat(path)?;
    Ok(EntryMeta { kind: m.kind, size: m.size, modified: m.modified.map(unix_nanos) })
}

/// Whether anything (a symlink included) is at `path`.
pub(crate) fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// The ancestors of `path` that do not exist, outermost first.
pub(crate) fn missing_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut missing: Vec<PathBuf> = path.ancestors().skip(1).take_while(|a| !exists(a)).map(Path::to_path_buf).collect();
    missing.reverse();
    missing
}

fn unix_nanos(t: SystemTime) -> i64 {
    let clamp = |n: u128| i64::try_from(n).unwrap_or(i64::MAX);
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => clamp(d.as_nanos()),
        Err(e) => -clamp(e.duration().as_nanos()),
    }
}
