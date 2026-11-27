//! How encoded packets flow from sources to consumers.
//!
//! ```text
//!  GPU video pipeline ─┐
//!  game-audio loopback ─┼─► PacketSink (engine's recorder) ─► DVR buffer
//!  mic + PTT ──────────┘                                  └─► Tee ─► session file
//!                                                                └─► manual recording
//! ```
//!
//! Sources only ever see a [`PacketSink`]: a cheap, thread-safe handle that must
//! not block. Behind it, the recorder copies each packet into the DVR buffer and
//! fans it out through a [`Tee`] to any active disk writers — one encode, many
//! sinks.

use std::sync::Arc;

use crate::media::PacketRef;

/// Where sources deliver packets. Implemented by the engine's recorder.
///
/// `push` runs on capture and encoder threads, so it must be fast and must not
/// allocate or block on I/O: copy the bytes into pre-allocated storage and return.
pub trait PacketSink: Send + Sync {
    /// Hands one packet to the recorder.
    fn push(&self, packet: PacketRef<'_>);
}

/// A shareable [`PacketSink`].
pub type SharedSink = Arc<dyn PacketSink>;

/// A single consumer of packets, owned by the recorder: a disk writer, a test probe…
pub trait PacketWriter: Send {
    /// Consumes one packet.
    ///
    /// # Errors
    /// A writer that fails (disk full, device gone) reports it here; the [`Tee`]
    /// keeps feeding the other writers regardless.
    fn write(&mut self, packet: PacketRef<'_>) -> Result<(), SinkError>;
}

/// Why a [`PacketWriter`] rejected a packet.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    /// The writer can't keep up and dropped the packet.
    #[error("sink is full; packet dropped")]
    Full,
    /// An I/O error from the underlying file.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Identifies a writer attached to a [`Tee`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SinkId(u32);

/// Fan-out: delivers every packet to all attached writers.
///
/// Running DVR and session recording at the same time is expected for cs2-style
/// profiles, so the tee exists from the start instead of being retrofitted.
/// A failing writer never stops delivery to the others.
#[derive(Default)]
pub struct Tee {
    writers: Vec<(SinkId, Box<dyn PacketWriter>)>,
    next_id: u32,
}

impl Tee {
    /// A tee with no writers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches a writer. Call off the hot path (it may allocate).
    pub fn attach(&mut self, writer: Box<dyn PacketWriter>) -> SinkId {
        let id = SinkId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.writers.push((id, writer));
        id
    }

    /// Detaches and returns a writer, e.g. to finalise its file.
    pub fn detach(&mut self, id: SinkId) -> Option<Box<dyn PacketWriter>> {
        let index = self.writers.iter().position(|(sink, _)| *sink == id)?;
        // `remove` keeps delivery order deterministic, unlike `swap_remove`.
        Some(self.writers.remove(index).1)
    }

    /// Number of attached writers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.writers.len()
    }

    /// Whether no writers are attached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// Delivers `packet` to every writer, calling `on_error` for each one that fails.
    /// Runs on the hot path, so it never allocates.
    pub fn write(&mut self, packet: PacketRef<'_>, mut on_error: impl FnMut(SinkId, &SinkError)) {
        for (id, writer) in &mut self.writers {
            if let Err(e) = writer.write(packet) {
                on_error(*id, &e);
            }
        }
    }
}

impl std::fmt::Debug for Tee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tee").field("writers", &self.writers.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::testing::SyntheticStream;

    /// Records the frame numbers it receives, or fails every write.
    struct Probe {
        seen: Arc<Mutex<Vec<u32>>>,
        fail: bool,
    }

    impl PacketWriter for Probe {
        fn write(&mut self, packet: PacketRef<'_>) -> Result<(), SinkError> {
            if self.fail {
                return Err(SinkError::Full);
            }
            self.seen.lock().unwrap().push(SyntheticStream::sequence_number(packet.data));
            Ok(())
        }
    }

    fn probe(fail: bool) -> (Box<dyn PacketWriter>, Arc<Mutex<Vec<u32>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (Box::new(Probe { seen: Arc::clone(&seen), fail }), seen)
    }

    #[test]
    fn tee_delivers_every_packet_to_every_writer() {
        let mut tee = Tee::new();
        let (a, seen_a) = probe(false);
        let (b, seen_b) = probe(false);
        tee.attach(a);
        tee.attach(b);
        let mut stream = SyntheticStream::new();
        for _ in 0..5 {
            let p = stream.next_video();
            tee.write(p.view(), |_, e| panic!("unexpected error: {e}"));
        }
        assert_eq!(*seen_a.lock().unwrap(), vec![0, 1, 2, 3, 4]);
        assert_eq!(*seen_b.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn tee_keeps_feeding_healthy_writers_when_one_fails() {
        let mut tee = Tee::new();
        let (bad, _) = probe(true);
        let (good, seen) = probe(false);
        let bad_id = tee.attach(bad);
        tee.attach(good);
        let mut failures = Vec::new();
        let p = SyntheticStream::new().next_video();
        tee.write(p.view(), |id, _| failures.push(id));
        assert_eq!(failures, vec![bad_id]);
        assert_eq!(*seen.lock().unwrap(), vec![0]);
    }

    #[test]
    fn tee_detach_stops_delivery() {
        let mut tee = Tee::new();
        let (w, seen) = probe(false);
        let id = tee.attach(w);
        assert!(tee.detach(id).is_some());
        assert!(tee.detach(id).is_none(), "already detached");
        assert!(tee.is_empty());
        let p = SyntheticStream::new().next_video();
        tee.write(p.view(), |_, _| {});
        assert!(seen.lock().unwrap().is_empty());
    }
}
