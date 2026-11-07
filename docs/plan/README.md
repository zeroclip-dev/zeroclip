# The build plan

This plan covers every remaining roadmap item as a small task card: 119 cards across
Foundations and Phases 0–6. It was made on 2026-10-04 by a read-only sweep of
every source file. That sweep found all 71 ignored specs, all 81 `todo!()`s and all 12
scaffold attributes, and each one belongs to a card. The cards are meant to be worked
one at a time.

| File | Covers |
|---|---|
| [F-P0.md](F-P0.md) | First build, CI, and the Phase 0 capture → encode spike |
| [P1.md](P1.md) | Phase 1: capture core (WGC, NV12, hardware encoder, screenshots) |
| [P2.md](P2.md) | Phase 2: audio, input, sync |
| [P3.md](P3.md) | Phase 3: DVR buffer, fMP4, DVR save, recording, robustness → **v0.1** |
| [P4.md](P4.md) | Phase 4: profiles, watchdog, crash rescue, tray |
| [P5.md](P5.md) | Phase 5: editor |
| [P6.md](P6.md) | Phase 6: Discord and YouTube |
| [X.md](X.md) | Portfolio polish (any time) |

Work the files in that order, and the cards top to bottom. Within a file the cards are
in dependency order. Each card lists its dependencies, so you can also pick any card
whose dependencies are ticked.

## Running a session

Start each session with:

> Do the next unchecked task in docs/plan/. Read its card and the files it names. Follow CONTRIBUTING.md.

The protocol:

1. **Branch:** work on `feat/<topic>` (or `fix/<topic>`) branched from `dev`, and merge
   it back into `dev` when its cards are green. `master` only moves when a phase is done
   and hardware-checked. One commit per card, with a message that names the card ID
   (`P1.7: WGC capture and resize`).
2. **Implement the card.** Remove the `#[ignore]` from its specs and make them pass.
   **Never weaken or delete a spec.** If a spec looks wrong, stop and ask 0xtikaa.
3. **Green gate:** `cargo fmt --all`,
   `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
   All three must pass before ticking.
4. **Tick:** check the card's box in its plan file and the matching box in
   `docs/ROADMAP.md`. Update ARCHITECTURE, DESIGN or DECISIONS when the card says to.
   For a new DECISIONS entry, use the next free D-number. D18–D22 are taken.
5. **Hardware cards (HW):** run whatever can run from the terminal (`cargo run -p …`,
   `ffprobe` on the output). Then write a short list of what 0xtikaa must check by eye
   (Task Manager, VLC, alt-tab, Netflix…). Leave the box unticked and append
   `(awaiting HW check)` to the checklist line. **Never tick a done-when you didn't see pass.**
6. **Stop and ask 0xtikaa** when a card is "blocked by" an open question, before publishing
   or tagging anything (P3.30, X.3), or when a step would add a dependency or a Windows
   feature the card doesn't name.
7. **Stay in the phase.** Pure cards (zc-core, zc-mux and zc-editor logic, which need no
   GPU) may be pulled forward from the next phase while a hardware check is pending, but
   only with 0xtikaa's OK.

Card legend: **Size** S is under a session, M is about one, and L may take two. "Depends
on" lists card IDs. "Specs to un-ignore" and "`todo!` sites removed" are exact. "Reuse"
lists what already exists; use it instead of writing something new.

## Decisions

Settled in planning and recorded in [DECISIONS.md](../DECISIONS.md):

- **D18: fMP4, synced every fragment** (was Q3). Recordings are written as
  `.mp4.partial`, with `FlushFileBuffers` per GOP, and repaired at startup. Worst case
  about 1 s lost. Cards P3.11, P3.19a, P3.21, P3.22 and P3.22a.
- **D19: snapshots copy** (was Q4). Measure the lock hold in P3.17.
- **D20: DVR and one recording at the same time** (was Q5).
- **D21: brief writer pause on resize** (was Q6).
- **D22: keyframe thumbnails and an external player** (was Q7). Cards P5.9 and P5.13.

Recommendations 0xtikaa accepted with the plan. Each one gets a DECISIONS entry when its
card lands:

- WASAPI through raw windows-rs, not the `wasapi` crate (P2.14).
- `zc_core::SampleTimeline` keeps audio on the QPC timeline, which bounds 30-minute drift
  (P2.4).
- Q2 (HDCP): retry with backoff only on an actual capture failure, with no
  placeholder-frame generator (P1.11, P1.15).
- Q8: primary monitor, SDR, encode size fixed at start, letterbox on resize (P1.8,
  P1.15).
- Q1: include mode, verified on hardware (P2.21).
- Profiles stay in `settings.toml` and are **sticky**: an unknown foreground app never
  resizes the buffer (P4.6).
- Rescue files are named `recovery-<unix>.<ext>.tmp` (P4.11).
- Phase 5 jobs run on std threads. tokio arrives in Phase 6, and reqwest uses `native-tls`
  (P6.2).
- Q9: check whether a bring-your-own YouTube project is locked to private uploads before
  writing any OAuth code (P6.0).
- Q10: Discord's size limit is learned from 413s only (P6.4).
- The encoder probe isn't cached, because zc-video can't depend on zc-store.
- `MicMode::Off` means no mic track at all.
- `image` (png only) is used for screenshots; `tray-icon` for the tray.

## Cross-cutting pitfalls

Every card repeats the pitfalls that apply to it locally. These apply everywhere:

1. **Unfulfilled `#[expect(dead_code)]` is an error** under `-D warnings`. Delete it in the
   same change that uses the item. If the scaffolding is used from `#[cfg(test)]`, write
   `#[cfg_attr(not(test), expect(...))]`.
2. **windows-rs 0.62 and Send.** D3D11 and WGC types are Send. Every `IMF*` type and
   `ICodecAPI` is not, so all MF objects live on one encoder thread. `TypedEventHandler`
   takes `Fn`, not `FnMut`: put a Mutex inside it, and never unwrap a poisoned lock on a
   capture or audio thread.
3. **COM leaks.** Drop the `ManuallyDrop` fields after every call:
   `D3D11_VIDEO_PROCESSOR_STREAM.pInputSurface`, `MFT_OUTPUT_DATA_BUFFER.pSample` and
   `MFT_OUTPUT_DATA_BUFFER.pEvents`.
4. **Async MFTs:** set `MF_TRANSFORM_ASYNC_UNLOCK` before anything else. Send
   `BEGIN_STREAMING` and `START_OF_STREAM`. Handle `MF_E_TRANSFORM_STREAM_CHANGE` on the
   first output.
5. **No allocation on the hot path.**
   - Unbounded `std::sync::mpsc` allocates about every 31 sends, so send health
     *transitions* only, never per-frame events.
   - Tee writers run under the recorder lock, so recordings go through the spool and an
     IO thread.
   - `crates/zc-core/tests/dvr_no_alloc.rs` and P3.17a's `recorder_no_alloc` enforce this.
6. **CI has no GPU.** Never write a test that creates a D3D11 device or touches WGC, MF or
   WASAPI. Test the pure helpers, and run Windows examples by hand.
7. **windows-rs features:** add one only when a card needs it. Known gaps are listed on
   the cards:
   - zc-video: `Win32_System_Com`, `_Ole` and `_Variant`; drop `Direct3D10`.
   - zc-audio: `Win32_Security`.
   - zc-win: `Win32_Security`, `Win32_System_Memory`, `Win32_System_Registry` and
     `Win32_Security_Cryptography`.
8. **Constants windows-rs lacks:** `CLSID_AACMFTEncoder` (define it locally) and the
   `HID_USAGE_*` values (use the literals). `u64QPCPosition` is already in 100 ns units,
   so don't convert it.
9. **Secrets and keystrokes never reach the logs.**
   - Redact the keystroke trace in the actor (P2.11).
   - Give `Secret` a redacted `Debug`, and write a manual `Debug` for `DiscordWebhook`.
   - Store the YouTube refresh token with DPAPI.
10. **ffmpeg spawns:**
    - Pass `CREATE_NO_WINDOW`, `-nostdin` and `-n`.
    - Put `-t` after `-i`.
    - Add `-tag:v hvc1` for HEVC only.
    - ffprobe's `pts_time` is absolute, so subtract `start_time`.
11. **Tray on eframe 0.36 (retired by D36; the rule carries over to GPUI).** The root window can't start hidden: send
    `ViewportCommand::Visible(false)` from `logic()`. Drain engine and tray events in
    `logic()`; `ui()` doesn't run while the window is hidden.
12. **SQLite:** turn on `PRAGMA foreign_keys` on every connection and use WAL. Never edit
    `0001_init.sql`; add `0002_*.sql` instead.
13. **windows-rs 0.62 gives `PROPVARIANT` a `Drop` that calls `PropVariantClear`**
    (in `src/extensions`, not the generated bindings). A `VT_BLOB` pointing at stack
    memory then gets `CoTaskMemFree`d and the heap is corrupted
    (`0xC0000374`). Wrap such a `PROPVARIANT` in `ManuallyDrop` (P2.16 found this the
    hard way).
14. **Panics across FFI abort the process.** Keep `extern "system"` callbacks and COM
    `_Impl` methods trivial, and use `try_borrow_mut` and `try_lock`, never `unwrap`.
15. **Docs that are out of date.** Each phase's close-out card fixes the drift in its
    own area:
    - ROADMAP "Current phase" line.
    - The ARCHITECTURE thread table: one IO thread per dump or recording, and tokio only
      from Phase 6.
    - DESIGN §7 ("SQLite" profiles, `sysinfo`), §8 (`recovery.tmp`), §10 (the
      `Destination`/`AuthState` shape) and §12 ("disk stall" fault).
    - The synthetic_harness watchdog comment, which contradicts DESIGN §8. Change it on
      purpose, and give the reason in the commit.

## Reuse and UI design

- **Reused things become one shared piece.** When the same UI widget, layout or service
  logic is needed a second time, move it into one shared place instead of copying it:
  UI building blocks in `src/ui/components/` (one small module per widget, used by every
  tab), services behind the existing ports in `zc-core`. Don't extract on first use;
  extract on the second.
- **Designing the app (Phase 4 UI cards and later):** load the `apple-design` and
  `emil-design-eng` skills before designing a screen, and apply their principles (restraint,
  clear hierarchy, typography, purposeful motion, reduced motion) within GPUI. They are
  written for the web, so translate rather than copy. Owner wants the UI to feel tasteful,
  not like a settings dump.

## 0xtikaa's checkpoints

These steps need a person, a GPU, or a decision:

| When | What |
|---|---|
| P0.3–P0.4 | Watch Task Manager's Video Encode graph; play the spike MP4 in VLC and a browser |
| P1.15 | Alt-tab, change resolution, open and close a Netflix tab while recording |
| P2.12, P2.19 | Hotkeys work while a game has focus; mic, PTT and game audio are audible |
| P2.20–P2.23 | 30-minute drift run; confirm include-mode loopback (Q1) |
| P3.22a, P3.27 | Kill ZeroClip mid-recording, then check the repaired file; a DVR dump at a random moment decodes and has audio |
| P3.30 | Approve tagging v0.1.0 |
| P4.17 | Kill the game from Task Manager's Details tab; the recovery prompt appears with intact video |
| P5.12 | 5-minute trim in under 1 s; plays in VLC and a browser |
| P6.0 | Submit the YouTube audit; test whether a bring-your-own project is locked to private |
| P6.12 | Pull the network cable mid-upload; it resumes; nothing is deleted unverified |
| X.2, X.3 | Your GitHub username and repo name, and approval to publish |
