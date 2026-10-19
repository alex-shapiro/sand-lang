//! Accounting of memory held outside the heap by pending I/O.
//!
//! The buffers pending I/O fills (file and socket reads, HTTP messages,
//! standard input, copies of data being written) are not heap objects until
//! the operation finishes. They are reserved here as they grow, and the heap
//! counts the reserved bytes against `max_memory` together with its own
//! ([`super::heap::Heap::would_exceed`]). A [`Reservation`] is released when
//! dropped: right before the operation's result is allocated as a value, or
//! when the operation fails or is cancelled. An operation whose buffers do
//! not fit fails with an error of kind `"limit"`; nothing beyond the limit
//! was allocated, so the run continues.

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
    /// Open resources: the number of live [`ResourceSlot`]s.
    resources: AtomicUsize,
    /// `max_open_resources`.
    max_resources: usize,
}

/// A reservation would exceed the memory limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLimit;

impl fmt::Display for MemoryLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the data does not fit in the memory limit")
    }
}

/// The most that may be reserved without a memory limit (as for heap
/// values): 1 TiB, or 2 GiB with 32-bit addresses (WebAssembly).
pub(crate) const UNLIMITED: usize = if usize::BITS > 32 { 1 << 40 } else { 1 << 31 };

impl std::error::Error for MemoryLimit {}

impl From<MemoryLimit> for std::io::Error {
    fn from(e: MemoryLimit) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::OutOfMemory, e)
    }
}

impl IoMemory {
    /// Accounting for a memory limit of `limit` bytes and at most
    /// `max_resources` open resources.
    pub fn new(limit: Option<usize>, max_resources: usize) -> Arc<IoMemory> {
        Arc::new(IoMemory { limit, max_resources, ..IoMemory::default() })
    }

    /// Bytes currently reserved by pending I/O.
    #[inline]
    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Records the heap's current size; reservations made off the VM thread
    /// are checked against it. The VM publishes it after every slice and
    /// whenever it checks an allocation against the limit, so it lags the
    /// heap by at most the small allocations made since.
    #[inline]
    pub fn publish_heap(&self, bytes: usize) {
        self.heap.store(bytes, Ordering::Relaxed);
    }

    /// The resources open now (live [`ResourceSlot`]s).
    #[inline]
    pub fn open_resources(&self) -> usize {
        self.resources.load(Ordering::Relaxed)
    }

    /// `max_open_resources`.
    pub fn max_resources(&self) -> usize {
        self.max_resources
    }

    fn try_reserve(&self, n: usize) -> Result<(), MemoryLimit> {
        self.try_reserve_leaving(n, 0)
    }

    /// Reserves `n` bytes if `spare` more would still fit.
    fn try_reserve_leaving(&self, n: usize, spare: usize) -> Result<(), MemoryLimit> {
        let (limit, heap) = match self.limit {
            Some(limit) => (limit, self.heap.load(Ordering::Relaxed)),
            None => (UNLIMITED, 0),
        };
        self.reserved
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |r| {
                let total = r.checked_add(n)?;
                (total.saturating_add(heap).saturating_add(spare) <= limit).then_some(total)
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

/// Memory that the fixed buffers of open resources leave free (see
/// [`ResourceSlot::reserve_buffers`]).
pub const RESOURCE_SPARE: usize = 16 << 10;

/// One open resource (a file, a socket, or the socket of a pending
/// operation), counted against `max_open_resources` while the slot lives.
///
/// A resource's heap object holds its slot until the resource is closed or
/// collected. An operation opening a resource off the VM thread holds the
/// slot from its start until the slot moves into the object, or until the
/// operation fails or is cancelled. A connection's fixed buffers are
/// reserved with its slot ([`ResourceSlot::reserve_buffers`]) and released
/// with it.
#[derive(Debug)]
pub struct ResourceSlot {
    mem: Arc<IoMemory>,
    buffers: Reservation,
}

impl ResourceSlot {
    /// Takes a slot, if fewer than `max_open_resources` are open.
    pub fn try_new(mem: &Arc<IoMemory>) -> Option<ResourceSlot> {
        let max = mem.max_resources;
        mem.resources.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < max).then_some(n + 1)).ok()?;
        Some(ResourceSlot { mem: mem.clone(), buffers: Reservation::new(mem) })
    }

    /// Reserves `bytes` (in total) for the fixed buffers of the resource.
    /// They are held while the resource is open, so they may not take the
    /// last [`RESOURCE_SPARE`] bytes of the memory limit: a script whose
    /// resources fill the memory can still handle the error.
    pub fn reserve_buffers(&mut self, bytes: usize) -> Result<(), MemoryLimit> {
        if bytes > self.buffers.bytes {
            let mem = &self.buffers.mem;
            mem.try_reserve_leaving(bytes - self.buffers.bytes, RESOURCE_SPARE)?;
            self.buffers.bytes = bytes;
        }
        Ok(())
    }

    /// The bytes reserved for the resource's buffers.
    pub fn buffers(&self) -> usize {
        self.buffers.bytes()
    }
}

impl Drop for ResourceSlot {
    fn drop(&mut self) {
        self.mem.resources.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_bounded_and_released() {
        let mem = IoMemory::new(Some(1000), 10);
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
        // A resource's buffers leave `RESOURCE_SPARE` bytes free.
        let mut slot = ResourceSlot::try_new(&mem).unwrap();
        assert_eq!(slot.reserve_buffers(64), Err(MemoryLimit));
        let mem = IoMemory::new(Some(RESOURCE_SPARE + 1000), 10);
        let mut slot = ResourceSlot::try_new(&mem).unwrap();
        slot.reserve_buffers(64).unwrap();
        slot.reserve_buffers(64).unwrap();
        assert_eq!((mem.reserved(), mem.open_resources()), (64, 1));
        assert_eq!(slot.reserve_buffers(1001), Err(MemoryLimit));
        drop(slot);
        assert_eq!((mem.reserved(), mem.open_resources()), (0, 0));
        // At most `max_resources` slots at once.
        let slots: Vec<ResourceSlot> = (0..10).map(|_| ResourceSlot::try_new(&mem).unwrap()).collect();
        assert!(ResourceSlot::try_new(&mem).is_none());
        drop(slots);
        assert!(ResourceSlot::try_new(&mem).is_some());
        let unlimited = IoMemory::new(None, 10);
        Reservation::new(&unlimited).ensure(1 << 39).unwrap();
        assert_eq!(unlimited.reserved(), 0);
        assert_eq!(Reservation::new(&unlimited).ensure(usize::MAX / 2), Err(MemoryLimit));
    }

    #[test]
    fn buffers_grow_exactly_near_the_limit() {
        let mem = IoMemory::new(Some(10_000), 10);
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
