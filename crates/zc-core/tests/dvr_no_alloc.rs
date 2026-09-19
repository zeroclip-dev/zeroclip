//! DESIGN §5: "All modes share one zero-allocation encode loop." This test swaps in
//! a counting global allocator and checks that `DvrBuffer::push` never allocates
//! once the buffer exists — including while it is evicting.
//!
//! It lives in its own integration-test binary because a `#[global_allocator]`
//! applies to the whole binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use zc_core::dvr::{DvrBuffer, DvrConfig};
use zc_core::testing::SyntheticStream;

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
#[ignore = "spec: DvrBuffer::push must not allocate (Phase 3)"]
fn dvr_push_never_allocates() {
    // Small rings so that pushing 30 s forces plenty of eviction.
    let mut dvr = DvrBuffer::new(DvrConfig { video_bytes: 300_000, audio_bytes: 100_000 });
    // Generate the packets before counting: only `push` is under test.
    let packets = SyntheticStream::new().take_secs(30);

    let allocations = allocations_during(|| {
        for p in &packets {
            dvr.push(p.view()).expect("push");
        }
    });
    assert_eq!(allocations, 0, "DvrBuffer::push allocated {allocations} times");
}

#[test]
fn the_counter_itself_works() {
    let n = allocations_during(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(v);
    });
    assert_eq!(n, 1);
}
