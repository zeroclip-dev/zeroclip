# Foundations and Phase 0: the capture → encode spike

Generated from the planning inventory (2026-10-04) and edited by hand. See [README](README.md) for how to work a card.

**Done when:** a 10-second hardware-encoded MP4 exists, plays in VLC and a browser, and Task Manager shows the GPU's Video Encode engine busy with no unexpected CPU copy overhead.

## Checklist

Tick a box when the card is done. Write `(awaiting HW check)` after the title while 0xtikaa still has to verify it on hardware.

- [x] F.0 · First build: get the untouched workspace to a green baseline
- [x] F.1 · Add the CI workflow (.github/workflows/ci.yml): fmt, clippy -D warnings, tests, rustdoc on windows-latest
- [x] P0.1 · Spike steps 1-2: hardware D3D11 device with multithread protection; GraphicsCaptureItem for the primary monitor (HW)
- [x] P0.2 · Spike step 3: Media Foundation sink writer with a DXGI device manager and hardware transforms (HW)
- [x] P0.3 · Spike step 4: WGC free-threaded frame pool, GPU copy, samples into the sink writer for 10 s (HW)
- [ ] P0.4 · Spike: try HEVC and AV1, check Phase 0 done-when, record findings, tick roadmap (HW)

Area notes (pitfalls, open-question background): see [P1.md](P1.md#notes-for-this-area).

## Cards

### F.0 · First build: get the untouched workspace to a green baseline

Size **S** · no hardware needed · depends on nothing

**Files:**
- (whatever the commands below report)

**Steps**

The workspace has never been compiled on this machine (there is no `target/`). Get it green before touching any phase.
1. `cargo build --workspace`. The first build downloads and compiles everything, which takes a while.
2. Run `cargo fmt --all -- --check`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo test --workspace`, then `$env:RUSTDOCFLAGS='-D warnings'; cargo doc --workspace --no-deps`.
3. Fix only what fails. Suspected failures are fields that are written but never read and have no scaffold attribute:
   - `crates/zc-engine/src/synthetic.rs:35` (`faults`)
   - `crates/zc-engine/src/synthetic.rs:37` (`realtime`)
   - `crates/zc-input/src/source.rs:7` (`thread`)
   Use the existing scaffold style `#[expect(dead_code, reason = "scaffold: Phase N …")]`, never `#[allow]`.
4. Don't implement any `todo!`. `cargo test -- --ignored` is expected to fail; it is the progress meter.
5. If nothing needed fixing, say so and tick the card without a commit.

**Verify**

All four commands green: fmt --check, clippy -D warnings, test, doc with RUSTDOCFLAGS=-D warnings.

---

### F.1 · Add the CI workflow (.github/workflows/ci.yml): fmt, clippy -D warnings, tests, rustdoc on windows-latest

Size **S** · no hardware needed · depends on nothing

**Files:**
- `.github/workflows/ci.yml`
- `docs/ROADMAP.md`

**Reuse:**
- rust-toolchain.toml (channel stable, rustfmt+clippy)
- rustfmt.toml
- workspace lints in Cargo.toml ([workspace.lints])

**Steps**

Context: `.github/` does not exist at all (checked), yet CLAUDE.md and ARCHITECTURE.md already describe CI as if it runs. The repo has one branch, `master`, and no remote.
1. Create `.github/workflows/ci.yml` (2-space YAML indent per .editorconfig):
```yaml
name: CI
on: [push, pull_request]
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true
env:
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: 0
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - run: cargo test --workspace --locked
      - run: cargo doc --workspace --no-deps --locked
        env:
          RUSTDOCFLAGS: -D warnings
```
   `--workspace` also builds `spikes/*`, which `default-members` leaves out. That's intended: the spike must keep compiling. rust-toolchain.toml pins `stable` with rustfmt and clippy, which matches the action.
2. Run the same four commands locally in PowerShell (`$env:RUSTDOCFLAGS='-D warnings'` before `cargo doc`). Fix anything already broken before committing the workflow. Likely suspects are rustdoc intra-doc links and `#[expect]` attributes that are no longer fulfilled. Don't add `#[allow]`s to get green.
3. Tick `- [ ] Add the CI workflow…` in docs/ROADMAP.md (Foundations).
4. Rule for every later task: CI runners have NO GPU. Never add a test that creates a D3D11 device or touches WGC/MF at runtime. Only pure helpers get unit tests.

**Verify**

Locally: cargo fmt --all -- --check; cargo clippy --workspace --all-targets --locked -- -D warnings; cargo test --workspace --locked; $env:RUSTDOCFLAGS='-D warnings'; cargo doc --workspace --no-deps --locked. Once a GitHub remote exists, push and confirm the job is green.

---

### P0.1 · Spike steps 1-2: hardware D3D11 device with multithread protection; GraphicsCaptureItem for the primary monitor

Size **S** · **hardware check: 0xtikaa** · depends on nothing

**Files:**
- `spikes/phase0-capture-encode/src/main.rs`

**`todo!` sites removed:**
- spikes/phase0-capture-encode/src/main.rs:79 todo!("Step 1: create the D3D11 device (see the doc comment)")
- spikes/phase0-capture-encode/src/main.rs:88 todo!("Step 2: GraphicsCaptureItem for the primary monitor")

**Reuse:**
- zc_win::com::Apartment (crates/zc-win/src/com.rs), already created in main()
- zc_win::mf::MediaFoundation (crates/zc-win/src/mf.rs), already created in main()

**Steps**

The only description of 'steps 1-4' is the doc comments in main.rs. There is no README or notes file. Throwaway code: anyhow errors, `?` everywhere.
Step 1, `create_device()`:
1. `let mut device = None; let mut context = None;` then, inside one `unsafe {}` with a `// SAFETY:` comment (out-params are valid `Option`s that live through the call): `D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]), D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?`.
2. `let device = device.context("D3D11CreateDevice returned no device")?;` (anyhow::Context on Option) and the same for `context`.
3. `device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true)` in an unsafe block. Ignore the returned BOOL, which is the previous state. ID3D11Multithread has the same IID as the ID3D10Multithread named in the doc comment, so this code moves unchanged into zc-video, which won't need the Direct3D10 feature.
Step 2, `capture_item()`:
4. `let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };`
5. `let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;` then `let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };`.
Imports: windows::core::{Interface, factory}; Win32::Foundation::{HMODULE, POINT}; Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1}; Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, ID3D11Multithread}; Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY}; Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop. Every feature needed is already in the spike's Cargo.toml.
Pitfall: do NOT `use zc_win::error::WinResultExt` in the spike. Its `.context()` clashes with `anyhow::Context`. windows::core::Error converts into anyhow through `?` on its own.

**Verify**

cargo clippy -p spike-phase0 --all-targets -- -D warnings. On a Windows machine with a GPU: `cargo run -p spike-phase0` prints 'Capturing the primary monitor at WxH…' with your real resolution, then panics at the Step 3 todo!, which is expected at this point.

---

### P0.2 · Spike step 3: Media Foundation sink writer with a DXGI device manager and hardware transforms

Size **M** · **hardware check: 0xtikaa** · depends on P0.1

**Files:**
- `spikes/phase0-capture-encode/src/main.rs`

**`todo!` sites removed:**
- spikes/phase0-capture-encode/src/main.rs:109 todo!("Step 3: configure the sink writer")

**Reuse:**
- constants OUTPUT/FPS/BITRATE_BPS at the top of main.rs

**Steps**

Implement `sink_writer(device, width, height)`. Remove the `let _ = (...)` placeholder. COM calls are unsafe, so group them into a few unsafe blocks, each with a `// SAFETY:` comment.
1. `let mut token = 0u32; let mut manager = None; MFCreateDXGIDeviceManager(&mut token, &mut manager)?; let manager = manager.context("…")?; manager.ResetDevice(device, token)?;`
2. `let mut attrs = None; MFCreateAttributes(&mut attrs, 3)?; let attrs = attrs.context("…")?;` then `attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?; attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?; attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;`
3. `let writer = MFCreateSinkWriterFromURL(&HSTRING::from(OUTPUT), None, &attrs)?;` (`None` works through `Param<T> for Option<&T>`).
4. Output type: `let out = MFCreateMediaType()?;` then SetGUID MF_MT_MAJOR_TYPE=MFMediaType_Video, MF_MT_SUBTYPE=MFVideoFormat_H264; SetUINT32 MF_MT_AVG_BITRATE=BITRATE_BPS and MF_MT_INTERLACE_MODE=MFVideoInterlace_Progressive.0 as u32; SetUINT64 MF_MT_FRAME_SIZE=(u64::from(width) << 32) | u64::from(height), MF_MT_FRAME_RATE=(u64::from(FPS) << 32) | 1 and MF_MT_PIXEL_ASPECT_RATIO=(1 << 32) | 1. Then `let stream = writer.AddStream(&out)?;`.
5. Input type: same attributes except MF_MT_SUBTYPE=MFVideoFormat_ARGB32, which is WGC's B8G8R8A8, and no bitrate. `writer.SetInputMediaType(stream, &input, None)?; writer.BeginWriting()?;`. If SetInputMediaType fails with MF_E_INVALIDMEDIATYPE, add MF_MT_DEFAULT_STRIDE = width*4 on the input type.
6. Return `(writer, stream)`.
Imports come from windows::Win32::Media::MediaFoundation::* (feature already enabled) and windows::core::HSTRING.

**Verify**

cargo clippy -p spike-phase0 --all-targets -- -D warnings. Hardware: `cargo run -p spike-phase0` gets past step 3 and panics at the Step 4 todo!. An empty or header-only spike-phase0.mp4 may appear.

---

### P0.3 · Spike step 4: WGC free-threaded frame pool, GPU copy, samples into the sink writer for 10 s

Size **M** · **hardware check: 0xtikaa** · depends on P0.2

**Files:**
- `spikes/phase0-capture-encode/src/main.rs`

**`todo!` sites removed:**
- spikes/phase0-capture-encode/src/main.rs:138 todo!("Step 4: capture and write frames")

**Reuse:**
- create_device / capture_item / sink_writer from P0.1-P0.2

**Steps**

Implement `record(...)`. Remove the placeholder `let _ = (...)`.
1. WinRT device: `let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)? }.cast()?;`
2. `let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size()?)?;`
3. `let (tx, rx) = std::sync::mpsc::channel::<(ID3D11Texture2D, i64)>();` The writer (IMFSinkWriter) is NOT Send in windows-rs 0.62, so samples get written on the main thread. ID3D11Device and ID3D11DeviceContext ARE Send+Sync, so clone them into the handler.
4. Handler: `TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| { ... Ok(()) })`. Note it takes `Fn + Send + 'static`, not FnMut. Inside: `let Some(pool) = pool.as_ref() else { return Ok(()) };` → `let frame = pool.TryGetNextFrame()?;` → `let time = frame.SystemRelativeTime()?.Duration;` → `let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;` → `let src: ID3D11Texture2D = unsafe { access.GetInterface()? };` → `src.GetDesc(&mut desc)`, set desc.Usage=D3D11_USAGE_DEFAULT, BindFlags=(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags=0, MiscFlags=0 → `device.CreateTexture2D(&desc, None, Some(&mut copy))` → `context.CopyResource(©, &src)` → `let _ = tx.send((copy, time));`. Creating a texture per frame is fine in a spike; Phase 1 uses a ring.
5. `pool.FrameArrived(&handler)?; let session = pool.CreateCaptureSession(item)?; session.StartCapture()?;`
6. Main loop until `Instant::now() >= start + DURATION`: `rx.recv_timeout(100ms)`. For each texture: `let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &tex, 0, false)?; let len = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?; buffer.SetCurrentLength(len)?; let sample = MFCreateSample()?; sample.AddBuffer(&buffer)?; sample.SetSampleTime(time - first_time)?; sample.SetSampleDuration(10_000_000 / i64::from(FPS))?; writer.WriteSample(stream, &sample)?;`. Count frames and sum the intervals between `time`s.
7. `session.Close()?; pool.Close()?;` Return Stats { frames, average_interval_ms }.
8. Imports: Foundation::TypedEventHandler, IInspectable (windows::core), Graphics::DirectX::DirectXPixelFormat, Graphics::DirectX::Direct3D11::IDirect3DDevice, Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess}, Win32::Graphics::Dxgi::IDXGIDevice. Every feature is already declared.
WGC may deliver at the display refresh rate (144/240 Hz). That's fine for the spike because the sink writer follows real timestamps.

**Verify**

cargo clippy -p spike-phase0 --all-targets -- -D warnings. Hardware: `cargo run -p spike-phase0 --release` prints frames and MB written. spike-phase0.mp4 should be about 10 s and play in VLC and in Chrome/Edge (drag the file in). While it runs, Task Manager → Performance → GPU should show 'Video Encode' busy and CPU near idle.

---

### P0.4 · Spike: try HEVC and AV1, check Phase 0 done-when, record findings, tick roadmap

Size **S** · **hardware check: 0xtikaa** · depends on P0.3

**Files:**
- `spikes/phase0-capture-encode/src/main.rs`
- `.gitignore`
- `docs/ROADMAP.md`
- `CLAUDE.md`
- `README.md`

**Steps**

1. Read the codec from `std::env::args().nth(1)`: "h264" (default) | "hevc" | "av1" → MFVideoFormat_H264 / MFVideoFormat_HEVC / MFVideoFormat_AV1. Write to `spike-phase0-<codec>.mp4`. Change the OUTPUT const into a fn or format the name in main.
2. In .gitignore, change `/spike-phase0.mp4` to `/spike-phase0*.mp4`.
3. Run each codec. Record the results in a `//! ## Findings` section of the spike's module doc: GPU and driver, which codecs worked, average frame interval, CPU %, whether Video Encode was busy, and whether the file plays in VLC and in a browser. HEVC MP4 plays in Chrome/Edge on Windows with hardware decode, but not in Firefox. AV1 needs RTX 40 / Arc / RDNA3.
4. If the done-when holds, tick both Phase 0 boxes in docs/ROADMAP.md. Change CLAUDE.md's 'Current phase' line to 'Phase 1, capture core (crates/zc-video)' and README's status blockquote to match.

**Verify**

cargo clippy -p spike-phase0 --all-targets -- -D warnings. Hardware: `cargo run -p spike-phase0 --release -- hevc` (and `-- av1` if supported). Phase 0 done-when: the 10 s MP4 plays in VLC and a browser, and Task Manager shows the Video Encode engine busy with low CPU.
