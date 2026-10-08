//! Fragmented MP4 boxes (ISO/IEC 14496-12, with the AVC and AAC sample descriptions of
//! 14496-15 and 14496-3). This is the container every recording and DVR dump is written in
//! (D18).
//!
//! A file is an init segment, `ftyp` and `moov`, which declares the tracks. Its `mvex` says
//! that fragments follow. Then there is one `moof` and `mdat` pair per GOP. Every number is
//! big-endian, and every box size counts the 8-byte header.

use std::io;

use tracing::warn;
use zc_core::media::{Codec, TrackInfo, TrackKind};

use crate::bitstream;

/// Ticks per second. Hns ticks are 100 ns, so every track uses them directly (D7).
const TIMESCALE: u32 = 10_000_000;

/// The unity transformation matrix (ISO/IEC 14496-12 §8.2.2.2), in 16.16 fixed point.
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// The packed ISO-639-2 code `und` (undetermined). Each letter is five bits, counted from
/// `a` = 1, and the top bit is padding: `u` = 0x15, `n` = 0x0E, `d` = 0x04.
const LANGUAGE_UND: u16 = 0x55C4;

/// Builds the init segment: `ftyp`, then `moov` with an `mvhd`, one `trak` per track and an
/// `mvex` that declares a `trex` per track. Track IDs are one-based positions in `tracks`.
///
/// `video_config` holds the in-band Annex-B parameter sets from the first keyframe. It is
/// used when the video track declares no `codec_private`.
///
/// # Errors
/// [`io::ErrorKind::Unsupported`] for a codec that is not written yet, and
/// [`io::ErrorKind::InvalidData`] for a size or configuration that the boxes cannot hold.
fn init_segment(tracks: &[TrackInfo], video_config: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let track_count = u32::try_from(tracks.len()).map_err(invalid_data)?;
    let mut moov = mvhd(track_count + 1)?;
    for (index, track) in tracks.iter().enumerate() {
        let track_id = u32::try_from(index + 1).map_err(invalid_data)?;
        moov.extend(trak(track_id, track, video_config)?);
    }
    let trexes = (1..=track_count).map(trex).collect::<io::Result<Vec<_>>>()?;
    moov.extend(boxed(b"mvex", &trexes.concat())?);

    let mut file = ftyp()?;
    file.extend(boxed(b"moov", &moov)?);
    Ok(file)
}

/// An `InvalidData` error, for input that the writer cannot represent.
fn invalid_data(reason: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// The 8-byte header of a box whose body is `body_len` bytes: a 32-bit size that counts the
/// header, then the four-character type.
fn box_header(kind: &[u8; 4], body_len: usize) -> io::Result<[u8; 8]> {
    let size = body_len.checked_add(8).and_then(|size| u32::try_from(size).ok());
    let size = size.ok_or_else(|| {
        invalid_data(format!("'{}' box exceeds 4 GiB", String::from_utf8_lossy(kind)))
    })?;
    let mut header = [0; 8];
    header[..4].copy_from_slice(&size.to_be_bytes());
    header[4..].copy_from_slice(kind);
    Ok(header)
}

/// A plain box: its header, then `body`.
fn boxed(kind: &[u8; 4], body: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = box_header(kind, body.len())?.to_vec();
    out.extend_from_slice(body);
    Ok(out)
}

/// A FullBox: a box whose body starts with a one-byte version and 24 bits of flags.
fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> io::Result<Vec<u8>> {
    let mut payload = vec![version];
    payload.extend_from_slice(&flags.to_be_bytes()[1..]);
    payload.extend_from_slice(body);
    boxed(kind, &payload)
}

/// Big-endian 32-bit values, for box bodies made of counts and sizes.
fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_be_bytes()).collect()
}

/// Appends the unity matrix.
fn push_matrix(body: &mut Vec<u8>) {
    for value in UNITY_MATRIX {
        body.extend_from_slice(&value.to_be_bytes());
    }
}

/// `ftyp`: major brand `iso6` (fragmented files need brand 6 or later), minor version 0,
/// and the brands that players such as `isom` and `mp41` may know the file by.
fn ftyp() -> io::Result<Vec<u8>> {
    let mut body = b"iso6".to_vec();
    body.extend_from_slice(&0u32.to_be_bytes());
    for brand in [b"iso6", b"isom", b"mp41"] {
        body.extend_from_slice(brand);
    }
    boxed(b"ftyp", &body)
}

/// `mvhd` (version 0). The movie duration is 0 because fragments carry the durations.
fn mvhd(next_track_id: u32) -> io::Result<Vec<u8>> {
    let mut body = vec![0; 8]; // creation_time, modification_time
    body.extend_from_slice(&TIMESCALE.to_be_bytes());
    body.extend_from_slice(&[0; 4]); // duration
    body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    body.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    body.extend_from_slice(&[0; 10]); // reserved
    push_matrix(&mut body);
    body.extend_from_slice(&[0; 24]); // pre_defined
    body.extend_from_slice(&next_track_id.to_be_bytes());
    full_box(b"mvhd", 0, 0, &body)
}

/// `tkhd` (version 0, flags 3: enabled and in the movie). Audio is at full volume and
/// video has none. `size` is the encoded size in pixels, zero for audio.
fn tkhd(track_id: u32, track: &TrackInfo, size: (u16, u16)) -> io::Result<Vec<u8>> {
    let mut body = vec![0; 8]; // creation_time, modification_time
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&[0; 4]); // reserved
    body.extend_from_slice(&[0; 4]); // duration
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&[0; 4]); // layer, alternate_group
    let volume: u16 = if track.track.is_audio() { 0x0100 } else { 0 };
    body.extend_from_slice(&volume.to_be_bytes());
    body.extend_from_slice(&[0; 2]); // reserved
    push_matrix(&mut body);
    // Width and height are 16.16 fixed point.
    body.extend_from_slice(&(u32::from(size.0) << 16).to_be_bytes());
    body.extend_from_slice(&(u32::from(size.1) << 16).to_be_bytes());
    full_box(b"tkhd", 0, 3, &body)
}

/// `mdhd` (version 0): the media timescale and the `und` language.
fn mdhd() -> io::Result<Vec<u8>> {
    let mut body = vec![0; 8]; // creation_time, modification_time
    body.extend_from_slice(&TIMESCALE.to_be_bytes());
    body.extend_from_slice(&[0; 4]); // duration
    body.extend_from_slice(&LANGUAGE_UND.to_be_bytes());
    body.extend_from_slice(&[0; 2]); // pre_defined
    full_box(b"mdhd", 0, 0, &body)
}

/// `hdlr`: the handler type (`vide` or `soun`) and the track's name, which players show.
fn hdlr(handler: &[u8; 4], name: &str) -> io::Result<Vec<u8>> {
    let mut body = vec![0; 4]; // pre_defined
    body.extend_from_slice(handler);
    body.extend_from_slice(&[0; 12]); // reserved
    // The name is NUL-terminated, so a NUL inside the label would end it early. Drop those.
    body.extend(name.bytes().filter(|&byte| byte != 0));
    body.push(0);
    full_box(b"hdlr", 0, 0, &body)
}

/// `vmhd`: video media header. Flags 1 is required by the spec, and the body is zero.
fn vmhd() -> io::Result<Vec<u8>> {
    full_box(b"vmhd", 0, 1, &[0; 8])
}

/// `smhd`: sound media header, centred balance.
fn smhd() -> io::Result<Vec<u8>> {
    full_box(b"smhd", 0, 0, &[0; 4])
}

/// `dinf`: one `url ` entry with flag 1, which says the file holds its own data.
fn dinf() -> io::Result<Vec<u8>> {
    let url = full_box(b"url ", 0, 1, &[])?;
    let mut dref = u32s(&[1]);
    dref.extend(url);
    boxed(b"dinf", &full_box(b"dref", 0, 0, &dref)?)
}

/// `stbl` around one sample description. Fragmented files keep the sample tables empty,
/// because the samples are listed in each fragment.
fn stbl(sample_entry: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut stsd = u32s(&[1]);
    stsd.extend(sample_entry);
    let children = [
        full_box(b"stsd", 0, 0, &stsd)?,
        full_box(b"stts", 0, 0, &u32s(&[0]))?,
        full_box(b"stsc", 0, 0, &u32s(&[0]))?,
        full_box(b"stsz", 0, 0, &u32s(&[0, 0]))?, // sample_size, sample_count
        full_box(b"stco", 0, 0, &u32s(&[0]))?,
    ];
    boxed(b"stbl", &children.concat())
}

/// `trex`: the defaults a fragment falls back to. Every fragment sets its own values, so
/// these are all zero, except that the description index is 1.
fn trex(track_id: u32) -> io::Result<Vec<u8>> {
    full_box(b"trex", 0, 0, &u32s(&[track_id, 1, 0, 0, 0]))
}

/// A `VisualSampleEntry` (ISO/IEC 14496-12 §12.1.3) of type `kind`, followed by its
/// configuration box. The 78 bytes before the children are fixed.
fn visual_entry(kind: &[u8; 4], size: (u16, u16), config_box: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = vec![0; 6]; // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    body.extend_from_slice(&[0; 16]); // pre_defined, reserved, pre_defined[3]
    body.extend_from_slice(&size.0.to_be_bytes());
    body.extend_from_slice(&size.1.to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution, 72 dpi
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution
    body.extend_from_slice(&[0; 4]); // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    body.extend_from_slice(&[0; 32]); // compressorname
    body.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    body.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    body.extend_from_slice(config_box);
    boxed(kind, &body)
}

/// An `AudioSampleEntry` of type `mp4a` (ISO/IEC 14496-12 §12.2.3), followed by its `esds`.
fn audio_entry(sample_rate: u32, channels: u16, esds: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = vec![0; 6]; // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&channels.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes()); // samplesize
    body.extend_from_slice(&[0; 4]); // pre_defined, reserved
    // samplerate is 16.16 fixed point, so it holds rates only up to 65 535 Hz. Above that
    // it is 0, and the rate comes from the AudioSpecificConfig in `esds`.
    let rate = u16::try_from(sample_rate).map_or(0, |rate| u32::from(rate) << 16);
    body.extend_from_slice(&rate.to_be_bytes());
    body.extend_from_slice(esds);
    boxed(b"mp4a", &body)
}

/// An `esds` FullBox (ISO/IEC 14496-14) that carries the AAC decoder configuration.
fn esds(config: &[u8]) -> io::Result<Vec<u8>> {
    // DecoderSpecificInfo (tag 5) is the AudioSpecificConfig itself.
    let specific = descriptor(0x05, config)?;
    // DecoderConfigDescriptor (tag 4): object type 0x40 is MPEG-4 audio, and stream type 5
    // (audio) sits in the top six bits of the next byte, with the reserved bit set to 1.
    // The buffer size and bit rates are unknown, so they are zero.
    let mut decoder = vec![0x40, 0x15, 0, 0, 0];
    decoder.extend_from_slice(&[0; 8]); // maxBitrate, avgBitrate
    decoder.extend(specific);
    // SLConfigDescriptor (tag 6): predefined 2 means the MP4 file layout.
    let sl_config = descriptor(0x06, &[0x02])?;
    // ES_Descriptor (tag 3): ES_ID 0, then flags 0.
    let mut es = vec![0, 0, 0];
    es.extend(descriptor(0x04, &decoder)?);
    es.extend(sl_config);
    full_box(b"esds", 0, 0, &descriptor(0x03, &es)?)
}

/// An MPEG-4 descriptor: a tag, a one-byte length and the payload. The length has to fit in
/// seven bits, so a longer payload is refused rather than encoded in the expandable form.
fn descriptor(tag: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
    let len = match u8::try_from(payload.len()) {
        Ok(len) if len < 0x80 => len,
        _ => return Err(invalid_data("AAC config longer than 127 bytes")),
    };
    let mut out = vec![tag, len];
    out.extend_from_slice(payload);
    Ok(out)
}

/// One `trak`: `tkhd`, then `mdia` with the media header, the handler, and the sample
/// description for the track's codec.
fn trak(track_id: u32, track: &TrackInfo, video_config: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let (handler, default_name, media_header, size, entry) = match &track.kind {
        TrackKind::Video { codec, width, height, .. } => {
            let size = (
                u16::try_from(*width).map_err(|_| invalid_data("video width exceeds 65535"))?,
                u16::try_from(*height).map_err(|_| invalid_data("video height exceeds 65535"))?,
            );
            // In-band parameter sets are used only when the track declares none.
            let params = if track.codec_private.is_empty() {
                video_config.unwrap_or(&[])
            } else {
                track.codec_private.as_slice()
            };
            let entry = video_entry(*codec, size, params)?;
            (b"vide", "Video", vmhd()?, size, entry)
        }
        TrackKind::Audio { sample_rate, channels } => {
            // The AudioSpecificConfig is ours to build, unless the adapter declared one.
            let config = if track.codec_private.is_empty() {
                bitstream::aac_lc_config(*sample_rate, *channels).to_vec()
            } else {
                track.codec_private.clone()
            };
            let entry = audio_entry(*sample_rate, *channels, &esds(&config)?)?;
            (b"soun", "Audio", smhd()?, (0, 0), entry)
        }
    };
    let name = if track.label.is_empty() { default_name } else { track.label.as_str() };

    let minf = boxed(b"minf", &[media_header, dinf()?, stbl(entry)?].concat())?;
    let mdia = boxed(b"mdia", &[mdhd()?, hdlr(handler, name)?, minf].concat())?;
    boxed(b"trak", &[tkhd(track_id, track, size)?, mdia].concat())
}

/// The sample description for a video track: `avc1` with `avcC`, from the first parameter
/// sets the stream carries.
fn video_entry(codec: Codec, size: (u16, u16), params: &[u8]) -> io::Result<Vec<u8>> {
    match codec {
        Codec::H264 => {
            let record = match bitstream::h264_parameter_sets(params) {
                Some((sps, pps)) => bitstream::avcc(sps, pps).map_err(invalid_data)?,
                None => {
                    warn!(
                        "no H.264 parameter sets before the first keyframe; the file declares none"
                    );
                    bitstream::avcc(&[], &[]).map_err(invalid_data)?
                }
            };
            visual_entry(b"avc1", size, &boxed(b"avcC", &record)?)
        }
        Codec::Hevc => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "HEVC fragments aren't written yet (P3.12)",
        )),
        Codec::Av1 => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AV1 fragments aren't written yet (P3.13)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use zc_core::media::TrackId;

    use super::*;

    fn video() -> TrackInfo {
        TrackInfo {
            track: TrackId::Video,
            label: "Video".into(),
            kind: TrackKind::Video { codec: Codec::H264, width: 320, height: 240, fps: 60 },
            codec_private: Vec::new(),
        }
    }

    fn audio(track: TrackId, label: &str) -> TrackInfo {
        TrackInfo {
            track,
            label: label.into(),
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            codec_private: Vec::new(),
        }
    }

    /// Splits a buffer into its boxes: each box's type and body (without the header).
    fn boxes(mut buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut found = Vec::new();
        while !buf.is_empty() {
            let size = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = buf[4..8].try_into().unwrap();
            assert!((8..=buf.len()).contains(&size), "box {kind:?} overruns its parent");
            found.push((kind, &buf[8..size]));
            buf = &buf[size..];
        }
        found
    }

    /// The body of the first box of type `kind` directly inside `buf`.
    fn child<'a>(buf: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
        let (_, body) = boxes(buf).into_iter().find(|(k, _)| k == kind).expect("box present");
        body
    }

    fn kinds(buf: &[u8]) -> Vec<String> {
        boxes(buf).iter().map(|(kind, _)| String::from_utf8_lossy(kind).into_owned()).collect()
    }

    /// The sample entries of the first track: `moov`, `trak`, `mdia`, `minf`, `stbl`, `stsd`.
    fn sample_entries(init: &[u8]) -> &[u8] {
        let moov = child(init, b"moov");
        let stsd =
            child(child(child(child(child(moov, b"trak"), b"mdia"), b"minf"), b"stbl"), b"stsd");
        &stsd[8..]
    }

    #[test]
    fn ftyp_names_the_iso6_brand() {
        let expected = [
            0, 0, 0, 28, // size
            b'f', b't', b'y', b'p', //
            b'i', b's', b'o', b'6', // major_brand
            0, 0, 0, 0, // minor_version
            b'i', b's', b'o', b'6', // compatible brands
            b'i', b's', b'o', b'm', //
            b'm', b'p', b'4', b'1', //
        ];
        assert_eq!(ftyp().unwrap(), expected);
    }

    #[test]
    fn trex_declares_default_description_one() {
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 32, b't', b'r', b'e', b'x', // size and type
            0, 0, 0, 0, // version 0, flags 0
            0, 0, 0, 2, // track_ID
            0, 0, 0, 1, // default_sample_description_index
            0, 0, 0, 0, // default_sample_duration
            0, 0, 0, 0, // default_sample_size
            0, 0, 0, 0, // default_sample_flags
        ];
        assert_eq!(trex(2).unwrap(), expected);
    }

    #[test]
    fn mdhd_uses_the_timescale_and_und_language() {
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 32, b'm', b'd', b'h', b'd',
            0, 0, 0, 0, // version 0, flags 0
            0, 0, 0, 0, // creation_time
            0, 0, 0, 0, // modification_time
            0x00, 0x98, 0x96, 0x80, // timescale 10 000 000
            0, 0, 0, 0, // duration
            0x55, 0xC4, // language: und
            0, 0, // pre_defined
        ];
        assert_eq!(mdhd().unwrap(), expected);
    }

    #[test]
    fn vmhd_and_smhd_are_literal() {
        #[rustfmt::skip]
        let vmhd_expected = [
            0, 0, 0, 20, b'v', b'm', b'h', b'd',
            0, 0, 0, 1, // version 0, flags 1
            0, 0, 0, 0, 0, 0, 0, 0, // graphicsmode and opcolor
        ];
        assert_eq!(vmhd().unwrap(), vmhd_expected);
        #[rustfmt::skip]
        let smhd_expected = [
            0, 0, 0, 16, b's', b'm', b'h', b'd',
            0, 0, 0, 0, // version 0, flags 0
            0, 0, 0, 0, // balance and reserved
        ];
        assert_eq!(smhd().unwrap(), smhd_expected);
    }

    #[test]
    fn hdlr_carries_the_handler_and_a_nul_terminated_name() {
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 37, b'h', b'd', b'l', b'r',
            0, 0, 0, 0, // version 0, flags 0
            0, 0, 0, 0, // pre_defined
            b's', b'o', b'u', b'n', // handler_type
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // reserved
            b'G', b'a', b'm', b'e', 0, // name
        ];
        assert_eq!(hdlr(b"soun", "Game").unwrap(), expected);
    }

    #[test]
    fn esds_carries_the_audio_specific_config() {
        // The 48 kHz stereo AudioSpecificConfig is 0x11 0x90 (see bitstream). The descriptor
        // lengths are 0x19 for ES_Descriptor and 0x11 for DecoderConfigDescriptor.
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 39, b'e', b's', b'd', b's',
            0, 0, 0, 0, // version 0, flags 0
            0x03, 0x19, 0, 0, 0, // ES_Descriptor, ES_ID 0, flags 0
            0x04, 0x11, 0x40, 0x15, // DecoderConfigDescriptor: MPEG-4 audio, stream type 5
            0, 0, 0, // bufferSizeDB
            0, 0, 0, 0, // maxBitrate
            0, 0, 0, 0, // avgBitrate
            0x05, 0x02, 0x11, 0x90, // DecoderSpecificInfo: the AudioSpecificConfig
            0x06, 0x01, 0x02, // SLConfigDescriptor, predefined MP4
        ];
        assert_eq!(esds(&[0x11, 0x90]).unwrap(), expected);
    }

    #[test]
    fn tkhd_is_92_bytes_and_carries_the_size_in_16_16() {
        let tkhd = tkhd(1, &video(), (320, 240)).unwrap();
        assert_eq!(tkhd.len(), 92);
        assert_eq!(&tkhd[..8], [0, 0, 0, 92, b't', b'k', b'h', b'd']);
        assert_eq!(&tkhd[8..12], [0, 0, 0, 3], "flags: enabled and in the movie");
        // Track ID 1, then the width and height as 16.16 fixed point.
        assert_eq!(&tkhd[20..24], [0, 0, 0, 1]);
        assert_eq!(&tkhd[84..], [0x01, 0x40, 0, 0, 0x00, 0xF0, 0, 0]);
    }

    #[test]
    fn mvhd_is_108_bytes_with_the_unity_matrix() {
        let mvhd = mvhd(4).unwrap();
        assert_eq!(mvhd.len(), 108);
        let matrix: Vec<u8> = UNITY_MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect();
        assert_eq!(&mvhd[44..80], matrix.as_slice());
        assert_eq!(&mvhd[104..], [0, 0, 0, 4], "next_track_ID");
    }

    #[test]
    fn init_segment_layout() {
        let tracks = [video(), audio(TrackId::GameAudio, "Game"), audio(TrackId::Mic, "Mic")];
        let in_band: [u8; 17] =
            [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80];
        let init = init_segment(&tracks, Some(&in_band)).unwrap();

        assert_eq!(kinds(&init), ["ftyp", "moov"]);
        let moov = child(&init, b"moov");
        assert_eq!(kinds(moov), ["mvhd", "trak", "trak", "trak", "mvex"]);
        let traks: Vec<&[u8]> =
            boxes(moov).iter().filter(|(k, _)| k == b"trak").map(|(_, b)| *b).collect();
        // Video first, then game audio, then the mic: the order players read as track 1, 2, 3.
        let handler = |trak: &[u8]| child(child(trak, b"mdia"), b"hdlr")[8..12].to_vec();
        assert_eq!(handler(traks[0]), b"vide");
        assert_eq!(handler(traks[1]), b"soun");
        assert_eq!(handler(traks[2]), b"soun");
        let track_ids: Vec<u32> = traks
            .iter()
            // The tkhd body is version and flags, creation and modification times, then the ID.
            .map(|trak| u32::from_be_bytes(child(trak, b"tkhd")[12..16].try_into().unwrap()))
            .collect();
        assert_eq!(track_ids, [1, 2, 3]);
        assert_eq!(kinds(child(moov, b"mvex")), ["trex", "trex", "trex"]);
    }

    #[test]
    fn avc1_carries_the_avcc_from_the_first_keyframe() {
        let in_band: [u8; 17] =
            [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80];
        let init = init_segment(&[video()], Some(&in_band)).unwrap();
        let avc1 = child(sample_entries(&init), b"avc1");
        // The visual fields: width at byte 24 and height at byte 26 of the entry body.
        assert_eq!(&avc1[24..28], [0x01, 0x40, 0x00, 0xF0], "320 x 240");
        // A VisualSampleEntry has 78 fixed bytes before its child boxes.
        #[rustfmt::skip]
        let expected = [
            1, 0x64, 0x00, 0x28, // version, profile, constraints, level
            0xFF, 0xE1, // 4-byte NAL lengths, one SPS
            0x00, 0x05, 0x67, 0x64, 0x00, 0x28, 0xAC, // SPS
            1, 0x00, 0x04, 0x68, 0xEE, 0x3C, 0x80, // one PPS
        ];
        assert_eq!(child(&avc1[78..], b"avcC"), expected);
    }

    #[test]
    fn declared_codec_private_wins_over_in_band_parameter_sets() {
        let mut track = video();
        track.codec_private =
            vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80];
        let init = init_segment(&[track], Some(&[0, 0, 0, 1, 0x67, 0x64, 0, 0x28])).unwrap();
        let avc1 = child(sample_entries(&init), b"avc1");
        // The avcC's profile, constraints and level come from the declared SPS.
        assert_eq!(child(&avc1[78..], b"avcC")[1..4], [0x42, 0xC0, 0x1E]);
    }

    #[test]
    fn hevc_and_av1_are_unsupported_until_their_cards() {
        let mut track = video();
        track.kind = TrackKind::Video { codec: Codec::Hevc, width: 320, height: 240, fps: 60 };
        assert_eq!(init_segment(&[track], None).unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
}
