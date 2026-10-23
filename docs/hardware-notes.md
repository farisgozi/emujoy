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

<!-- Phase 1 starts here: CPU entries from the ARM TRM. -->
