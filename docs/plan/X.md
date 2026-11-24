# Portfolio polish (any time)

Generated from the planning inventory (2026-10-04) and edited by hand. See [README](README.md) for how to work a card.

**Done when:** the repo is public with a green CI badge.

## Checklist

Tick a box when the card is done. Write `(awaiting HW check)` after the title while 0xtikaa still has to verify it on hardware.

- [x] X.2 · GitHub Sponsors: .github/FUNDING.yml
- [ ] X.3 · Publish the repo + CI badge in the README

## Cards

### X.2 · GitHub Sponsors: .github/FUNDING.yml

Size **S** · no hardware needed · depends on nothing

**Files:**
- `.github/FUNDING.yml`
- `docs/ROADMAP.md`

**Steps**

Ask 0xtikaa for his GitHub username first (not in the repo). File contents: `github: [<username>]`. Sponsors itself must be enabled on his GitHub profile (owner action). Tick the ROADMAP polish box.

**Verify**

After pushing: the repo page shows a Sponsor button.

---

### X.3 · Publish the repo + CI badge in the README

Size **S** · no hardware needed · depends on F.1

**Files:**
- `README.md`
- `docs/ROADMAP.md`

**Steps**

Owner publishes the repo (needs owner/repo names). Under the README title add `[![CI](https://github.com/<owner>/<repo>/actions/workflows/ci.yml/badge.svg)](https://github.com/<owner>/<repo>/actions/workflows/ci.yml)`. Tick "Publish the repo; add the CI badge to the README".

**Verify**

The badge renders on GitHub and links to the workflow.

---

### X.4 � Single-instance guard

Size **S** � no hardware needed to build � depends on nothing

Two running copies would both open the disk-ring DVR file, and one would truncate the other's buffer (data loss).

**Files:**
- `crates/zc-win/src/single_instance.rs` (new, the only `unsafe`), `crates/zc-win/src/lib.rs`
- `src/main.rs`
- `docs/ROADMAP.md`

**Reuse:** `zc_win::error::WinResultExt` for the error type; the existing `windows` features (no `Cargo.toml` change).

**Steps**

1. `zc_win::single_instance::acquire(name)`: `CreateMutexW` on a `Local\` name (per session, so fast user switching doesn't block). `ERROR_ALREADY_EXISTS` means another copy runs: close our handle, return `Ok(None)`. Else return the `InstanceGuard`, whose `Drop` closes the handle.
2. `main`: acquire right after logging starts. `None`: log a warning, say so on stderr, exit with status 0. An error from `acquire` is logged and startup continues. Hold the guard for the whole of `main`.
3. Unit test: a uniquely named mutex is refused the second time and free again after the first guard drops.

**Verify**

Unit test passes. Owner check: start two copies, the second exits at once; kill the first, a new copy starts.

(A visible message box for the refused copy needs a small `MessageBoxW` helper in `zc-win` (the feature is already enabled) and a decision; the release build has no console, so today the user sees only the log line.)
