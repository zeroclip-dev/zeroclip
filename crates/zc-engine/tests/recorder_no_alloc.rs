//! `Recorder::push` runs on the encoder and audio threads, so it must not allocate
//! either (CONTRIBUTING.md rule 2). Unlike `dvr_no_alloc.rs`, which covers the DVR buffer
//! alone, this covers the whole path: the master clock, the DVR buffer and the tee.
//!
//! It lives in its own integration-test binary because a `#[global_allocator]`
//! applies to the whole binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use zc_core::dvr::DvrConfig;
use zc_core::media::PacketRef;
use zc_core::sink::{PacketSink, PacketWriter, SinkError};
use zc_core::testing::SyntheticStream;
use zc_engine::disk_dvr::{DiskRing, Persister};
use zc_engine::recorder::{Recorder, RecorderStats};
use zc_mux::pacing::WritePacer;

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

/// A disk writer stand-in: it only adds up the bytes it is handed. The count is shared
/// so the test can check that the tee really reached it.
struct ByteCounter(Arc<AtomicU64>);

impl PacketWriter for ByteCounter {
    fn write(&mut self, packet: PacketRef<'_>) -> Result<(), SinkError> {
        self.0.fetch_add(packet.data.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn recorder_push_never_allocates() {
    // About 5 s of 1 KB video frames and 4 s of 480-byte audio per track.
    let recorder = Recorder::new(DvrConfig { video_bytes: 300_000, audio_bytes: 100_000 });
    let written = Arc::new(AtomicU64::new(0));
    recorder.attach(Box::new(ByteCounter(Arc::clone(&written))));
    let mut stream = SyntheticStream::new();

    // Warm up: 10 s overflows both rings, so eviction has run, and the first keyframe
    // anchors the master clock.
    for p in stream.take_secs(10) {
        recorder.push(p.view());
    }

    // 600 mixed packets (video and both audio tracks), generated before counting.
    let mut batch = stream.take_secs(4);
    batch.truncate(600);
    let allocations = allocations_during(|| {
        for p in &batch {
            recorder.push(p.view());
        }
    });
    assert_eq!(allocations, 0, "Recorder::push allocated {allocations} times");
    // Guard against a vacuous pass: every packet must really have been stored and teed.
    assert_eq!(recorder.stats(), RecorderStats::default());
    assert!(written.load(Ordering::Relaxed) > 0, "the tee never reached its writer");
}

/// Disk mode: the hot path still only copies into the staging area, while the persister
/// thread writes GOPs to the ring file at the same time. Only the pushing thread is counted.
#[test]
fn recorder_push_never_allocates_in_disk_mode() {
    let dir = std::env::temp_dir().join(format!("zc-no-alloc-ring-{}", std::process::id()));
    let ring = Arc::new(DiskRing::create(&dir, 1_000_000).expect("ring file"));
    let recorder =
        Arc::new(Recorder::new(DvrConfig { video_bytes: 300_000, audio_bytes: 100_000 }));
    let persister = Persister::start(&recorder, &ring, WritePacer::unlimited()).expect("thread");
    let mut stream = SyntheticStream::new();
    for p in stream.take_secs(10) {
        recorder.push(p.view());
    }
    let mut batch = stream.take_secs(4);
    batch.truncate(600);
    let allocations = allocations_during(|| {
        for p in &batch {
            recorder.push(p.view());
        }
    });
    drop(persister);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(allocations, 0, "Recorder::push allocated {allocations} times in disk mode");
    assert_eq!(recorder.stats(), RecorderStats::default());
}

#[test]
fn the_counter_itself_works() {
    let n = allocations_during(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(v);
    });
    assert_eq!(n, 1);
}
