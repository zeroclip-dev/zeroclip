# Roadmap

The phases are a dependency chain: each one is unblocked by the one before. Every phase
has a *done when* — if it isn't true, the phase isn't finished. Tick the boxes as you go.

**v0.1 = Phases 0–3**: a working DVR clipper with proper audio and crash-safe files,
with a README that explains the architecture. Ship that before anything else — a shipped,
documented core beats a perfect product that isn't finished.

How to see what is left in the code: `cargo test -- --ignored` runs every spec that
hasn't been implemented yet.

**Current phase: Phase 3 is done on hardware except P3.28 (README GIF), P3.29 (launch post) and P3.30 (tag v0.1.0). Next code: Phase 4, start at P4.1 ([docs/plan/P4.md](plan/P4.md)).**

Every box below is broken into task cards in [docs/plan/](plan/README.md). Work the
cards; tick a box here when its cards are done.

## Foundations — done

- [x] Cargo workspace with one crate per subsystem, and the dependency rules between them
- [x] `zc-core` domain types; specs for the DVR buffer, watchdog, hotkeys, profiles and clock
- [x] Engine actor with ports for the platform adapters, and a synthetic-source harness
- [x] egui app shell: settings persistence, logging, profile editor, activity feed
- [x] Design, architecture and decision docs
- [x] Add the CI workflow as `.github/workflows/ci.yml` (format, lint, test and docs on Windows)

## Phase 0 — Spike (a weekend)

Prove the riskiest assumption before building on it: WGC frame → Media Foundation
hardware encoder → file on disk, with no system-memory round trip for the frame.

- [x] `spikes/phase0-capture-encode`: steps 1–4
- [x] Try H.264 first, then HEVC (and AV1 if your GPU has it). H.264 and HEVC work; AV1 isn't supported on this GPU. Findings are in the spike's module doc.

*Done when:* a 10-second hardware-encoded MP4 exists, plays in VLC and a browser, and Task
Manager shows the GPU's Video Encode engine busy with no unexpected CPU copy overhead.

## Phase 1 — Capture core

- [x] `zc-video::device` — the shared D3D11 device, multithread-protected
- [x] `zc-video::capture` — WGC session, resize handling
- [x] `zc-video::convert` — BGRA → NV12 on the GPU (video processor)
- [x] `zc-video::encoder` — capability probe and the async hardware MFT; fixed GOP, VBR, no B-frames
- [x] Specs: `Codec::step_down`, `Codec::select`, `Backoff` (HDCP retry)
- [x] `zc-video::screenshot`, and wire `Command::Screenshot` in the engine
- [x] Wire `GpuVideoSource` into `src/wiring.rs`; show the codec in the status bar

*Done when:* recording survives alt-tabbing, resolution changes, and a Netflix tab opening and closing.

## Phase 2 — Audio, input, sync

- [x] Specs: `MasterClock`, `ChordMatcher`, `Compressor`, `mouse_buttons`
- [x] `SampleTimeline` (audio clock drift)
- [x] `zc-win::message_thread`; `zc-input` Raw Input + foreground hook
- [x] `zc-audio` game audio (process loopback, include mode) and mic with push-to-talk
- [x] Resolve the loopback include/exclude question: include mode, per-app tracks (D27)
- [x] Hotkeys drive commands in the engine

*Done when:* a 30-minute recording has no measurable A/V drift at the end.

Measured 2026-10-05 with `spike-phase2`: every track within ±11 ms of its minute-1 position over 30 minutes. Repeat it end to end once Phase 3 writes real files (a clap or flash test). A constant offset, such as AAC encoder priming, isn't drift; it's corrected with `MasterClock::set_track_offset`.

## Phase 3 — Buffers, modes, muxing

- [x] Specs: `DvrBuffer` (all of them, including `dvr_push_never_allocates`), `Tee`, `ChunkedWriter`, `WritePacer`
- [x] Implement `zc_mux::container::create` (fMP4, decided in D18)
- [x] Crash-safe recordings: sync every fragment, `.partial` files, startup repair (D18)
- [x] `Recorder` spec; `SyntheticVideoSource`; the `synthetic_harness` save-clip spec
- [x] DVR save, manual recording, page-lock the buffer (`VirtualLock`); automatic session recording is P4.7
- [x] `zc-win::drive` + HDD pacing for dumps

*Done when:* a DVR dump taken at any arbitrary moment decodes from its first frame and has audio.

**→ v0.1 release:** polish the README with a GIF, write a short "how it works" post, tag it.

## Phase 4 — UI, profiles, watchdogs

- [x] Specs: `Watchdog::on_event`, `ProfileSet`, `clamp_window` (`clamp_window` done early, in Phase 3: P3.16)
- [x] `zc-store` (rusqlite, bundled) and the migrations
- [ ] Profile engine on foreground change, with safe buffer resizing
- [ ] Watchdog wiring and the `synthetic_harness` recovery spec; crash rescue (`recovery.tmp` + prompt)
- [ ] Flight recorder: in-memory ring of recent events, dumped with crash rescue
- [ ] System tray; close-to-tray; start with Windows

*Done when:* killing the game process via Task Manager reliably produces a recovery prompt with intact video.

## Phase 5 — Editor

- [ ] Keyframe thumbnail strip; preview in an external player (D22)
- [ ] Specs: `snap_in_point`, `check_compatible`, `rms_peaks`
- [ ] `zc-editor::ffmpeg` jobs; trim with visible snap points; stitch; export remux
- [ ] Waveform extraction on a background task, cached by file hash

*Done when:* trimming a 5-minute clip completes in under a second and the result plays in VLC and a browser.

## Phase 6 — Destinations

- [ ] Spec: `fits`; add `tokio` + `reqwest`
- [ ] Drop-target row rendered from the `Registry`; queue uploads; copy the URL on success
- [ ] Discord webhook first; learn the size limit from rejections
- [ ] YouTube with bring-your-own credentials; resumable, throttled uploads; verified deletion
- [ ] Submit the Google compliance audit at the start of the phase

*Done when:* a clip dropped on Discord posts and copies its URL, and a YouTube upload interrupted
by pulling the network cable resumes and completes with no local file deleted before verification.

## Per-app audio tracks and a track mixer (D26)

Spread over the phases above; the cards are P2.24, P3.0a (do it before P3.1), P4.18
and P5.14.

- [x] Generalise `TrackId` (P3.0a)
- [ ] `AppAudioSource`: one track per chosen app (P2.24)
- [ ] Audio tracks tab in Settings (P4.18)
- [ ] Track mixer in the editor: volume, mute, solo, mixed export (P5.14)

## Portfolio polish (any time)

- [ ] Publish the repo; add the CI badge to the README
- [x] GitHub Sponsors (`.github/FUNDING.yml`) — ten minutes, no downside
- [ ] Screenshots / a short GIF of saving a clip
