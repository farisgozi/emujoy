# emu-core

A clean-room emulation core built from scratch — first target **Game Boy
Advance** (acceptance game: *Harvest Moon: Friends of Mineral Town*),
then **PlayStation 1** (acceptance game: *Harvest Moon: Back to Nature*),
from one codebase to **Linux (Arch)**, **Android (APK)**, and **Web
(WASM)**.

This project deliberately does **not** fork or bundle an existing core.
Existing cores (mGBA, NanoBoyAdvance, Beetle PSX HW, PCSX-ReARMed,
DuckStation, …) were *audited* for their known problems — accuracy gaps,
documented CVEs in file parsers, versionless save states, global state,
input-latency afterthoughts — and this design answers those problems
from the first commit instead of patching them later.

## Status: Phase 0 — foundation (skeleton)

What exists and is tested:

- **Workspace**: `gba-core` (no_std core), `emu-parser` (hardened file
  parsers, `unsafe` forbidden), `headless` (test harness binary).
- **Scheduler** — one global cycle clock; events fire in time order.
  Nothing in the core counts time privately.
- **Bus skeleton** — GBATEK address decoding (EWRAM/IWRAM/IO/palette/
  VRAM/OAM/ROM/SRAM) with mirroring; timing arrives in Phase 2/4.
- **No globals** — `Core` owns all state by value; two instances in one
  process are independent (acceptance test).
- **Save state format v1** — magic, schema version, per-section
  length + CRC32, ROM hash; total validation before anything is
  applied; corruption or a foreign ROM is a hard rejection.
- **Parsers that treat files as hostile** — GBA header validation
  (fixed value + complement check), CUE parser with a hard path-length
  limit (the CVE-2025-9809 lesson), mutation smoke tests proving no
  panics on garbage input.
- **Host trait** — the core never touches files, sockets, threads or a
  clock. Frontends provide video/audio/input/virtual-files.

What does **not** exist yet: CPU execution (Phase 1), PPU/APU
(Phases 2–3), timing accuracy (Phase 4), JIT (Phase 5+), PS1 (Phase 6+).

## Run it

```sh
cargo test                    # unit tests incl. acceptance tests
cargo run -p headless         # synthetic ROM, 60 frames x2, determinism check
cargo run -p headless -- path/to/game.gba   # header-check a real ROM
```

## Clean-room rules

Code is written only from (a) vendor documentation (ARM7TDMI TRM),
(b) public hardware references rewritten into our own tests first
(GBATEK; for PS1: OpenBIOS behaviour — PSX-SPX is **not** clean-room
material and is treated as hypothesis only), and (c) homebrew/test ROMs
we run ourselves. No commercial ROM or BIOS is ever committed; the
`fuzz/corpus` directory holds only synthetic inputs. Decision log lives
in `docs/decisions/`, per-subsystem hardware notes in
`docs/hardware-notes.md`.

## Roadmap (estimates: solo, full-time; part-time ≈ ×2.5)

| Phase | Goal | Cumulative |
|---|---|---|
| 0 | Foundation (this skeleton) | 2–4 weeks |
| 1 | ARM/THUMB CPU interpreter — ARMWrestler & FuzzARM 100% | 1.5–3 months |
| 2 | Minimum boot system — first commercial game title screen | 2–4 months |
| 3 | FOMT playable end-to-end, save-survival tested | 4–7 months |
| 4 | GBA v1: timing accuracy + Linux/Android/Web frontends | 6–12 months |
| 5 | Optional JIT (W^X, interpreter fallback, differential-tested) | +2–4 months |
| 6–8 | PS1 core → BTN playable | 15–30 months total |

## License

Dual-licensed under MIT OR Apache-2.0 — chosen on day one and not
changing mid-project (see `docs/decisions/0001-rust-stack.md`).
