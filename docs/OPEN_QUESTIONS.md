# Open questions

Things to verify or decide before building on them. Several shape the UI, so resolving
them early avoids rework. When one is settled, move it to DECISIONS.md.

Settled so far: Q2 → D23, Q3 → D18, Q4 → D19, Q5 → D20, Q6 → D21, Q7 → D22.

| # | Question | Decide by | Notes |
|---|---|---|---|
| Q1 | **Process loopback: include or exclude mode?** Can several exclusions be expressed at all? | Phase 2 (P2.21) | D9 already chose include mode; P2.21 confirms it on hardware and closes this. DESIGN §6 |
| Q8 | **Multi-monitor and HDR.** Which display is captured when the game moves to another monitor, and when does real HDR arrive? | Phase 4, after v0.1 | v0.1 is settled by D24 (primary monitor, SDR, fixed size). Open: follow-the-game (Phase 4) and HDR. DESIGN §12 |
| Q9 | **Will the YouTube compliance audit pass?** And are bring-your-own projects locked to private uploads? | Phase 6 (P6.0) | Out of our control; check the private lock before writing any OAuth code. DESIGN §10 |
| Q10 | **Discord's current size limit.** Can it be read from the API, or only learned from a 413? | Phase 6 (P6.4) | The webhook starts from a guess and learns. DESIGN §10 |
