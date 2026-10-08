//! Opens a clip the way the Windows media player does, through Media Foundation's source
//! reader, and decodes every sample of every stream. A file VLC plays but Windows doesn't
//! shows up here as an error, with the stream and time it happened at.
//!
//! `cargo run -p zc-video --example mf_read -- "<clip.mp4>"`

#![allow(clippy::print_stdout, reason = "a command-line check that reports on stdout")]

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_ANY_STREAM, MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MFAudioFormat_PCM, MFCreateMediaType, MFCreateSourceReaderFromURL,
    MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_NV12,
};
use windows::core::{GUID, HSTRING};
use zc_win::com::Apartment;
use zc_win::mf::MediaFoundation;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: mf_read <clip.mp4>")?;
    let _apartment = Apartment::multithreaded()?;
    let _mf = MediaFoundation::start()?;
    // SAFETY: plain COM calls on this thread's apartment; every out-pointer is a live local.
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_str()), None)?;
        // Players size their timeline from this; 0 or missing means "unknown length".
        match reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        {
            Ok(value) => match u64::try_from(&value) {
                Ok(duration) => println!("duration: {:.3} s", duration as f64 / 1e7),
                Err(_) => println!("duration: present but not a number"),
            },
            Err(e) => println!("duration: none ({e})"),
        }
        let mut streams = 0;
        while let Ok(native) = reader.GetNativeMediaType(streams, 0) {
            let major = native.GetGUID(&MF_MT_MAJOR_TYPE)?;
            let sub = native.GetGUID(&MF_MT_SUBTYPE)?;
            // Ask for decoded output, so the decoders load just as they do for playback.
            let out: IMFMediaType = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &major)?;
            let decoded: GUID =
                if major == MFMediaType_Video { MFVideoFormat_NV12 } else { MFAudioFormat_PCM };
            out.SetGUID(&MF_MT_SUBTYPE, &decoded)?;
            let kind = if major == MFMediaType_Video {
                "video"
            } else if major == MFMediaType_Audio {
                "audio"
            } else {
                "other"
            };
            match reader.SetCurrentMediaType(streams, None, &out) {
                Ok(()) => println!("stream {streams}: {kind} {sub:?}, decoder ready"),
                Err(e) => println!("stream {streams}: {kind} {sub:?}, NO DECODER: {e}"),
            }
            reader.SetStreamSelection(streams, true)?;
            streams += 1;
        }
        println!("{streams} streams; reading every sample…");

        let mut samples = vec![0u32; streams as usize];
        let mut last = vec![0i64; streams as usize];
        let mut ended = 0;
        while ended < streams {
            let (mut index, mut flags, mut time) = (0u32, 0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            if let Err(e) = reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&raw mut index),
                Some(&raw mut flags),
                Some(&raw mut time),
                Some(&raw mut sample),
            ) {
                println!("ReadSample failed after {samples:?} samples: {e}");
                return Ok(());
            }
            let i = index as usize;
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                println!("stream {index}: error flag at {:.3} s", time as f64 / 1e7);
                return Ok(());
            }
            if sample.is_some() && i < samples.len() {
                samples[i] += 1;
                last[i] = time;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                ended += 1;
            }
        }
        for i in 0..samples.len() {
            println!("stream {i}: {} samples, last at {:.3} s", samples[i], last[i] as f64 / 1e7);
        }
    }
    Ok(())
}
