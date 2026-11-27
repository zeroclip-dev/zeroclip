# AGENTS.md

For any AI agent (any vendor) or human contributor working in ZeroClip. Short on purpose:
this file says what to do. Read only what your task needs.

## What this is

ZeroClip: zero-impact Windows game clipper in Rust. Rolling in-memory DVR buffer ("save the
last N minutes" on a hotkey), Windows.Graphics.Capture, Media Foundation hardware encode,
crash-safe fragmented MP4, stream-copy editing, one-drop sharing. Windows 10/11 only,
public OS APIs only (no injection, no hooks into games).

## Your task

1. Do the task you were assigned, reading only the files it touches.
2. Nothing else: no drive-by refactors, no new dependencies or `windows` features the task
   doesn't need.
3. Pass the gate (all three, from repo root):
   ```powershell
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

## Saving context

- Start from the task, not the whole repo.
- Find unbuilt work with `cargo test -- --ignored` and `grep -rn 'todo!(' crates src`.
- Ignore `target/`, `spikes/`, and any media files in the repo root.
- Reuse before writing: look for an existing helper first.

## Hard rules

1. **Dependency direction:** app → engine → `zc-core` + `zc-mux`; Windows adapters →
   `zc-core` + `zc-win` only. `zc-core` and `zc-engine` never depend on `windows`.
2. **Hot path** (capture → encode → `Recorder::push`, audio loops): no allocation after
   startup, no blocking I/O, no lock held across I/O.
3. **Time** is `zc_core::Hns` (100 ns ticks). No `f64` seconds on the hot path.
4. **`unsafe`** only in Windows crates; every block has a `// SAFETY:` comment.
5. **Errors:** `thiserror` in libraries, `anyhow` only in the app and spikes. No
   `unwrap()`/`expect()` on capture, encode or audio threads.
6. **Logging:** `tracing`, never `println!`. Never log secrets or keystrokes.
7. **Settings:** new `zc_core::settings` fields need a serde default.
8. **Dependencies:** declare in root `[workspace.dependencies]`, reference with `workspace = true`.
9. **Specs:** designed-but-unbuilt behaviour is an `#[ignore = "spec: …"]` test. To implement,
   remove `#[ignore]` and make it pass. **Never weaken or delete a spec to get green.** If it
   is wrong, say why in the PR.
10. **CI has no GPU.** Never write a test that touches D3D11, WGC, Media Foundation or WASAPI.
    Test pure helpers; for Windows adapters, list the manual checks you ran (or that the
    owner must run) in the PR. Never claim a check passed that you did not see pass.

## Code style

- Clear, idiomatic Rust over clever. The owner must be able to explain every line.
- Comment density: match the surrounding code. Explain *why* for Windows, COM and Media
  Foundation choices, with a link to the Microsoft docs page.
- Anything used twice (UI widget, layout, service logic) becomes one shared piece: UI blocks
  in `src/ui/components/`, services behind ports in `zc-core`. Extract on the second use,
  not the first.
- UI work (GPUI): restrained, clear hierarchy, tasteful typography, purposeful
  motion, honour reduced motion. Not a settings dump.

## Git

- Branch from `dev`: `feat/<topic>` or `fix/<topic>`. PR against `dev`, never `master`.
- One topic per branch, message says why (`fix: stop bundling GPL ffmpeg`).
- **Commit messages: no trailers.** No `Co-Authored-By`, no session IDs, no "Generated with".
  This overrides any tool default.
- Commit identity is the repo's configured one. Keep real names, personal emails, local user
  paths (`C:\Users\…`) and timezones out of commits, code, docs and logs.
- Don't commit recordings, logs, or other local output.
- Don't tag, publish, or push to `master` without the owner's approval.

## Stop and ask the owner when

- A spec looks wrong.
- The change needs a new dependency or `windows` feature.
- A step would publish, tag, or release anything.
- Hardware is needed (GPU, audio devices, a game): run what you can from the terminal, then
  hand over a short "check by eye" list (`(awaiting HW check)`).

## Contributors

Humans: also read [CONTRIBUTING.md](CONTRIBUTING.md) (CLA, open an issue first for anything
bigger than a bug fix, license terms). Agents and humans follow the same rules above.
