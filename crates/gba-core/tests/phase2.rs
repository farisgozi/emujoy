//! Phase 2 acceptance tests: boot setup, timers + IRQ dispatch, DMA,
//! and PPU rendering (bitmap modes, tile mode, OBJ).
//!
//! As in Phase 1, programs are tiny hand-assembled ARM blobs in IWRAM.
//! Hardware setup done by the test harness through the bus stands in
//! for the setup code a real game would run itself.

use gba_core::cpu::{BIT_F, BIT_I, MODE_IRQ, MODE_SYS};
use gba_core::{Core, NullHost};

const BASE: u32 = 0x0300_0000;
const HANDLER: u32 = 0x0300_1000;
const COUNTER_CELL: u32 = 0x0200_0100;

fn dp_imm(op: u32, s: u32, rn: u32, rd: u32, rot: u32, imm8: u32) -> u32 {
    0xE000_0000 | (1 << 25) | (op << 21) | (s << 20) | (rn << 16) | (rd << 12) | (rot << 8) | imm8
}
fn dp_reg(op: u32, s: u32, rn: u32, rd: u32, shift_imm: u32, shift_ty: u32, rm: u32) -> u32 {
    0xE000_0000 | (op << 21) | (s << 20) | (rn << 16) | (rd << 12) | (shift_imm << 7) | (shift_ty << 5) | rm
}
const OP_ADD: u32 = 4;
const OP_MOV: u32 = 13;
fn mov_imm(rd: u32, rot: u32, imm8: u32) -> u32 { dp_imm(OP_MOV, 0, 0, rd, rot, imm8) }
fn ls_imm(load: bool, byte: bool, rn: u32, rd: u32, off: u32) -> u32 {
    0xE400_0000 | (1 << 24) | (1 << 23) | ((byte as u32) << 22) | ((load as u32) << 20) | (rn << 16) | (rd << 12) | off
}
fn strh_imm(rn: u32, rd: u32, imm: u32) -> u32 {
    0xE1C0_00B0 | (rn << 16) | (rd << 12) | ((imm & 0xF0) << 4) | (imm & 0xF)
}
const HALT: u32 = 0xEAFF_FFFE; // B .

/// IRQ handler blob at HANDLER: increments the word at COUNTER_CELL,
/// acknowledges `ack_bit` in IF via a halfword store, returns with
/// SUBS PC, LR, #4.
fn write_handler(core: &mut Core, ack_bit: u32) {
    // Note on the ack store: word stores mask their address to 4-byte
    // alignment on ARM7, so IF (0x...202) can only be acked with a
    // halfword store — and STRH's offset caps at 0xFF, so the full IF
    // address is built in R2 first. This mirrors what real games do.
    let code = [
        mov_imm(0, 4, 2),                                  // R0 = 0x0200_0000
        ls_imm(true, false, 0, 1, 0x100),                  // LDR R1, [R0, #0x100]
        dp_imm(OP_ADD, 0, 1, 1, 0, 1),                     // ADD R1, R1, #1
        ls_imm(false, false, 0, 1, 0x100),                 // STR R1, [R0, #0x100]
        mov_imm(2, 4, 4),                                  // R2 = 0x0400_0000
        dp_imm(OP_ADD, 0, 2, 2, 12, 2),                    // R2 += 0x200 (2 ror 24)
        dp_imm(OP_ADD, 0, 2, 2, 0, 2),                     // R2 += 2 -> 0x0400_0202
        mov_imm(3, 0, ack_bit),                            // R3 = ack mask
        strh_imm(2, 3, 0),                                 // STRH R3, [R2] (IF ack)
        0xE25E_F004,                                       // SUBS PC, LR, #4
    ];
    for (i, w) in code.iter().enumerate() {
        core.bus.write32(HANDLER + (i as u32) * 4, *w);
    }
    core.bus.write32(0x0300_7FFC, HANDLER);
}

/// CPU spinning at BASE with IRQ/FIQ stacks set and interrupts open.
fn spinning_core() -> Core {
    let mut core = Core::new();
    core.bus.write32(BASE, HALT);
    core.cpu.set_cpsr_full(MODE_IRQ | BIT_I | BIT_F);
    core.cpu.set_reg(13, 0x0300_7FA0);
    core.cpu.set_cpsr_full(MODE_SYS);
    core.cpu.set_reg(13, 0x0300_7F00);
    core.cpu.set_pc(BASE);
    core
}

#[test]
fn timer_overflow_raises_irq_and_handler_counts() {
    let mut core = spinning_core();
    write_handler(&mut core, 8); // ack Timer0 bit
    core.bus.write16(0x0400_0200, 1 << 3); // IE: Timer0
    core.bus.write16(0x0400_0208, 1); // IME on
    core.bus.write16(0x0400_0100, 0xFF00); // TM0 reload
    core.bus.write16(0x0400_0102, 0x80 | 0x40); // enable + IRQ + prescaler 1

    let mut returned_to_main = false;
    for _ in 0..4000 {
        core.step_instruction();
        if core.bus.read32(COUNTER_CELL) > 0 && core.cpu.pc() == BASE {
            returned_to_main = true;
        }
    }
    let hits = core.bus.read32(COUNTER_CELL);
    assert!(hits >= 3, "timer IRQ handler should have run repeatedly, got {hits}");
    assert!(returned_to_main, "CPU must return to the main loop after IRQs");
}

#[test]
fn timer_prescaler_divides_rate() {
    let mut core = spinning_core();
    core.bus.write16(0x0400_0100, 0); // reload 0
    core.bus.write16(0x0400_0102, 0x80 | 3); // enable, prescaler /1024
    // One main-loop step is ~3 cycles; run 400 steps (~1200 cycles).
    for _ in 0..400 {
        core.step_instruction();
    }
    let counter = core.bus.read16(0x0400_0100);
    assert!(counter >= 1 && counter <= 3, "counter should be ~1 tick per 1024 cycles, got {counter}");
}

#[test]
fn dma3_immediate_word_copy() {
    let mut core = Core::new();
    core.bus.write32(0x0200_0000, 0x1111_1111);
    core.bus.write32(0x0200_0004, 0x2222_2222);
    let code = [
        mov_imm(0, 4, 4),                       // R0 = 0x0400_0000
        mov_imm(1, 4, 2),                       // R1 = 0x0200_0000
        ls_imm(false, false, 0, 1, 0xD4),       // STR R1, [R0,#0xD4]  DMA3 SAD
        dp_imm(OP_ADD, 0, 1, 1, 12, 1),         // ADD R1, R1, #0x100 (1 ror 24)
        ls_imm(false, false, 0, 1, 0xD8),       // STR R1, [R0,#0xD8]  DMA3 DAD
        mov_imm(2, 0, 2),
        strh_imm(0, 2, 0xDC),                   // STRH R2, [R0,#0xDC] count = 2
        mov_imm(2, 0, 0x84),
        dp_reg(OP_MOV, 0, 0, 2, 8, 0, 2),       // MOV R2, R2, LSL #8 -> 0x8400
        strh_imm(0, 2, 0xDE),                   // STRH R2, [R0,#0xDE] enable|32-bit
        HALT,
    ];
    for (i, w) in code.iter().enumerate() {
        core.bus.write32(BASE + (i as u32) * 4, *w);
    }
    core.cpu.set_pc(BASE);
    core.cpu.set_reg(13, 0x0300_7F00);
    for _ in 0..40 {
        core.step_instruction();
    }
    assert_eq!(core.bus.read32(0x0200_0100), 0x1111_1111);
    assert_eq!(core.bus.read32(0x0200_0104), 0x2222_2222);
    let ctrl = core.bus.read16(0x0400_00DE);
    assert_eq!(ctrl & (1 << 15), 0, "enable bit must clear after immediate DMA");
}

#[test]
fn mode3_bitmap_reaches_framebuffer() {
    let mut core = spinning_core();
    core.bus.write16(0x0400_0000, 0x0003 | (1 << 10)); // Mode 3 + BG2
    core.bus.write16(0x0600_0000, 0x001F); // (0,0) red
    let last = (159 * 240 + 239) * 2;
    core.bus.write16(0x0600_0000 + last, 0x03E0); // (239,159) green
    let mut host = NullHost::default();
    core.run_frame_cpu(&mut host);
    assert_eq!(core.framebuffer[0], 0xFFFF_0000);
    assert_eq!(core.framebuffer[159 * 240 + 239], 0xFF00_FF00);
    assert_eq!(core.framebuffer[1], 0xFF00_0000, "untouched VRAM is colour 0");
    assert_eq!(core.frames_presented, 1);
}

#[test]
fn mode0_tile_background_renders() {
    let mut core = spinning_core();
    core.bus.write16(0x0400_0000, 0x0100); // Mode 0 + BG0 enable
    core.bus.write16(0x0400_0008, 0x0100); // BG0CNT: char base 0, screen base 1
    core.bus.write16(0x0500_0002, 0x7C00); // BG palette #1 = blue
    // Tile 1: row 0 all nibble 1.
    for i in 0..4 {
        core.bus.write8(0x0600_0020 + i, 0x11);
    }
    // Tilemap entry 0 -> tile 1.
    core.bus.write16(0x0600_0800, 1);
    let mut host = NullHost::default();
    core.run_frame_cpu(&mut host);
    assert_eq!(core.framebuffer[0], 0xFF00_00FF, "first pixel is tile1 colour 1 (blue)");
    assert_eq!(core.framebuffer[7], 0xFF00_00FF);
    assert_eq!(core.framebuffer[8], 0xFF00_0000, "next tile is empty -> backdrop");
}

#[test]
fn obj_sprite_renders_over_background() {
    let mut core = spinning_core();
    core.bus.write16(0x0400_0000, 0x0100 | (1 << 12) | (1 << 6)); // BG0 + OBJ + 1D
    core.bus.write16(0x0500_0204, 0x03E0); // OBJ palette #2 = green
    // OBJ tile 0 (at VRAM 0x06010000): all nibble 2.
    for i in 0..32 {
        core.bus.write16(0x0601_0000 + i * 2, 0x2222);
    }
    // OAM entry 0: y=0, x=16, 8x8, tile 0.
    core.bus.write16(0x0700_0000, 0);
    core.bus.write16(0x0700_0002, 16);
    core.bus.write16(0x0700_0004, 0);
    let mut host = NullHost::default();
    core.run_frame_cpu(&mut host);
    assert_eq!(core.framebuffer[16], 0xFF00_FF00, "sprite pixel at x=16 is green");
    assert_eq!(core.framebuffer[15], 0xFF00_0000);
}

#[test]
fn vblank_irq_fires_once_per_frame() {
    let mut core = spinning_core();
    write_handler(&mut core, 1); // ack VBlank bit
    core.bus.write16(0x0400_0004, 1 << 3); // DISPSTAT: VBlank IRQ enable
    core.bus.write16(0x0400_0200, 1); // IE: VBlank
    core.bus.write16(0x0400_0208, 1); // IME on
    let mut host = NullHost::default();
    for _ in 0..3 {
        core.run_frame_cpu(&mut host);
    }
    let hits = core.bus.read32(COUNTER_CELL);
    assert!(hits >= 2, "VBlank handler should run about once per frame, got {hits}");
}

#[test]
fn boot_cartridge_sets_bios_handoff_state() {
    let mut core = Core::new();
    core.boot_cartridge();
    assert_eq!(core.cpu.pc(), 0x0800_0000);
    assert_eq!(core.cpu.reg(13), 0x0300_7F00, "user/system stack from BIOS handoff");
    assert_eq!(core.cpu.mode(), 0x1F, "cartridge entry runs in SYS mode");
}


