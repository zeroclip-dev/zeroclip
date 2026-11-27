//! Chunked writes.
//!
//! Buffer in RAM and hand the file system 8–16 MB at a time instead of one frame
//! at a time: far fewer head repositions on an HDD, and fewer syscalls everywhere.

use std::io::{self, Write};

/// A [`Write`] adapter that forwards data to `inner` only in whole `chunk_size`
/// blocks, except when explicitly flushed.
///
/// Dropping it without calling [`flush`](Write::flush) or [`into_inner`](Self::into_inner)
/// loses the pending partial chunk. There is no `Drop` impl that flushes for you.
///
/// The chunk buffer is allocated once, in [`new`](Self::new).
#[derive(Debug)]
pub struct ChunkedWriter<W: Write> {
    inner: W,
    chunk: Vec<u8>,
    chunk_size: usize,
}

impl<W: Write> ChunkedWriter<W> {
    /// Wraps `inner`, buffering `chunk_size` bytes at a time.
    ///
    /// # Panics
    /// If `chunk_size` is zero.
    #[must_use]
    pub fn new(inner: W, chunk_size: usize) -> Self {
        assert!(chunk_size > 0, "chunk size must be positive");
        Self { inner, chunk: Vec::with_capacity(chunk_size), chunk_size }
    }

    /// The configured chunk size.
    #[must_use]
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Bytes accepted but not yet forwarded to the inner writer.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.chunk.len()
    }

    /// Flushes whatever is buffered and returns the inner writer.
    ///
    /// # Errors
    /// Any error from the inner writer.
    pub fn into_inner(mut self) -> io::Result<W> {
        self.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        // The chunk is forwarded as soon as it fills, so there is always room for
        // at least one byte here. Extending within the capacity from `new` never allocates.
        let room = self.chunk_size - self.chunk.len();
        let n = data.len().min(room);
        let before = self.chunk.len();
        self.chunk.extend_from_slice(&data[..n]);
        if self.chunk.len() == self.chunk_size {
            if let Err(e) = self.inner.write_all(&self.chunk) {
                // Drop this call's bytes so the caller's retry cannot send them twice.
                self.chunk.truncate(before);
                return Err(e);
            }
            self.chunk.clear();
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.chunk.is_empty() {
            self.inner.write_all(&self.chunk)?;
            self.chunk.clear();
        }
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the size of every write it receives.
    #[derive(Default)]
    struct Recorder {
        writes: Vec<usize>,
        bytes: Vec<u8>,
    }

    impl Write for Recorder {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.writes.push(data.len());
            self.bytes.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const MIB: usize = 1024 * 1024;

    #[test]
    fn chunked_forwards_only_whole_chunks() {
        let mut w = ChunkedWriter::new(Recorder::default(), 8 * MIB);
        let frame = vec![7u8; 3 * MIB];
        for _ in 0..10 {
            w.write_all(&frame).unwrap();
        }
        // 30 MiB written: three whole 8 MiB chunks forwarded, 6 MiB pending.
        assert_eq!(w.pending(), 6 * MIB);
        let inner = w.into_inner().unwrap();
        assert_eq!(inner.writes, [8 * MIB, 8 * MIB, 8 * MIB, 6 * MIB]);
        assert_eq!(inner.bytes.len(), 30 * MIB);
    }

    #[test]
    fn chunked_holds_small_writes_until_a_chunk_fills() {
        let mut w = ChunkedWriter::new(Recorder::default(), 1_000);
        for _ in 0..9 {
            w.write_all(&[1u8; 100]).unwrap();
        }
        assert_eq!(w.pending(), 900, "nothing forwarded yet");
        w.write_all(&[1u8; 100]).unwrap();
        assert_eq!(w.pending(), 0, "exactly one chunk forwarded");
    }

    #[test]
    fn chunked_flush_forwards_the_partial_chunk_in_order() {
        let mut w = ChunkedWriter::new(Recorder::default(), 4);
        w.write_all(b"abcdef").unwrap();
        w.flush().unwrap();
        assert_eq!(w.pending(), 0);
        w.write_all(b"gh").unwrap();
        let inner = w.into_inner().unwrap();
        assert_eq!(inner.bytes, b"abcdefgh");
        assert_eq!(inner.writes, [4, 2, 2]);
    }
}
