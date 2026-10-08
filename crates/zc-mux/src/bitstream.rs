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
}
