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

fn run_once(rom: &[u8], frames: u64, execute: bool) -> (u64, u64) {
    let mut core = Core::new();
    let header = core.load_rom(rom).expect("ROM header should validate");
    println!(
        "loaded: title={:?} game_code={:?} maker={:?} version={} save={:?}",
        header.title, header.game_code, header.maker_code, header.software_version,
        core.bus.save.kind
    );
    if execute {
        // Real ROM: boot like the BIOS handoff and run the CPU.
        if let Ok(sav) = std::fs::read("game.sav") {
            core.load_save_data(&sav);
        }
        core.boot_cartridge();
    }
    let mut host = NullHost::default();
    for _ in 0..frames {
        if execute {
            core.run_frame_cpu(&mut host);
        } else {
            core.run_frame(&mut host);
        }
    }
    (host.frames_seen, core.framebuffer_hash())
}

fn main() {
    if std::env::var("EMU_TRACE").is_ok() {
        let path = std::env::args().nth(1).expect("path");
        let rom = std::fs::read(&path).expect("read rom");
        trace(&rom);
        return;
    }
    let path_arg = std::env::args().nth(1);
    let execute = path_arg.is_some();
    let rom = match &path_arg {
        Some(path) => std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(2);
        }),
        None => synthetic_rom(),
    };

    let frames: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (frames_a, hash_a) = run_once(&rom, frames, execute);
    let (frames_b, hash_b) = run_once(&rom, frames, execute);

    println!("run A: {frames_a} frames, framebuffer hash {hash_a:#018x}");
    println!("run B: {frames_b} frames, framebuffer hash {hash_b:#018x}");
    if hash_a == hash_b {
        println!("determinism: OK (identical hashes)");
    } else {
        eprintln!("determinism: FAILED");
        std::process::exit(1);
    }

    // Optional: dump the final frame of a fresh run as a binary PPM
    // (EMU_DUMP_FRAME=out.ppm) for visual inspection of test ROMs.
    if let Ok(out) = std::env::var("EMU_DUMP_FRAME") {
        let mut core = Core::new();
        core.load_rom(&rom).expect("reload for dump");
        if execute {
            core.boot_cartridge();
        }
        let mut host = NullHost::default();
        for _ in 0..frames {
            if execute {
                core.run_frame_cpu(&mut host);
            } else {
                core.run_frame(&mut host);
            }
        }
        let mut ppm = Vec::with_capacity(15 + 240 * 160 * 3);
        ppm.extend_from_slice(b"P6\n240 160\n255\n");
        for px in core.framebuffer.iter() {
            ppm.push((px >> 16) as u8);
            ppm.push((px >> 8) as u8);
            ppm.push(*px as u8);
        }
        std::fs::write(&out, ppm).expect("write ppm");
        println!("frame dumped to {out}");
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

// Debug entry: EMU_TRACE=1 runs the ROM instruction by instruction and
// reports where the PC settles. Not part of the normal harness flow.
#[allow(dead_code)]
fn trace(rom: &[u8]) {
    let mut core = Core::new();
    core.load_rom(rom).expect("load");
    core.boot_cartridge();
    let mut last_pc = 0u32;
    let mut same = 0u64;
    let mut verbose = std::env::var("EMU_TRACE_FIRST").is_ok();
    let mut history: std::collections::VecDeque<(u64, u32, u32, u32)> = std::collections::VecDeque::new();
    for i in 0..40_000_000u64 {
        if verbose && i < 160 {
            println!("step {i}: pc={:#010x} r12={:#x} cpsr={:#010x}", core.cpu.pc(), core.cpu.reg(12), core.cpu.cpsr());
        }
        let prev_pc = core.cpu.pc();
        let instr_word = core.bus.read32(prev_pc & !3);
        history.push_back((i, prev_pc, instr_word, core.cpu.reg(13)));
        if history.len() > 48 { history.pop_front(); }
        core.step_instruction();
        let pc = core.cpu.pc();
        if prev_pc == 0x0800_1d70 {
            println!("EVAL REACHED at step {i}: r12={} mode={:#x} sp={:#010x}", core.cpu.reg(12), core.cpu.mode(), core.cpu.reg(13));
        }
        if std::env::var("EMU_TRACE_MODE").is_ok() && core.cpu.mode() == 0x11 && prev_pc >= 0x0800_0000 {
            println!("IN FIQ at step {i}: pc={prev_pc:#010x} instr={instr_word:#010x} sp={:#010x} lr={:#010x}", core.cpu.reg(13), core.cpu.reg(14));
        }
        let sp_now = core.cpu.reg(13);
        if verbose && !(0x0300_0000..0x0300_8000).contains(&sp_now) {
            println!("SP CORRUPT at step {i}: after pc={prev_pc:#010x} instr={instr_word:#010x} sp={sp_now:#010x} r0={:#x} r1={:#x} r2={:#x} r3={:#x} r4={:#x}", core.cpu.reg(0), core.cpu.reg(1), core.cpu.reg(2), core.cpu.reg(3), core.cpu.reg(4));
            verbose = false;
        }
        if verbose && pc < 0x0200_0000 && prev_pc >= 0x0200_0000 {
            println!("LEFT VALID MEMORY at step {i}: {prev_pc:#010x} -> {pc:#010x} r14={:#010x} sp={:#010x} cpsr={:#010x}", core.cpu.reg(14), core.cpu.reg(13), core.cpu.cpsr());
            for (si, sp_pc, iw, sp) in &history {
                println!("  hist step {si}: pc={sp_pc:#010x} instr={iw:#010x} sp={sp:#010x}");
            }
        }
        if pc == last_pc {
            same += 1;
        } else {
            last_pc = pc;
            same = 0;
        }
        if i % 500_000 == 0 || same > 200_000 {
            println!(
                "step {i}: pc={pc:#010x} mode={:#x} cpsr={:#010x} dispstat={:#x} dispcnt={:#x} ie={:#x} if={:#x} ime={:#x} handler_ptr={:#010x}",
                core.cpu.mode(),
                core.cpu.cpsr(),
                core.bus.read16(0x0400_0004),
                core.bus.read16(0x0400_0000),
                core.bus.read16(0x0400_0200),
                core.bus.read16(0x0400_0202),
                core.bus.read16(0x0400_0208),
                core.bus.read32(0x0300_7FFC),
            );
            if same > 200_000 {
                println!("PC settled at {pc:#010x} — dumping surrounding state and stopping");
                break;
            }
        }
    }
}
