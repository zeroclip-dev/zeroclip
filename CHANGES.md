# ZeroClip Implementation & Changes Report

This document records the architectural decisions, code changes, and verification performed on **Phase 3** cards (`P3.13` and `P3.14`).

---

## 1. P3.13: fMP4 AV1 Support

### Objective
Enable fragmented MP4 writing for AV1 video streams (`Codec::Av1`):
- Generate the `av01` visual sample entry with the `av1C` configuration record.
- Strip Temporal Delimiter (TD) OBUs (type 2) per the AV1-ISOBMFF specification.
- Ensure size fields are present on all OBUs packed into MP4 samples.

### Files Modified
- [`crates/zc-mux/src/bitstream.rs`](crates/zc-mux/src/bitstream.rs)
- [`crates/zc-mux/src/fmp4.rs`](crates/zc-mux/src/fmp4.rs)
- [`docs/plan/P3.md`](docs/plan/P3.md)

### Design & Implementation Details
1. **OBU Parsing (`av1_obus`)**:
   - Header byte unpacks `obu_type = (b >> 3) & 0xF`, `extension_flag = b & 0x4`, `has_size = b & 0x2`.
   - Skips extension byte when present; parses leb128 size field when `has_size` is set.
2. **Sequence Header Inspection (`av1_sequence_header`, `BitReader`, `av1c`)**:
   - Implemented `BitReader` with variable bit-width reader and `uvlc()` parser.
   - Parses `seq_profile`, `reduced_still_picture_header`, `seq_level_idx_0`, `seq_tier_0`, and timing info.
   - Builds 4-byte `av1C` header: `[0x81, (seq_profile << 5) | level, (tier << 7) | 0b0000_1100, 0x00]` followed by the Sequence Header OBU bytes as `configOBUs`.
3. **Sample Framing (`append_av1_sample`)**:
   - Drops OBU type 2 (Temporal Delimiters) as required when packaging AV1 in ISO-BMFF.
   - Ensures each OBU written has its `has_size` flag set and length prefixed in leb128 format.
4. **Integration in `Fmp4Writer`**:
   - `video_entry` produces `av01` box with child `av1C`.
   - `append_sample` routes based on `TrackKind::Video { codec: Codec::Av1, .. }`.
   - Updated constructor validator to accept `Codec::Av1`.
5. **Testing**:
   - Unit test `bitstream::tests::av1c_from_a_sequence_header` verifies bit-exact `av1C` generation.
   - Integration test `fmp4::tests::av01_carries_the_av1c_from_the_first_keyframe` verifies container generation.

---

## 2. P3.14: Annex-B to fMP4 Dev Check Example

### Objective
Provide a standalone verification harness that muxes raw Annex-B H.264 streams into fragmented MP4 using `zc-mux`, verifying stream decoding with FFmpeg and ffprobe.

### Files Created / Modified
- [`crates/zc-mux/examples/annexb_to_fmp4.rs`](crates/zc-mux/examples/annexb_to_fmp4.rs)
- [`docs/plan/P3.md`](docs/plan/P3.md)

### Design & Implementation Details
1. Written using Rust standard library and workspace dependencies (`zc-core`, `zc-mux`).
2. Reads raw Annex-B byte stream, parses NAL units via `bitstream::nal_units`.
3. Detects Access Unit Delimiters (AUD type 9) to split frames into access units.
4. Checks for IDR slice NALs (type 5) to flag keyframes.
5. Calculates deterministic PTS and duration based on 30 fps (10,000,000 / 30 Hns units).
6. Writes through `container::create(ContainerFormat::FragmentedMp4, ...)`.

### Verification Output
Executed against FFmpeg synthetic test generator (`testsrc2=duration=5:size=640x360:rate=30`):
```text
ffmpeg -v error -i out.mp4 -f null -
-> 0 errors (clean exit)

ffprobe -v error -show_entries stream=codec_name,width,height:format=duration out.mp4
[STREAM]
codec_name=h264
width=640
height=360
[/STREAM]
[FORMAT]
duration=5.000000
[/FORMAT]
```

---

## 3. Toolchain & Test Stability Fix

### Issue
`crates/zc-win/src/message_thread.rs` test `message_thread_delivers_posted_messages_and_stops` flaked on Windows when receiving OS window messages (such as `0x031F` / `WM_DWMNCRENDERINGCHANGED`) before the test's posted `WM_APP + 1` message.

### Fix
- Updated message handler callback in test to filter specifically for `WM_APP + 1`.

---

## 4. Pending Hardware Checks (For Review When Back)

The following items are hardware-dependent cards from Phase 2 / Phase 3 that need physical GPU/hardware inspection:

1. **P3.27: Phase 3 Done-When Hardware Test**:
   - Run the full app: `cargo run`.
   - Take a DVR dump clip at an arbitrary moment during live capture.
   - Confirm it starts decoding from the first frame in VLC/browser with audio in sync.
2. **P3.22a: Startup Repair**:
   - Terminate ZeroClip abruptly while writing a manual recording.
   - Re-launch the app and verify the `.mp4.partial` file is truncated and recovered to a playable `.mp4`.
3. **P3.23 - P3.25: Drive Pacing & Page-Lock**:
   - Test DVR buffer allocation and disk writes on non-SSD secondary drive (if available).
