//! Fragmented MP4 boxes (ISO/IEC 14496-12, with the AVC and HEVC sample descriptions of
//! 14496-15 and the AAC ones of 14496-3). This is the container every recording and DVR dump
//! is written in (D18).
//!
//! A file is an init segment, `ftyp` and `moov`, which declares the tracks. Its `mvex` says
//! that fragments follow, and its `mehd` holds the file's length, which is filled in once the
//! file is finished. Then there is one `moof` and `mdat` pair per GOP, and at the end an `mfra`
//! index of those fragments. Every number is big-endian, and every box size counts the 8-byte
//! header.

use std::io::{self, Write};

use tracing::warn;
use zc_core::media::{Codec, PacketRef, TrackId, TrackInfo, TrackKind};
use zc_core::time::Hns;

use crate::bitstream;
use crate::container::ContainerWriter;

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
    // The length is 0 until the file is finished. The writer cannot seek back to set it, so
    // `repair::set_fragment_duration` fills it in. Media Foundation reads 0 as no length.
    let mut mvex = mehd(0)?;
    mvex.extend(trexes.concat());
    moov.extend(boxed(b"mvex", &mvex)?);

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

/// `tkhd` (version 0, flags 3: enabled and in the movie; 2 for audio other than Master). Audio is at full volume and
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
    // Players play the first enabled audio track. Only Master is enabled (flags 3); the other
    // audio tracks are in the movie but switched off (flags 2), so a clip sounds right everywhere.
    // The editor and ffmpeg still see them all.
    let flags = if track.track.is_audio() && track.track != TrackId::Master { 2 } else { 3 };
    full_box(b"tkhd", 0, flags, &body)
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

/// `mehd` (version 1): the length of the whole fragmented presentation, in the movie timescale.
/// Players use it as the duration. Without it they measure the fragments, which misses the end.
fn mehd(fragment_duration: u64) -> io::Result<Vec<u8>> {
    full_box(b"mehd", 1, 0, &fragment_duration.to_be_bytes())
}

/// One `tfra` entry: a fragment in which a track has samples, and where that fragment starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RandomAccessEntry {
    /// The track's base media decode time in the fragment, which is its `tfdt`.
    pub(crate) time: u64,
    /// The offset of the fragment's `moof` from the start of the file.
    pub(crate) moof_offset: u64,
    /// The one-based position of the track's `traf` inside that `moof`.
    pub(crate) traf_number: u8,
}

/// `mfra`, the movie fragment random access box (ISO/IEC 14496-12 §8.8.9): one `tfra` per track,
/// which pairs each track ID with its entries, then an `mfro`. Without an index, Media Foundation
/// reports no duration for a fragmented file, so the Windows player has no timeline. The reader is
/// Media Foundation's [MPEG-4 file source](https://learn.microsoft.com/en-us/windows/win32/medfound/mpeg-4-file-source).
pub(crate) fn mfra(tracks: &[(u32, &[RandomAccessEntry])]) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    for &(track_id, entries) in tracks {
        body.extend(tfra(track_id, entries)?);
    }
    // The size in mfro counts the whole mfra: its 8-byte header, the tfras, and the 16-byte mfro.
    let size = u32::try_from(body.len() + 8 + 16).map_err(invalid_data)?;
    body.extend(full_box(b"mfro", 0, 0, &size.to_be_bytes())?);
    boxed(b"mfra", &body)
}

/// `tfra` (version 1, so times and offsets are 64-bit). Its length fields are zero, so the traf,
/// trun and sample numbers each take one byte. A fragment starts on its sync sample, which is the
/// first sample of the first trun, so the trun and sample numbers are always 1.
fn tfra(track_id: u32, entries: &[RandomAccessEntry]) -> io::Result<Vec<u8>> {
    let count = u32::try_from(entries.len()).map_err(invalid_data)?;
    // The word after track_ID is 26 reserved bits, then three 2-bit length fields (size minus one).
    let mut body = u32s(&[track_id, 0, count]);
    for entry in entries {
        body.extend_from_slice(&entry.time.to_be_bytes());
        body.extend_from_slice(&entry.moof_offset.to_be_bytes());
        body.extend_from_slice(&[entry.traf_number, 1, 1]);
    }
    full_box(b"tfra", 1, 0, &body)
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

/// The sample description for a video track: `avc1` with `avcC`, or `hvc1` with `hvcC`, from
/// the parameter sets the stream carries.
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
        Codec::Hevc => {
            let record = match bitstream::hevc_parameter_sets(params) {
                Some((vps, sps, pps)) => bitstream::hvcc(vps, sps, pps).map_err(invalid_data)?,
                None => {
                    warn!(
                        "no HEVC parameter sets before the first keyframe; the file declares none"
                    );
                    bitstream::hvcc(&[], &[], &[]).map_err(invalid_data)?
                }
            };
            visual_entry(b"hvc1", size, &boxed(b"hvcC", &record)?)
        }
        Codec::Av1 => {
            let record = match bitstream::av1_sequence_header(params) {
                Some(seq_obu) => bitstream::av1c(seq_obu),
                None => {
                    warn!(
                        "no AV1 sequence header before the first keyframe; the file declares none"
                    );
                    bitstream::av1c(&[])
                }
            };
            visual_entry(b"av01", size, &boxed(b"av1C", &record)?)
        }
    }
}

/// The fragmented MP4 writer behind [`container::create`](crate::container::create).
///
/// Packets wait per track until the next video keyframe, which closes a fragment. A fragment
/// is one `moof` and one `mdat`, and it starts on a sync sample.
pub(crate) struct Fmp4Writer {
    out: Box<dyn Write + Send>,
    /// The declared tracks, in [`TrackId::index`] order, so game audio comes first (pitfall 8).
    tracks: Vec<TrackInfo>,
    /// Each track's position in `tracks`, by [`TrackId::index`].
    slot: [Option<usize>; TrackId::COUNT],
    /// The first keyframe's in-band parameter sets, for a video track that declares none.
    /// Dropped once the init segment is written.
    video_config: Option<Vec<u8>>,
    init_written: bool,
    /// The PTS of the first video keyframe, which becomes time zero.
    origin: Option<Hns>,
    /// The last video PTS accepted. A step backwards is an error, since B-frames are off.
    last_video_pts: Option<Hns>,
    /// The last PTS accepted per track position, for audio. A step backwards drops the packet.
    last_audio_pts: Vec<Option<Hns>>,
    /// Fragments written so far, so the next one has `sequence + 1` in its `mfhd`.
    sequence: u32,
    /// Samples waiting for the next fragment, per track position.
    samples: Vec<Vec<Sample>>,
    /// Their encoded bytes, per track position.
    data: Vec<Vec<u8>>,
    /// Bytes written to the output so far, which is where the next box starts.
    written: u64,
    /// Each track's fragments, per track position, for the `mfra` written at finish.
    index: Vec<Vec<RandomAccessEntry>>,
}

/// One sample waiting in a fragment.
struct Sample {
    /// Presentation time since the origin.
    pts: Hns,
    /// The packet's own duration. Only the last sample of a fragment uses it, because the
    /// others run up to the next PTS (pitfall 7).
    duration: Hns,
    /// Length in bytes, as it was appended to the track's data.
    size: u32,
    /// A sync sample: a video keyframe, or any audio frame.
    sync: bool,
}

impl Fmp4Writer {
    /// Writes nothing yet. The init segment waits for the first fragment, so it can take its
    /// parameter sets from the first keyframe.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] for a track list the writer cannot use: no video track,
    /// a track declared twice, or a kind that does not match its track ID.
    /// [`io::ErrorKind::Unsupported`] for a video codec that is not written yet.
    pub(crate) fn new(out: Box<dyn Write + Send>, mut tracks: Vec<TrackInfo>) -> io::Result<Self> {
        tracks.sort_by_key(|track| track.track.index());
        let mut slot = [None; TrackId::COUNT];
        for (position, track) in tracks.iter().enumerate() {
            let kind_matches = matches!(
                (track.track, &track.kind),
                (TrackId::Video, TrackKind::Video { .. })
                    | (TrackId::Audio(_), TrackKind::Audio { .. })
            );
            if !kind_matches {
                let message = format!("track {:?} is declared with the wrong kind", track.track);
                return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
            }
            if slot[track.track.index()].replace(position).is_some() {
                let message = format!("track {:?} is declared twice", track.track);
                return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
            }
            if let TrackKind::Video { codec, .. } = &track.kind
                && !matches!(*codec, Codec::H264 | Codec::Hevc | Codec::Av1)
            {
                let message = format!("{} fragments aren't written yet", codec.name());
                return Err(io::Error::new(io::ErrorKind::Unsupported, message));
            }
        }
        if slot[TrackId::Video.index()].is_none() {
            let message = "a fragmented MP4 needs a video track";
            return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        }
        let count = tracks.len();
        Ok(Self {
            out,
            tracks,
            slot,
            video_config: None,
            init_written: false,
            origin: None,
            last_video_pts: None,
            last_audio_pts: vec![None; count],
            sequence: 0,
            samples: (0..count).map(|_| Vec::new()).collect(),
            data: (0..count).map(|_| Vec::new()).collect(),
            written: 0,
            index: (0..count).map(|_| Vec::new()).collect(),
        })
    }

    /// The position in `tracks` of a declared track.
    fn position(&self, track: TrackId) -> Option<usize> {
        self.slot[track.index()]
    }

    /// Whether any track has samples waiting.
    fn has_pending(&self) -> bool {
        self.samples.iter().any(|samples| !samples.is_empty())
    }

    /// Writes the init segment and drops the first keyframe's parameter sets.
    fn write_init(&mut self) -> io::Result<()> {
        let init = init_segment(&self.tracks, self.video_config.as_deref())?;
        self.out.write_all(&init)?;
        self.written += u64::try_from(init.len()).map_err(invalid_data)?;
        self.init_written = true;
        self.video_config = None;
        Ok(())
    }

    /// The `moof` for the tracks at `active` positions. Track `i`'s data starts
    /// `data_offsets[i]` bytes after the start of the moof (default-base-is-moof).
    fn moof(&self, active: &[usize], data_offsets: &[i32]) -> io::Result<Vec<u8>> {
        let mut body = full_box(b"mfhd", 0, 0, &self.sequence.to_be_bytes())?;
        for (&position, &data_offset) in active.iter().zip(data_offsets) {
            body.extend(self.traf(position, data_offset)?);
        }
        boxed(b"moof", &body)
    }

    /// The `traf` for the samples of the track at `position`.
    fn traf(&self, position: usize, data_offset: i32) -> io::Result<Vec<u8>> {
        let Some(first) = self.samples[position].first() else {
            return Ok(Vec::new());
        };
        let track_id = u32::try_from(position + 1).map_err(invalid_data)?;
        // default-base-is-moof (0x020000): the trun offsets count from the start of the moof.
        let tfhd = full_box(b"tfhd", 0, 0x0002_0000, &track_id.to_be_bytes())?;
        // The base time is the first sample's true PTS, so timing restarts at every fragment.
        let base = u64::try_from(first.pts.ticks()).map_err(invalid_data)?;
        let tfdt = full_box(b"tfdt", 1, 0, &base.to_be_bytes())?;
        let trun = self.trun(position, data_offset)?;
        boxed(b"traf", &[tfhd, tfdt, trun].concat())
    }

    /// The `trun` listing the track's samples. Each duration runs to the next PTS of the same
    /// track, because WGC delivers frames only when the screen changes (pitfall 7).
    fn trun(&self, position: usize, data_offset: i32) -> io::Result<Vec<u8>> {
        let samples = &self.samples[position];
        let count = u32::try_from(samples.len()).map_err(invalid_data)?;
        let mut body = u32s(&[count]);
        body.extend_from_slice(&data_offset.to_be_bytes());
        for (index, sample) in samples.iter().enumerate() {
            let duration = match samples.get(index + 1) {
                Some(next) => next.pts.saturating_sub(sample.pts),
                None => sample.duration,
            };
            body.extend_from_slice(&clamp_u32(duration.ticks()).to_be_bytes());
            body.extend_from_slice(&sample.size.to_be_bytes());
            // Sync samples depend on nothing. Other video frames depend on an earlier frame.
            let flags: u32 = if sample.sync { 0x0200_0000 } else { 0x0101_0000 };
            body.extend_from_slice(&flags.to_be_bytes());
        }
        // Flags 0x701: data offset, sample duration, sample size and sample flags are present.
        full_box(b"trun", 0, 0x0000_0701, &body)
    }
}

impl ContainerWriter for Fmp4Writer {
    fn write_packet(&mut self, packet: PacketRef<'_>) -> io::Result<()> {
        let meta = packet.meta;
        // A track that was not declared is ignored.
        let Some(position) = self.position(meta.track) else {
            return Ok(());
        };
        let is_video = !meta.track.is_audio();
        // A file starts on a video keyframe, which becomes time zero. Everything before it
        // is dropped, so no file starts on a P-frame.
        let origin = match self.origin {
            Some(origin) => origin,
            None if is_video && meta.keyframe => {
                if self.tracks[position].codec_private.is_empty() {
                    self.video_config = Some(packet.data.to_vec());
                }
                self.origin = Some(meta.pts);
                meta.pts
            }
            None => return Ok(()),
        };
        if meta.pts < origin {
            return Ok(());
        }
        if is_video {
            // B-frames are off (pitfall 27). A PTS that steps back would make a broken file,
            // so refuse it and say why.
            if let Some(last) = self.last_video_pts
                && meta.pts < last
            {
                let message = format!(
                    "video PTS went backwards ({} after {}): the MP4 writer assumes no B-frames",
                    meta.pts, last
                );
                return Err(invalid_data(message));
            }
            // Each keyframe closes a fragment, so every fragment starts on a sync sample.
            if meta.keyframe && self.has_pending() {
                self.flush_fragment()?;
            }
            self.last_video_pts = Some(meta.pts);
        } else {
            // Audio that steps back would get a zero duration and shift the track. Drop it
            // rather than fail the whole file.
            let last = &mut self.last_audio_pts[position];
            if last.is_some_and(|last| meta.pts < last) {
                warn!(track = ?meta.track, pts = ?meta.pts, "audio PTS went backwards; packet dropped");
                return Ok(());
            }
            *last = Some(meta.pts);
        }
        let size =
            append_sample(&mut self.data[position], packet.data, &self.tracks[position].kind)?;
        self.samples[position].push(Sample {
            pts: meta.pts.saturating_sub(origin),
            duration: meta.duration,
            size,
            sync: meta.keyframe || !is_video,
        });
        Ok(())
    }

    fn flush_fragment(&mut self) -> io::Result<()> {
        if !self.has_pending() {
            return Ok(());
        }
        if !self.init_written {
            self.write_init()?;
        }
        self.sequence = self.sequence.wrapping_add(1);

        let active: Vec<usize> = self
            .samples
            .iter()
            .enumerate()
            .filter(|(_, samples)| !samples.is_empty())
            .map(|(position, _)| position)
            .collect();
        // The moof's size does not depend on its offsets, so measure it with placeholders.
        let moof_len = self.moof(&active, &vec![0; active.len()])?.len();
        // The data follows the moof and the 8-byte mdat header, one track after another.
        let mut offset = moof_len + 8;
        let mut offsets = Vec::with_capacity(active.len());
        for &position in &active {
            let data_offset = i32::try_from(offset)
                .map_err(|_| invalid_data("fragment too large for a trun data offset"))?;
            offsets.push(data_offset);
            offset += self.data[position].len();
        }
        let moof = self.moof(&active, &offsets)?;

        let payload: usize = active.iter().map(|&position| self.data[position].len()).sum();
        let mdat = box_header(b"mdat", payload)?;
        let moof_offset = self.written;
        self.out.write_all(&moof)?;
        self.out.write_all(&mdat)?;
        for &position in &active {
            self.out.write_all(&self.data[position])?;
        }
        self.written += u64::try_from(moof.len() + mdat.len() + payload).map_err(invalid_data)?;
        // Each track's traf starts with its first sample, which is a sync sample.
        for (index, &position) in active.iter().enumerate() {
            if let Some(first) = self.samples[position].first() {
                self.index[position].push(RandomAccessEntry {
                    time: u64::try_from(first.pts.ticks()).map_err(invalid_data)?,
                    moof_offset,
                    traf_number: u8::try_from(index + 1).map_err(invalid_data)?,
                });
            }
        }
        for &position in &active {
            self.samples[position].clear();
            self.data[position].clear();
        }
        self.out.flush()
    }

    fn finish(mut self: Box<Self>) -> io::Result<()> {
        self.flush_fragment()?;
        if !self.init_written {
            self.write_init()?;
        }
        // The index goes after the last fragment, where Media Foundation looks for it.
        let mut tracks = Vec::new();
        for (position, entries) in self.index.iter().enumerate() {
            if !entries.is_empty() {
                let track_id = u32::try_from(position + 1).map_err(invalid_data)?;
                tracks.push((track_id, entries.as_slice()));
            }
        }
        let trailer = mfra(&tracks)?;
        self.out.write_all(&trailer)?;
        self.out.flush()
    }
}

/// Appends one packet's bytes to a track's buffer and returns how many were added.
/// H.264/HEVC become length-prefixed NAL units (ISO/IEC 14496-15).
/// AV1 strips temporal delimiters and keeps/adds OBU size fields (AV1-ISOBMFF).
/// Audio is copied as it is. On an error the buffer is left as it was.
fn append_sample(buffer: &mut Vec<u8>, data: &[u8], kind: &TrackKind) -> io::Result<u32> {
    let start = buffer.len();
    let converted = match kind {
        TrackKind::Video { codec: Codec::H264 | Codec::Hevc, .. } => {
            bitstream::append_length_prefixed(data, buffer).map_err(invalid_data)
        }
        TrackKind::Video { codec: Codec::Av1, .. } => {
            bitstream::append_av1_sample(data, buffer).map_err(invalid_data)
        }
        TrackKind::Audio { .. } => {
            buffer.extend_from_slice(data);
            Ok(())
        }
    };
    let added = converted.and_then(|()| u32::try_from(buffer.len() - start).map_err(invalid_data));
    if added.is_err() {
        buffer.truncate(start);
    }
    added
}

/// `ticks` clamped into a u32, for a sample duration. A negative gap counts as zero.
fn clamp_u32(ticks: i64) -> u32 {
    u32::try_from(ticks.clamp(0, i64::from(u32::MAX))).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use zc_core::media::{ContainerFormat, Packet, PacketMeta};
    use zc_core::testing::SyntheticStream;

    use super::*;
    use crate::container::create;

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
    fn mehd_is_version_1_with_a_64_bit_length() {
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 20, b'm', b'e', b'h', b'd',
            1, 0, 0, 0, // version 1, flags 0
            0, 0, 0, 0, 0, 0, 1, 2, // fragment_duration
        ];
        assert_eq!(mehd(0x0102).unwrap(), expected);
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
        assert_eq!(kinds(child(moov, b"mvex")), ["mehd", "trex", "trex", "trex"]);
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
    fn av01_carries_the_av1c_from_the_first_keyframe() {
        let mut track = video();
        track.kind = TrackKind::Video { codec: Codec::Av1, width: 320, height: 240, fps: 60 };
        // Sequence header OBU
        let seq_obu = vec![0x0A, 0x04, 0x00, 0x00, 0x00, 0x40];
        let init = init_segment(&[track], Some(&seq_obu)).unwrap();
        let av01 = child(sample_entries(&init), b"av01");
        let av1c = child(&av01[78..], b"av1C");
        assert_eq!(av1c[0], 0x81);
        assert_eq!(av1c[1], 0x08, "seq_profile 0, level 8");
        assert_eq!(av1c[2], 0x0C);
        assert_eq!(&av1c[4..], &seq_obu);
    }

    #[test]
    fn hvc1_carries_the_hvcc_from_the_first_keyframe() {
        let mut track = video();
        track.kind = TrackKind::Video { codec: Codec::Hevc, width: 320, height: 240, fps: 60 };
        // VPS, SPS and PPS in Annex B, then an IDR slice. The SPS has emulation-prevention
        // bytes in its profile constraints, as real ones do.
        #[rustfmt::skip]
        let in_band: &[u8] = &[
            0, 0, 0, 1, 0x40, 0x01, 0x0C, // VPS
            0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, // SPS
            0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x3C, 0xA0,
            0, 0, 0, 1, 0x44, 0x01, 0xC1, // PPS
            0, 0, 1, 0x26, 0x01, // IDR
        ];
        let init = init_segment(&[track], Some(in_band)).unwrap();
        let hvc1 = child(sample_entries(&init), b"hvc1");
        assert_eq!(&hvc1[24..28], [0x01, 0x40, 0x00, 0xF0], "320 x 240");
        let hvcc = child(&hvc1[78..], b"hvcC");
        // configurationVersion 1, then the general profile_tier_level from the SPS.
        assert_eq!(hvcc[0], 1);
        assert_eq!(hvcc[1..4], [0x01, 0x60, 0x00]);
        // Three arrays: VPS, SPS and PPS are declared.
        assert_eq!(hvcc[22], 3);
    }

    /// A writer output that the test keeps a handle on, to read the file back.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("the lock is never poisoned in tests").clone()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("the lock is never poisoned in tests").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The tracks a recording declares: video, then game audio, then the mic.
    fn recording_tracks() -> Vec<TrackInfo> {
        vec![video(), audio(TrackId::GameAudio, "Game"), audio(TrackId::Mic, "Mic")]
    }

    /// A video packet at `ms` milliseconds, lasting 16 ms, with a 4-byte payload.
    fn video_packet(ms: i64, keyframe: bool) -> Packet {
        Packet {
            meta: PacketMeta {
                track: TrackId::Video,
                pts: Hns::from_millis(ms),
                duration: Hns::from_millis(16),
                keyframe,
            },
            data: vec![0x65, 1, 2, 3],
        }
    }

    /// Writes `packets` through [`create`] and returns the file's bytes.
    fn mux(tracks: &[TrackInfo], packets: &[Packet]) -> Vec<u8> {
        let out = SharedBuf::default();
        let mut writer =
            create(ContainerFormat::FragmentedMp4, Box::new(out.clone()), tracks).unwrap();
        for packet in packets {
            writer.write_packet(packet.view()).unwrap();
        }
        writer.finish().unwrap();
        out.bytes()
    }

    fn u32_at(buf: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(buf[at..at + 4].try_into().unwrap())
    }

    fn i32_at(buf: &[u8], at: usize) -> i32 {
        i32::from_be_bytes(buf[at..at + 4].try_into().unwrap())
    }

    fn u64_at(buf: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(buf[at..at + 8].try_into().unwrap())
    }

    /// Each fragment as (moof body, mdat body), in file order.
    fn fragments(file: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut found = Vec::new();
        let mut moof = None;
        for (kind, body) in boxes(file) {
            match &kind {
                b"moof" => moof = Some(body),
                b"mdat" => found.push((moof.take().expect("an mdat follows its moof"), body)),
                _ => {}
            }
        }
        found
    }

    /// The `traf` bodies of a moof body, in order.
    fn trafs(moof: &[u8]) -> Vec<&[u8]> {
        boxes(moof).into_iter().filter(|(kind, _)| kind == b"traf").map(|(_, body)| body).collect()
    }

    /// The base media decode time of a traf: its `tfdt` is version 1, so the time is 8 bytes.
    fn tfdt_base(traf: &[u8]) -> u64 {
        u64_at(child(traf, b"tfdt"), 4)
    }

    /// (duration, size, flags) of each sample in a trun body. The entries follow the
    /// version and flags, the sample count and the data offset.
    fn trun_samples(trun: &[u8]) -> Vec<(u32, u32, u32)> {
        trun[12..]
            .as_chunks::<12>()
            .0
            .iter()
            .map(|entry| (u32_at(entry, 0), u32_at(entry, 4), u32_at(entry, 8)))
            .collect()
    }

    /// The name of a track, from its `hdlr`, without the NUL terminator.
    fn track_name(trak: &[u8]) -> String {
        let hdlr = child(child(trak, b"mdia"), b"hdlr");
        String::from_utf8_lossy(&hdlr[24..hdlr.len() - 1]).into_owned()
    }

    #[test]
    fn fmp4_is_init_then_one_fragment_per_gop() {
        // Three seconds of video, game audio and mic: keyframes at 0, 1 and 2 s.
        let packets = SyntheticStream::new().take_secs(3);
        let file = mux(&recording_tracks(), &packets);
        assert_eq!(
            kinds(&file),
            ["ftyp", "moov", "moof", "mdat", "moof", "mdat", "moof", "mdat", "mfra"]
        );
    }

    #[test]
    fn fmp4_timestamps_start_at_zero() {
        // A recording that starts 543 s into the session still begins at zero. Every later
        // fragment's base time is its own true PTS.
        let mut packets = SyntheticStream::new().take_secs(2);
        for packet in &mut packets {
            packet.meta.pts += Hns::from_secs(543);
        }
        let file = mux(&recording_tracks(), &packets);
        let bases: Vec<u64> =
            fragments(&file).iter().map(|(moof, _)| tfdt_base(trafs(moof)[0])).collect();
        assert_eq!(bases, [0, 10_000_000]);
    }

    #[test]
    fn fmp4_drops_packets_before_the_first_keyframe() {
        // Starting at frame 10 of a 60-frame GOP, the first 50 video frames and the audio
        // before the keyframe at 1 s are dropped.
        let from = SyntheticStream::new().frame_pts(10);
        let packets: Vec<Packet> = SyntheticStream::new()
            .take_secs(2)
            .into_iter()
            .filter(|p| p.meta.pts >= from)
            .collect();
        let file = mux(&recording_tracks(), &packets);
        let (moof, _) = fragments(&file)[0];
        let video = trafs(moof)[0];
        assert_eq!(u32_at(child(video, b"trun"), 4), 60, "the first GOP, frames 60 to 119");
        assert_eq!(tfdt_base(video), 0);
    }

    #[test]
    fn fmp4_ignores_tracks_that_were_not_declared() {
        let packets = SyntheticStream::new().take_secs(1);
        let tracks = [video(), audio(TrackId::GameAudio, "Game")];
        let file = mux(&tracks, &packets);
        let (moof, _) = fragments(&file)[0];
        assert_eq!(trafs(moof).len(), 2, "no traf for the mic");
    }

    #[test]
    fn fmp4_file_has_the_expected_box_tree() {
        let packets = SyntheticStream::new().take_secs(2);
        let file = mux(&recording_tracks(), &packets);

        let moov = child(&file, b"moov");
        assert_eq!(kinds(moov), ["mvhd", "trak", "trak", "trak", "mvex"]);
        assert_eq!(kinds(child(moov, b"mvex")), ["mehd", "trex", "trex", "trex"]);

        let (moof, mdat) = fragments(&file)[0];
        assert_eq!(kinds(moof), ["mfhd", "traf", "traf", "traf"]);
        assert_eq!(u32_at(child(moof, b"mfhd"), 4), 1, "the first fragment is number 1");
        let video = trafs(moof)[0];
        assert_eq!(kinds(video), ["tfhd", "tfdt", "trun"]);
        assert_eq!(&child(video, b"tfhd")[..4], [0, 2, 0, 0], "default-base-is-moof");
        assert_eq!(&child(video, b"trun")[..4], [0, 0, 7, 1], "offset, duration, size, flags");

        // The data offset counts from the start of the moof, and the data starts after the
        // 8-byte mdat header.
        let moof_len = 8 + moof.len();
        let offset = i32::try_from(moof_len + 8).unwrap();
        assert_eq!(i32_at(child(video, b"trun"), 8), offset);

        // The sample sizes of all three tracks add up to the mdat body.
        let total: u32 = trafs(moof)
            .iter()
            .flat_map(|traf| trun_samples(child(traf, b"trun")))
            .map(|(_, size, _)| size)
            .sum();
        assert_eq!(usize::try_from(total).unwrap(), mdat.len());

        // A keyframe is a sync sample. The next frame depends on it.
        let samples = trun_samples(child(video, b"trun"));
        assert_eq!(samples[0].2, 0x0200_0000);
        assert_eq!(samples[1].2, 0x0101_0000);
        // Each frame is one NAL unit (the synthetic 1000 bytes) behind a 4-byte length.
        assert_eq!(samples[0].1, 1004);
    }

    #[test]
    fn durations_run_to_the_next_pts_and_the_last_uses_the_packet_duration() {
        // Variable frame rate (pitfall 7): the gaps are 16, 34 and 16 ms. The last frame of the
        // fragment has no next PTS, so it keeps its own 16 ms.
        let packets = [
            video_packet(0, true),
            video_packet(16, false),
            video_packet(50, false),
            video_packet(66, false),
            video_packet(1000, true),
        ];
        let file = mux(&[video()], &packets);
        let fragments = fragments(&file);
        assert_eq!(fragments.len(), 2);

        let samples = trun_samples(child(trafs(fragments[0].0)[0], b"trun"));
        let durations: Vec<u32> = samples.iter().map(|sample| sample.0).collect();
        assert_eq!(durations, [160_000, 340_000, 160_000, 160_000]);
        assert!(samples.iter().all(|sample| sample.1 == 8), "4-byte NAL behind a 4-byte length");
        // The second fragment starts at its keyframe's own PTS, one second in.
        assert_eq!(tfdt_base(trafs(fragments[1].0)[0]), 10_000_000);
    }

    #[test]
    fn an_audio_packet_that_steps_back_is_dropped() {
        let tracks = [video(), audio(TrackId::GameAudio, "Game")];
        let audio_packet = |ms: i64| Packet {
            meta: PacketMeta {
                track: TrackId::GameAudio,
                pts: Hns::from_millis(ms),
                duration: Hns::from_millis(20),
                keyframe: true,
            },
            data: vec![1, 2, 3],
        };
        let packets = [
            video_packet(0, true),
            audio_packet(0),
            audio_packet(20),
            audio_packet(10),
            audio_packet(40),
            video_packet(1000, true),
        ];
        let file = mux(&tracks, &packets);
        let (moof, _) = fragments(&file)[0];
        let samples = trun_samples(child(trafs(moof)[1], b"trun"));
        assert_eq!(samples.len(), 3, "the packet at 10 ms is dropped");
    }

    #[test]
    fn video_pts_going_backwards_is_an_error() {
        // B-frames are off (pitfall 27). A PTS that steps back must fail with a clear error,
        // not write a broken file.
        let mut writer =
            create(ContainerFormat::FragmentedMp4, Box::new(SharedBuf::default()), &[video()])
                .unwrap();
        writer.write_packet(video_packet(0, true).view()).unwrap();
        writer.write_packet(video_packet(20, false).view()).unwrap();
        let error = writer.write_packet(video_packet(10, false).view()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("went backwards"), "{error}");
    }

    #[test]
    fn init_takes_parameter_sets_from_the_first_keyframe() {
        // An Annex-B keyframe: SPS, PPS, then an IDR slice.
        let keyframe = Packet {
            meta: PacketMeta {
                track: TrackId::Video,
                pts: Hns::ZERO,
                duration: Hns::from_millis(16),
                keyframe: true,
            },
            data: vec![
                0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC, // SPS
                0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80, // PPS
                0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
            ],
        };
        let packets = [keyframe, video_packet(16, false), video_packet(1000, true)];
        let file = mux(&[video()], &packets);
        let avc1 = child(sample_entries(&file), b"avc1");
        let avcc = child(&avc1[78..], b"avcC");
        assert_eq!(avcc[..6], [1, 0x64, 0x00, 0x28, 0xFF, 0xE1], "profile and level from the SPS");
    }

    #[test]
    fn game_audio_is_declared_before_the_mic_whatever_the_input_order() {
        // Some players and Discord use only the first audio track (pitfall 8).
        let tracks = [audio(TrackId::Mic, "Mic"), video(), audio(TrackId::GameAudio, "Game")];
        let file = mux(&tracks, &SyntheticStream::new().take_secs(1));
        let traks: Vec<&[u8]> = boxes(child(&file, b"moov"))
            .into_iter()
            .filter(|(kind, _)| kind == b"trak")
            .map(|(_, body)| body)
            .collect();
        let names: Vec<String> = traks.iter().map(|trak| track_name(trak)).collect();
        assert_eq!(names, ["Video", "Game", "Mic"]);
    }

    #[test]
    fn create_refuses_track_lists_it_cannot_write() {
        let kind_of = |tracks: &[TrackInfo]| {
            let out = Box::new(SharedBuf::default());
            match create(ContainerFormat::FragmentedMp4, out, tracks) {
                Ok(_) => None,
                Err(error) => Some(error.kind()),
            }
        };
        let no_video = [audio(TrackId::GameAudio, "Game")];
        assert_eq!(kind_of(&no_video), Some(io::ErrorKind::InvalidInput));
        assert_eq!(kind_of(&[video(), video()]), Some(io::ErrorKind::InvalidInput));
        let mut video_as_audio = audio(TrackId::Mic, "Mic");
        video_as_audio.track = TrackId::Video;
        assert_eq!(kind_of(&[video_as_audio]), Some(io::ErrorKind::InvalidInput));
        let mut av1 = video();
        av1.kind = TrackKind::Video { codec: Codec::Av1, width: 320, height: 240, fps: 60 };
        assert_eq!(kind_of(&[av1]), None);
    }

    #[test]
    fn matroska_is_unsupported() {
        let result = create(ContainerFormat::Matroska, Box::new(SharedBuf::default()), &[video()]);
        assert_eq!(result.err().map(|error| error.kind()), Some(io::ErrorKind::Unsupported));
    }

    #[test]
    fn flush_fragment_writes_only_what_is_pending() {
        let out = SharedBuf::default();
        let mut writer =
            create(ContainerFormat::FragmentedMp4, Box::new(out.clone()), &[video()]).unwrap();
        writer.write_packet(video_packet(0, true).view()).unwrap();
        writer.write_packet(video_packet(16, false).view()).unwrap();
        writer.flush_fragment().unwrap();
        assert_eq!(kinds(&out.bytes()), ["ftyp", "moov", "moof", "mdat"]);
        // Nothing is pending, so a second flush writes nothing. Finish adds only the index.
        writer.flush_fragment().unwrap();
        writer.finish().unwrap();
        assert_eq!(kinds(&out.bytes()), ["ftyp", "moov", "moof", "mdat", "mfra"]);
    }

    /// The start offset of each top-level box of `file`, in order.
    fn box_starts(file: &[u8]) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut at = 0;
        while at < file.len() {
            starts.push(at);
            at += u32_at(file, at) as usize;
        }
        starts
    }

    #[test]
    fn mfra_holds_a_tfra_per_track_and_an_mfro_with_its_size() {
        // One track with one fragment: time 0x0102, the moof at offset 0x30, the first traf.
        let entry = RandomAccessEntry { time: 0x0102, moof_offset: 0x30, traf_number: 1 };
        let tracks = [(2, &[entry][..])];
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 67, b'm', b'f', b'r', b'a', // mfra: 8 + 43 + 16 bytes
            0, 0, 0, 43, b't', b'f', b'r', b'a', // tfra: 12 + 12 + 19 bytes
            1, 0, 0, 0, // version 1, flags 0
            0, 0, 0, 2, // track_ID
            0, 0, 0, 0, // reserved, then three length fields of 0: one byte each
            0, 0, 0, 1, // number_of_entry
            0, 0, 0, 0, 0, 0, 1, 2, // time
            0, 0, 0, 0, 0, 0, 0, 0x30, // moof_offset
            1, 1, 1, // traf_number, trun_number, sample_number
            0, 0, 0, 16, b'm', b'f', b'r', b'o', // mfro: 16 bytes
            0, 0, 0, 0, // version 0, flags 0
            0, 0, 0, 67, // the size of the mfra
        ];
        assert_eq!(mfra(&tracks).unwrap(), expected);
    }

    #[test]
    fn fmp4_ends_with_an_mfra_that_points_at_each_track_fragment() {
        let file = mux(&recording_tracks(), &SyntheticStream::new().take_secs(3));
        let mfra_at = *box_starts(&file).last().expect("a top-level box");
        let index = &file[mfra_at..];
        assert_eq!(&index[4..8], b"mfra", "mfra is the last box");
        assert_eq!(u32_at(index, 0) as usize, index.len(), "mfra runs to the end of the file");
        // mfro is the last 16 bytes, and its size field is the size of the mfra.
        assert_eq!(u32_at(index, index.len() - 4) as usize, index.len(), "mfro holds the size");

        let body = child(&file, b"mfra");
        assert_eq!(kinds(body), ["tfra", "tfra", "tfra", "mfro"], "one tfra per track");
        for (_, tfra) in boxes(body).into_iter().filter(|(kind, _)| kind == b"tfra") {
            let track_id = u32_at(tfra, 4);
            assert_eq!(u32_at(tfra, 12), 3, "one entry per fragment");
            for entry in tfra[16..].as_chunks::<19>().0.iter() {
                let offset = u64_at(entry, 8) as usize;
                assert_eq!(&file[offset + 4..offset + 8], b"moof", "the entry points at a moof");
                let moof = &file[offset + 8..offset + u32_at(&file, offset) as usize];
                let traf = trafs(moof)[usize::from(entry[16]) - 1];
                assert_eq!(u32_at(child(traf, b"tfhd"), 4), track_id, "the traf is this track's");
                assert_eq!(tfdt_base(traf), u64_at(entry, 0), "the time is the traf's base time");
                assert_eq!(&entry[17..19], [1, 1], "the first trun, and its first sample");
            }
        }
    }
}
