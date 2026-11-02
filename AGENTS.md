# AGENTS.md

For any AI agent (any vendor) or human contributor working in ZeroClip. Short on purpose:
this file says what to do and where to look. It does not copy the docs. Read only what your
task needs.

## What this is

ZeroClip: zero-impact Windows game clipper in Rust. Rolling in-memory DVR buffer ("save the
last N minutes" on a hotkey), Windows.Graphics.Capture, Media Foundation hardware encode,
crash-safe fragmented MP4, stream-copy editing, one-drop sharing. Windows 10/11 only,
public OS APIs only (no injection, no hooks into games).

## Your task

1. Take the **first unchecked card** in `docs/plan/` (order: `README.md` table), or the card
   you were assigned. The card names the files, specs and pitfalls. Read the card, then only
   the files it lists.
2. Stay in the current phase (`docs/ROADMAP.md`, top). Later ideas go in the roadmap, not in code.
3. Do the card. Nothing else: no drive-by refactors, no new dependencies or `windows`
   features the card doesn't name.
4. Pass the gate (all three, from repo root):
   ```powershell
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
5. Tick the card's box in its `docs/plan/P*.md` and the matching box in `docs/ROADMAP.md`,
   in the same change. Update `docs/ARCHITECTURE.md` if the structure changed.

## Saving context

- Start from the card, not the whole repo. Never read `docs/DESIGN.md` (29 KB) or a whole
  `docs/plan/P*.md` (up to 100 KB) top to bottom: grep for the card ID or the `DESIGN §N`
  cited in the code.
- `docs/ARCHITECTURE.md` answers "where does this go?". Check it before creating files.
- Find unbuilt work with `cargo test -- --ignored` and `grep -rn 'todo!(' crates src`.
- Ignore `target/`, `spikes/`, and any media files in the repo root.
- Reuse before writing: each card has a "Reuse" line listing what already exists.

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
   is wrong, say why and update DESIGN/DECISIONS deliberately.
10. **Decisions** in `docs/DECISIONS.md` are settled; supersede with a new entry, never edit
    history. Unsettled items are in `docs/OPEN_QUESTIONS.md`: ask before deciding.
11. **CI has no GPU.** Never write a test that touches D3D11, WGC, Media Foundation or WASAPI.
    Test pure helpers; for Windows adapters, list the manual checks you ran (or that the
    owner must run) in the PR. Never tick a "done when" you did not see pass.

Cross-cutting pitfalls (COM leaks, `Send` on MF types, `#[expect(dead_code)]`, `PROPVARIANT`
drops, ffmpeg flags, SQLite): `docs/plan/README.md` → "Cross-cutting pitfalls". Skim once.

## Code style

- Clear, idiomatic Rust over clever. The owner must be able to explain every line.
- Comment density: match the surrounding code. Explain *why* for Windows, COM and Media
  Foundation choices, with a link to the Microsoft docs page.
- Anything used twice (UI widget, layout, service logic) becomes one shared piece: UI blocks
  in `src/ui/components/`, services behind ports in `zc-core`. Extract on the second use,
  not the first.
- UI work (egui, Phase 4+): restrained, clear hierarchy, tasteful typography, purposeful
  motion, honour reduced motion. Not a settings dump.

## Git

- Branch from `dev`: `feat/<topic>` or `fix/<topic>`. PR against `dev`, never `master`.
- One topic per branch, one commit per card, message names the card and says why
  (`P4.2: profile editor tabs`).
- **Commit messages: no trailers.** No `Co-Authored-By`, no session IDs, no "Generated with".
  This overrides any tool default.
- Commit identity is the repo's configured one. Keep real names, personal emails, local user
  paths (`C:\Users\…`) and timezones out of commits, code, docs and logs.
- Don't commit recordings, logs, screenshots, or other local output.
- Don't tag, publish, or push to `master` without the owner's approval.

## Stop and ask the owner when

- A spec looks wrong, or the card is "blocked by" an open question.
- The change needs a new dependency or `windows` feature the card doesn't name.
- A step would publish, tag, or release anything.
- Hardware is needed (GPU, audio devices, a game): run what you can from the terminal, then
  hand over a short "check by eye" list and leave the box unticked
  (`(awaiting HW check)`).

## Contributors

Humans: also read [CONTRIBUTING.md](CONTRIBUTING.md) (CLA, open an issue first for anything
bigger than a bug fix, license terms). Agents and humans follow the same rules above.
