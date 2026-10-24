//! Phase 4 renderer tests: affine backgrounds, windows, colour
//! effects and OBJ priority/affine behaviour. Each test programs the
//! registers exactly as a game would and inspects the framebuffer.

use gba_core::{Core, NullHost};

fn halt_core() -> Core {
    let mut core = Core::new();
    core.bus.write32(0x0200_0000, 0xEAFF_FFFE); // B .
    core.cpu.set_cpsr_full(0x1F); // SYS
    core.cpu.set_reg(13, 0x0300_7F00);
    core.cpu.set_pc(0x0200_0000);
    core
}

fn run_one(core: &mut Core) {
    let mut host = NullHost::default();
    core.run_frame_cpu(&mut host);
}

fn set_pal(core: &mut Core, index: u32, color: u16) {
    core.bus.write16(0x0500_0000 + index * 2, color);
}

/// BG0: screen base 1, tile 1 filled with palette nibble 1.
/// BG1: screen base 2, tile 2 filled with palette nibble 2.
fn two_bg_setup(core: &mut Core, bg0_prio: u16, bg1_prio: u16) {
    for i in 0..32u32 {
        core.bus.write8(0x0600_0020 + i, 0x11); // tile 1
        core.bus.write8(0x0600_0040 + i, 0x22); // tile 2
    }
    for e in 0..32u32 {
        core.bus.write16(0x0600_0800 + e * 2, 1); // BG0 map row 0
        core.bus.write16(0x0600_1000 + e * 2, 2); // BG1 map row 0
    }
    core.bus.write16(0x0400_0008, 0x0100 | bg0_prio); // BG0CNT
    core.bus.write16(0x0400_000A, 0x0200 | bg1_prio); // BG1CNT
    set_pal(core, 1, 0x001F); // red
    set_pal(core, 2, 0x7C00); // blue
}

#[test]
fn affine_bg_identity_renders_tiles() {
    let mut core = halt_core();
    core.bus.write16(0x0400_0000, 0x0002 | (1 << 10)); // Mode 2 + BG2
    core.bus.write16(0x0400_000C, 0x0100); // BG2CNT: screen base 1, size 16x16
    core.bus.write16(0x0400_0020, 0x0100); // BG2PA = 1.0
    core.bus.write16(0x0400_0026, 0x0100); // BG2PD = 1.0
    core.bus.write16(0x0600_0800, 1); // map[0] = tile 1 (halfword store: no byte duplication)
    for i in 0..32u32 {
        core.bus.write16(0x0600_0040 + i * 2, 0x0505); // tile 1: all palette 5
    }
    set_pal(&mut core, 5, 0x03E0); // green
    run_one(&mut core);
    assert_eq!(core.framebuffer[0], 0xFF00_FF00, "affine tile pixel (0,0)");
    assert_eq!(core.framebuffer[7 * 240 + 7], 0xFF00_FF00);
    assert_eq!(core.framebuffer[8], 0xFF00_0000, "tile 0 is transparent -> backdrop (black)");
}

#[test]
fn affine_bg_half_scale_samples_every_other_texel() {
    let mut core = halt_core();
    core.bus.write16(0x0400_0000, 0x0002 | (1 << 10));
    core.bus.write16(0x0400_000C, 0x0100);
    core.bus.write16(0x0400_0020, 0x0080); // PA = 0.5
    core.bus.write16(0x0400_0026, 0x0080); // PD = 0.5
    core.bus.write16(0x0600_0800, 1);
    // Tile 1: texel = x coordinate (0..7) as palette index. Written
    // as packed halfwords — byte stores to BG VRAM duplicate into
    // both halves on hardware (and here).
    for ty in 0..8u32 {
        for pair in 0..4u32 {
            let lo = (pair * 2 + 1) as u16;
            let hi = (pair * 2 + 2) as u16;
            core.bus.write16(0x0600_0040 + ty * 8 + pair * 2, lo | (hi << 8));
        }
    }
    for i in 1..8u16 {
        set_pal(&mut core, u32::from(i), i * 0x20); // distinct greens
    }
    run_one(&mut core);
    // Screen x=2 samples texel 1 (0.5 scale), x=4 samples texel 2.
    let c = |x: usize| core.framebuffer[x];
    assert_ne!(c(2), c(0));
    assert_eq!(c(2), { let v = 2u16 * 0x20; gba_core::ppu::rgb555(v & 0x7FFF) });
    assert_eq!(c(4), { let v = 3u16 * 0x20; gba_core::ppu::rgb555(v & 0x7FFF) });
}

#[test]
fn alpha_blend_mixes_two_backgrounds() {
    let mut core = halt_core();
    two_bg_setup(&mut core, 0, 1);
    core.bus.write16(0x0400_0000, 0x0300); // Mode 0 + BG0 + BG1
    core.bus.write16(0x0400_0050, 0x0241); // BLDCNT: alpha, 1st=BG0, 2nd=BG1
    core.bus.write16(0x0400_0052, 0x0808); // EVA=8, EVB=8
    run_one(&mut core);
    // red (31,0,0) over blue (0,0,31) at 8/16 each -> (15,0,15)
    assert_eq!(core.framebuffer[0], 0xFF7B_007B);
}

#[test]
fn brighten_effect_pushes_toward_white() {
    let mut core = halt_core();
    two_bg_setup(&mut core, 0, 1);
    core.bus.write16(0x0400_0000, 0x0100); // Mode 0 + BG0 only
    core.bus.write16(0x0400_0050, 0x0081); // BLDCNT: brighten, 1st=BG0
    core.bus.write16(0x0400_0054, 0x0010); // EVY=16 (full)
    run_one(&mut core);
    assert_eq!(core.framebuffer[0], 0xFFFF_FFFF);
}

#[test]
fn window_masks_layers_per_region() {
    let mut core = halt_core();
    two_bg_setup(&mut core, 1, 0); // BG1 in front when unmasked
    core.bus.write16(0x0400_0000, 0x0300 | (1 << 13)); // BG0+BG1 + WIN0
    core.bus.write16(0x0400_0040, 120); // WIN0H: x 0..120
    core.bus.write16(0x0400_0044, 160); // WIN0V: y 0..160
    core.bus.write16(0x0400_0048, 0x0001); // WININ: inside = BG0 only
    core.bus.write16(0x0400_004A, 0x0002); // WINOUT: outside = BG1 only
    run_one(&mut core);
    assert_eq!(core.framebuffer[0], 0xFFFF_0000, "inside window: BG0 red");
    assert_eq!(core.framebuffer[200], 0xFF00_00FF, "outside window: BG1 blue");
}

#[test]
fn obj_priority_tie_goes_to_obj_and_affine_obj_renders() {
    let mut core = halt_core();
    two_bg_setup(&mut core, 0, 1);
    core.bus.write16(0x0400_0000, 0x0100 | (1 << 12)); // BG0 + OBJ
    // OBJ0: affine, 8x8 at (0,0), tile 1, priority 0.
    core.bus.write16(0x0700_0000, 0x0100); // attr0: y=0, affine enable
    core.bus.write16(0x0700_0002, 0x0000); // attr1: x=0, group 0, size 0
    core.bus.write16(0x0700_0004, 0x0001); // attr2: tile 1, prio 0
    core.bus.write16(0x0700_0006, 0x0100); // group0 PA = 1.0
    core.bus.write16(0x0700_001E, 0x0100); // group0 PD = 1.0 (offset +30)
    for i in 0..16u32 {
        // (Byte stores to OBJ VRAM are ignored by the hardware —
        // and by this bus — so tile data goes in as halfwords.)
        core.bus.write16(0x0601_0000 + 32 + i * 2, 0x1111); // OBJ tile 1
    }
    set_pal(&mut core, 256 + 1, 0x03E0); // OBJ palette 1 = green
    run_one(&mut core);
    // Same priority 0: the OBJ wins the tie (GBATEK display order at
    // equal priority is OBJ, BG0, BG1, BG2, BG3) — green, not red.
    // (An earlier version of this test asserted the opposite from a
    // misremembered rule; FOMT's title screen proved it wrong: its
    // prio-0 OBJ subtitle plank must cover prio-0 BG3 heads.)
    assert_eq!(core.framebuffer[0], 0xFF00_FF00, "tie at prio 0: OBJ in front");
    // BG0 at priority 1 stays behind the prio-0 sprite as well.
    core.bus.write16(0x0400_0008, 0x0101); // BG0CNT prio 1
    run_one(&mut core);
    assert_eq!(core.framebuffer[0], 0xFF00_FF00, "affine OBJ in front at prio 0 vs BG prio 1");
}
