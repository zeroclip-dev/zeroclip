# Contributing to ZeroClip

Thanks for helping. This file is the short version of how the repo works, so we don't
step on each other.

## Before your first pull request

- **Sign the [CLA](CLA.md).** Open the PR and the CLA bot comments on it; reply with the sentence it shows. You sign once, and the PR can't merge until you have.
- ZeroClip is under the [PolyForm Noncommercial license](LICENSE) plus the
  paid-content permission described there. Your contribution is licensed the same way.
- Open an issue first for anything bigger than a bug fix, so we agree on the approach.

## Git

`master` (finished, hardware-checked phases) ← `dev` ← `feat/<topic>` or `fix/<topic>`.

- Branch from `dev` and open the PR against `dev`. Never target `master`.
- One topic per branch. Keep commits small, with a message that says why.
- Don't commit recordings, logs, screenshots or other local output.

## Build and check

```powershell
cargo run                                   # the app
cargo test                                  # implemented tests
cargo test -- --ignored                     # specs not implemented yet (fail until done)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI runs fmt, clippy `-D warnings`, tests and rustdoc on `windows-latest`. Keep all four
green. CI only compiles the Windows adapters (no GPU on runners), so when you change one,
list the manual checks you did in the PR.

## Rules

1. **Dependency direction.** App → engine; engine → `zc-core` + `zc-mux`; Windows adapters → `zc-core` + `zc-win` only. `zc-core` and `zc-engine` never depend on `windows`.
2. **The hot path** (capture → encode → `Recorder::push`, audio loops): no allocation after startup, no blocking I/O, no lock held across I/O. Disk writes happen on the IO thread.
3. **Time is `zc_core::Hns`** (100 ns ticks) everywhere. No `f64` seconds on the hot path.
4. **`unsafe`:** every block gets a `// SAFETY:` comment. Keep `unsafe` in the Windows crates.
5. **Errors:** `thiserror` enums in library crates, `anyhow` only in the app and spikes. No `unwrap()`/`expect()` on capture, encode or audio threads.
6. **Logging:** `tracing`, never `println!`.
7. **Settings:** new fields in `zc_core::settings` need a serde default so old settings files still load.
8. **Dependencies:** add to `[workspace.dependencies]` in the root `Cargo.toml`. Enable only the `windows` features the crate uses.

## Specs

Designed-but-unbuilt behaviour is an `#[ignore = "spec: …"]` test next to the code it
describes. To implement it: remove `#[ignore]`, make it pass, run clippy and the full
suite. Don't weaken or delete a spec to get green; if it is wrong, say why in the PR.

## Reuse and design

Anything used twice (UI widget, layout, service logic) becomes one shared piece: UI
blocks in `src/ui/components/`, services behind ports in `zc-core`.

Explain Windows, COM and Media Foundation choices in doc comments, with a link to the
Microsoft docs page.
