# ZeroClip

A game clipper for Windows, written in Rust. ZeroClip keeps the last few minutes of your
gameplay in a rolling buffer and saves them when you press a hotkey, so you never have to
decide to record in advance.

> **Status: early (v0.1).** It works on the author's machine and is still being tested on
> other hardware. Expect rough edges. Known gaps are listed below.

## Features

Working now:

- **DVR buffer.** Press a hotkey (default Alt+F10) to save the last N minutes as a clip.
  Manual recording is Alt+F9; there are also screenshot and bookmark hotkeys.
- **Hardware encoding** through Media Foundation, with HEVC or H.264. Capture uses Windows.Graphics.Capture, a public Windows API.
- **Audio tracks.** A master mix, game audio, your microphone (always on or push-to-talk) and
  optional per-app tracks. Per-app capture needs Windows 11.
- **Profiles per game.** Buffer length and other settings follow the game that has focus.
- **Crash-safe files.** Recordings are fragmented MP4, repaired on the next start if a crash cut
  them short. If a game dies, the buffer is saved and you are asked whether to keep it.
- **Overlay toasts and failure popups** (click-through, not captured in your recording).
- **Clips library** with game filters, a screenshot viewer and a built-in video player.
- **Editor.** Reorder pieces, leave gaps, cut a range, split, and set the volume or mute of each
  audio track per piece. Exports are stream copies, so they are fast.
- **Tray, start with Windows, hotkey capture** (click a field and press the keys).
- **Updates.** ZeroClip checks GitHub Releases at startup and once a day (can be turned off in
  Settings). Updating is always your click, never while recording or saving, and the download
  is checked against the release's SHA-256 before it is installed.

Planned, not built:

- Sharing and one-drop upload to Discord and YouTube. The `zc-destinations` crate is a stub.
- MKV output.

Known limits: browsers and Discord play only the first audio track (the master mix); the game
and mic tracks are heard in players such as VLC. Game audio follows the focused window.

## Install

1. Download `ZeroClip-vX.Y.Z-windows-x64.zip` (or the bare `.exe`) and `SHA256SUMS.txt` from
   [Releases](https://github.com/zeroclip-dev/zeroclip/releases).
2. Check the download against the published hash:

   ```powershell
   (Get-FileHash .\ZeroClip-v0.1.0-windows-x64.zip -Algorithm SHA256).Hash.ToLower()
   Select-String 'windows-x64.zip' .\SHA256SUMS.txt
   ```

   The two hex strings must match. Note that this only catches a corrupted download: the hash
   comes from the same release page as the file. The exe is not code-signed yet, so Windows
   SmartScreen may warn on first run. Release builds also carry a GitHub build attestation
   (`gh attestation verify <file> --repo zeroclip-dev/zeroclip`).
3. Unzip anywhere and run `zeroclip.exe`.

### Requirements

- Windows 10 1903 or later (Windows 11 for per-app audio tracks).
- A GPU with a hardware video encoder (recent NVIDIA, AMD or Intel). ZeroClip only uses a
  hardware encoder; which GPUs work has not been tested widely.
- `ffmpeg.exe` for the editor, thumbnails and making clips seekable in other players. Recording
  and saving clips do not need it. ZeroClip looks next to `zeroclip.exe`, then on `PATH`, or use
  the path set in Settings. ffmpeg is not bundled: its builds carry their own (L)GPL terms, so
  install it yourself, for example with `winget install Gyan.FFmpeg`.
- `curl.exe` (ships with Windows 10 1803+) for update checks.

## Build from source

You need Rust 1.95 or newer ([rustup](https://rustup.rs)), the MSVC build tools (the *Desktop
development with C++* workload) and the Windows SDK (its `fxc.exe` is needed by the UI
toolkit's shader build).

```powershell
cargo run --release
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The design is in [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) (short) and
[docs/DESIGN.md](docs/DESIGN.md); crate layout in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md);
what comes next in [docs/ROADMAP.md](docs/ROADMAP.md).

## License

ZeroClip is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE), plus one
extra permission: you may use it to make videos, clips and streams that earn money. You may not
sell ZeroClip, software made from its code, or include either in a commercial product. Read
[LICENSE](LICENSE) for the exact terms; this paragraph is a summary.

## Contributing

Open an issue first for anything bigger than a bug fix. Contributions need a signed
[CLA](CLA.md) (the bot on your pull request walks you through it). See
[CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md).
