//! WASAPI process loopback (include mode), shared by [`GameAudioSource`](crate::GameAudioSource)
//! and [`AppAudioSource`](crate::AppAudioSource): activation of one process tree, and the
//! start-up every process-loopback capture thread shares.
//!
//! Process loopback is an activation-time option, not a device: the client is created
//! through [`ActivateAudioInterfaceAsync`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync)
//! with the `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` target. See
//! [loopback recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording).

use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, SyncSender};
use std::time::Duration;

use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, IAgileObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, Ref, implement};
use zc_core::media::TrackId;
use zc_core::mix::MixInput;
use zc_core::ports::{AudioStart, SourceError};
use zc_win::com::Apartment;
use zc_win::error::{WinResultExt, platform_error};
use zc_win::mf::MediaFoundation;

use crate::capture::Encode;

/// Signals when the asynchronous activation finishes. Agile and panic-free, as
/// COM callbacks must be. Implements [`IActivateAudioInterfaceCompletionHandler`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-iactivateaudiointerfacecompletionhandler).
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion {
    done: SyncSender<()>,
}

// IAgileObject is a marker interface: no methods, it just tells COM the handler can
// be called from any thread, so COM may call it on its own worker thread instead of
// marshalling back to ours. See [`IAgileObject`](https://learn.microsoft.com/en-us/windows/win32/api/objidl/nn-objidl-iagileobject).
impl windows::Win32::System::Com::IAgileObject_Impl for Completion_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.done.try_send(());
        Ok(())
    }
}

/// An audio client capturing process `pid` and its children. Needs Windows 11 (build
/// 20348+); older systems get `SourceError::Unsupported`.
///
/// Activation is asynchronous only, so we wait on a channel with a timeout. The
/// parameters travel as a `VT_BLOB` [`PROPVARIANT`](https://learn.microsoft.com/en-us/windows/win32/api/propidlbase/ns-propidlbase-propvariant)
/// holding an [`AUDIOCLIENT_ACTIVATION_PARAMS`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_activation_params).
/// Include mode records the target's whole tree, so launcher and child processes are both covered.
pub(crate) fn activate(pid: u32) -> Result<IAudioClient, SourceError> {
    // Boxed, so a timeout can leak it: COM may still read it after we give up waiting.
    let params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    });
    // ManuallyDrop: windows-rs gives PROPVARIANT a Drop that calls
    // [`PropVariantClear`](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-propvariantclear),
    // which would CoTaskMemFree this blob's stack pointer and corrupt the heap.
    // `params` outlives the wait below.
    let prop = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw const *params).cast_mut().cast(),
                    },
                },
                ..Default::default()
            }),
        },
    });
    let (tx, rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Completion { done: tx }.into();
    let unsupported =
        || SourceError::Unsupported("process loopback needs Windows 11 (build 20348+)".into());
    // SAFETY: valid device id, IID, property and handler; `params` lives until the activation
    // has completed, and is leaked on a timeout (below) because COM may still read it.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop),
            &handler,
        )
    }
    .map_err(|e| {
        if e.code() == E_NOTIMPL || e.code() == E_NOINTERFACE {
            unsupported()
        } else {
            platform_error("ActivateAudioInterfaceAsync", &e)
        }
    })?;
    if rx.recv_timeout(Duration::from_secs(5)).is_err() {
        // 12 bytes per timeout, instead of a dangling read if COM completes late.
        std::mem::forget(params);
        return Err(SourceError::Other("process loopback activation timed out".into()));
    }
    let mut hr = windows::core::HRESULT(0);
    let mut unknown: Option<IUnknown> = None;
    // SAFETY: the operation has completed; valid out-parameters.
    unsafe { operation.GetActivateResult(&mut hr, &mut unknown) }.context("GetActivateResult")?;
    if hr == E_NOTIMPL || hr == E_NOINTERFACE {
        return Err(unsupported());
    }
    hr.ok().context("process loopback activation")?;
    unknown
        .ok_or_else(|| SourceError::Other("activation returned no client".into()))?
        .cast()
        .context("IAudioClient")
}

/// The start-up every process-loopback capture thread shares: COM, Media Foundation and the
/// track's encoder. The caller then calls `ready(Ok(()))` and runs its loop. The guards must
/// live as long as the thread.
pub(crate) fn setup(
    track: TrackId,
    config: &AudioStart,
    mix: Option<MixInput>,
) -> Result<(Apartment, MediaFoundation, Encode), SourceError> {
    let apartment = Apartment::multithreaded()?;
    let mf = MediaFoundation::start()?;
    let encode = Encode::new(track, config)?.with_mix(mix);
    Ok((apartment, mf, encode))
}
