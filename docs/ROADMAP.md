# Roadmap

The phases are a dependency chain: each one is unblocked by the one before. Every phase
has a *done when* — if it isn't true, the phase isn't finished. Tick the boxes as you go.

**v0.1 = Phases 0–3**: a working DVR clipper with proper audio and crash-safe files,
with a README that explains the architecture. Ship that before anything else — a shipped,
documented core beats a perfect product that isn't finished.

How to see what is left in the code: `cargo test -- --ignored` runs every spec that
hasn't been implemented yet.

Every box below is broken into task cards in [docs/plan/](plan/README.md). Work the
cards; tick a box here when its cards are done.

## Foundations — done

- [x] Cargo workspace with one crate per subsystem, and the dependency rules between them
- [x] `zc-core` domain types; specs for the DVR buffer, watchdog, hotkeys, profiles and clock
- [x] Engine actor with ports for the platform adapters, and a synthetic-source harness
- [x] egui app shell: settings persistence, logging, profile editor, activity feed
- [x] Design, architecture and decision docs; CLAUDE.md for Claude Code
- [x] Add the CI workflow as `.github/workflows/ci.yml` (format, lint, test and docs on Windows)

## Phase 0 — Spike (a weekend)

Prove the riskiest assumption before building on it: WGC frame → Media Foundation
hardware encoder → file on disk, with no system-memory round trip for the frame.

- [x] `spikes/phase0-capture-encode`: steps 1–4
- [x] Try H.264 first, then HEVC (and AV1 if your GPU has it). H.264 and HEVC work; AV1 isn't supported on this GPU. Findings are in the spike's module doc.

*Done when:* a 10-second hardware-encoded MP4 exists, plays in VLC and a browser, and Task
Manager shows the GPU's Video Encode engine busy with no unexpected CPU copy overhead.

## Phase 1 — Capture core

- [ ] `zc-video::device` — the shared D3D11 device, multithread-protected
- [ ] `zc-video::capture` — WGC session, resize handling
- [ ] `zc-video::convert` — BGRA → NV12 on the GPU (video processor)
- [ ] `zc-video::encoder` — capability probe and the async hardware MFT; fixed GOP, VBR, no B-frames
- [ ] Specs: `Codec::step_down`, `Codec::select`, `Backoff` (HDCP retry)
- [ ] `zc-video::screenshot`, and wire `Command::Screenshot` in the engine
- [ ] Wire `GpuVideoSource` into `src/wiring.rs`; show the codec in the status bar

*Done when:* recording survives alt-tabbing, resolution changes, and a Netflix tab opening and closing.

## Phase 2 — Audio, input, sync

- [ ] Specs: `MasterClock`, `ChordMatcher`, `Compressor`, `mouse_buttons`
- [ ] `zc-win::message_thread`; `zc-input` Raw Input + foreground hook
- [ ] `zc-audio` game audio (process loopback, include mode) and mic with push-to-talk
- [ ] Resolve the loopback include/exclude question (docs/OPEN_QUESTIONS.md) before building the audio settings UI
- [ ] Hotkeys drive commands in the engine

*Done when:* a 30-minute recording has no measurable A/V drift at the end.

## Phase 3 — Buffers, modes, muxing

- [ ] Specs: `DvrBuffer` (all of them, including `dvr_push_never_allocates`), `Tee`, `ChunkedWriter`, `WritePacer`
- [ ] Implement `zc_mux::container::create` (fMP4, decided in D18)
- [ ] Crash-safe recordings: sync every fragment, `.partial` files, startup repair (D18)
- [ ] `Recorder` spec; `SyntheticVideoSource`; the `synthetic_harness` save-clip spec
- [ ] DVR save, manual recording, session recording; page-lock the buffer (`VirtualLock`)
- [ ] `zc-win::drive` + HDD pacing for dumps

*Done when:* a DVR dump taken at any arbitrary moment decodes from its first frame and has audio.

**→ v0.1 release:** polish the README with a GIF, write a short "how it works" post, tag it.

## Phase 4 — UI, profiles, watchdogs

- [ ] Specs: `Watchdog::on_event`, `ProfileSet`, `clamp_window`
- [ ] `zc-store` (rusqlite, bundled) and the migrations
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

## Portfolio polish (any time)

- [ ] Publish the repo; add the CI badge to the README
- [ ] GitHub Sponsors (`.github/FUNDING.yml`) — ten minutes, no downside
- [ ] Screenshots / a short GIF of saving a clip
