# 0002 — No global state; Core is a value

Date: 2026-10-23 · Status: accepted

## Context
Global mutable state in older cores blocks multi-instance use,
run-ahead second-instance, rollback netplay, TAS determinism, and
parallel headless testing. PCSX-Redux's global `System` coordinator is
the documented modern example.

## Decision
`gba_core::Core` owns every subsystem by value (CPU, Scheduler, Bus,
framebuffer). Subsystems communicate through the Scheduler and the bus,
never through globals or cross-pointers. Acceptance test (Phase 0):
two `Core`s in one process, one loaded and run, the other must remain
pristine — `crates/gba-core/src/lib.rs::tests::two_instances_are_independent`.
