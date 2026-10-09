//! Accounting of memory held outside the heap by pending I/O.
//!
//! Buffers that pending I/O operations fill (file and socket reads, HTTP
//! messages, standard input, copies of data being written) are not heap
//! objects until the operation finishes. They are reserved here, while they
//! grow, and the heap counts the reserved bytes against `max_memory` together
//! with its own (see [`super::heap::Heap::would_exceed`]). A reservation is
//! released when dropped: when the operation's result is turned into a value
//! (the finisher drops it right before allocating), fails, or is cancelled.
//! An operation whose buffers do not fit fails with an error of kind
//! `"limit"`: nothing beyond the limit was allocated, so the run continues.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared between the VM and the futures and blocking tasks of its I/O.
#[derive(Debug, Default)]
pub struct IoMemory {
    /// Bytes held by live [`Reservation`]s.
    reserved: AtomicUsize,
    /// The heap's size as last published by the VM.
    heap: AtomicUsize,
    limit: Option<usize>,
    /// Sockets held by pending operations (see [`ResourceSlot`]).
    pending_resources: AtomicUsize,
}

/// A reservation would exceed the memory limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLimit;

impl fmt::Display for MemoryLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the data does not fit in the memory limit")
    }
}

/// The most that may be reserved without a memory limit (as for heap values).
const UNLIMITED: usize = 1 << 40;

impl std::error::Error for MemoryLimit {}

impl From<MemoryLimit> for std::io::Error {
    fn from(e: MemoryLimit) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::OutOfMemory, e)
    }
}

impl IoMemory {
    pub fn new(limit: Option<usize>) -> Arc<IoMemory> {
        Arc::new(IoMemory { limit, ..IoMemory::default() })
    }

    /// Bytes currently reserved by pending I/O.
    #[inline]
    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Records the heap's current size; reservations made off the VM thread
    /// are checked against it.
    #[inline]
    pub fn publish_heap(&self, bytes: usize) {
        self.heap.store(bytes, Ordering::Relaxed);
    }

    /// Sockets currently held by pending operations ([`ResourceSlot`]s);
    /// they count against `max_open_resources` with the heap's open
    /// resources.
    #[inline]
    pub fn pending_resources(&self) -> usize {
        self.pending_resources.load(Ordering::Relaxed)
    }

    fn try_reserve(&self, n: usize) -> Result<(), MemoryLimit> {
        let (limit, heap) = match self.limit {
            Some(limit) => (limit, self.heap.load(Ordering::Relaxed)),
            None => (UNLIMITED, 0),
        };
        self.reserved
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |r| {
                let total = r.checked_add(n)?;
                (total.saturating_add(heap) <= limit).then_some(total)
            })
            .map(|_| ())
            .map_err(|_| MemoryLimit)
    }
}

/// Bytes reserved for the buffers of one I/O operation; released on drop.
#[derive(Debug)]
pub struct Reservation {
    mem: Arc<IoMemory>,
    bytes: usize,
}

impl Reservation {
    /// An empty reservation.
    pub fn new(mem: &Arc<IoMemory>) -> Reservation {
        Reservation { mem: mem.clone(), bytes: 0 }
    }

    /// An empty reservation against the same budget, e.g. for a temporary
    /// buffer released before the operation ends.
    pub fn sibling(&self) -> Reservation {
        Reservation::new(&self.mem)
    }

    /// Takes over the bytes of `other` (a sibling), e.g. when its buffer
    /// becomes part of this operation's result.
    pub fn absorb(&mut self, mut other: Reservation) {
        debug_assert!(Arc::ptr_eq(&self.mem, &other.mem));
        self.bytes += std::mem::take(&mut other.bytes);
    }

    /// Grows the reservation to at least `total` bytes.
    pub fn ensure(&mut self, total: usize) -> Result<(), MemoryLimit> {
        if total > self.bytes {
            self.mem.try_reserve(total - self.bytes)?;
            self.bytes = total;
        }
        Ok(())
    }

    /// Reserves `n` more bytes.
    pub fn grow(&mut self, n: usize) -> Result<(), MemoryLimit> {
        self.ensure(self.bytes.saturating_add(n))
    }

    /// Makes room for `extra` more bytes in `buf`, growing it (and the
    /// reservation, which covers the buffer's capacity) geometrically when
    /// possible and exactly otherwise.
    pub fn reserve_buf(&mut self, buf: &mut Vec<u8>, extra: usize) -> Result<(), MemoryLimit> {
        self.reserve_buf_upto(buf, extra, usize::MAX)
    }

    /// Like [`Reservation::reserve_buf`], but grows the buffer's capacity
    /// beyond what is needed only up to `cap` bytes.
    pub fn reserve_buf_upto(&mut self, buf: &mut Vec<u8>, extra: usize, cap: usize) -> Result<(), MemoryLimit> {
        let needed = buf.len().checked_add(extra).ok_or(MemoryLimit)?;
        if needed <= buf.capacity() {
            return Ok(());
        }
        let generous = needed.max(buf.capacity().saturating_mul(2)).max(64).min(cap.max(needed));
        let target = if generous > needed && self.ensure(generous).is_ok() {
            generous
        } else {
            self.ensure(needed)?;
            needed
        };
        buf.try_reserve_exact(target - buf.len()).map_err(|_| MemoryLimit)
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.mem.reserved.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// One open resource (a socket being connected, accepted or used by a
/// pending operation) counted against `max_open_resources` until it becomes
/// a heap object, which the heap counts instead, or until it is dropped
/// (the operation failed, finished or was cancelled).
#[derive(Debug)]
pub struct ResourceSlot {
    mem: Arc<IoMemory>,
}

impl ResourceSlot {
    /// Takes a slot; the caller has checked the limit (on the VM thread,
    /// against the heap's open resources plus [`IoMemory::pending_resources`]).
    pub fn new(mem: &Arc<IoMemory>) -> ResourceSlot {
        mem.pending_resources.fetch_add(1, Ordering::Relaxed);
        ResourceSlot { mem: mem.clone() }
    }
}

impl Drop for ResourceSlot {
    fn drop(&mut self) {
        self.mem.pending_resources.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_bounded_and_released() {
        let mem = IoMemory::new(Some(1000));
        mem.publish_heap(400);
        let mut a = Reservation::new(&mem);
        a.ensure(500).unwrap();
        assert_eq!(mem.reserved(), 500);
        let mut b = Reservation::new(&mem);
        assert_eq!(b.grow(101), Err(MemoryLimit));
        b.grow(100).unwrap();
        assert_eq!(mem.reserved(), 600);
        drop(a);
        assert_eq!(mem.reserved(), 100);
        let mut buf = Vec::new();
        b.reserve_buf(&mut buf, 10).unwrap();
        assert!(buf.capacity() >= 10 && b.bytes() >= buf.capacity());
        drop(b);
        assert_eq!(mem.reserved(), 0);
        let unlimited = IoMemory::new(None);
        Reservation::new(&unlimited).ensure(1 << 39).unwrap();
        assert_eq!(unlimited.reserved(), 0);
        assert_eq!(Reservation::new(&unlimited).ensure(usize::MAX / 2), Err(MemoryLimit));
    }

    #[test]
    fn buffers_grow_exactly_near_the_limit() {
        let mem = IoMemory::new(Some(10_000));
        let mut r = Reservation::new(&mem);
        let mut buf = vec![0u8; 6000];
        r.ensure(buf.capacity()).unwrap();
        // Doubling would exceed the limit; the exact size still fits.
        r.reserve_buf(&mut buf, 3000).unwrap();
        assert!(buf.capacity() >= 9000 && r.bytes() <= 10_000);
        buf.resize(9000, 0);
        assert_eq!(r.reserve_buf(&mut buf, 2000), Err(MemoryLimit));
    }
}
