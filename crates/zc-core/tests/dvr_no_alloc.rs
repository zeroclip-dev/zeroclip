//! Hot-path code must not allocate (CLAUDE.md rule 2; DESIGN §5: "All modes share
//! one zero-allocation encode loop"). These tests swap in a counting global
//! allocator and check the DVR push, the packet ring, hotkey matching, the mic DSP,
//! the audio timeline and the Master mix bus.
//!
//! It lives in its own integration-test binary because a `#[global_allocator]`
//! applies to the whole binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use zc_core::Hns;
use zc_core::dsp::{Compressor, CompressorParams};
use zc_core::dvr::{DvrBuffer, DvrConfig};
use zc_core::hotkey::{Binding, Chord, ChordMatcher, Key, Trigger};
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::mix::MixBus;
use zc_core::ring::PacketRing;
use zc_core::testing::SyntheticStream;
use zc_core::time::SampleTimeline;

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
fn chord_matcher_on_key_never_allocates() {
    let binding = |keys: Vec<Key>, action, trigger| Binding {
        chord: Chord::new(keys).expect("chord"),
        action,
        trigger,
    };
    let mut matcher = ChordMatcher::new(vec![
        binding(vec![Key::ALT, Key::f(10)], 1, Trigger::Press),
        binding(vec![Key::ALT, Key::f(9)], 2, Trigger::Press),
        binding(vec![Key::MOUSE5], 3, Trigger::Hold),
    ]);
    let keys = [Key::ALT, Key::f(10), Key::f(9), Key::MOUSE5, Key(0x57)];
    let allocations = allocations_during(|| {
        for i in 0..1_000 {
            let key = keys[i % keys.len()];
            std::hint::black_box(matcher.on_key(key, true));
            std::hint::black_box(matcher.on_key(key, true)); // auto-repeat
            if i % 3 == 0 {
                std::hint::black_box(matcher.on_key(key, false));
            }
        }
        matcher.reset();
    });
    assert_eq!(allocations, 0, "ChordMatcher::on_key allocated {allocations} times");
}

#[test]
fn compressor_process_never_allocates() {
    let mut compressor = Compressor::new(CompressorParams::default());
    let mut samples = vec![0.5f32; 48_000];
    let allocations = allocations_during(|| compressor.process(&mut samples));
    assert_eq!(allocations, 0, "Compressor::process allocated {allocations} times");
}

#[test]
fn sample_timeline_place_never_allocates() {
    let mut timeline = SampleTimeline::new(48_000);
    let allocations = allocations_during(|| {
        for k in 0..10_000_i64 {
            let jitter = match k % 100 {
                50 => Hns::from_millis(60),  // a gap: silence
                75 => Hns::from_millis(-40), // a fast device: skip
                _ => Hns::ZERO,
            };
            std::hint::black_box(timeline.place(Hns(k * 100_000) + jitter, 480));
        }
    });
    assert_eq!(allocations, 0, "SampleTimeline::place allocated {allocations} times");
}

#[test]
fn mix_bus_write_and_mix_never_allocate() {
    // Two sources write 10 ms blocks a little apart; the mixer takes 10 ms at a time, and one
    // write is late, so every path runs.
    let mut bus = MixBus::new(2, 2);
    bus.anchor(0);
    bus.set_gain(0, 0.5);
    let block = [1_000i16; 480 * 2];
    let mut out = [0i16; 480 * 2];
    let allocations = allocations_during(|| {
        for k in 0..500_i64 {
            bus.write(0, k * 480, &block);
            bus.write(1, k * 480 + 336, &block);
            std::hint::black_box(bus.mix_until((k + 1) * 480, &mut out));
        }
        bus.write(0, 0, &block); // long behind the mix point: dropped
    });
    assert_eq!(allocations, 0, "MixBus allocated {allocations} times");
}

#[test]
fn ring_push_and_pop_never_allocate() {
    // 3-byte packets in a 10-byte ring: most pushes wrap, and some are refused.
    let mut ring = PacketRing::new(10, 4);
    let payload = [7_u8; 3];
    // Reserved up front, as the spool does; `pop_front_into` must not grow it.
    let mut out = Vec::with_capacity(payload.len());
    let allocations = allocations_during(|| {
        for i in 0..1_000_i64 {
            let meta =
                PacketMeta { track: TrackId::Video, pts: Hns(i), duration: Hns(1), keyframe: true };
            std::hint::black_box(ring.push(PacketRef { meta, data: &payload }));
            if i % 2 == 1 {
                std::hint::black_box(ring.pop_front_into(&mut out));
            }
        }
    });
    assert_eq!(allocations, 0, "PacketRing push or pop allocated {allocations} times");
}

#[test]
fn the_counter_itself_works() {
    let n = allocations_during(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(v);
    });
    assert_eq!(n, 1);
}
