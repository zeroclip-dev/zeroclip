//! PCM → AAC with the Media Foundation AAC encoder
//! ([AAC Encoder](https://learn.microsoft.com/windows/win32/medfound/aac-encoder)).

use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::GUID;
use zc_core::Hns;
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::ports::SourceError;
use zc_core::sink::PacketSink;
use zc_win::error::WinResultExt;

/// `CLSID_AACMFTEncoder` from wmcodecdsp.h; windows-rs 0.62 doesn't define it.
const CLSID_AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);

/// The Microsoft encoder only accepts 96, 128, 160 or 192 kbps (as bytes per
/// second); snap to the nearest.
fn aac_bytes_per_second(bitrate_bps: u32) -> u32 {
    let bytes = bitrate_bps / 8;
    [12_000u32, 16_000, 20_000, 24_000]
        .into_iter()
        .min_by_key(|&b| b.abs_diff(bytes))
        .unwrap_or(24_000)
}

/// Microsoft's AAC encoder MFT, fed interleaved 16-bit PCM.
///
/// Raw AAC out (`MF_MT_AAC_PAYLOAD_TYPE` = 0): one packet per 1024-sample frame
/// (21.33 ms at 48 kHz). The muxer builds the AudioSpecificConfig itself. Input and
/// output samples are allocated once, so encoding allocates nothing.
#[derive(Debug)]
pub(crate) struct AacEncoder {
    transform: IMFTransform,
    track: TrackId,
    rate: u32,
    channels: u16,
    input: IMFSample,
    input_buffer: IMFMediaBuffer,
    /// Input buffer size, in frames.
    input_frames: u32,
    output: IMFSample,
    output_buffer: IMFMediaBuffer,
}

impl AacEncoder {
    /// Opens the encoder for `track`. COM and Media Foundation must be started on
    /// this thread.
    pub(crate) fn new(
        track: TrackId,
        sample_rate: u32,
        channels: u16,
        bitrate_bps: u32,
    ) -> Result<Self, SourceError> {
        // `encode` and the mic loop divide by the channel count.
        if channels == 0 || sample_rate == 0 {
            return Err(SourceError::Other(format!(
                "AAC needs at least one channel and a sample rate (got {channels} ch, {sample_rate} Hz)"
            )));
        }
        let block = u32::from(channels) * 2;
        // SAFETY: plain COM calls on objects created here.
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_AAC_ENCODER, None, CLSCTX_INPROC_SERVER)
                    .context("CoCreateInstance(AAC encoder)")?;
            let audio_type = |subtype: &GUID| -> windows::core::Result<_> {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
                t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(channels))?;
                Ok(t)
            };
            let output = audio_type(&MFAudioFormat_AAC).context("AAC output type")?;
            output
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, aac_bytes_per_second(bitrate_bps))
                .context("AAC bitrate")?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).context("AAC payload type")?;
            transform.SetOutputType(0, &output, 0).context("AAC SetOutputType")?;
            let input = audio_type(&MFAudioFormat_PCM).context("PCM input type")?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block).context("block alignment")?;
            input
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * block)
                .context("PCM byte rate")?;
            transform.SetInputType(0, &input, 0).context("AAC SetInputType")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;

            // Up to one second of PCM per input sample; the output size is the
            // encoder's own figure (one AAC frame is at most 1536 bytes for stereo).
            let input_buffer =
                MFCreateMemoryBuffer(sample_rate * block).context("MFCreateMemoryBuffer")?;
            let input_sample = MFCreateSample().context("MFCreateSample")?;
            input_sample.AddBuffer(&input_buffer).context("AddBuffer")?;
            let out_size =
                transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?.cbSize.max(2048);
            let output_buffer = MFCreateMemoryBuffer(out_size).context("MFCreateMemoryBuffer")?;
            let output_sample = MFCreateSample().context("MFCreateSample")?;
            output_sample.AddBuffer(&output_buffer).context("AddBuffer")?;
            Ok(Self {
                transform,
                track,
                rate: sample_rate,
                channels,
                input: input_sample,
                input_buffer,
                input_frames: sample_rate,
                output: output_sample,
                output_buffer,
            })
        }
    }

    fn frame_time(&self, frames: u64) -> Hns {
        let ticks = i128::from(frames) * i128::from(Hns::PER_SECOND) / i128::from(self.rate);
        Hns(i64::try_from(ticks).unwrap_or(i64::MAX))
    }

    /// Encodes interleaved PCM whose first frame is at raw time `pts`; pushes AAC
    /// frames into `sink`.
    pub(crate) fn encode(
        &mut self,
        pcm: &[i16],
        pts: Hns,
        sink: &dyn PacketSink,
    ) -> Result<(), SourceError> {
        let ch = usize::from(self.channels);
        let frames = u32::try_from(pcm.len() / ch).unwrap_or(u32::MAX);
        self.feed(frames, pts, sink, |dst, first, n| {
            let src = &pcm[first as usize * ch..(first + n) as usize * ch];
            // SAFETY: `dst` holds at least `n * ch` i16s (checked by `feed`).
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.cast::<i16>(), src.len()) };
        })
    }

    /// Encodes `frames` frames of silence starting at `pts`.
    pub(crate) fn encode_silence(
        &mut self,
        frames: u32,
        pts: Hns,
        sink: &dyn PacketSink,
    ) -> Result<(), SourceError> {
        let ch = usize::from(self.channels);
        self.feed(frames, pts, sink, |dst, _, n| {
            // SAFETY: `dst` holds at least `n * ch` i16s (checked by `feed`).
            unsafe { std::ptr::write_bytes(dst.cast::<i16>(), 0, n as usize * ch) };
        })
    }

    /// Feeds `frames` frames in chunks that fit the input buffer; `fill(dst, first,
    /// n)` writes frames `first..first + n` into the locked buffer.
    fn feed(
        &mut self,
        frames: u32,
        pts: Hns,
        sink: &dyn PacketSink,
        mut fill: impl FnMut(*mut u8, u32, u32),
    ) -> Result<(), SourceError> {
        let block = u32::from(self.channels) * 2;
        let mut done = 0u32;
        while done < frames {
            let n = (frames - done).min(self.input_frames);
            // SAFETY: the buffer is locked while written and holds `input_frames`
            // frames, so `n` frames fit.
            unsafe {
                let mut data = std::ptr::null_mut();
                self.input_buffer.Lock(&mut data, None, None).context("Lock (PCM)")?;
                fill(data, done, n);
                self.input_buffer.Unlock().context("Unlock (PCM)")?;
                self.input_buffer.SetCurrentLength(n * block).context("SetCurrentLength")?;
                let time = pts + self.frame_time(u64::from(done));
                self.input.SetSampleTime(time.ticks()).context("SetSampleTime")?;
                self.input
                    .SetSampleDuration(self.frame_time(u64::from(n)).ticks())
                    .context("SetSampleDuration")?;
                match self.transform.ProcessInput(0, &self.input, 0) {
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        self.drain_outputs(sink)?;
                        self.transform
                            .ProcessInput(0, &self.input, 0)
                            .context("ProcessInput (AAC)")?;
                    }
                    other => other.context("ProcessInput (AAC)")?,
                }
            }
            self.drain_outputs(sink)?;
            done += n;
        }
        Ok(())
    }

    /// Pushes every AAC frame the encoder has ready.
    fn drain_outputs(&mut self, sink: &dyn PacketSink) -> Result<(), SourceError> {
        loop {
            // SAFETY: plain COM calls on objects we own. Both ManuallyDrop fields
            // are dropped exactly once after ProcessOutput, or COM references leak.
            let result = unsafe {
                self.output_buffer.SetCurrentLength(0).context("SetCurrentLength")?;
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(self.output.clone())),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut out, &mut status);
                ManuallyDrop::drop(&mut out[0].pSample);
                ManuallyDrop::drop(&mut out[0].pEvents);
                result
            };
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e).context("ProcessOutput (AAC)"),
            }
            // SAFETY: the buffer stays locked while the slice is alive.
            unsafe {
                let pts = Hns(self.output.GetSampleTime().context("GetSampleTime")?);
                let duration = Hns(self.output.GetSampleDuration().unwrap_or(0));
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                self.output_buffer.Lock(&mut data, None, Some(&mut len)).context("Lock (AAC)")?;
                // A null pointer or zero length would make the slice undefined behaviour.
                let bytes: &[u8] = if data.is_null() || len == 0 {
                    &[]
                } else {
                    std::slice::from_raw_parts(data, len as usize)
                };
                if !bytes.is_empty() {
                    sink.push(PacketRef {
                        meta: PacketMeta { track: self.track, pts, duration, keyframe: true },
                        data: bytes,
                    });
                }
                self.output_buffer.Unlock().context("Unlock (AAC)")?;
            }
        }
    }

    /// Flushes the frames the encoder still holds: on a timeline restart and on stop.
    pub(crate) fn drain(&mut self, sink: &dyn PacketSink) -> Result<(), SourceError> {
        // SAFETY: plain COM call.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("drain (AAC)")?;
        self.drain_outputs(sink)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use zc_core::Packet;
    use zc_win::com::Apartment;
    use zc_win::mf::MediaFoundation;

    use super::*;

    #[test]
    fn aac_bitrate_snaps_to_supported_values() {
        assert_eq!(aac_bytes_per_second(192_000), 24_000);
        assert_eq!(aac_bytes_per_second(64_000), 12_000);
        assert_eq!(aac_bytes_per_second(140_000), 16_000);
        assert_eq!(aac_bytes_per_second(320_000), 24_000);
    }

    #[derive(Default)]
    struct Collect(Mutex<Vec<Packet>>);

    impl PacketSink for Collect {
        fn push(&self, packet: PacketRef<'_>) {
            self.0.lock().unwrap().push(packet.to_packet());
        }
    }

    #[test]
    fn encodes_a_second_of_sine_then_silence() {
        let (Ok(_apartment), Ok(_mf)) = (Apartment::multithreaded(), MediaFoundation::start())
        else {
            return; // no Media Foundation on this image
        };
        let mut encoder = AacEncoder::new(TrackId::GameAudio, 48_000, 2, 192_000).expect("encoder");
        let sink = Collect::default();
        let mut phase = 0.0f32;
        for k in 0..100i64 {
            let pcm: Vec<i16> = (0..480)
                .flat_map(|_| {
                    phase += 440.0 * std::f32::consts::TAU / 48_000.0;
                    let s = (phase.sin() * 8_000.0) as i16;
                    [s, s]
                })
                .collect();
            encoder.encode(&pcm, Hns(k * 100_000), &sink).expect("encode");
        }
        // Then 100 ms of silence, as the timeline inserts for a gap.
        encoder.encode_silence(4_800, Hns(100 * 100_000), &sink).expect("silence");
        let before_drain = sink.0.lock().unwrap().len();
        encoder.drain(&sink).expect("drain");
        let packets = sink.0.into_inner().unwrap();
        // The encoder holds a couple of frames of lookahead until drained.
        assert!(before_drain < packets.len(), "drain flushed nothing");
        // 52,800 samples is 51.6 AAC frames, plus encoder priming.
        assert!((52..=55).contains(&packets.len()), "{} packets", packets.len());
        assert!(packets.iter().all(|p| p.meta.keyframe && p.meta.track == TrackId::GameAudio));
        assert!(packets.windows(2).all(|w| w[1].meta.pts > w[0].meta.pts));
        for p in &packets {
            assert!((p.meta.duration.ticks() - 213_333).abs() <= 1, "{:?}", p.meta.duration);
            assert!(!p.data.is_empty() && p.data.len() <= 1536);
        }
    }
}
