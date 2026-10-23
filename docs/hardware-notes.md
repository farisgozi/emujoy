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
