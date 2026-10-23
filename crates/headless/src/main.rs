//! Headless harness — the Phase 0 acceptance runner.
//!
//! Usage:
//!   cargo run -p headless              # synthetic ROM, 60 frames, twice
//!   cargo run -p headless -- game.gba  # a real ROM file (header check
//!                                      # + frames; CPU arrives in Phase 1)
//!
//! It proves the plumbing end to end: header parsing, ROM loading,
//! scheduler-driven frames, host presentation, and — the important
//! part — determinism: two runs from identical state must produce
//! identical frame hashes.

use gba_core::{Core, NullHost};

fn synthetic_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x200];
    rom[0xA0..0xA6].copy_from_slice(b"PANEN!");
    rom[0xAC..0xB0].copy_from_slice(b"TEST");
    rom[0xB0..0xB2].copy_from_slice(b"01");
    rom[0xB2] = 0x96;
    let mut sum: u8 = 0x19;
    for &b in &rom[0xA0..=0xBC] {
        sum = sum.wrapping_add(b);
    }
    rom[0xBD] = 0u8.wrapping_sub(sum);
    rom
}

fn run_once(rom: &[u8], frames: u64) -> (u64, u64) {
    let mut core = Core::new();
    let header = core.load_rom(rom).expect("ROM header should validate");
    println!(
        "loaded: title={:?} game_code={:?} maker={:?} version={}",
        header.title, header.game_code, header.maker_code, header.software_version
    );
    let mut host = NullHost::default();
    for _ in 0..frames {
        core.run_frame(&mut host);
    }
    (host.frames_seen, core.framebuffer_hash())
}

fn main() {
    let rom = match std::env::args().nth(1) {
        Some(path) => std::fs::read(&path).unwrap_or_else(|e| {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(2);
        }),
        None => synthetic_rom(),
    };

    let (frames_a, hash_a) = run_once(&rom, 60);
    let (frames_b, hash_b) = run_once(&rom, 60);

    println!("run A: {frames_a} frames, framebuffer hash {hash_a:#018x}");
    println!("run B: {frames_b} frames, framebuffer hash {hash_b:#018x}");
    if hash_a == hash_b {
        println!("determinism: OK (identical hashes)");
    } else {
        eprintln!("determinism: FAILED");
        std::process::exit(1);
    }

    // Save-state smoke check through the public API.
    let mut core = Core::new();
    core.load_rom(&rom).expect("reload");
    let blob = gba_core::savestate::save(&core);
    let mut restored = Core::new();
    restored.load_rom(&rom).expect("reload 2");
    gba_core::savestate::load(&mut restored, &blob).expect("save state should load");
    println!("save state: OK ({} bytes, format v{})", blob.len(), gba_core::savestate::FORMAT_VERSION);
}
