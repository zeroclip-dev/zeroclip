# ZeroClip — Design (v2)

> The technical design behind ZeroClip: constraints, pipeline, memory strategy, reliability and roadmap.
> Imported from the v2 blueprint (Claude doc, 18 Sep 2026) and canonical from here on: edit this file, not the doc.
> Code comments cite sections as `DESIGN §N`. How the design maps onto crates and threads is in [ARCHITECTURE.md](ARCHITECTURE.md).

## 1. Core Philosophy & Constraints

Six non-negotiables. Every decision below traces back to one of these.

**Anti-cheat safe.** Strictly external. No DLL injection, no memory reading, no `SetWindowsHookEx`. Native Windows APIs only: Raw Input, Windows.Graphics.Capture, WASAPI.

**Zero-impact.** Frames never leave the GPU until encoded. System RAM handles compressed output, audio buffers, and lightweight UI — nothing else. Critically, encoded data lives in system RAM, *not* VRAM: VRAM is the one resource directly contested with the game, and reserving several hundred MB there causes texture thrashing that breaks this promise outright.

**GOP-aware buffering.** A retrospective clip must begin at an IDR frame or it will not decode. The ring buffer therefore tracks keyframe offsets and evicts in whole GOPs, never individual NAL units. The encoder is pinned to a fixed keyframe interval (1–2s) to make this tractable — which also sets the editor's trim granularity.

**Crash-survivable containers.** Standard MP4 writes its index at the end of the file, so a crash mid-session leaves an unplayable file. Since crash recovery is a headline feature, all direct-to-disk recording uses fragmented MP4 or MKV, both of which stay playable at any truncation point. Remux to plain MP4 at export — still a stream copy, still instant.

**Automated cloud storage.** Local files are temporary; output syncs to YouTube to conserve disk. Deletion is gated on verified upload, and off by default.

**Lightning-fast editing.** A built-in zero-encode editor for trimming and stitching via FFmpeg stream-copy, accepting keyframe-snap granularity in exchange for instant processing.

## 2. Tech Stack

The significant change from v1: **the encode path is decided.** v1 left it as "Media Foundation *or* ffmpeg-next," but these have very different zero-copy stories. MF accepts an `ID3D11Texture2D` directly and keeps the VRAM promise cleanly; ffmpeg-next can do it through d3d11va hwframes, but the Rust bindings for that path are rough. So: **MF for encode, FFmpeg for remux and concat only.**

| Layer | Choice | Why |
|---|---|---|
| Language | Rust | Memory safety, speed, fearless concurrency across capture/encode/IO threads |
| UI | egui | Immediate-mode, fully asleep when backgrounded |
| Capture | Direct3D 11 + Windows.Graphics.Capture | Native, external, no injection |
| Encode | Media Foundation (`mfplat`) | Takes D3D11 textures directly; true zero-copy |
| Remux / concat | `ffmpeg` (CLI or `ffmpeg-next`) | Stream-copy only — never touches the live capture path |
| Audio | `wasapi` | Process-scoped loopback capture |
| Input | Raw Input API (`WM_INPUT`) | Invisible to anti-cheat, handles chords without hook latency |
| Screenshots | `image` | Fast PNG/JPEG encode |
| Metadata | `rusqlite` | Embedded, zero-config |
| Foreground detection | `SetWinEventHook` | Event-driven; replaces `sysinfo` polling |
| Process enumeration | `sysinfo` | Still useful for PID lookup, just not for the hot path |
| Playback | `egui_video` or `libmpv` | See note below |
| Async runtime | `tokio` | Uploads, waveform extraction, background IO |

**Playback decision, flagged early:** `libmpv` is a much heavier Windows runtime dependency than anything else here, and it affects packaging and installer size. Decide this before Phase 5 rather than during it. `egui_video` is lighter but less mature; if scrubbing performance disappoints, the fallback is spawning an external player for preview rather than pulling in libmpv.

## 3. Video Pipeline

**Capture.** WGC produces an `ID3D11Texture2D` in VRAM. The D3D11 device sets `ID3D10Multithread::SetMultithreadProtected(TRUE)` so the capture API and encoder can't collide on the immediate context. The texture is handed straight to the MF encoder — it never touches system memory uncompressed.

**Encoder capability ladder.** New in v2, and important: hardware AV1 encode requires Arc, RTX 40-series, or RDNA3+. Assuming it exists means a hard failure on launch for a large slice of users. At first run, probe and store the result:

1. AV1 (best compression, newest hardware only)
2. HEVC / H.265 (broad support, good compression)
3. H.264 (universal fallback)

Store the winner in SQLite and re-probe only on GPU or driver change. Surface the active codec in the UI so the user knows what they're getting.

**Keyframe interval, forced.** The encoder is configured for a fixed IDR interval of 1–2 seconds. This isn't cosmetic — it's what makes GOP-aware ring buffer eviction possible and what determines editor trim granularity. Do not leave it to encoder defaults, which may use scene-change-adaptive keyframes and produce wildly uneven GOP sizes.

**Bitrate (VBR).** Minimum around 5 Mbps to save space on static scenes, maximum around 20 Mbps to allocate data during high-motion fights. Note the interaction with buffer sizing in §4: the ring buffer must be sized for the *maximum*, so a wide min/max spread means a lot of allocated-but-usually-unused RAM. That's an accepted tradeoff, but it's worth exposing max bitrate in the UI since it directly drives the memory footprint.

**HDCP / DRM handling.** WGC gives no event for protected-content transitions, and in practice it keeps delivering frames: on the Phase 1 test machine a Netflix tab was simply recorded, and hardware-DRM players are expected to show up black. Capture is retried with exponential backoff (250 ms up to 5 s) only if starting it actually fails with access denied; the repeat-last-frame ticker keeps PTS continuous whenever WGC goes quiet (D23).

## 4. Memory & Storage Strategy

### Why RAM, not VRAM

VRAM was considered for the ring buffer and rejected on three independent grounds, any one of which is disqualifying:

- MF hands back `IMFSample` with **system-memory** buffers. Putting encoded output in VRAM means uploading it back across PCIe for no benefit.
- It would have to come back to system RAM to be written to disk anyway. Extra hop, not a shortcut.
- VRAM is directly contested with the game. Taking 300–750 MB at 1440p/4K causes texture thrashing — the exact stutter the whole design exists to avoid.

VRAM is right for the *frame*, pre-encode. It is wrong for everything after.

### Ring buffer sizing

At 20 Mbps maximum VBR, that's 2.5 MB/s:

| Buffer duration | RAM required |
|---|---|
| 1 min | ~150 MB |
| 2 min | ~300 MB |
| 5 min | ~750 MB |

Pre-allocate at launch, and **page-lock it** (`VirtualLock`) so it can never be paged out mid-firefight. Sizing is against maximum bitrate, so a 5 Mbps static scene leaves headroom unused — accepted.

### GOP-aware eviction

The buffer is not a flat `VecDeque` of NAL units. It maintains a parallel index of GOP boundaries: offset, timestamp, and byte length per GOP. Eviction removes whole GOPs from the front. A dump walks back from the newest GOP until the requested duration is covered, then starts at that GOP's IDR. Without this, roughly half of all clips would start mid-GOP and fail to decode.

### Disk strategy, and the HDD problem

The HDD issue isn't throughput — 2.5 MB/s is nothing for a spinning disk. It's **seek contention**: capture writes interleaving with the game's asset streaming off the same spindle makes both stutter, and a 750 MB DVR dump saturates the drive for roughly seven seconds.

Mitigations, all cheap to implement:

- **Detect drive type at startup** via `StorageDeviceSeekPenaltyProperty`. Warn if the output directory shares a physical drive with the detected game.
- **Chunked writes.** Buffer in RAM and flush in 8–16 MB blocks rather than per-frame. Far fewer head repositions.
- **Preallocate session files** with `SetEndOfFile` so they don't fragment across a long session.
- **Pace DVR dumps on HDD.** Write at ~30–50 MB/s on a low-priority IO thread instead of blasting it. Costs a few seconds, keeps the game playable. On SSD, dump at full speed.

### On compressing the buffer

Don't. AV1/HEVC output is already entropy-coded; LZ4 or zstd over it yields maybe 1–2% for real CPU cost on the hot path. (A *dynamics* compressor on the mic is a different thing entirely and is covered in §6.)

## 5. Recording Modes

All modes share one zero-allocation encode loop. Memory is pre-allocated at launch so the system allocator is never touched during gameplay.

**Retrospective DVR (clip last X minutes).** Encoded NAL units and both audio tracks flow into their respective ring buffers (§4, §6). The hotkey walks back to the covering GOP boundary and dumps video plus time-aligned audio to disk. This is the only mode where GOP-awareness matters.

**Manual record (start/stop).** Bypasses the ring buffer entirely. The hotkey opens a direct fMP4/MKV stream and writes until toggled off.

**Session recording.** Triggered by the Game Profiles engine (§7) on game launch. Streams direct-to-disk for the life of the process. Because the container is fragmented, an abrupt game crash leaves a playable file rather than a corrupt one.

**Screenshots.** Intercepts a single uncompressed `ID3D11Texture2D`, maps it to system memory, encodes to PNG or JPEG. Separate keybind, zero latency, independent of whether any recording mode is active.

**Running DVR and session recording simultaneously** is supported and expected for cs2-style profiles — the encoder output tees to both the ring buffer and the disk writer, so there's one encode, two sinks. Worth building the tee abstraction in Phase 3 rather than retrofitting it.

## 6. Audio Pipeline

### Parallel audio ring buffers (the v1 gap)

v1 described the ring buffer for video only. Retrospective DVR needs the last X minutes of **both** audio tracks held in matching time-windowed buffers, evicted in step with the video GOPs. Without this, every DVR clip comes out silent. Audio is cheap — two tracks of AAC at 192 kbps is about 48 KB/s, so five minutes is roughly 14 MB against the video's 750 MB. Eviction is driven by the video GOP index: when a GOP is dropped, audio older than that GOP's start PTS is dropped with it.

### Master clock and PTS

The pipeline extracts `SystemRelativeTime` from the WGC `Direct3D11CaptureFrame` and maps it to the muxer's presentation timestamps. This is the anchor for everything; both audio tracks are aligned against it rather than against each other.

### Track 1 — system audio

v1 proposed `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` with a UI for picking multiple apps to ignore. **This doesn't map cleanly onto the API** — activation params target a single process tree per client, so "exclude Discord *and* Spotify" doesn't express directly.

Inverting to include-mode — capture the game's process tree, using `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` — is simpler and usually gets the same result, since the game is already identified by the profile engine. Verify against the `AUDIOCLIENT_ACTIVATION_PARAMS` docs before designing the UI, because this choice determines whether the UI says "capture from" or "ignore these."

### Track 2 — microphone

Bound to a push-to-talk key via Raw Input. Writes silence when released so track length stays constant and PTS alignment holds.

**Dynamics compressor, mic only.** Worth adding: it stops shouting from clipping and makes quiet callouts audible. Leave game audio uncompressed — flattening its dynamic range is exactly what makes clips sound worse. Starting point: ratio 3:1, threshold around −18 dBFS, attack 5 ms, release 100 ms, with makeup gain.

**Latency gotcha:** if a lookahead limiter is used, that lookahead is real delay and must be subtracted in the PTS mapping, or the mic drifts ahead of the video by exactly the lookahead window. Make the compensation explicit and constant rather than discovered later.

## 7. Game Profiles Engine

**Event-driven, not polled.** v1 tied this to `sysinfo` scanning the foreground window. Replace that with `SetWinEventHook` on `EVENT_SYSTEM_FOREGROUND` — the OS notifies on focus change, costing nothing between events instead of burning a timer forever. `sysinfo` stays around for PID and executable-path lookup once a change fires.

On a foreground change the engine resolves the executable, looks up a profile in SQLite, and applies it:

| Executable | Buffer | Session recording |
|---|---|---|
| `cs2.exe` | 2 min | Auto-start on launch |
| `RainbowSix.exe` | 5 min | Off |
| *default* | 1 min | Off |

**Resizing a live ring buffer** needs care. Growing means allocating and copying while the encoder is still pushing; shrinking means dropping the oldest GOPs. Do it on a profile-change boundary with a brief writer pause rather than in-place under contention. If the target size exceeds a configured RAM ceiling, clamp it and tell the user rather than silently allocating 2 GB.

Profiles should be user-editable from the UI, and unknown executables should be easy to promote into a profile from a "recent games" list rather than requiring manual path entry.

## 8. Reliability & Watchdogs

### Crash rescue

Detects unexpected termination of the tracked game process, dumps the current ring buffer to `recovery.tmp`, and prompts on next focus: *"Game closed unexpectedly. Keep the last X minutes?"* Because session recordings use a fragmented container, a crash mid-session needs no rescue at all — the file is already playable.

### Encoder watchdog — make it a state machine

v1 said the watchdog "reinitializes the D3D11 pipeline without crashing the app." That's the right goal, but reinitializing a device mid-session while the ring buffer is live is genuinely tricky and will corrupt the buffer if done ad hoc. Model it explicitly:

```text
RUNNING
  ├─ 10 consecutive dropped frames, or encoder HRESULT fault
  ▼
DRAINING     — stop accepting new frames; flush encoder's in-flight samples
  ▼           into the buffer; freeze the GOP index
FLUSHING     — write every complete GOP to disk (nothing is lost)
  ▼
REINIT       — tear down and rebuild D3D11 device + MF encoder
  ▼           (on repeated failure, step down the codec ladder from §3)
RESUMING     — new IDR; buffer restarts from empty; PTS continues from
  ▼           the master clock, not from zero
RUNNING
```

The PTS detail matters: restarting timestamps at zero after a reinit produces files that seek incorrectly. Continue from the master clock and let the gap show as a discontinuity.

### Monitoring inputs

Watch encoder `HRESULT` returns, frame delta times against expected frame interval, and disk write latency. A slow disk producing back-pressure looks like encoder stalls from the inside but needs a different response (pace writes, don't reinit the encoder) — so distinguish them before acting.

### Raw Input needs a message pump

A detail that bites people: `WM_INPUT` requires a window with a running message loop. Don't rely on the sleeping egui window — give the input system a dedicated `HWND_MESSAGE` window on its own thread so hotkeys work reliably regardless of UI state.

## 9. The Lightning Editor

Speed comes from never re-encoding. FFmpeg stream-copy slices and joins files essentially instantly.

**Playback.** `egui_video` scrubs the clip inside the Rust UI. See the packaging note in §2 before committing to libmpv as the alternative.

**Instant trimming.** The user sets in/out points; the backend cuts at the nearest IDR. Because §3 pins the keyframe interval to 1–2s, snap granularity is known and consistent rather than variable. **Show the snap points in the scrub bar** — users accept the limitation readily when they can see where cuts will land, and find it maddening when they can't.

**Stitching.** FFmpeg concat demuxer joins clips instantly, provided codec and resolution match. Clips recorded under different profiles or after a codec-ladder fallback won't concat cleanly — detect the mismatch up front and either refuse with a clear reason or offer a one-time re-encode, rather than producing a broken file.

**Waveform.** A background `tokio` task computes RMS peaks and hands an array to egui to draw. Cache the result in SQLite keyed by file hash so reopening a clip is instant.

**Worth adding:** keep the original file until the trimmed export is verified, and make trimming non-destructive by default. "Instant" editing that silently overwrites the source is how people lose the clip they were trying to save.

## 10. Destinations & Upload Pipeline

Rewritten from v1's YouTube-only plan. The core insight: **YouTube is the wrong default for a clipper.** Most clips go to five friends in a Discord channel, not to the public internet. The architecture should treat upload targets as pluggable, and the build order should start with the easiest one, not the most prestigious.

### The `Destination` trait

Every target implements one interface, so the UI is generated from whatever is registered rather than hand-built per platform:

```rust
trait Destination {
    fn name(&self) -> &str;
    fn icon(&self) -> &Icon;
    fn max_size(&self) -> Option<u64>;      // None = no practical limit
    fn auth_state(&self) -> AuthState;      // None | Ready | Expired
    async fn upload(&self, clip: &Clip, progress: ProgressSink)
        -> Result<UploadedUrl, UploadError>;
}
```

Adding a platform later becomes one impl, not a UI change. Drop targets render from the registry.

### Drag-and-drop UX

The editor shows a row of drop targets. Three details that make it feel finished:

- **Show each target's size ceiling before the drop**, and grey out targets the clip won't fit. Discovering the limit after a 40-second upload is the worst version of this.
- **Copy the resulting URL to clipboard on success.** That's the actual goal every single time — the user wants to paste a link, not admire a progress bar.
- **Queue rather than block.** Uploads run on `tokio`; the editor stays responsive and multiple clips can be in flight.

### Destination 1 — Discord (build this first)

A webhook POST: multipart form to a URL, no OAuth, no quota, no API project, no audit. The entire flow can be proven in an afternoon, which makes it the right vehicle for validating the drag-drop-upload plumbing before touching anything painful.

The constraint is size. Discord raised the free per-file cap to 20 MB in August 2026 (up from the 10 MB set in 2024); Nitro Basic is 50 MB, full Nitro 500 MB. At 20 Mbps that's roughly eight seconds on the free tier. So the Discord path needs one of:

- a **"compress for Discord" re-encode** — the one place the zero-encode rule is deliberately broken, targeting a bitrate computed from clip duration and the detected cap; or
- a **link fallback** — upload elsewhere, post the URL instead.

Detect the cap where possible rather than hardcoding 20 MB, since these numbers have moved repeatedly.

### Destination 2 — YouTube (harder than it looks)

Two API constraints that shape the whole design, and neither is obvious until you hit it:

**Unlisted is exactly what gets blocked.** Videos uploaded via `videos.insert` from unverified API projects created after 28 July 2020 are forced to *private* viewing mode, liftable only by passing a compliance audit. Private clips can't be shared, so unaudited the feature silently fails at its actual purpose.

**Quota is per API project, not per user.** The default allocation is 100 `videos.insert` calls per day, shared across every user of a shipped `client_secret`. Fifty users would exhaust it before lunch. (The per-upload cost did improve — `videos.insert` dropped from 1,600 units to 100 — so quota is no longer the constraint for a *single* user. It is fatal for a distributed one.)

Two viable paths:

| Path | Upside | Downside |
|---|---|---|
| Pass the compliance audit | Clean UX, unlisted works, one-click | Needs a domain, homepage, privacy policy; individual devs report repeat attempts; timeline not under your control |
| Bring-your-own-credentials | Works immediately, each user has their own quota | Users must create a Google Cloud project and paste a client secret |

**Decision: ship BYO-credentials, attempt the audit in parallel.** Phase 6 must not be gated on Google's approval timeline. If the audit lands, BYO becomes the fallback rather than the default.

**Unlisted is distribution, not archival.** YouTube re-encodes everything, so a 20 Mbps AV1 master comes back at YouTube's bitrate. Content ID scans unlisted videos, so in-game licensed music or background Spotify can get a clip claimed or blocked. YouTube has also retroactively changed unlisted visibility before. Also note the 15-minute upload ceiling without phone verification.

### Destination 3+ — the rest

- **Streamable** — the classic clip-sharing target, link-first, has an upload API. Check current free-tier terms.
- **Bunny Stream / Cloudflare Stream** — developer CDNs, user owns the hosting. Bunny is roughly $0.01/GB storage and $0.005/GB delivery as of mid-2026; Cloudflare Stream charges storage plus delivery with no encoding fees and no egress. Good advanced option, wrong default (needs an account and a card).
- **Twitch, Imgur, S3, local NAS, plain folder** — each is a small impl once the trait exists.

### Deletion policy

Unchanged from the v2 position, and everything above reinforces it:

1. **Off by default.** Opt-in, with the risk stated plainly.
2. **Verify before deleting.** A 200 is not proof. Gate on a returned video ID *plus* a `videos.list` confirming processing completed and the upload wasn't rejected or claimed.

Given that Content ID can block an unlisted clip after upload succeeds, and that platform visibility rules have changed retroactively, auto-deleting the only local copy is the worst available failure mode for a tool whose pitch is reliability.

## 11. Development Roadmap

The phase order is a genuine dependency chain — each phase is unblocked by the one before. Every phase has a done-when so you can tell whether you've actually finished it.

**Phase 0 — Spike (a weekend).** Before committing to the architecture, prove the riskiest assumption: WGC frame → MF hardware encoder → file on disk, with no system-memory round trip for the frame. Everything else is conventional; this is the load-bearing bit. *Done when:* a 10-second hardware-encoded MP4 exists and Task Manager shows no unexpected copy overhead.

**Phase 1 — Capture core.** D3D11 device with `ID3D10Multithread`, WGC session, the codec capability ladder, forced keyframe interval, HDCP backoff handling. *Done when:* recording survives alt-tabbing, resolution changes, and a Netflix tab opening and closing.

**Phase 2 — Audio, input, sync.** WASAPI loopback with the process include/exclude model resolved, Raw Input on its own `HWND_MESSAGE` thread, PTS mapping from `SystemRelativeTime`, mic compressor with latency compensation. *Done when:* a 30-minute recording has no measurable A/V drift at the end.

**Phase 3 — Buffers, modes, muxing.** GOP-indexed video ring buffer, parallel audio ring buffers, the encoder-output tee, direct-to-disk fMP4 writer, interleaved muxing. *Done when:* a DVR dump taken at any arbitrary moment decodes from the first frame and has audio.

**Phase 4 — UI, profiles, watchdogs.** egui frontend, SQLite metadata, `SetWinEventHook` profile engine with safe buffer resizing, the watchdog state machine, crash rescue. *Done when:* killing the game process via Task Manager reliably produces a recovery prompt with intact video.

**Phase 5 — Editor.** Playback, waveform extraction and caching, stream-copy trim with visible snap points, concat with codec-mismatch detection. *Done when:* trimming a 5-minute clip completes in under a second and the result plays in VLC and a browser.

**Phase 6 — Destinations.** The `Destination` trait and drop-target UI, Discord webhook first (no auth, proves the plumbing in an afternoon), then YouTube with BYO credentials, resumable throttled upload, verified deletion. Submit the compliance audit at the start of this phase so it runs in the background rather than blocking. *Done when:* a clip drag-dropped to Discord posts and copies its URL, and a YouTube upload interrupted by pulling the network cable resumes and completes with no local file deleted before verification.

### Suggested build order within phases

Write the disk writer and muxer against synthetic frames before wiring real capture into them. Debugging a ring buffer is far easier when frames are numbered 1, 2, 3 than when they're real video — you can assert exactly which frames should survive eviction.

## 12. Open Questions

Things to verify against documentation before designing around them. Several of these shape the UI, so resolving them early avoids rework.

- **WASAPI process loopback mode.** Confirm whether include-mode or exclude-mode fits better, and whether multiple exclusions are expressible at all. This determines the entire audio settings UI (§6).
- **HDCP resume signalling.** Does WGC surface any usable event on protected-content transitions, or is polling with backoff genuinely the only option? (§3)
- **libmpv vs egui_video.** Affects installer size and packaging. Decide before Phase 5. (§2, §9)
- **Buffer resize under load.** Is a brief writer pause acceptable, or is double-buffering the allocation worth the complexity? (§7)
- **fMP4 vs MKV.** Both survive truncation. MKV tooling is more forgiving; fMP4 needs no remux for browser playback. Pick one and be consistent.
- **Multi-monitor and HDR.** Not addressed in v1 at all. Which display gets captured when the game moves, and what happens to HDR content on an SDR encode path? Tone mapping is not free and will need a decision.
- **Will the YouTube compliance audit pass?** Unknown and outside your control, which is why BYO-credentials ships first. Submit early, treat approval as a bonus. (§10)

### Two suggestions not yet placed in the design

**Structured logging from day one.** A ring-buffered log of the last few thousand events (frame timings, encoder returns, state transitions) dumped alongside crash recovery will save enormous time. Capture bugs are frequently unreproducible, and "it stuttered once last Tuesday" is unfixable without a trace.

**A synthetic-load test harness.** A mode that feeds generated frames at a configurable rate, with injectable faults — dropped frames, encoder errors, disk stalls — lets the watchdog state machine be tested deterministically instead of by hoping a real crash happens while you're watching.
