# Decisions

Settled choices, newest last. Each one says what was decided, why, and what it
costs. To change one, add a new entry that supersedes it — don't edit history.

### D1 · Rust, Windows-only
Rust for memory safety and fearless concurrency across capture, encode and IO threads.
Windows-only because the whole approach relies on Windows.Graphics.Capture, WASAPI
process loopback and Media Foundation.

### D2 · Capture with Windows.Graphics.Capture on D3D11
External, public API: nothing injected into the game, so it is anti-cheat safe. The
D3D11 device is shared by capture, colour conversion and encode, with multithread
protection on. (DESIGN §1, §3)

### D3 · Media Foundation encodes; FFmpeg only remuxes and concatenates
MF accepts `ID3D11Texture2D` input directly, which keeps capture zero-copy. FFmpeg's
Rust bindings for hardware frames are rough, so FFmpeg is used only for stream-copy
editing — and as an external `ffmpeg.exe` process, which keeps its LGPL/GPL build
choices away from our licensing. (DESIGN §2)

### D4 · Encoded data in system RAM, never VRAM
MF returns system-memory samples; VRAM is contested with the game; data has to reach
RAM to be written anyway. The DVR buffer is pre-allocated, sized for the *maximum*
bitrate and page-locked. (DESIGN §4)

### D5 · GOP-aware DVR buffer with a fixed keyframe interval
Clips must start on an IDR. The buffer evicts whole GOPs; the encoder uses a fixed 1–2 s
GOP (never scene-adaptive), which also fixes the editor's trim precision. Audio rings
are evicted in step with video. (DESIGN §4, §6)

### D6 · No B-frames
Decode order equals presentation order, so packets carry a single timestamp and the
muxer never reorders. Costs a little compression; buys a lot of simplicity.

### D7 · One time unit: 100 ns ticks (`Hns`)
It's the native unit of WGC, WASAPI QPC positions and MF. The `Recorder` owns the master
clock and maps raw capture time to session PTS for every track. (DESIGN §6)

### D8 · Crash-survivable containers
Direct-to-disk recording uses fragmented MP4 or MKV; export remuxes to plain MP4.
Which of the two is the default is still open (OPEN_QUESTIONS.md). (DESIGN §1)

### D9 · Game audio by process-loopback include mode
Capture the focused game's process tree rather than excluding a list of apps: the
activation parameters take one process tree per client, and the profile engine already
knows the game. Needs Windows 11; older systems fall back to endpoint loopback. To be
verified in Phase 2. (DESIGN §6)

### D10 · Raw Input on a dedicated message thread; `SetWinEventHook` for focus
Raw Input with `RIDEV_INPUTSINK` works while a game has focus and involves no hooks.
It needs a message loop, so it gets its own `HWND_MESSAGE` window thread rather than the
sleeping UI window. Foreground changes are event-driven, not polled. (DESIGN §7, §8)

### D11 · The watchdog is an explicit state machine
Running → Draining → Flushing → Reinit → Resuming, stepping down the codec ladder on
repeated failures. Pure logic, so it's tested deterministically with synthetic faults.
(DESIGN §8)

### D12 · Ports and adapters
The engine talks to `VideoSource` / `AudioSource` / `InputSource` traits in `zc-core`;
Windows crates implement them; the app wires them in. Keeps the engine testable on any
OS and the Windows code at the edges.

### D13 · egui via eframe, glow renderer
Immediate-mode UI that sleeps when nothing changes. The glow (OpenGL) backend builds far
faster than wgpu; switch the eframe feature to `wgpu` if OpenGL misbehaves on a target
machine.

### D14 · Destinations are pluggable; Discord first; YouTube with BYO credentials
Most clips go to friends on Discord: a webhook needs no OAuth or quota, so it proves the
drag-drop-upload plumbing first. YouTube forces uploads from unaudited API projects to
private, and quota is per project, so users bring their own credentials while the audit
is attempted in parallel. (DESIGN §10)

### D15 · Never delete unverified; auto-delete off by default
Deletion requires a returned id plus a status check, never just HTTP 200. Platforms
re-encode, can block clips after upload, and change visibility rules. (DESIGN §10)

### D16 · GPL-3.0-or-later (superseded by D28)
Open source for the portfolio; copyleft so nobody can resell it closed. As sole copyright
holder, dual-licensing stays possible later. The project name is separate from the
license: forks can't ship under the ZeroClip name.

### D17 · Specs as ignored tests
Designed-but-unbuilt behaviour lives as `#[ignore = "spec: …"]` tests beside the code.
Progress is `cargo test -- --ignored`; a spec is only changed deliberately, with a reason.

### D18 · Fragmented MP4, synced every fragment (settles Q3; refines D8)
fMP4 is the only container we write. `create(Matroska)` returns `Unsupported`, but the
enum variant stays so older settings files still load. Each GOP (1–2 s, D5) becomes one
`moof`+`mdat` fragment. Recordings and crash-rescue dumps flush the IO stack and call
`FlushFileBuffers` at every fragment boundary, even when an IO chunk isn't full yet. They
are written as `*.mp4.partial` and renamed when finished. At startup the app truncates
any leftover `.partial` to its last complete fragment and offers it to the user. That puts
the worst case at about 1 s lost, whether the app crashes, the game crashes or the power
fails. DVR clip dumps sync once at the end, because their source is still in RAM.
fMP4 plays in browsers and Discord without a remux, which matters because Discord is the
main destination (D14). Truncation-safety is the same in either container; how often we
sync is what bounds the loss. Costs: about 300 more lines of box writing than MKV, and
one fsync per second while recording.

### D19 · DVR snapshots copy (settles Q4)
`DvrBuffer::snapshot` copies the window out, as the specs already assume. The engine
logs how long the recorder lock is held for each snapshot. Revisit with shared
fixed-size segments only if a 300 s snapshot holds the lock for more than 20 ms or audio
glitches during a dump. Cost: RAM briefly doubles during a dump.

### D20 · DVR and recording run together, one recording at a time (settles Q5)
The `Tee` feeds the DVR and at most one recording (manual or session) from a single
encode. `ToggleRecording` stops whatever is recording. Keeping them exclusive would save
nothing, because the DVR keeps buffering for free.

### D21 · DVR resize is a brief writer pause (settles Q6)
A profile switch resizes the buffer under the recorder mutex, and the pause is logged.
Profiles are sticky, so resizes only happen when the tracked game changes. If the
logged pause gets near 100 ms (10 dropped frames at 60 fps is 166 ms and trips the
watchdog), allocate and page-lock outside the lock, then swap. No double buffering: it
would double peak RAM.

### D22 · Editor playback: keyframe thumbnails and an external player (settles Q7)
The timeline shows keyframe thumbnails made by `ffmpeg.exe`
(`-skip_frame nokey`), cached by file. "Preview" runs `ffplay -ss … -t …` from next to
`ffmpeg.exe`, falling back to the default player. No libmpv (a heavy runtime DLL and an
installer concern) and no `egui_video` (it links FFmpeg as a library, which breaks D3).
Cost: no frame-accurate in-app scrubbing. Our cuts snap to keyframes anyway (D5).

### D23 · Protected content: back off only on a real failure (settles Q2)
WGC raises no event when protected (DRM) content appears or goes away. On
2026-10-05, a Netflix tab in Brave on an NVIDIA RTX 5000 didn't interrupt capture at
all: frames kept arriving and the video was recorded. A browser using hardware DRM
(e.g. Edge with PlayReady) may instead draw the area black; either way WGC keeps
delivering frames. So `GpuVideoSource` restarts capture with `Backoff::hdcp()` only
when starting capture actually fails with access denied
(`SourceError::Protected`), and there is no placeholder-frame generator. The
repeat-last-frame ticker keeps the stream continuous whenever WGC goes quiet.
Encoders may refuse `CODECAPI_AVEncMPVDefaultBPictureCount`; NVIDIA does, but its
low-latency mode still emits only I and P frames. Check `pict_type` with ffprobe on
each new GPU vendor, because the muxer relies on no B-frames (D6).

### D24 · v0.1 captures the primary monitor in SDR at a fixed size (narrows Q8)
`CaptureTarget::PrimaryMonitor`, BGRA8, with DWM converting HDR to SDR. The encode
size is fixed when capture starts (rounded down to even); later sizes are scaled and
letterboxed by the video processor, so the encoder and the stream never restart.
Following the game to another monitor waits for the Phase 4 profile engine; real HDR
(FP16 capture, 10-bit HEVC/AV1) comes after v0.1.

### D25 · WASAPI through windows-rs, not the `wasapi` crate
Mic and game-audio capture call WASAPI directly through the `windows` crate. That
avoids a second windows-rs version in the build, keeps the audio loop's no-allocation
rule under our control (the `wasapi` crate reads into a `VecDeque`), and keeps every
line explainable. Cost: about 300 more lines in `zc-audio`, including the process-
loopback activation, whose `PROPVARIANT` must be wrapped in `ManuallyDrop` (see the
plan README's pitfalls).

### D26 · Audio tracks are per source, and editable (extends D9)
Besides the focused game and the mic, ZeroClip can record chosen apps (Discord,
Spotify, a browser…) as their **own tracks**: one process-loopback client in include
mode per app, all running at once. (D9's limit was about *exclude* mode, which takes
one process tree per client; many include clients side by side are fine.) Each
track keeps its own AAC stream, so the editor can set a volume, mute or solo each
one afterwards. The original file keeps every track; "Share" exports one mixed
track, because browsers and Discord play only the first audio track of an MP4.
Costs: `TrackId` stops being a fixed three-value enum (a video track plus up to 6
audio tracks); about 24 KB/s of RAM per extra track in the DVR buffer; an app that
isn't running yet gets a silent track until it starts; export re-encodes audio
(never video).

### D27 · Audio is hybrid include-mode: a chosen set of apps, each its own track (settles Q1; builds on D9 and D26)
Confirmed on hardware (2026-10-05: a browser's process tree captured by pid, in sync
with the mic). Windows' process loopback takes one process tree per client, so an
"ignore these apps" list can't be expressed, but any number of "include this app"
clients can run side by side. So audio is configured as a list of tracks, each one
either the focused game, the microphone, or a named app (`Discord.exe`,
`Spotify.exe`…), and **each track can be switched on or off** (an off track is not
recorded at all; the editor can also mute a recorded one, D26). **Defaults:** the
focused-game track on, and Discord and Spotify pre-listed and on, with the mic off.
An app that isn't running yet records silence until it starts. Costs: up to 6 audio
tracks (D26), and audio from a process outside every listed tree (a launcher's helper
process, a UWP game hosted by `ApplicationFrameHost.exe`) is missed.

### D28 · PolyForm Noncommercial 1.0.0, plus permission to make money with clips (supersedes D16)
Anyone may download, use, study, change and share ZeroClip for free, but nobody may sell
it or software built from its code. One added permission lets anyone use it for paid
content (monetized streams and videos, client work), which PolyForm's "personal use"
wording might otherwise leave in doubt. Copyright notices name the handle `0xtikaa`, not
a real name. All dependencies are permissive (MIT, Apache-2.0, Zlib, Unicode) or offer a
permissive option; `option-ext` is MPL-2.0, which only covers its own files. ffmpeg stays
a separate `ffmpeg.exe` process (D3), so its LGPL/GPL build never mixes with this
license. Costs: ZeroClip is source-available, not "open source" by the OSI definition,
and GitHub lists the license as "Other". Copies taken while the repo was public under
GPL (2026-10-08, about two hours) keep their GPL rights for that version.

### D29 · DVR snapshots copy in short locked chunks (revisits D19)
P3.17 measured the one-shot copy under the recorder lock: 115–135 ms for a 60 s window,
0.7–1.0 s for 300 s, against D19's 20 ms limit. The audio capture buffer is 100 ms, so
even a default save would drop audio and video. Instead of shared segments, the snapshot
now plans under the lock (packet metadata and absolute byte positions only), allocates
and pre-faults the copy outside it, then copies a few MiB per lock hold, oldest first.
Every ring counts the bytes it has ever written, so a packet the writer has overwritten
in the meantime is detected, not copied; the clip then starts at the next keyframe. The
2 s of headroom `DvrConfig::for_window` adds makes that rare. Cost: a save takes about
as long as before in total, but no hold is longer than a few milliseconds. Measured after
P3.17b (release build, 20 Mbps): longest single hold 0.99 ms for a 60 s window (89 ms in
total) and 1.24 ms for 300 s (470 ms in total), down from 115–135 ms and about 1 s.

### D30 · Audio track 1 is a live Master mix; the per-source tracks are for editing (extends D26, D27)
Found in the Phase 3 hardware test: Windows' player, browsers and Discord play only the
first audio track, so the mic (track 2) was "missing" everywhere but VLC. Now the first
audio track is **Master**, a live mix of every enabled source (game, mic, app tracks),
encoded once more as AAC. The per-source tracks follow it unchanged, for the editor (D26).
Layout: `Audio(0)` Master, `Audio(1)` Game, `Audio(2)` Microphone, `Audio(3..)` app tracks;
`MAX_AUDIO_TRACKS` goes from 6 to 7, so four app tracks still fit. The mixer works on one
48 kHz sample timeline taken from each source's QPC-aligned PTS, mixes 200 ms behind real
time so a late source still lands in place, treats a missing source as silence, and
soft-clips the sum. Each source's mix gain is a setting (default 0 dB). Costs: one more
AAC encode (about 1% of a core); Master trails the side tracks by 200 ms. A DVR save cuts
every track at the moment of the press and waits 300 ms before copying, so the clip ends at
the press with Master complete. A recording's Master ends about 200 ms before its video
(cutting per track at stop isn't built). Mix gains apply at the next start, and a source's
volume in Master is fixed at record time (rebalance from the side tracks in the editor,
P5.14).

### D31 · Capture mode: game window by default, whole screen optional (supersedes the primary-monitor part of D24)
ideo.capture_mode is game (default) or screen. In **Game** mode ZeroClip records only the
window of a game that has a profile (CaptureTarget::Window), and only while that game has focus.
Nothing from the desktop is captured. When another app takes focus, the video and audio sources stop,
but the DVR buffer and the input thread keep running: hotkeys work, and a save cuts a clip from the
kept buffer. Focusing the same game again resumes into the same buffer. Focusing a different profiled
game clears the buffer first, so a clip never mixes two games. ZeroClip's own windows and explorer.exe
(taskbar, Start menu) don't pause capture. **Screen** mode is D24's behaviour: ideo.monitor, from start.
Costs: the buffer keeps footage from before and after a pause, so a clip can jump in time where the game
was away; each resume starts a new encoder; a window resize during a pause doesn't clear the buffer (only
a different game does); the mic and app tracks run only while a profiled game is captured; on Windows 10
the game-audio fallback is whole-system loopback (D9), so the desktop's sound can reach the game track.
The DVR ring is still sized at startup from the default profile, so a profile window longer than that is
capped by the ring until P4.6 adds Recorder::resize.

### D32 · DVR buffer storage: RAM, disk or hybrid, sized to free memory (amends D4, D21)
`dvr.storage` is `ram`, `disk` or `hybrid`. **RAM** is D4 as built: pre-allocated, page-locked.
**Disk** writes each finished GOP to a fixed-size ring file on a drive the user picks (default: a
non-system drive if present); only the in-flight GOP stays in RAM. **Hybrid** keeps the newest
`dvr.ram_secs` (default 20) in RAM and spills older GOPs to the ring file, so a long window costs
about 50-100 MB of RAM. A dump reads RAM first, then the ring, and writes oldest first, so it no
longer doubles RAM. The RAM part is capped at a share of *available* memory read at startup
(`GlobalMemoryStatusEx`, default 5% of total); a window that doesn't fit is shortened and the UI says
so, and the cost per window length is shown next to the setting. Under `QueryMemoryResourceNotification`
low-memory the RAM part shrinks and page-locking is dropped. Default is hybrid with a 60 s window.
Costs: continuous writes (about 9 GB/hour at 20 Mbps, 22 GB/hour at 50 Mbps) wear the drive and can
contend with game asset streaming (pacing from P3.23-25 applies); a ring file is overwritten in place,
never grown; a crash leaves the ring on disk, which P4.11 rescue can read; clips older than the RAM
part are slower to dump than RAM-only.
The ring defaults to a different drive than the recordings folder when one exists, so session writes and ring writes don't compete.
