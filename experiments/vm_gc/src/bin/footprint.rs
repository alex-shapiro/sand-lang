//! B7: memory footprint and collection pause for live users.
//!
//! `cargo run --release --bin footprint`. This binary counts every process
//! allocation with a global allocator, so it never runs B1 to B3 timings.
//! The counting allocator is the only `unsafe` code in the experiment, and
//! it exists only in this measurement binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use vm_gc::shared::bench::footprint;
use vm_gc::shared::programs::Variant;
use vm_gc::shared::suite::{ArmEngine, engine, roomy_limits};

static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);
static LIVE_ALLOCS: AtomicIsize = AtomicIsize::new(0);

/// Counts live bytes and allocations of the process.
struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        LIVE_ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded from the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        LIVE_ALLOCS.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: forwarded from the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE_BYTES.fetch_add(new_size as isize - layout.size() as isize, Ordering::Relaxed);
        // SAFETY: forwarded from the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live() -> (isize, isize) {
    (LIVE_BYTES.load(Ordering::Relaxed), LIVE_ALLOCS.load(Ordering::Relaxed))
}

fn measure<E: ArmEngine>(users: i64) {
    let mut e: E = engine(Variant::Structural, roomy_limits());
    e.collect();
    let (bytes_before, allocs_before) = live();
    let f = footprint(&mut e, users);
    let (bytes_after, allocs_after) = live();
    println!(
        "{:<5} {users:>6} {:>14} {:>13} {:>14} {:>13} {:>10.2}",
        E::NAME,
        f.tracked_bytes,
        f.live_objects,
        bytes_after - bytes_before,
        allocs_after - allocs_before,
        f.pause.as_secs_f64() * 1e3
    );
}

fn main() {
    println!("live users; collection pause is the fastest of 5");
    println!(
        "{:<5} {:>6} {:>14} {:>13} {:>14} {:>13} {:>10}",
        "arm", "users", "tracked bytes", "VM objects", "process bytes", "process allocs", "pause ms"
    );
    for users in [1000, 4000, 4900, 5000, 6000, 10_000] {
        measure::<vm_gc::arm1::Engine>(users);
        measure::<vm_gc::arm2::Engine>(users);
        measure::<vm_gc::arm3::Engine>(users);
    }
}
