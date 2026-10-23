# 0001 — Core language: Rust

Date: 2026-10-23 · Status: accepted

## Context
Audit of existing cores (see research, 2026-10-23) scored Rust 35,
C++23 29, Zig 25 across memory safety, interpreter performance, future
JIT, cross-compilation (x86_64 Linux / arm64 Android / wasm32),
emulator ecosystem, tooling, learning curve, and no_std suitability.
Documented parser CVEs (CVE-2025-9809 in CUE parsing, CVE-2025-9136)
are memory-safety bugs — the class Rust removes structurally in safe
code.

## Decision
- Core in **Rust**, `#![no_std]`, as a pure library with its own host
  trait (not libretro's ABI).
- `unsafe` is forbidden in parser/save-state crates and allowed only in
  future, separately-crated JIT and SIMD code, with written justification.
- Renderer (frontend side): **wgpu**. Android UI shell: Kotlin (thin).
- License: MIT OR Apache-2.0, fixed from the first commit.

## Fallback clause
If, after Phase 1, the borrow checker is still consuming more time than
emulation logic, switching the core to C++23 (same architecture: one
owning struct, no globals, sanitizers + libFuzzer) is a rational move,
not a failure. Zig was rejected for this project: pre-1.0 and an empty
emulator ecosystem for a solo developer.
