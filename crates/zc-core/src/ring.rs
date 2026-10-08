//! An allocation-free FIFO of encoded packets, stored in one pre-allocated byte ring
//! (DESIGN §4, §5).
//!
//! The DVR buffer keeps one [`PacketRing`] per track, and the recording spool uses one
//! too. Packet bytes go into a single `Box<[u8]>`. A packet may run past the end of the
//! ring and continue at the start; [`PacketRing::iter`] returns such a packet as two
//! slices instead of copying it. Only [`PacketRing::new`] allocates, so `push` and
//! `pop_front` are safe on the capture and encode threads.

use std::collections::VecDeque;
use std::fmt;

use crate::media::{PacketMeta, PacketRef};

/// Where one packet lives in the byte ring, plus its metadata.
#[derive(Clone, Copy)]
struct Entry {
    meta: PacketMeta,
    /// Offset of the packet's first byte in the ring.
    start: usize,
    /// Payload length in bytes.
    len: usize,
}

/// A FIFO of packets: payload bytes in one ring, metadata in a bounded queue.
///
/// Both limits are hard. A push that would exceed the byte capacity or the packet
/// count is refused and nothing is evicted, so the caller decides what to drop.
pub struct PacketRing {
    /// The byte ring. Fixed size, zeroed on creation, never reallocated.
    bytes: Box<[u8]>,
    /// One entry per packet, oldest first.
    entries: VecDeque<Entry>,
    /// Limit on `entries`. Kept here because `VecDeque` may reserve more than asked.
    max_packets: usize,
    /// Offset of the oldest packet. Equals `entries[0].start` when non-empty.
    head: usize,
    /// Payload bytes stored, across all packets.
    used: usize,
}

impl PacketRing {
    /// Creates an empty ring with `capacity_bytes` of payload space and room for
    /// `max_packets` packets.
    ///
    /// This is the only allocation the ring ever makes. `vec![0; n]` is a zeroed
    /// (calloc) allocation, so the pages are committed lazily.
    #[must_use]
    pub fn new(capacity_bytes: usize, max_packets: usize) -> Self {
        Self {
            bytes: vec![0; capacity_bytes].into_boxed_slice(),
            entries: VecDeque::with_capacity(max_packets),
            max_packets,
            head: 0,
            used: 0,
        }
    }

    /// Byte capacity of the ring.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.len()
    }

    /// Payload bytes currently stored.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used
    }

    /// Number of packets currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ring holds no packets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Metadata of the oldest packet.
    #[must_use]
    pub fn front(&self) -> Option<PacketMeta> {
        self.entries.front().map(|entry| entry.meta)
    }

    /// Metadata of the newest packet.
    #[must_use]
    pub fn back(&self) -> Option<PacketMeta> {
        self.entries.back().map(|entry| entry.meta)
    }

    /// Whether [`push`](Self::push) would accept a packet of `len` bytes.
    ///
    /// Written as a subtraction so a huge `len` can't overflow. `used` never exceeds
    /// `capacity`, so the subtraction can't underflow.
    #[must_use]
    pub fn has_room_for(&self, len: usize) -> bool {
        self.capacity() > 0
            && len <= self.capacity() - self.used
            && self.entries.len() < self.max_packets
    }

    /// Copies `p` in as the newest packet. Returns `false` and stores nothing if it
    /// doesn't fit, either by bytes or by count. Never allocates.
    #[must_use]
    pub fn push(&mut self, p: PacketRef<'_>) -> bool {
        let len = p.data.len();
        if !self.has_room_for(len) {
            return false;
        }
        // `has_room_for` guarantees capacity > 0, so this `%` can't divide by zero.
        let capacity = self.capacity();
        let tail = (self.head + self.used) % capacity;
        // The packet may wrap: `first` bytes fit before the end, the rest go at the start.
        let first = len.min(capacity - tail);
        self.bytes[tail..tail + first].copy_from_slice(&p.data[..first]);
        self.bytes[..len - first].copy_from_slice(&p.data[first..]);
        self.entries.push_back(Entry { meta: p.meta, start: tail, len });
        self.used += len;
        true
    }

    /// Removes the oldest packet and returns its metadata. Its bytes are left in place
    /// until a later push overwrites them. Use [`pop_front_into`](Self::pop_front_into)
    /// to keep them.
    pub fn pop_front(&mut self) -> Option<PacketMeta> {
        let entry = self.entries.pop_front()?;
        self.used -= entry.len;
        // An entry exists, so capacity > 0 and the `%` is safe.
        self.head = (entry.start + entry.len) % self.capacity();
        if self.entries.is_empty() {
            self.head = 0;
        }
        Some(entry.meta)
    }

    /// Copies the oldest packet's bytes into `out`, replacing its contents, then removes
    /// the packet and returns its metadata. `out` is cleared even when the ring is empty.
    /// The only allocation is whatever `out` needs to grow.
    pub fn pop_front_into(&mut self, out: &mut Vec<u8>) -> Option<PacketMeta> {
        out.clear();
        let entry = *self.entries.front()?;
        for piece in self.pieces(entry) {
            out.extend_from_slice(piece);
        }
        self.pop_front()
    }

    /// Iterates oldest-first over `(metadata, bytes)`. Each packet comes back as two
    /// slices that concatenate to its payload. The second slice is empty unless the
    /// packet wraps around the end of the ring.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (PacketMeta, [&[u8]; 2])> + '_ {
        self.entries.iter().map(move |entry| (entry.meta, self.pieces(*entry)))
    }

    /// Drops every packet. The bytes are left as they are.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.head = 0;
        self.used = 0;
    }

    /// The one or two slices that hold `entry`'s payload, in order.
    fn pieces(&self, entry: Entry) -> [&[u8]; 2] {
        let first = entry.len.min(self.capacity() - entry.start);
        [&self.bytes[entry.start..entry.start + first], &self.bytes[..entry.len - first]]
    }
}

impl fmt::Debug for PacketRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The bytes are deliberately left out: a ring can be hundreds of megabytes.
        f.debug_struct("PacketRing")
            .field("capacity", &self.capacity())
            .field("used", &self.used)
            .field("len", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TrackId;
    use crate::time::Hns;

    fn meta(pts_ms: i64) -> PacketMeta {
        PacketMeta {
            track: TrackId::Video,
            pts: Hns::from_millis(pts_ms),
            duration: Hns::from_millis(16),
            keyframe: true,
        }
    }

    #[test]
    fn ring_wraps_packets_around_the_end() {
        let mut ring = PacketRing::new(10, 8);
        assert!(ring.push(PacketRef { meta: meta(0), data: b"aaaa" }));
        assert!(ring.push(PacketRef { meta: meta(1), data: b"bbbb" }));
        assert_eq!(ring.pop_front(), Some(meta(0)));
        // 'b' occupies bytes 4..8, so 'c' starts at offset 8 and only 2 bytes fit before the end.
        assert!(ring.push(PacketRef { meta: meta(2), data: b"cccc" }));

        let packets: Vec<_> = ring.iter().collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].0, meta(1));
        assert_eq!(packets[0].1.concat(), b"bbbb");
        assert_eq!(packets[1].0, meta(2));
        assert_eq!(packets[1].1.map(|piece| piece.len()), [2, 2]);
        assert_eq!(packets[1].1.concat(), b"cccc");
        assert_eq!((ring.front(), ring.back()), (Some(meta(1)), Some(meta(2))));

        // The copying pop used by the spool sees the same wrapped bytes.
        let mut out = Vec::new();
        assert_eq!(ring.pop_front_into(&mut out), Some(meta(1)));
        assert_eq!(out, b"bbbb");
        assert_eq!(ring.pop_front_into(&mut out), Some(meta(2)));
        assert_eq!(out, b"cccc");
        assert_eq!(ring.pop_front_into(&mut out), None);
        assert!(out.is_empty());
    }

    #[test]
    fn ring_refuses_what_does_not_fit() {
        // Full by bytes: 8 of 8 are used, so even one more byte is refused.
        let mut by_bytes = PacketRing::new(8, 4);
        assert!(by_bytes.push(PacketRef { meta: meta(0), data: b"12345678" }));
        assert!(!by_bytes.push(PacketRef { meta: meta(1), data: b"x" }));
        assert_eq!((by_bytes.len(), by_bytes.used()), (1, 8));

        // Full by count: 2 packets fill max_packets, though 98 bytes are free.
        let mut by_count = PacketRing::new(100, 2);
        assert!(by_count.push(PacketRef { meta: meta(0), data: b"x" }));
        assert!(by_count.push(PacketRef { meta: meta(1), data: b"x" }));
        assert!(!by_count.push(PacketRef { meta: meta(2), data: b"x" }));
        assert_eq!((by_count.len(), by_count.used()), (2, 2));

        // Capacity 0 refuses everything, even an empty packet, and must not divide by zero.
        let mut none = PacketRing::new(0, 4);
        assert!(!none.push(PacketRef { meta: meta(0), data: b"x" }));
        assert!(!none.push(PacketRef { meta: meta(0), data: b"" }));
        assert!(none.is_empty());
        assert_eq!(none.used(), 0);
    }
}
