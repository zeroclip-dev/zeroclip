//! Bitstream helpers: Annex-B NAL units, length prefixes, and the decoder configuration
//! records MP4 keeps apart from the samples (ISO/IEC 14496-15, ISO/IEC 14496-3).
//!
//! Hardware encoders output Annex-B byte streams: NAL units separated by `00 00 01` (or
//! `00 00 00 01`) start codes. MP4 stores each NAL unit behind a 4-byte big-endian length
//! and keeps the parameter sets in a separate configuration record. Nothing here knows
//! about a container, so MKV can use it too.

use std::num::TryFromIntError;

/// The three-byte start code that separates Annex-B NAL units.
const START_CODE: [u8; 3] = [0, 0, 1];

/// H.264 `nal_unit_type` of a sequence parameter set.
const H264_SPS: u8 = 7;
/// H.264 `nal_unit_type` of a picture parameter set.
const H264_PPS: u8 = 8;

/// H.265 `nal_unit_type` of a video parameter set.
const HEVC_VPS: u8 = 32;
/// H.265 `nal_unit_type` of a sequence parameter set.
const HEVC_SPS: u8 = 33;
/// H.265 `nal_unit_type` of a picture parameter set.
const HEVC_PPS: u8 = 34;

/// AAC-LC `audioObjectType`.
const AAC_LC: u8 = 2;
/// Sampling frequencies by `samplingFrequencyIndex` (ISO/IEC 14496-3).
const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Splits an Annex-B byte stream into NAL units, without their start codes.
///
/// Handles 3-byte and 4-byte start codes, and the zero bytes around them. Bytes before the
/// first start code are dropped and empty units are skipped. A buffer with no start code at
/// all is one NAL unit, which is what synthetic payloads look like. Malformed input yields
/// fewer units but never panics.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = match split_at_start_code(data) {
        Some((_, after)) => after,
        None => data,
    };
    std::iter::from_fn(move || {
        while !rest.is_empty() {
            let (unit, after) = split_at_start_code(rest).unwrap_or((rest, &[]));
            rest = after;
            let unit = trim_trailing_zeros(unit);
            if !unit.is_empty() {
                return Some(unit);
            }
        }
        None
    })
}

/// Appends each NAL unit of an Annex-B stream to `out`, each behind a 4-byte big-endian
/// length. That is the form MP4 samples use (ISO/IEC 14496-15).
///
/// # Errors
/// A NAL unit longer than `u32::MAX` bytes. The units before it stay in `out`.
pub fn append_length_prefixed(data: &[u8], out: &mut Vec<u8>) -> Result<(), TryFromIntError> {
    for unit in nal_units(data) {
        out.extend_from_slice(&u32::try_from(unit.len())?.to_be_bytes());
        out.extend_from_slice(unit);
    }
    Ok(())
}

/// The first SPS and the first PPS of an H.264 Annex-B stream. Hardware encoders repeat them
/// ahead of each IDR frame, so the first keyframe carries both.
#[must_use]
pub fn h264_parameter_sets(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let sps = first_nal_of_type(data, H264_SPS)?;
    let pps = first_nal_of_type(data, H264_PPS)?;
    Some((sps, pps))
}

/// Builds the `avcC` record (ISO/IEC 14496-15) for one SPS and one PPS. The profile, the
/// constraint flags and the level are copied from the SPS.
///
/// Without a real SPS (synthetic payloads) it declares zero parameter sets, so the file
/// still parses.
///
/// # Errors
/// A parameter set longer than 65 535 bytes, which the 16-bit length cannot hold.
pub fn avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, TryFromIntError> {
    // Byte 0 of the SPS is its NAL header, so profile_idc, the constraint flags and
    // level_idc are bytes 1 to 3.
    let Some(&[_, profile, constraints, level]) = sps.first_chunk::<4>() else {
        // Baseline profile, level 3.1, zero SPS and zero PPS.
        return Ok(vec![1, 0x42, 0x00, 0x1F, 0xFF, 0xE0, 0x00]);
    };
    // configurationVersion, the three profile bytes, 6 reserved bits and lengthSizeMinusOne
    // = 3 (4-byte prefixes), 3 reserved bits and numOfSequenceParameterSets = 1.
    let mut record = vec![1, profile, constraints, level, 0xFF, 0xE1];
    push_length16(&mut record, sps)?;
    // numOfPictureParameterSets = 1.
    record.push(1);
    push_length16(&mut record, pps)?;
    // ponytail: the High-profile extension (chroma format, bit depths, SPS extensions) is
    // not written. Add it when a player rejects a High-profile file.
    Ok(record)
}

/// The 2-byte `AudioSpecificConfig` for AAC-LC (ISO/IEC 14496-3), which MP4's `esds` box
/// carries. Channel configuration 1 is mono and 2 is stereo.
///
/// A sample rate that is not in the standard table is taken as 48 kHz.
#[must_use]
pub fn aac_lc_config(sample_rate: u32, channels: u16) -> [u8; 2] {
    let index = AAC_SAMPLE_RATES.iter().position(|&rate| rate == sample_rate).unwrap_or(3) as u8;
    // Bits: audioObjectType (5), samplingFrequencyIndex (4), channelConfiguration (4), then
    // the three GASpecificConfig flags, which are all 0 for AAC-LC.
    [(AAC_LC << 3) | (index >> 1), ((index & 1) << 7) | (((channels as u8) & 0x0F) << 3)]
}

/// The first VPS, SPS and PPS of an HEVC Annex-B stream, in that order. Hardware encoders
/// repeat them ahead of each IDR frame.
#[must_use]
pub fn hevc_parameter_sets(data: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let vps = first_hevc_nal_of_type(data, HEVC_VPS)?;
    let sps = first_hevc_nal_of_type(data, HEVC_SPS)?;
    let pps = first_hevc_nal_of_type(data, HEVC_PPS)?;
    Some((vps, sps, pps))
}

/// Removes the emulation-prevention bytes from a NAL unit. A NAL unit never contains
/// `00 00` followed by `00` to `03`, so the encoder inserts a `03` after such a pair. The
/// parameter-set fields are defined on the bytes without them.
#[must_use]
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0usize;
    for &byte in nal {
        if zeros >= 2 && byte == 0x03 {
            // The emulation_prevention_three_byte. The count of zeros starts again after it.
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Builds the `hvcC` record (ISO/IEC 14496-15 §8.3.3) for one VPS, SPS and PPS. The general
/// profile, tier, level and constraint flags are copied from the SPS, read with its
/// emulation-prevention bytes removed. The parameter sets themselves are stored as they are.
///
/// Without all three parameter sets (synthetic payloads) it declares none, so the file still
/// parses.
///
/// # Errors
/// A parameter set longer than 65 535 bytes, which the 16-bit length cannot hold.
pub fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, TryFromIntError> {
    // Byte 2 of the SPS holds sps_video_parameter_set_id, sps_max_sub_layers_minus1 and
    // sps_temporal_id_nesting_flag. Bytes 3 to 14 are the 12 profile_tier_level bytes that
    // the record repeats.
    let plain = unescape(sps);
    let complete = !vps.is_empty() && !pps.is_empty();
    let fields = match (plain.get(2), plain.get(3..15)) {
        (Some(&byte), Some(ptl)) if complete => Some((byte, ptl)),
        _ => None,
    };

    // configurationVersion, then the general profile_tier_level.
    let mut record = vec![1];
    match fields {
        Some((_, ptl)) => record.extend_from_slice(ptl),
        None => record.extend_from_slice(&[0; 12]),
    }
    // min_spatial_segmentation_idc 0, parallelismType 0, chromaFormat 1 (4:2:0), 8-bit luma
    // and chroma, and avgFrameRate unknown. The reserved bits are ones, as the spec requires.
    // ponytail: the bit depths and chroma format are fixed for 8-bit NV12. Revisit with HDR
    // (open question Q8).
    record.extend_from_slice(&[0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0x00, 0x00]);
    // constantFrameRate 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne 3.
    let (layers, nested) = match fields {
        // sps_max_sub_layers_minus1 is at most 6, so the layer count fits its three bits.
        Some((byte, _)) => (((byte >> 1) & 0x07).min(6) + 1, byte & 1),
        None => (1, 0),
    };
    record.push((layers << 3) | (nested << 2) | 3);

    if fields.is_none() {
        record.push(0); // numOfArrays
        return Ok(record);
    }
    record.push(3); // numOfArrays: VPS, SPS and PPS
    for (nal_type, nal) in [(HEVC_VPS, vps), (HEVC_SPS, sps), (HEVC_PPS, pps)] {
        // array_completeness 1, then the NAL unit type.
        record.push(0x80 | nal_type);
        record.extend_from_slice(&1u16.to_be_bytes()); // numNalus
        record.extend_from_slice(&u16::try_from(nal.len())?.to_be_bytes());
        record.extend_from_slice(nal);
    }
    Ok(record)
}

/// The first NAL unit of an HEVC stream whose `nal_unit_type` is `nal_type`. In HEVC the type
/// is six bits, from bit 1 of the first byte.
fn first_hevc_nal_of_type(data: &[u8], nal_type: u8) -> Option<&[u8]> {
    nal_units(data).find(|nal| nal.first().is_some_and(|&b| (b >> 1) & 0x3F == nal_type))
}

/// The first NAL unit whose `nal_unit_type` (the low five bits of its first byte) is `nal_type`.
fn first_nal_of_type(data: &[u8], nal_type: u8) -> Option<&[u8]> {
    nal_units(data).find(|nal| nal.first().is_some_and(|&b| b & 0x1F == nal_type))
}

/// Appends `unit` behind a big-endian 16-bit length, as `avcC` parameter sets are stored.
fn push_length16(record: &mut Vec<u8>, unit: &[u8]) -> Result<(), TryFromIntError> {
    record.extend_from_slice(&u16::try_from(unit.len())?.to_be_bytes());
    record.extend_from_slice(unit);
    Ok(())
}

/// Splits `data` at its first start code into the bytes before it and the bytes after it.
fn split_at_start_code(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let pos = data.windows(START_CODE.len()).position(|w| w == START_CODE)?;
    let (before, from_code) = data.split_at_checked(pos)?;
    Some((before, from_code.strip_prefix(&START_CODE)?))
}

/// Drops trailing zero bytes. A NAL unit never ends in `0x00`, so the zeros at its end
/// belong to the next start code (`00 00 00 01`) or are trailing_zero_8bits.
fn trim_trailing_zeros(unit: &[u8]) -> &[u8] {
    let end = unit.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
    unit.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_units_handles_3_and_4_byte_start_codes() {
        // An SPS behind a 4-byte start code, then a PPS behind a 3-byte one. The first
        // unit also ends in two zero bytes that belong to the next start code.
        let stream = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 0, 1, 0x68, 0xBB, 0xCC];
        let expected: [&[u8]; 2] = [&[0x67, 0xAA], &[0x68, 0xBB, 0xCC]];
        assert_eq!(nal_units(&stream).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn nal_units_without_start_code_is_one_unit() {
        let expected: [&[u8]; 1] = [&[0x65, 0x01]];
        assert_eq!(nal_units(&[0x65, 0x01, 0x00]).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn nal_units_survives_malformed_input() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 0]).count(), 0);
        // A start code with nothing after it.
        assert_eq!(nal_units(&[0, 0, 1]).count(), 0);
        assert_eq!(nal_units(&[0x67, 0x64, 0, 0, 1]).count(), 0);
        // Empty units between start codes are skipped.
        assert_eq!(nal_units(&[0, 0, 1, 0, 0, 1]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1, 0x65, 0, 0, 1]).count(), 1);
    }

    #[test]
    fn length_prefixed_round_trip() {
        let stream = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x65, 0x11, 0x22, 0x33];
        let mut out = Vec::new();
        append_length_prefixed(&stream, &mut out).unwrap();
        // Length 2 then the SPS, length 4 then the IDR slice, both big-endian.
        assert_eq!(out, [0, 0, 0, 2, 0x67, 0xAA, 0, 0, 0, 4, 0x65, 0x11, 0x22, 0x33]);

        let mut rest = out.as_slice();
        let mut decoded = Vec::new();
        while let Some((len, tail)) = rest.split_first_chunk::<4>() {
            let (unit, next) = tail.split_at(u32::from_be_bytes(*len) as usize);
            decoded.push(unit);
            rest = next;
        }
        let expected: [&[u8]; 2] = [&[0x67, 0xAA], &[0x65, 0x11, 0x22, 0x33]];
        assert_eq!(decoded, expected);
    }

    #[test]
    fn avcc_copies_profile_and_level() {
        // SPS: header 0x67, profile_idc 0x64 (High), constraint flags 0x00, level_idc 0x28
        // (4.0), then one more byte. PPS: header 0x68 and three more bytes.
        let sps = [0x67, 0x64, 0x00, 0x28, 0xAC];
        let pps = [0x68, 0xEE, 0x3C, 0x80];
        assert_eq!(
            avcc(&sps, &pps).unwrap(),
            [
                1, // configurationVersion
                0x64, 0x00, 0x28, // profile_idc, constraint flags, level_idc from the SPS
                0xFF, // 6 reserved bits, lengthSizeMinusOne = 3
                0xE1, // 3 reserved bits, numOfSequenceParameterSets = 1
                0x00, 0x05, // SPS length
                0x67, 0x64, 0x00, 0x28, 0xAC, // SPS
                1,    // numOfPictureParameterSets
                0x00, 0x04, // PPS length
                0x68, 0xEE, 0x3C, 0x80, // PPS
            ]
        );
    }

    #[test]
    fn avcc_without_parameter_sets_declares_none() {
        // Baseline 3.1, 4-byte prefixes, zero SPS (0xE0), zero PPS.
        assert_eq!(avcc(&[], &[]).unwrap(), [1, 0x42, 0x00, 0x1F, 0xFF, 0xE0, 0x00]);
    }

    #[test]
    fn h264_parameter_sets_are_the_first_sps_and_pps() {
        let stream = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, // SPS
            0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80, // PPS
            0, 0, 1, 0x65, 0x88, // IDR slice
        ];
        let expected: (&[u8], &[u8]) = (&[0x67, 0x64, 0x00, 0x28], &[0x68, 0xEE, 0x3C, 0x80]);
        assert_eq!(h264_parameter_sets(&stream), Some(expected));
        assert_eq!(h264_parameter_sets(&[0, 0, 1, 0x65, 0x88]), None);
    }

    #[test]
    fn aac_config_48k_stereo_is_1190() {
        // Object type 2 = 00010, index 3 (48 kHz) = 0011, channels 2 = 0010, flags 000.
        // Bits 00010001 10010000 = 0x11 0x90.
        assert_eq!(aac_lc_config(48_000, 2), [0x11, 0x90]);
        // A rate outside the table falls back to 48 kHz.
        assert_eq!(aac_lc_config(47_999, 2), [0x11, 0x90]);
    }

    #[test]
    fn aac_config_44k_mono_is_1208() {
        // Object type 00010, index 4 (44.1 kHz) = 0100, channels 1 = 0001, flags 000.
        // Bits 00010010 00001000 = 0x12 0x08.
        assert_eq!(aac_lc_config(44_100, 1), [0x12, 0x08]);
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        // 00 00 03 is a zero pair and an escape byte, so only the escape goes.
        assert_eq!(unescape(&[0, 0, 3, 1]), [0, 0, 1]);
        // The count of zeros starts again after an escape: 00 00 03 00 00 03 is four zeros.
        assert_eq!(unescape(&[0, 0, 3, 0, 0, 3, 1]), [0, 0, 0, 0, 1]);
        // A 03 that is not after two zeros is data.
        assert_eq!(unescape(&[0x42, 3, 0, 3]), [0x42, 3, 0, 3]);
    }

    #[test]
    fn hevc_parameter_sets_are_the_first_vps_sps_and_pps() {
        // VPS (type 32), SPS (33), PPS (34) and an IDR slice (19), behind 4-byte start codes.
        let stream = [
            0, 0, 0, 1, 0x40, 0x01, 0x0C, // VPS
            0, 0, 0, 1, 0x42, 0x01, 0x01, // SPS
            0, 0, 0, 1, 0x44, 0x01, 0xC1, // PPS
            0, 0, 1, 0x26, 0x01, 0xAF, // IDR
        ];
        let expected: (&[u8], &[u8], &[u8]) =
            (&[0x40, 0x01, 0x0C], &[0x42, 0x01, 0x01], &[0x44, 0x01, 0xC1]);
        assert_eq!(hevc_parameter_sets(&stream), Some(expected));
        // Without a PPS there is nothing to declare.
        assert_eq!(hevc_parameter_sets(&stream[..11]), None);
    }

    /// An SPS whose general profile constraint flags contain emulation-prevention bytes.
    /// Unescaped, its header is `42 01 01`, its profile_tier_level bytes are `01`, `60 00 00
    /// 00`, `90 00 00 00 00 00` and `3C` (level 60), and then `A0`.
    const ESCAPED_SPS: [u8; 19] = [
        0x42, 0x01, 0x01, 0x01, 0x60, // header, sub-layer byte, profile, compatibility flags
        0x00, 0x00, 0x03, 0x00, // compatibility flags, escaped
        0x90, // constraint flags
        0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, // constraint flags, escaped
        0x3C, 0xA0, // level, then one more byte of SPS
    ];

    #[test]
    fn hvcc_copies_the_ptl_after_unescaping() {
        let vps = [0x40, 0x01, 0x0C];
        let pps = [0x44, 0x01, 0xC1];
        let record = hvcc(&vps, &ESCAPED_SPS, &pps).unwrap();
        // The record's bytes 1 to 12 are profile_tier_level, read without the escapes.
        assert_eq!(record[1..13], [0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x3C]);
    }

    #[test]
    fn hvcc_record_layout_is_literal() {
        let vps = [0x40, 0x01, 0x0C];
        let pps = [0x44, 0x01, 0xC1];
        let expected = [
            vec![1],                                              // configurationVersion
            vec![0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x3C], // PTL, 12 bytes
            vec![0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0x00, 0x00], // reserved fields
            vec![0x0F, 3], // 1 layer, nested 1, lengthSizeMinusOne 3, 3 arrays
            vec![0xA0, 0x00, 0x01, 0x00, 0x03, 0x40, 0x01, 0x0C], // VPS, complete
            vec![0xA1, 0x00, 0x01, 0x00, 0x13],
            ESCAPED_SPS.to_vec(),
            vec![0xA2, 0x00, 0x01, 0x00, 0x03, 0x44, 0x01, 0xC1], // PPS, complete
        ]
        .concat();
        assert_eq!(hvcc(&vps, &ESCAPED_SPS, &pps).unwrap(), expected);
    }

    #[test]
    fn hvcc_without_parameter_sets_declares_none() {
        // One layer, nested 0, lengthSizeMinusOne 3, and zero arrays.
        let mut expected = vec![1];
        expected.extend_from_slice(&[0; 12]);
        expected.extend_from_slice(&[0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0x00, 0x00, 0x0B, 0]);
        assert_eq!(hvcc(&[], &[], &[]).unwrap(), expected);
    }
}
