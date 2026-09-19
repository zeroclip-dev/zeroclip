//! Media Foundation hardware encoding and the codec capability ladder (DESIGN §3).

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use zc_core::Hns;
use zc_core::media::Codec;
use zc_core::ports::{SourceError, VideoStart};
use zc_core::sink::PacketSink;

/// Which codecs this GPU can encode in hardware, best first.
///
/// Phase 1 plan: for each of `MFVideoFormat_AV1`, `MFVideoFormat_HEVC`,
/// `MFVideoFormat_H264`, call `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER,
/// MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output_type))`.
/// Enumeration isn't proof: confirm by actually configuring each candidate (a
/// driver can list an encoder it can't open). The result is cached per GPU and
/// driver version (Phase 4: in SQLite), and fed to [`Codec::select`].
#[must_use]
pub fn probe(device: &ID3D11Device) -> Vec<Codec> {
    let _ = device;
    todo!("Phase 1: MFTEnumEx per codec, then a trial configuration")
}

/// A hardware encoder MFT, fed NV12 textures, producing encoded packets.
///
/// Phase 1 plan:
/// 1. Activate the MFT from `probe`'s winner; it's asynchronous: unlock with
///    `MF_TRANSFORM_ASYNC_UNLOCK`.
/// 2. `MFCreateDXGIDeviceManager` + `ResetDevice(device)`, then
///    `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager)` — this is what keeps
///    input zero-copy.
/// 3. Output type first (codec, `MF_MT_AVG_BITRATE`, frame size/rate), then the NV12
///    input type.
/// 4. Via `ICodecAPI`: peak-constrained VBR (`CODECAPI_AVEncCommonRateControlMode`,
///    mean = min bitrate, `CODECAPI_AVEncCommonMaxBitRate` = max), fixed GOP
///    (`CODECAPI_AVEncMPVGOPSize` = fps × keyframe interval — never adaptive),
///    no B-frames (`CODECAPI_AVEncMPVDefaultBPictureCount` = 0) and low latency.
/// 5. Event loop on `IMFMediaEventGenerator`: `METransformNeedInput` →
///    `MFCreateDXGISurfaceBuffer` + `MFCreateSample` + `SetSampleTime(pts)` →
///    `ProcessInput`; `METransformHaveOutput` → `ProcessOutput` →
///    `MFSampleExtension_CleanPoint` (keyframe), `GetSampleTime`, bytes →
///    [`PacketSink::push`].
/// 6. Report failing `HRESULT`s as `HealthEvent::EncoderFault` — the watchdog decides.
#[derive(Debug)]
pub struct HwEncoder {
    codec: Codec,
}

impl HwEncoder {
    /// Opens a hardware encoder for `config.codec` on `device`.
    ///
    /// # Errors
    /// No such hardware encoder, or it rejected the configuration.
    pub fn new(
        device: &ID3D11Device,
        config: &VideoStart,
        size: (u32, u32),
    ) -> Result<Self, SourceError> {
        let _ = (device, config, size);
        todo!("Phase 1: activate + configure the MFT (see the plan above)")
    }

    /// The codec in use.
    #[must_use]
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Queues one NV12 frame. Encoded packets arrive asynchronously in `sink`.
    ///
    /// # Errors
    /// Encoder faults (the caller reports them to the watchdog).
    pub fn encode(
        &mut self,
        frame: &ID3D11Texture2D,
        pts: Hns,
        sink: &dyn PacketSink,
    ) -> Result<(), SourceError> {
        let _ = (frame, pts, sink);
        todo!("Phase 1: ProcessInput / ProcessOutput")
    }

    /// Makes the next frame an IDR (`CODECAPI_AVEncVideoForceKeyFrame`).
    ///
    /// # Errors
    /// The encoder rejected the request.
    pub fn force_keyframe(&mut self) -> Result<(), SourceError> {
        todo!("Phase 1: ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)")
    }
}
