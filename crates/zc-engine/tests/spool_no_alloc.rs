//! The recording spool's hot path never allocates (CONTRIBUTING.md rule 2). The spool is written
//! under the recorder's lock, so `SpoolWriter::write` may only copy into its
//! pre-allocated ring.
//!
//! It lives in its own integration-test binary because a `#[global_allocator]`
//! applies to the whole binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use zc_core::sink::PacketWriter;
use zc_core::testing::SyntheticStream;
use zc_engine::spool::spool_with_capacity;

struct CountingAllocator;

thread_local! {
    // `const` initialisation and no destructor: touching these never allocates,
    // which matters inside an allocator.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every method forwards to the system allocator unchanged; the only extra
// work is bumping a thread-local counter, which cannot allocate or unwind.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: same contract as our caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as our caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: same contract as our caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Runs `f` and returns how many allocations it made on this thread.
fn allocations_during(f: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn spool_write_never_allocates() {
    // 64 KiB holds about 64 of the 1,000-byte frames, so the spool overflows partway
    // through and the rejection path is covered too.
    let (mut writer, reader) = spool_with_capacity(64 << 10);
    // Generate the packets before counting: only `write` is under test.
    let packets = SyntheticStream::new().take_secs(3);

    let mut rejected = 0;
    let allocations = allocations_during(|| {
        for p in &packets {
            if std::hint::black_box(writer.write(p.view())).is_err() {
                rejected += 1;
            }
        }
    });
    assert!(rejected > 0, "the spool must overflow for this test to mean anything");
    assert_eq!(allocations, 0, "SpoolWriter::write allocated {allocations} times");

    // The packets the spool accepted come out the other end.
    drop(writer);
    let mut buf = Vec::new();
    let mut drained = 0;
    while reader.next(&mut buf).is_some() {
        drained += 1;
    }
    assert!(drained > 0);
}
