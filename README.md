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

## Status: Phase 3 — save hardware, BIOS HLE, frontend, external suites ✅

- **Save hardware**: SRAM (32 KiB) and Flash 64K/128K with the full
  command state machine (unlock sequence, AND-semantics programming,
  chip/sector erase, ID mode, 128K bank switching), selected by the
  ROM's embedded save-type string. Byte-wide bus rules are exact:
  halfword/word stores to the save region land the single byte the
  chip would see, selected by the unaligned address bits.
- **Save-state format v2**: adds DEVICES (pending IRQ, timer
  counters/reloads, PPU timing) and SAVEMEM (chip kind, bank, full
  contents) sections. The loader still accepts v1 files.
- **BIOS HLE** (when no BIOS dump is loaded, activated at cartridge
  boot): SoftReset, RegisterRamReset, Halt/IntrWait/VBlankIntrWait
  (CPU genuinely parks while devices advance), Div, Sqrt, CpuSet,
  CpuFastSet, LZ77 decompression. With a user BIOS dump, the real
  BIOS runs instead.
- **Linux frontend** (`crates/frontend`, minifb): window at 4x scale,
  keyboard → GBA keys, `.sav` loaded at boot and rewritten whenever
  the game saves. `emu-frontend <game.gba> [save.sav]`.
- **External acceptance — jsmolka/gba-tests: ALL FIVE SUITES PASS**
  (arm ≈550 tests, thumb, memory, flash64, flash128; see
  `tests-roms/`). Getting there flushed out a dozen real CPU/bus
  quirks, each now covered and written up in
  `docs/hardware-notes.md`: MSR-immediate decode, user-bank
  save/restore across FIQ round trips, ROR-by-zero carry, PC reads
  at +12 for register shifts and STR/STM, CMP-with-Rd=15 restoring
  CPSR from SPSR, load writeback winning over Rd==Rn, OAM byte-store
  ignore, palette/VRAM byte duplication, GamePak mirroring, and
  empty-register-list block transfers.
- **Still open** (Phase 4 accuracy work): APU/sound, affine BGs,
  windows/blend/mosaic, cycle-exact timing, EEPROM saves, more
  test-ROM families (ppu/sound suites). FOMT itself needs your own
  ROM dump to boot-test — the core is ready for it.

## Phase 2 — minimum boot system

- **Timers 0–3** with prescalers, cascade, reload-on-enable, overflow
  IRQs. **IRQ path end-to-end**: device → IF (write-one-to-clear,
  intercepted in the bus so CPU acks can't be confused with device
  flags) → IE/IME gating → CPU exception; without a BIOS the core
  emulates the BIOS dispatcher contract via the handler pointer at
  `0x03007FFC`.
- **DMA 0–3**: immediate + VBlank + HBlank start timing, inc/dec/fixed
  addressing, 16/32-bit units, completion IRQ, auto-clear of the
  enable bit for one-shot transfers.
- **PPU**: real scanline timing (1232×228, DISPSTAT/VCOUNT live),
  Mode 0 tile backgrounds (4/8bpp, flips, scroll, 512 sizes, priority),
  Mode 3/4 bitmaps, basic OBJ sprites (1D mapping). `run_frame_cpu()`
  is the frontend frame loop.
- **Boot**: `boot_cartridge()` reproduces the BIOS handoff state
  (stacks, SYS mode, entry 0x08000000); `boot_bios()` runs a
  user-supplied BIOS dump from address 0.
- **Verified by 8 Phase 2 tests** (38 total): timer IRQ handler
  counting + return-to-main, prescaler rate, DMA3 word copy, Mode 3
  pixels, Mode 0 tile pixels, OBJ over BG, ~1 VBlank IRQ per frame,
  BIOS handoff state.
- **Honest limits**: rendering is per-scanline (mid-line effects
  approximate), no affine/windows/blend/mosaic yet, OBJ priority vs
  BG layers is simplified, save-state v1 does not yet snapshot the
  new device state (format v2 bump lands with Phase 3 save hardware).

## Phase 1 — CPU interpreter

- **ARM7TDMI interpreter** (`crates/gba-core/src/cpu/`): complete ARM
  state (data processing + barrel shifter, multiply incl. long,
  MRS/MSR, single & halfword/signed load-store, block transfer, B/BL,
  BX, SWP, SWI exceptions) and complete THUMB state (formats 1–19),
  with banked registers per mode and SPSR handling.
- **Verified by 16 program tests** (`tests/cpu_phase1.rs`): hand-built
  ARM/THUMB programs — flag behaviour, shifter carry, unaligned load
  rotation inputs, UMULL/SMLAL, loops, fib(10), block copy, MRS/MSR
  round-trip, SWI mode entry, THUMB BL/BX interworking, PUSH/POP.
  Full suite: 30 tests, zero warnings.
- **Not yet**: external test ROMs (ARMWrestler, jsmolka gba-tests)
  report through the PPU, so they join as acceptance in Phase 2;
  cycle counts are approximations until Phase 4.

## Phase 0 — foundation

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
