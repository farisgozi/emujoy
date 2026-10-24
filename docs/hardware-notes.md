# Hardware notes — per subsystem

One entry per non-trivial behaviour, written BEFORE the code that
implements it. This log is the project's clean-room evidence: every
behaviour names its source and the test that proves our implementation.

Template:

## <Subsystem> — <behaviour>
- Behaviour: (one sentence, in our own words)
- Source: ARM7TDMI TRM DDI 0029G §… / GBATEK §… / own homebrew test …
- Test: (test ROM / unit test name that fails if this regresses)
- Notes: (edge cases, disagreements between sources)

## PPU/APU/saves — Phase 4 field notes

- **BIOS leaves identity affine matrices.** After its logo
  animation, the real BIOS hands off with BG2PA=BG2PD=BG3PA=
  BG3PD=0x100. Software relies on it: the jsmolka suites draw
  their Mode 4 results screen without ever programming the
  matrix, and a zero matrix collapses the bitmap onto texel
  (0,0) — an all-white screen that looks like a renderer bug
  but is a boot-state bug. `boot_cartridge()` reproduces the
  leftover state. Modes 3/4/5 really are matrix-sampled on
  hardware (mGBA agrees); there is no 1:1 shortcut.
- **OBJ VRAM byte stores are ignored** — the Phase 3 bus rule
  bit a Phase 4 unit test that filled sprite tiles with STRB.
  Hardware behaviour; fill OBJ tiles with halfword stores.
- **Sound trigger bits are edges, not levels.** Games rewrite
  CNT_X with the trigger bit set for every note; polling the
  register can never see the second note. The bus records
  trigger writes (high byte of CNT_X, only when bit 7 of the
  written byte is set) into a mask the core drains, mirroring
  the IF write-to-clear intercept.
- **EEPROM address width is not in the bit stream.** A 0
  address bit is indistinguishable from the stop bit, so the
  width (6 bits / 512 B, 14 bits / 8 KiB) is a fixed chip
  property; the core takes it from the loaded save image size,
  which is also how a real cart's fixed chip behaves from the
  game's point of view.
- **Sound DMA register geometry**: CNT_L/CNT_H for DMA1 live at
  0x040000C4/0xC6 (12-byte channel stride from 0xB0). A Phase 4
  test first wrote CNT_H at 0xCA — that's DMA2's SAD high half —
  and the FIFO stayed silent with everything else correct.
- **OBJ wins priority ties against BGs.** The Phase 4 compositor
  originally sorted candidates by (priority, BG0<BG1<BG2<BG3<OBJ)
  and a unit test asserted "BG wins the tie" — both wrong. GBATEK's
  display order at equal priority is OBJ, BG0, BG1, BG2, BG3.
  FOMT's title screen proves it in practice: the subtitle plank is
  a prio-0 OBJ strip and the cast heads behind it are prio-0 BG3;
  with the wrong order the heads paint over the subtitle text.
  Found by diffing a reference photo + per-layer dumps + raw VRAM
  tile decode — the tile data and palettes were correct all along;
  only the compositor's tie-break was inverted.

## CPU — pipeline-visible PC (Phase 1)
- Behaviour: reading R15 during execution yields instruction address
  +8 (ARM) / +4 (THUMB), aligned down to 2 in THUMB for PC-relative
  calculations.
- Source: ARM7TDMI TRM DDI 0029G (3-stage pipeline description).
- Test: `cpu_phase1.rs` — THUMB PC-relative paths via BL/BX programs;
  ARM branch targets in loop tests.
- Notes: implementation keeps `regs[15]` = next fetch address and adds
  the state-dependent offset on read. Known simplification: R15 as a
  *stored* value in block transfers and as shift operand may differ by
  4 from hardware in edge cases; targeted for the Phase 4 timing pass.

## CPU — mode banking & exceptions (Phase 1)
- Behaviour: FIQ banks R8–R14; SVC/ABT/UND/IRQ bank R13–R14 and each
  owns an SPSR. SWI enters SVC, writes return address to LR_svc and
  the old CPSR to SPSR_svc, clears T, sets I, jumps to 0x08.
- Source: ARM7TDMI TRM (programmer's model, exception entry).
- Test: `arm_swi_enters_supervisor_with_spsr`, `two_instances_are_independent`.

## IRQ dispatch — the BIOS stub contract (found booting FOMT)
When no BIOS dump is loaded, jumping straight to the handler at
0x03007FFC with the raw exception LR corrupts real games: handlers
are written against the *BIOS stub*, which pushes R0-R3, R12, LR on
the IRQ stack, calls the handler with LR pointing back into the
stub, then pops everything and does SUBS PC, LR, #4. FOMT's
handler under the naive contract returned to the interrupted PC in
the wrong state and executed THUMB code as ARM (undefined-exception
slide into unmapped space). The core now emulates the stub: push,
sentinel LR (0x0000_03FC, intercepted before fetch), epilogue pops
and restores CPSR from SPSR.

## BIOS HLE — CpuSet/CpuFastSet count semantics (playable campaign)
GBATEK: the control word's bits 0–20 are a count of *transfer
units* — halfwords for 16-bit CpuSet, **words for CpuFastSet**
(which always moves 32-bit words; the count must be a multiple of
8, but it is NOT a count of 8-word blocks). Bit 24 = fixed source
(fill), bit 26 selects 32-bit units for CpuSet only. Our HLE
originally multiplied the count by 8 and ran 8× too far: FOMT
zero-fills a 0x100-byte field at payload+4 of each 0x104-byte heap
block with ctrl 0x01000040 (fill, 0x40 words), and the overrun
marched past every buffer zeroing the heap block headers — the
game's allocator then returned NULL for a 16-byte request and
FOMT's wrapper waits forever on a NULL failure hook: the
permanent white screen after pressing Start. Found by validating
the game's heap block chain on every allocator entry (the chain
broke exactly where the fills landed) plus a watchpoint on the
corrupted header. Lesson: an HLE that is "close" corrupts state
far from the call site; the symptom (allocator) was three layers
away from the cause (BIOS copy semantics).

## Save states — v3 completeness (playable campaign)
Format v2 silently lost state: the CPU section held only the
active registers + CPSR (banked shadows — including the IRQ
stack pointer — were dropped, so the first IRQ after a load ran
away with SP=0), and the memory section omitted palette RAM,
OAM and the IO page (DISPCNT/registers restored as 0 → black
screen). v3 snapshots the full register file (44 words: active +
user bank + FIQ bank + per-mode pairs + all SPSRs), palette, OAM
and IO, plus DMA edge latches, the BIOS-stub-active flag and the
HLE wait state. v1/v2 files still load (banked state then keeps
the receiving core's values — documented imperfection). Still
approximate: APU channel state (music re-triggers via the game's
engine) and PPU affine internal reference points (resync at the
next frame start).

## CPU — the jsmolka quirk harvest (Phase 3)
Each of these was found by a numbered jsmolka/gba-tests failure and
is covered by that suite passing in full (arm/thumb/memory/flash):
- **MSR immediate decode**: field mask is bits 19..16 of the
  instruction, any value — demanding 0xF there misroutes MSR #imm
  into data processing (arm 001).
- **Mode banking**: USR/SYS R8..R14 live in a user shadow; a FIQ round
  trip must restore them exactly, and privileged modes share R8..R12
  with the user bank (arm psr tests; SP corruption otherwise).
- **ROR by register 0**: value *and* carry pass through; ROR by
  32/64 keeps the value but carry becomes bit 31 (arm 166, thumb 68).
- **PC reads at +12**: when Rm/Rs/Rn is R15 in a data-processing op
  with register-specified shift, and for STR/STM storing R15
  (arm 224/225/356/515). Plain PC reads stay at +8.
- **CMP/TST/TEQ/CMN with Rd=15, S=1**: restores CPSR from SPSR —
  the documented way "bad" compares change the mode (arm 234).
- **Load writeback vs Rd == Rn**: writeback lands first, the loaded
  value wins (arm 360/412).
- **STM base in list (not first)**: stores the writeback value
  (thumb 230). **STM/LDM empty list**: R15 transferred, base
  adjusts 0x40, transfer address computed as if 16 regs (arm 515/530).
- **STM/LDM with S bit**: transfers the *user* bank registers from
  any mode (arm 511).
- **STRH at odd addresses**: aligned halfword store in normal
  memory; single high-byte store in the byte-wide save region
  (arm 407 vs flash 006 — the region decides, not the CPU).
- **Video byte stores**: OAM ignores STRB; palette and BG VRAM
  duplicate the byte into both halfword halves; OBJ VRAM ignores
  (memory video_strb). Byte-store rules must not leak into 16/32-bit
  stores — the bus has a separate raw path for those.
- **GamePak mirror**: the full 32 MiB window mirrors the ROM by
  window offset (memory 007).

## Flash — command protocol (Phase 3)
- Behaviour: writes are ignored unless preceded by the unlock pair
  0xAA@0x5555, 0x55@0x2AAA. Program (0xA0) stores byte with AND
  semantics (only clears bits). Erase (0x80 + fresh unlock) takes
  0x10 = chip (current bank) / 0x30 = 4 KiB sector. ID mode (0x90)
  makes offsets 0/1 read maker/device IDs until 0xF0. Bank switch
  (0xB0 then write bank# at 0x0000) applies to 128 KiB parts only.
- Source: GBATEK (flash ROM).
- Chip identities modelled: Macronix — 64K maker 0xC2 device 0x1C,
  128K maker 0xC2 device 0x09.
- Test: `phase3.rs` — program/bit-clearing, ID mode round trip,
  sector erase isolation, bank independence, 20-cycle save survival.

## Save type detection (Phase 3)
- Behaviour: the ROM embeds an ASCII marker ("FLASH1M_V...",
  "FLASH512_V...", "FLASH_V...", "SRAM_V..."); the chip fitted is
  chosen from it at load, defaulting to SRAM.
- Source: GBATEK (save-type strings).
- Test: `detection_from_rom_strings`.

## IRQ — IF is write-one-to-clear (Phase 2)
- Behaviour: writing 1 bits to IF (0x04000202) clears those flags;
  word stores cannot ack IF because ARM word stores align their
  address down (a STR to 0x...202 lands on 0x...200) — real code
  acks with STRH.
- Source: GBATEK (IF register, ARM addressing behaviour).
- Implementation: the bus intercepts byte writes to 0x04000202/3 into
  a clear-mask the core drains each instruction; the visible IF value
  is driven only by the core's pending bits.
- Test: `phase2.rs` timer/VBlank handlers ack via STRH and return to
  the main loop (an early STR-based handler re-triggered forever,
  which is how this was pinned down).

## IRQ — handler pointer contract without BIOS (Phase 2)
- Behaviour: the real BIOS dispatcher calls the user handler whose
  address the game stores at 0x03007FFC; the handler returns with
  SUBS PC, LR, #4. Without a BIOS dump the core reproduces this
  contract directly (LR/SPSR prepared identically).
- Source: GBATEK (BIOS interrupt handling).
- Test: `timer_overflow_raises_irq_and_handler_counts`,
  `vblank_irq_fires_once_per_frame`.

## Timers — reload on enable (Phase 2)
- Behaviour: writing TMxCNT_L sets the reload value; the counter
  adopts it when the timer is enabled (rising edge) and on overflow,
  not at write time.
- Source: GBATEK (timers).
- Test: `timer_overflow_raises_irq_and_handler_counts` (reload
  0xFF00 → overflow cadence ≈256 cycles), `timer_prescaler_divides_rate`.

## CPU — unaligned word loads (Phase 1)
- Behaviour: LDR from a non-word-aligned address returns the aligned
  word rotated right by (addr & 3) * 8; word stores are aligned down.
- Source: ARM ARM (ARMv4 LDR addressing).
- Test: `arm_load_store_word_and_byte` (byte store into word then
  aligned word load observes both bytes).
