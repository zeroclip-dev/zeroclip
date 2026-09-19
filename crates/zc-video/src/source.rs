//! [`GpuVideoSource`]: the pipeline behind the engine's `VideoSource` port.

use std::path::Path;

use zc_core::media::Codec;
use zc_core::ports::{Emit, SourceError, VideoSource, VideoStart};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;

/// Capture → convert → encode, running on the frame pool's thread.
///
/// `start` wires [`device`](crate::device), [`capture`](crate::capture),
/// [`convert`](crate::convert) and [`encoder`](crate::encoder) together; `stop`
/// tears them down (and is what the watchdog's re-init calls before starting again).
#[derive(Debug, Default)]
pub struct GpuVideoSource {
    running: bool,
}

impl GpuVideoSource {
    /// A stopped pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoSource for GpuVideoSource {
    fn start(
        &mut self,
        config: &VideoStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<Codec, SourceError> {
        let _ = (config, sink, health);
        todo!(
            "Phase 1: Apartment + MediaFoundation guards, create_device, probe → Codec::select, \
             capture_item, NvConverter, HwEncoder, WgcCapture::start with a callback that \
             converts + encodes each frame and reports FrameDelivered/FrameDropped to `health`"
        )
    }

    fn stop(&mut self) {
        if self.running {
            todo!("Phase 1: stop the session, drain the encoder, then release the device")
        }
    }

    fn request_keyframe(&mut self) {
        todo!("Phase 1: HwEncoder::force_keyframe")
    }

    fn screenshot(&mut self, path: &Path) -> Result<(), SourceError> {
        let _ = path;
        todo!("Phase 1: grab the latest frame → screenshot::save_png")
    }
}
