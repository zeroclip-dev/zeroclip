# Open questions

Things to verify or decide before building on them. Several shape the UI, so resolving
them early avoids rework. When one is settled, move it to DECISIONS.md.

| # | Question | Decide by | Notes |
|---|---|---|---|
| Q1 | **Process loopback: include or exclude mode?** Can several exclusions be expressed at all? | Phase 2 | Decides whether the audio UI says "capture from" or "ignore these". DESIGN §6 |
| Q2 | **HDCP resume signal.** Does WGC expose any usable event when protected content goes away, or is backoff polling the only option? | Phase 1 | Backoff is spec'd (`zc_core::backoff`). DESIGN §3 |
| Q3 | **fMP4 or MKV** as the default container? | Phase 3 | MKV tooling is more forgiving; fMP4 plays in browsers without a remux. DESIGN §12 |
| Q4 | **Snapshot memory strategy.** Copy the window out of the DVR buffer, or share fixed-size segments by reference so a dump needs no second copy? | Phase 3 | `DvrBuffer::snapshot` copies for now; a 5-minute window is ~750 MB. |
| Q5 | **DVR and session recording at the same time**, or mutually exclusive? | Phase 3 | The design assumes both (the `Tee`); exclusive would make Phase 3 simpler. |
| Q6 | **Buffer resize under load.** Brief writer pause, or double-buffer the allocation? | Phase 4 | DESIGN §7 |
| Q7 | **Playback: `egui_video`, libmpv, or an external player?** | Phase 5 | libmpv is a heavy runtime dependency and affects the installer. DESIGN §2, §9 |
| Q8 | **Multi-monitor and HDR.** Which display is captured when the game moves? What happens to HDR content on an SDR encode path? | Phase 1–2 | Tone mapping isn't free. DESIGN §12 |
| Q9 | **Will the YouTube compliance audit pass?** | Phase 6 | Out of our control; BYO credentials ship regardless. DESIGN §10 |
| Q10 | **Discord's current size limit.** Can it be read from the API, or only learned from a 413? | Phase 6 | The webhook starts from a guess and learns. DESIGN §10 |
