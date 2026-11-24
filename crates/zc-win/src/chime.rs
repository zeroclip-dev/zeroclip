//! Short chimes for clip saved, recording start and stop, and screenshots.
//!
//! The sounds are made from sine notes at first use and played with
//! [`PlaySoundW`](https://learn.microsoft.com/en-us/windows/win32/api/playsoundapi/nf-playsoundapi-playsoundw)
//! from memory, so there are no sound files to ship. `SND_ASYNC` returns at once; the buffers
//! live in a static, because Windows reads them after the call returns.

use std::sync::OnceLock;

use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

/// Which chime to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chime {
    ClipSaved,
    RecordingStarted,
    RecordingStopped,
    Screenshot,
}

const RATE: u32 = 22_050;

/// `(frequency in Hz, length in ms)` per note.
fn notes(chime: Chime) -> &'static [(f32, u32)] {
    match chime {
        Chime::ClipSaved => &[(784.0, 90), (1175.0, 160)],
        Chime::RecordingStarted => &[(660.0, 80), (880.0, 140)],
        Chime::RecordingStopped => &[(880.0, 80), (660.0, 140)],
        Chime::Screenshot => &[(1568.0, 70)],
    }
}

/// A mono 16-bit WAV file of `notes`, each faded in and out so it doesn't click.
fn wav(notes: &[(f32, u32)]) -> Vec<u8> {
    let mut pcm: Vec<i16> = Vec::new();
    for &(hz, ms) in notes {
        let n = (RATE * ms / 1000) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let fade = (i.min(n - i) as f32 / (RATE as f32 * 0.01)).min(1.0);
            let sample = (t * hz * std::f32::consts::TAU).sin() * 0.22 * fade;
            pcm.push((sample * f32::from(i16::MAX)) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Plays `chime` without waiting for it. A failure is ignored: a missing chime is harmless.
pub fn play(chime: Chime) {
    static SOUNDS: OnceLock<[Vec<u8>; 4]> = OnceLock::new();
    let sounds = SOUNDS.get_or_init(|| {
        [Chime::ClipSaved, Chime::RecordingStarted, Chime::RecordingStopped, Chime::Screenshot]
            .map(|c| wav(notes(c)))
    });
    let bytes = &sounds[chime as usize];
    // SAFETY: with SND_MEMORY the first argument points to a WAV image. `bytes` is in a static,
    // so it outlives the asynchronous playback.
    let _ = unsafe {
        PlaySoundW(PCWSTR(bytes.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_matches_its_data() {
        let w = wav(notes(Chime::Screenshot));
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len);
        assert_eq!(data_len, 2 * (RATE * 70 / 1000) as usize);
    }
}
