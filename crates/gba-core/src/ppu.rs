//! PPU — Phase 2 subset.
//!
//! Timing: 1232 cycles per scanline, 228 lines per frame, 160 visible.
//! Rendering happens one whole scanline at a time when the line starts
//! (mid-line register changes are therefore approximate until the
//! Phase 4 per-dot pass). Supported now:
//! - Mode 0: all four tile backgrounds (4bpp & 8bpp, flips, scroll,
//!   256/512 sizes, priority ordering) + basic OBJ (1D mapping).
//! - Mode 3 & 4: bitmap backgrounds.
//! Not yet: windows, blending, mosaic, affine backgrounds (Mode 0
//! only), OBJ 2D mapping nuances, mid-scanline effects.

use crate::bus::Bus;
use crate::{SCREEN_H, SCREEN_W};

pub const CYCLES_PER_LINE: u64 = 1232;
pub const LINES_PER_FRAME: u16 = 228;
pub const HBLANK_START: u64 = 1006;

pub struct Ppu {
    pub line: u16,
    cycle_in_frame: u64,
    hblank_signalled: bool,
    line_rendered: bool,
    pub frame_ready: bool,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

/// Events the core needs to react to while time advances.
#[derive(Debug, Default, Clone, Copy)]
pub struct PpuEvents {
    pub vblank: bool,
    pub hblank: bool,
    pub vcounter_match: bool,
}

impl Ppu {
    pub fn new() -> Self {
        Self { line: 0, cycle_in_frame: 0, hblank_signalled: false, line_rendered: false, frame_ready: false }
    }

    /// Advance PPU time. Rendering + DISPSTAT/VCOUNT maintenance happen
    /// against the bus; IRQ-raising events are returned for the core.
    pub fn advance(
        &mut self,
        cycles: u64,
        bus: &mut Bus,
        framebuffer: &mut [u32; SCREEN_W * SCREEN_H],
    ) -> PpuEvents {
        let mut events = PpuEvents::default();
        self.cycle_in_frame += cycles;

        // A visible line is drawn when it begins.
        if self.line < SCREEN_H as u16 && !self.line_rendered {
            self.line_rendered = true;
            render_scanline(bus, framebuffer, self.line as usize);
        }

        loop {
            let line_start = u64::from(self.line) * CYCLES_PER_LINE;
            let pos_in_line = self.cycle_in_frame - line_start.min(self.cycle_in_frame);

            // HBlank begins at cycle 1006 of the line.
            if !self.hblank_signalled && pos_in_line >= HBLANK_START {
                self.hblank_signalled = true;
                events.hblank = true;
                set_dispstat_bits(bus, 0b010, 0b010);
            }

            if pos_in_line < CYCLES_PER_LINE {
                break;
            }
            // Cross into the next line.
            self.cycle_in_frame -= CYCLES_PER_LINE;
            self.hblank_signalled = false;
            set_dispstat_bits(bus, 0b010, 0);
            self.line += 1;
            if self.line >= LINES_PER_FRAME {
                self.line = 0;
            }
            // VCOUNT + vcounter match.
            write_vcount(bus, self.line);
            let dispstat = bus.read16(0x0400_0004);
            let lyc = (dispstat >> 8) as u16;
            if lyc == self.line {
                set_dispstat_bits(bus, 0b100, 0b100);
                events.vcounter_match = true;
            } else {
                set_dispstat_bits(bus, 0b100, 0);
            }

            if self.line == SCREEN_H as u16 {
                events.vblank = true;
                self.frame_ready = true;
                set_dispstat_bits(bus, 0b001, 0b001);
            }
            if self.line == 0 {
                set_dispstat_bits(bus, 0b001, 0);
            }
            self.line_rendered = false;
            if self.line < SCREEN_H as u16 {
                self.line_rendered = true;
                render_scanline(bus, framebuffer, self.line as usize);
            }
        }
        events
    }
}

fn set_dispstat_bits(bus: &mut Bus, mask: u16, values: u16) {
    let addr = 0x0400_0004;
    let cur = bus.read16(addr);
    bus.write16(addr, (cur & !mask) | (values & mask));
}

fn write_vcount(bus: &mut Bus, line: u16) {
    let addr = 0x0400_0006;
    let cur = bus.read16(addr);
    bus.write16(addr, (cur & 0xFF00) | (line & 0xFF));
}

/// Convert a GBA 15-bit colour (xBBBBBGGGGGRRRRR) to XRGB8888.
pub fn rgb555(c: u16) -> u32 {
    let r = u32::from(c & 0x1F);
    let g = u32::from((c >> 5) & 0x1F);
    let b = u32::from((c >> 10) & 0x1F);
    let r = (r << 3) | (r >> 2);
    let g = (g << 3) | (g >> 2);
    let b = (b << 3) | (b >> 2);
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

fn bg_palette(bus: &Bus, index: usize) -> u32 {
    let c = u16::from_le_bytes([bus.palette[index * 2], bus.palette[index * 2 + 1]]);
    rgb555(c)
}

fn obj_palette(bus: &Bus, index: usize) -> u32 {
    let base = 256 + index;
    let c = u16::from_le_bytes([bus.palette[base * 2], bus.palette[base * 2 + 1]]);
    rgb555(c)
}

fn render_scanline(bus: &Bus, fb: &mut [u32; SCREEN_W * SCREEN_H], y: usize) {
    let dispcnt = bus.read16(0x0400_0000);
    let mode = dispcnt & 0x7;
    let row_start = y * SCREEN_W;
    let backdrop = bg_palette(bus, 0);
    for px in &mut fb[row_start..row_start + SCREEN_W] {
        *px = backdrop;
    }
    match mode {
        3 => {
            for x in 0..SCREEN_W {
                let off = (y * SCREEN_W + x) * 2;
                let c = u16::from_le_bytes([bus.vram[off], bus.vram[off + 1]]);
                fb[row_start + x] = rgb555(c);
            }
        }
        4 => {
            let page = if dispcnt & (1 << 4) != 0 { 0xA000 } else { 0 };
            for x in 0..SCREEN_W {
                let idx = bus.vram[page + y * SCREEN_W + x] as usize;
                fb[row_start + x] = bg_palette(bus, idx);
            }
        }
        0 => {
            render_tile_mode(bus, fb, y, dispcnt);
        }
        // Modes 1, 2, 5 arrive with the affine work (Phase 3).
        _ => {}
    }
    if mode <= 2 {
        render_objs(bus, fb, y, dispcnt);
    }
}

struct BgOut {
    color: u32,
    opaque: bool,
}

fn render_tile_mode(bus: &Bus, fb: &mut [u32; SCREEN_W * SCREEN_H], y: usize, dispcnt: u16) {
    // Draw backgrounds in reverse priority order (highest number
    // first, BG3 before BG0 on ties) so the front-most lands last.
    let mut order: [(u16, usize); 4] = [(0, 0), (0, 1), (0, 2), (0, 3)];
    for i in 0..4 {
        let cnt = bus.read16(0x0400_0008 + (i as u32) * 2);
        order[i] = (cnt & 0x3, i);
    }
    order.sort_by(|a, b| b.cmp(a));

    let row_start = y * SCREEN_W;
    for &(_prio, bg) in order.iter() {
        if dispcnt & (1 << (8 + bg)) == 0 {
            continue;
        }
        let cnt = bus.read16(0x0400_0008 + (bg as u32) * 2);
        let hofs = u32::from(bus.read16(0x0400_0010 + (bg as u32) * 4) & 0x1FF);
        let vofs = u32::from(bus.read16(0x0400_0012 + (bg as u32) * 4) & 0x1FF);
        let char_base = usize::from((cnt >> 2) & 0x3) * 0x4000;
        let screen_base = usize::from((cnt >> 8) & 0x1F) * 0x800;
        let wide = cnt & (1 << 14) != 0;
        let tall = cnt & (1 << 15) != 0;
        let eight_bpp = cnt & (1 << 7) != 0;

        let sy = (u32::from(y as u16) + vofs) & if tall { 511 } else { 255 };
        for x in 0..SCREEN_W {
            let sx = (x as u32 + hofs) & if wide { 511 } else { 255 };
            // Screen block selection for 512-wide/tall maps.
            let mut map_base = screen_base;
            let (mx, my) = if wide && sx >= 256 {
                map_base += 0x800;
                (sx - 256, sy)
            } else {
                (sx, sy)
            };
            let (mx, my) = if tall && my >= 256 {
                map_base += if wide { 0x1000 } else { 0x800 };
                (mx, my - 256)
            } else {
                (mx, my)
            };
            let entry_off = map_base + (my as usize / 8) * 64 + (mx as usize / 8) * 2;
            let entry = u16::from_le_bytes([bus.vram[entry_off], bus.vram[entry_off + 1]]);
            let tile = usize::from(entry & 0x3FF);
            let hflip = entry & (1 << 10) != 0;
            let vflip = entry & (1 << 11) != 0;
            let pal_bank = usize::from((entry >> 12) & 0xF);
            let mut tx = (mx % 8) as usize;
            let mut ty = (my % 8) as usize;
            if hflip { tx = 7 - tx; }
            if vflip { ty = 7 - ty; }
            let out = if eight_bpp {
                let off = char_base + tile * 64 + ty * 8 + tx;
                let idx = bus.vram[off] as usize;
                BgOut { color: bg_palette(bus, idx), opaque: idx != 0 }
            } else {
                let off = char_base + tile * 32 + ty * 4 + tx / 2;
                let byte = bus.vram[off];
                let nib = if tx % 2 == 0 { byte & 0xF } else { byte >> 4 } as usize;
                BgOut {
                    color: bg_palette(bus, pal_bank * 16 + nib),
                    opaque: nib != 0,
                }
            };
            if out.opaque {
                fb[row_start + x] = out.color;
            }
        }
    }
}

const OBJ_SIZES: [[(u8, u8); 4]; 4] = [
    // shape 0 square, 1 wide, 2 tall, 3 prohibited (treated square)
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
    [(8, 8), (16, 16), (32, 32), (64, 64)],
];

fn render_objs(bus: &Bus, fb: &mut [u32; SCREEN_W * SCREEN_H], y: usize, dispcnt: u16) {
    if dispcnt & (1 << 12) == 0 {
        return;
    }
    let one_dimensional = dispcnt & (1 << 6) != 0;
    // OAM is drawn in reverse index order so OBJ0 ends up front-most.
    for i in (0..128usize).rev() {
        let base = i * 8;
        let a0 = u16::from_le_bytes([bus.oam[base], bus.oam[base + 1]]);
        let a1 = u16::from_le_bytes([bus.oam[base + 2], bus.oam[base + 3]]);
        let a2 = u16::from_le_bytes([bus.oam[base + 4], bus.oam[base + 5]]);
        // Disabled (attr0 bits 8..9 = 10) or double-size/affine: skip for now.
        if a0 & 0x0300 == 0x0200 {
            continue;
        }
        let shape = usize::from((a0 >> 14) & 0x3);
        let size = usize::from((a1 >> 14) & 0x3);
        let (w, h) = OBJ_SIZES[shape][size];
        let (w, h) = (usize::from(w), usize::from(h));
        let obj_y = usize::from(a0 & 0xFF);
        let obj_x_raw = usize::from(a1 & 0x1FF);
        let obj_x = if obj_x_raw >= 256 { obj_x_raw as i32 - 512 } else { obj_x_raw as i32 };
        // Y wrap at 256.
        let ly = (y + 256 - obj_y) % 256;
        if ly >= h {
            continue;
        }
        let eight_bpp = a0 & (1 << 13) != 0;
        let hflip = a1 & (1 << 12) != 0;
        let vflip = a1 & (1 << 13) != 0;
        let tile_base = usize::from(a2 & 0x3FF);
        let pal_bank = usize::from((a2 >> 12) & 0xF);
        let tile_y = if vflip { h - 1 - ly } else { ly };
        for sx in 0..w {
            let screen_x = obj_x + sx as i32;
            if !(0..SCREEN_W as i32).contains(&screen_x) {
                continue;
            }
            let tile_x = if hflip { w - 1 - sx } else { sx };
            let tile_index = if one_dimensional {
                tile_base + (tile_y / 8) * (w / 8) + (tile_x / 8)
            } else {
                tile_base + (tile_y / 8) * 32 + (tile_x / 8)
            };
            let px = tile_x % 8;
            let py = tile_y % 8;
            let (idx, opaque) = if eight_bpp {
                let off = 0x10000 + tile_index * 64 + py * 8 + px;
                if off >= bus.vram.len() { continue; }
                let v = bus.vram[off] as usize;
                (v, v != 0)
            } else {
                let off = 0x10000 + tile_index * 32 + py * 4 + px / 2;
                if off >= bus.vram.len() { continue; }
                let byte = bus.vram[off];
                let nib = if px % 2 == 0 { byte & 0xF } else { byte >> 4 } as usize;
                (pal_bank * 16 + nib, nib != 0)
            };
            if opaque {
                fb[y * SCREEN_W + screen_x as usize] = obj_palette(bus, idx);
            }
        }
    }
}
