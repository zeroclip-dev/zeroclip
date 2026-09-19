//! PCM → AAC with the Media Foundation AAC encoder.

use windows::Win32::Media::MediaFoundation::IMFTransform;
use zc_core::Hns;
use zc_core::ports::SourceError;
use zc_core::sink::PacketSink;

/// Wraps Microsoft's AAC encoder MFT (`CLSID_AACMFTEncoder`).
///
/// Phase 2 plan: input type PCM 16-bit (or float → convert), 48 kHz, 2 channels;
/// output type `MFAudioFormat_AAC` with `MF_MT_AUDIO_AVG_BYTES_PER_SECOND` = bitrate/8
/// and `MF_MT_AAC_PAYLOAD_TYPE` = 0 (raw AAC; the container carries the
/// `AudioSpecificConfig` from `MF_MT_USER_DATA`). Each output sample is one
/// 1024-sample frame (21.33 ms at 48 kHz) → one packet.
#[derive(Debug)]
pub(crate) struct AacEncoder {
    #[expect(dead_code, reason = "scaffold: Phase 2")]
    transform: IMFTransform,
}

impl AacEncoder {
    /// Opens the encoder.
    #[expect(dead_code, reason = "scaffold: Phase 2")]
    pub(crate) fn new(
        sample_rate: u32,
        channels: u16,
        bitrate_bps: u32,
    ) -> Result<Self, SourceError> {
        let _ = (sample_rate, channels, bitrate_bps);
        todo!("Phase 2: CoCreateInstance(CLSID_AACMFTEncoder) + media types")
    }

    /// Encodes interleaved PCM captured at raw time `pts`; pushes AAC frames into `sink`.
    #[expect(dead_code, reason = "scaffold: Phase 2")]
    pub(crate) fn encode(
        &mut self,
        pcm: &[i16],
        pts: Hns,
        sink: &dyn PacketSink,
    ) -> Result<(), SourceError> {
        let _ = (pcm, pts, sink);
        todo!("Phase 2: ProcessInput / ProcessOutput")
    }
}
