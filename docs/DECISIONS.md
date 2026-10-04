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

### D16 · GPL-3.0-or-later
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
