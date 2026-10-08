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
    /// Absolute position of the packet's first byte: the bytes written before it.
    abs: u64,
    /// Payload length in bytes.
    len: usize,
}

/// A FIFO of packets: payload bytes in one ring, metadata in a bounded queue.
///
/// Both limits are hard. A push that would exceed the byte capacity or the packet
/// count is refused and nothing is evicted, so the caller decides what to drop.
///
/// Every byte ever pushed has an absolute position, counted from 0. Byte `p` is stored
/// at offset `p % capacity`, so a popped packet's bytes stay put until the writer laps
/// them. [`copy_if_intact`](Self::copy_if_intact) uses that to detect overwritten data.
pub struct PacketRing {
    /// The byte ring. Fixed size, zeroed on creation, never reallocated.
    bytes: Box<[u8]>,
    /// One entry per packet, oldest first.
    entries: VecDeque<Entry>,
    /// Limit on `entries`. Kept here because `VecDeque` may reserve more than asked.
    max_packets: usize,
    /// Payload bytes stored, across all packets.
    used: usize,
    /// Payload bytes ever pushed. Never reset, not even by `clear`.
    written: u64,
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
            used: 0,
            written: 0,
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
        // `has_room_for` guarantees capacity > 0, so `offset` can't divide by zero.
        let capacity = self.capacity();
        let tail = Self::offset(self.written, capacity);
        // The packet may wrap: `first` bytes fit before the end, the rest go at the start.
        let first = len.min(capacity - tail);
        self.bytes[tail..tail + first].copy_from_slice(&p.data[..first]);
        self.bytes[..len - first].copy_from_slice(&p.data[first..]);
        self.entries.push_back(Entry { meta: p.meta, abs: self.written, len });
        self.used += len;
        self.written += len as u64;
        true
    }

    /// Removes the oldest packet and returns its metadata. Its bytes are left in place
    /// until a later push overwrites them. Use [`pop_front_into`](Self::pop_front_into)
    /// to keep them.
    pub fn pop_front(&mut self) -> Option<PacketMeta> {
        let entry = self.entries.pop_front()?;
        self.used -= entry.len;
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

    /// Oldest-first `(metadata, absolute position, length)` of each packet, without touching
    /// the bytes. Pass a position to [`copy_if_intact`](Self::copy_if_intact) to copy it.
    pub fn positions(&self) -> impl DoubleEndedIterator<Item = (PacketMeta, u64, usize)> + '_ {
        self.entries.iter().map(|entry| (entry.meta, entry.abs, entry.len))
    }

    /// Copies the `len` bytes written at absolute position `abs` into `out`. Returns `false`,
    /// and leaves `out` alone, if any of them has been overwritten, was never written, or if
    /// `out` isn't `len` bytes long.
    ///
    /// The writer reuses byte `p` only after `capacity` more bytes have been written, so the
    /// bytes are intact while `abs + capacity >= written`. Call this under the same lock as
    /// [`push`](Self::push), so the check and the copy see the same bytes.
    #[must_use]
    pub fn copy_if_intact(&self, abs: u64, len: usize, out: &mut [u8]) -> bool {
        let capacity = self.capacity();
        let intact = capacity > 0
            && len <= capacity
            && out.len() == len
            && abs.saturating_add(len as u64) <= self.written
            && self.written <= abs.saturating_add(capacity as u64);
        if !intact {
            return false;
        }
        let start = Self::offset(abs, capacity);
        let first = len.min(capacity - start);
        out[..first].copy_from_slice(&self.bytes[start..start + first]);
        out[first..].copy_from_slice(&self.bytes[..len - first]);
        true
    }

    /// Drops every packet. The bytes are left as they are, and `written` keeps counting.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }

    /// The one or two slices that hold `entry`'s payload, in order.
    fn pieces(&self, entry: Entry) -> [&[u8]; 2] {
        let capacity = self.capacity();
        let start = Self::offset(entry.abs, capacity);
        let first = entry.len.min(capacity - start);
        [&self.bytes[start..start + first], &self.bytes[..entry.len - first]]
    }

    /// Offset in the ring of the byte at absolute position `abs`. Needs `capacity > 0`.
    fn offset(abs: u64, capacity: usize) -> usize {
        // The remainder is below `capacity`, so it fits in a `usize`.
        (abs % capacity as u64) as usize
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

    #[test]
    fn ring_copy_if_intact_detects_overwritten_bytes() {
        let mut ring = PacketRing::new(10, 8);
        assert!(ring.push(PacketRef { meta: meta(0), data: b"aaaa" }));
        assert!(ring.push(PacketRef { meta: meta(1), data: b"bbbbbb" }));
        // Popping leaves "aaaa" in the ring, so it can still be copied.
        assert!(ring.pop_front().is_some());
        let mut out = [0; 4];
        assert!(ring.copy_if_intact(0, 4, &mut out));
        assert_eq!(&out, b"aaaa");
        // "cccc" lands on the bytes "aaaa" used. "bbbbbb" sits clear of them.
        assert!(ring.push(PacketRef { meta: meta(2), data: b"cccc" }));
        assert!(!ring.copy_if_intact(0, 4, &mut out), "overwritten");
        let mut b = [0; 6];
        assert!(ring.copy_if_intact(4, 6, &mut b));
        assert_eq!(&b, b"bbbbbb");
        // A buffer of the wrong length is refused, not truncated.
        assert!(!ring.copy_if_intact(4, 6, &mut out));
    }
}
