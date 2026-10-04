//! Media Foundation hardware encoding and the codec capability ladder (DESIGN §3).
//!
//! Hardware encoders are asynchronous MFTs
//! ([Asynchronous MFTs](https://learn.microsoft.com/windows/win32/medfound/asynchronous-mfts)):
//! they say when they want input (`METransformNeedInput`) and when output is ready
//! (`METransformHaveOutput`). [`HwEncoder`] exposes exactly that, without blocking,
//! and the `zc-video` encoder thread drives it.
//!
//! No `IMF*` interface is `Send` in windows-rs, so an encoder is created and used on
//! one thread.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncMPVSceneDetection, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateSample, MFMediaType_Video, MFNominalRange_16_235, MFSampleExtension_CleanPoint,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_PeakConstrainedVBR,
    eAVEncH264VProfile_High, eAVEncH265VProfile_Main_420_8, eAVEncMPVSceneDetection_None,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};
use zc_core::Hns;
use zc_core::media::{Codec, PacketMeta, PacketRef, TrackId};
use zc_core::ports::{SourceError, VideoStart};
use zc_core::sink::PacketSink;
use zc_win::error::WinResultExt;

use crate::device::adapter_desc;

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::Av1 => MFVideoFormat_AV1,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::H264 => MFVideoFormat_H264,
    }
}

/// Hardware encoders for `codec` that take NV12, on the same GPU as `device`
/// (which matters on hybrid laptops).
fn enum_encoders(device: &ID3D11Device, codec: Codec) -> Result<Vec<IMFActivate>, SourceError> {
    let luid = adapter_desc(device)?.AdapterLuid;
    let mut luid_bytes = [0u8; 8];
    luid_bytes[..4].copy_from_slice(&luid.LowPart.to_ne_bytes());
    luid_bytes[4..].copy_from_slice(&luid.HighPart.to_ne_bytes());
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output =
        MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut ptr = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: valid attribute store and out-parameters. `MFTEnum2` returns a
    // CoTaskMemAlloc'd array of `count` activates: we take each one, then free the
    // array exactly once.
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1).context("MFCreateAttributes")?;
        let attributes = attributes
            .ok_or_else(|| SourceError::Other("MFCreateAttributes returned nothing".into()))?;
        attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes).context("MFT_ENUM_ADAPTER_LUID")?;
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attributes,
            &mut ptr,
            &mut count,
        )
        .context("MFTEnum2")?;
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        let activates = std::slice::from_raw_parts_mut(ptr, count as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(ptr.cast()));
        Ok(activates)
    }
}

/// Which codecs this GPU can encode in hardware, best first. Enumeration only: if
/// an encoder lists but won't open, `GpuVideoSource` steps down the ladder.
/// Media Foundation must be started on this thread.
#[must_use]
pub fn probe(device: &ID3D11Device) -> Vec<Codec> {
    let supported: Vec<Codec> = Codec::LADDER
        .into_iter()
        .filter(|&c| enum_encoders(device, c).is_ok_and(|e| !e.is_empty()))
        .collect();
    tracing::info!(?supported, "hardware video encoders");
    supported
}

/// Frames per GOP for a fixed keyframe interval (at least 1).
fn gop_frames(fps: u32, interval: Hns) -> u32 {
    let frames = i64::from(fps) * interval.ticks() / Hns::PER_SECOND;
    u32::try_from(frames.max(1)).unwrap_or(u32::MAX)
}

/// What the encoder wants next. See [`HwEncoder::next_event`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderEvent {
    /// Ready for one more frame ([`HwEncoder::submit`]).
    NeedInput,
    /// A packet is ready ([`HwEncoder::take_output`]).
    HaveOutput,
    /// Everything queued has come out after [`HwEncoder::drain`] began.
    DrainComplete,
    /// Anything else; ignore it.
    Other,
}

/// A hardware encoder MFT, fed NV12 textures from the converter's ring, producing
/// encoded packets.
///
/// Configured with peak-constrained VBR, a fixed GOP (never scene-adaptive, D5), no
/// B-frames (D6) and low latency. Input is zero-copy: the MFT gets our D3D11 device
/// through a DXGI device manager, and each NV12 texture is wrapped in a sample once,
/// up front.
#[derive(Debug)]
pub struct HwEncoder {
    codec: Codec,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    activate: IMFActivate,
    _manager: IMFDXGIDeviceManager,
    samples: Vec<IMFSample>,
    frame_duration: Hns,
}

impl HwEncoder {
    /// Opens a hardware encoder for `config.codec` on `device`, encoding `size`
    /// pixels, with one input sample per texture in `inputs`.
    ///
    /// # Errors
    /// No such hardware encoder, or it rejected the configuration.
    pub fn new(
        device: &ID3D11Device,
        config: &VideoStart,
        size: (u32, u32),
        inputs: &[ID3D11Texture2D],
    ) -> Result<Self, SourceError> {
        let codec = config.codec;
        let activate = enum_encoders(device, codec)?.into_iter().next().ok_or_else(|| {
            SourceError::Unsupported(format!("no hardware {} encoder", codec.name()))
        })?;
        let frame_size = (u64::from(size.0) << 32) | u64::from(size.1);
        let frame_rate = (u64::from(config.fps.max(1)) << 32) | 1;

        // SAFETY: plain COM calls on objects created here; out-parameters are valid
        // `Option`s; the device manager is kept alive in the struct for as long as
        // the transform holds it.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;
            let attributes = transform.GetAttributes().context("IMFTransform::GetAttributes")?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 1 {
                return Err(SourceError::Unsupported(
                    "the hardware encoder isn't asynchronous".into(),
                ));
            }
            // Must come before any other IMFTransform call (MF_E_TRANSFORM_ASYNC_LOCKED).
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .context("MF_TRANSFORM_ASYNC_UNLOCK")?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 1 {
                return Err(SourceError::Unsupported(
                    "the hardware encoder can't take D3D11 input".into(),
                ));
            }

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .context("MFCreateDXGIDeviceManager")?;
            let manager =
                manager.ok_or_else(|| SourceError::Other("no DXGI device manager".into()))?;
            manager.ResetDevice(device, token).context("IMFDXGIDeviceManager::ResetDevice")?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("MFT_MESSAGE_SET_D3D_MANAGER")?;

            // Rate control and GOP before the media types, as FFmpeg's mfenc does.
            let codec_api: ICodecAPI = transform.cast().context("ICodecAPI")?;
            let gop = gop_frames(config.fps, config.keyframe_interval);
            codec_api
                .SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop))
                .context("CODECAPI_AVEncMPVGOPSize")?; // DESIGN §3: a fixed GOP is required.
            let optional: [(&GUID, VARIANT, &str); 6] = [
                (
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
                    "peak-constrained VBR",
                ),
                (
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(config.min_bitrate_bps),
                    "mean bitrate",
                ),
                (
                    &CODECAPI_AVEncCommonMaxBitRate,
                    VARIANT::from(config.max_bitrate_bps),
                    "max bitrate",
                ),
                (&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames"),
                (&CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency"),
                (
                    &CODECAPI_AVEncMPVSceneDetection,
                    VARIANT::from(eAVEncMPVSceneDetection_None.0 as u32),
                    "no scene detection",
                ),
            ];
            for (api, value, what) in &optional {
                if let Err(e) = codec_api.SetValue(*api, value) {
                    tracing::warn!(%e, setting = what, "encoder ignored a setting");
                }
            }

            let video_type = |subtype: &GUID| -> windows::core::Result<IMFMediaType> {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
                t.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                Ok(t)
            };
            let output = video_type(&subtype(codec)).context("encoder output type")?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, config.min_bitrate_bps)
                .context("MF_MT_AVG_BITRATE")?;
            // Must match the converter's output colour space (BT.709, limited range).
            for (key, value) in [
                (&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32),
                (&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32),
                (&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32),
                (&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32),
            ] {
                output.SetUINT32(key, value).context("encoder colour tags")?;
            }
            let profile = match codec {
                Codec::H264 => Some(eAVEncH264VProfile_High.0 as u32),
                Codec::Hevc => Some(eAVEncH265VProfile_Main_420_8.0 as u32),
                Codec::Av1 => None,
            };
            if let Some(profile) = profile {
                output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).context("MF_MT_MPEG2_PROFILE")?;
            }
            transform.SetOutputType(0, &output, 0).context("SetOutputType")?;
            let input = video_type(&MFVideoFormat_NV12).context("encoder input type")?;
            transform.SetInputType(0, &input, 0).context("SetInputType")?;

            let info = transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                return Err(SourceError::Unsupported(
                    "the hardware encoder doesn't allocate its output".into(),
                ));
            }
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;

            // One sample per ring texture, made once, so the hot path creates nothing.
            let samples = inputs
                .iter()
                .map(|texture| -> windows::core::Result<IMFSample> {
                    let buffer =
                        MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
                    buffer
                        .SetCurrentLength(buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?)?;
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    Ok(sample)
                })
                .collect::<windows::core::Result<Vec<_>>>()
                .context("encoder input samples")?;

            let events: IMFMediaEventGenerator =
                transform.cast().context("IMFMediaEventGenerator")?;
            tracing::info!(codec = codec.name(), ?size, gop, "hardware encoder ready");
            Ok(Self {
                codec,
                transform,
                events,
                codec_api,
                activate,
                _manager: manager,
                samples,
                frame_duration: Hns(Hns::PER_SECOND / i64::from(config.fps.max(1))),
            })
        }
    }

    /// The codec in use.
    #[must_use]
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The next encoder event, if any. Never blocks, so a wedged encoder can't hang
    /// shutdown.
    ///
    /// # Errors
    /// Encoder faults.
    pub fn next_event(&self) -> Result<Option<EncoderEvent>, SourceError> {
        // SAFETY: plain COM calls on a live event generator.
        let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
            Err(e) => return Err(e).context("IMFMediaEventGenerator::GetEvent"),
        };
        // SAFETY: as above.
        let kind = unsafe { event.GetType() }.context("IMFMediaEvent::GetType")?;
        Ok(Some(match kind {
            k if k == METransformNeedInput.0 as u32 => EncoderEvent::NeedInput,
            k if k == METransformHaveOutput.0 as u32 => EncoderEvent::HaveOutput,
            k if k == METransformDrainComplete.0 as u32 => EncoderEvent::DrainComplete,
            _ => EncoderEvent::Other,
        }))
    }

    /// Queues the NV12 texture in ring `slot`, captured at raw time `pts`. Only call
    /// after [`EncoderEvent::NeedInput`].
    ///
    /// # Errors
    /// Encoder faults, or a slot out of range.
    pub fn submit(&mut self, slot: usize, pts: Hns) -> Result<(), SourceError> {
        let sample = self
            .samples
            .get(slot)
            .ok_or_else(|| SourceError::Other(format!("no encoder input for slot {slot}")))?;
        // SAFETY: plain COM calls on a live sample and transform.
        unsafe {
            sample.SetSampleTime(pts.ticks()).context("SetSampleTime")?;
            sample.SetSampleDuration(self.frame_duration.ticks()).context("SetSampleDuration")?;
            self.transform.ProcessInput(0, sample, 0).context("ProcessInput")
        }
    }

    /// Takes one encoded packet and pushes it into `sink`. Only call after
    /// [`EncoderEvent::HaveOutput`]. Allocates nothing on our side.
    ///
    /// # Errors
    /// Encoder faults.
    pub fn take_output(&mut self, sink: &dyn PacketSink) -> Result<(), SourceError> {
        let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, ..Default::default() }];
        let mut status = 0u32;
        // SAFETY: valid output array and status for the call.
        match unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder settled its output format (e.g. after the first frame).
                // SAFETY: plain COM calls on a live transform.
                return unsafe {
                    self.transform
                        .GetOutputAvailableType(0, 0)
                        .and_then(|t| self.transform.SetOutputType(0, &t, 0))
                }
                .context("output stream change");
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        // SAFETY: both fields are `ManuallyDrop`; each is taken or dropped exactly
        // once. Skipping this leaks a sample every frame.
        let sample = unsafe {
            ManuallyDrop::drop(&mut out[0].pEvents);
            ManuallyDrop::take(&mut out[0].pSample)
        };
        let Some(sample) = sample else { return Ok(()) };
        // SAFETY: plain COM calls on the sample the encoder just handed us; the
        // buffer stays locked while the slice is alive and is unlocked right after.
        unsafe {
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let pts = Hns(sample.GetSampleTime().context("GetSampleTime")?);
            let duration = Hns(sample.GetSampleDuration().unwrap_or(self.frame_duration.ticks()));
            let buffer = sample.ConvertToContiguousBuffer().context("ConvertToContiguousBuffer")?;
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len)).context("IMFMediaBuffer::Lock")?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            sink.push(PacketRef {
                meta: PacketMeta { track: TrackId::Video, pts, duration, keyframe },
                data: bytes,
            });
            buffer.Unlock().context("IMFMediaBuffer::Unlock")?;
        }
        Ok(())
    }

    /// Flushes everything queued into `sink`, waiting up to 2 s. Errors are logged:
    /// this runs while shutting down.
    pub fn drain(&mut self, sink: &dyn PacketSink) {
        // SAFETY: plain COM calls on a live transform.
        let started = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .and_then(|()| self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0))
        };
        if let Err(e) = started {
            tracing::warn!(%e, "encoder drain failed to start");
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.next_event() {
                Ok(Some(EncoderEvent::HaveOutput)) => {
                    if let Err(e) = self.take_output(sink) {
                        tracing::warn!(%e, "encoder drain");
                        return;
                    }
                }
                Ok(Some(EncoderEvent::DrainComplete)) => return,
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => {
                    tracing::warn!(%e, "encoder drain");
                    return;
                }
            }
        }
        tracing::warn!("encoder drain timed out");
    }

    /// Makes the next frame an IDR (`CODECAPI_AVEncVideoForceKeyFrame`).
    ///
    /// # Errors
    /// The encoder rejected the request.
    pub fn force_keyframe(&mut self) -> Result<(), SourceError> {
        // SAFETY: plain COM call with a valid VARIANT.
        unsafe { self.codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) }
            .context("CODECAPI_AVEncVideoForceKeyFrame")
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        // SAFETY: shutting down objects we own; failures here have no remedy.
        unsafe {
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gop_frames_follow_the_keyframe_interval() {
        assert_eq!(gop_frames(60, Hns::from_secs(1)), 60);
        assert_eq!(gop_frames(144, Hns::from_secs(2)), 288);
        assert_eq!(gop_frames(30, Hns::from_millis(500)), 15);
        assert_eq!(gop_frames(60, Hns::ZERO), 1);
    }
}
