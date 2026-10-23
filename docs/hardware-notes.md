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

## CPU — unaligned word loads (Phase 1)
- Behaviour: LDR from a non-word-aligned address returns the aligned
  word rotated right by (addr & 3) * 8; word stores are aligned down.
- Source: ARM ARM (ARMv4 LDR addressing).
- Test: `arm_load_store_word_and_byte` (byte store into word then
  aligned word load observes both bytes).
