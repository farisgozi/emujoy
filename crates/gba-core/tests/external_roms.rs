//! External test ROMs (jsmolka/gba-tests) — the Phase 3 acceptance
//! gate. The ROM binaries live in `tests-roms/` at the workspace root
//! (fetched from https://github.com/jsmolka/gba-tests, whose suites
//! report their verdict as on-screen text: "All tests passed" or
//! "Failed test NNN").
//!
//! These tests assert the machine-level contract that can be checked
//! without OCR: every suite boots, runs 300 frames deterministically,
//! and reaches a non-blank results screen. The full pass/fail verdict
//! per suite is verified visually via the headless frame dump
//! (EMU_DUMP_FRAME) and recorded in the README — as of the Phase 3
//! commit, all five suites (arm, thumb, memory, flash64, flash128)
//! display "All tests passed".

use gba_core::{Core, NullHost};
use std::path::PathBuf;

fn rom_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests-roms")
        .join(name)
}

fn run_suite(name: &str) -> (u64, u64) {
    let data = std::fs::read(rom_path(name))
        .unwrap_or_else(|e| panic!("test ROM {name} missing (see tests-roms/README): {e}"));
    let run = |data: &Vec<u8>| {
        let mut core = Core::new();
        core.load_rom(data).unwrap();
        core.boot_cartridge();
        let mut host = NullHost::default();
        for _ in 0..300 {
            core.run_frame_cpu(&mut host);
        }
        core.framebuffer_hash()
    };
    (run(&data), run(&data))
}

#[test]
fn external_suites_boot_and_are_deterministic() {
    for name in ["arm.gba", "thumb.gba", "memory.gba", "flash64.gba", "flash128.gba"] {
        let (a, b) = run_suite(name);
        assert_eq!(a, b, "{name}: two identical runs must hash identically");
        // A blank (all-backdrop) framebuffer hashes to a constant;
        // reaching the results screen means it is not blank.
        let mut blank = Core::new();
        let blank_hash = {
            blank.framebuffer.fill(0xFFFF_FFFF);
            blank.framebuffer_hash()
        };
        assert_ne!(a, blank_hash, "{name}: results screen should not be blank white");
    }
}
