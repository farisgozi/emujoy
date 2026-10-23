//! PPU — Phase 4 renderer.
//!
//! Timing: 1232 cycles per scanline, 228 lines per frame, 160 visible.
//! Rendering happens one whole scanline at a time when the line starts
//! (mid-line register changes are therefore approximate). Supported:
//! - Text backgrounds (all modes), 4bpp & 8bpp, flips, scroll, sizes.
//! - Affine backgrounds (Modes 1 & 2) with internal reference-point
//!   tracking, plus affine-transformed bitmap Modes 3, 4 and 5.
//! - Objects: normal and affine (incl. double-size), 1D/2D mapping,
//!   per-pixel priority against backgrounds, semi-transparent mode,
//!   OBJ window, mosaic.
//! - Windows WIN0/WIN1/OBJ with WININ/WINOUT masks, colour effects
//!   (alpha blend, brighten, darken), forced blank.

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
    /// Internal affine reference points (BG2, BG3) — hardware reloads
    /// them from the written registers at frame start and whenever
    /// the registers are rewritten, then steps them by (PB, PD) per
    /// scanline.
    aff_int: [(i32, i32); 2],
    aff_written: [(i32, i32); 2],
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
        Self {
            line: 0,
            cycle_in_frame: 0,
            hblank_signalled: false,
            line_rendered: false,
            frame_ready: false,
            aff_int: [(0, 0); 2],
            aff_written: [(0, 0); 2],
        }
    }

    /// (line, cycle accumulator) for save states.
    pub fn timing_state(&self) -> (u16, u64) {
        (self.line, self.cycle_in_frame)
    }

    pub fn restore_timing(&mut self, line: u16, cycle_in_frame: u64) {
        self.line = line % LINES_PER_FRAME;
        self.cycle_in_frame = cycle_in_frame;
        self.hblank_signalled = false;
        self.line_rendered = false;
        self.frame_ready = false;
        self.aff_int = [(0, 0); 2];
        self.aff_written = [(0, 0); 2];
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

        if self.line < SCREEN_H as u16 && !self.line_rendered {
            self.line_rendered = true;
            self.render_line(bus, framebuffer);
        }

        loop {
            let line_start = u64::from(self.line) * CYCLES_PER_LINE;
            let pos_in_line = self.cycle_in_frame - line_start.min(self.cycle_in_frame);

            if !self.hblank_signalled && pos_in_line >= HBLANK_START {
                self.hblank_signalled = true;
                events.hblank = true;
                set_dispstat_bits(bus, 0b010, 0b010);
            }

            if pos_in_line < CYCLES_PER_LINE {
                break;
            }
            self.cycle_in_frame -= CYCLES_PER_LINE;
            self.hblank_signalled = false;
            set_dispstat_bits(bus, 0b010, 0);
            self.line += 1;
            if self.line >= LINES_PER_FRAME {
                self.line = 0;
                // Frame start: internal affine refs reload.
                for i in 0..2 {
                    self.aff_int[i] = read_affine_refs(bus, i);
                    self.aff_written[i] = self.aff_int[i];
                }
            }
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
                self.render_line(bus, framebuffer);
            }
        }
        events
    }

    fn render_line(&mut self, bus: &Bus, fb: &mut [u32; SCREEN_W * SCREEN_H]) {
        // Mid-frame rewrites of the reference registers reload the
        // internal copies, exactly like hardware.
        for i in 0..2 {
            let w = read_affine_refs(bus, i);
            if w != self.aff_written[i] {
                self.aff_written[i] = w;
                self.aff_int[i] = w;
            }
        }
        render_scanline(bus, fb, self.line as usize, self.aff_int);
        // Per-line step of the internal reference points.
        for i in 0..2 {
            let base = 0x0400_0020 + (i as u32) * 0x10;
            let pb = i32::from(bus.read16(base + 2) as i16);
            let pd = i32::from(bus.read16(base + 6) as i16);
            self.aff_int[i].0 = self.aff_int[i].0.wrapping_add(pb);
            self.aff_int[i].1 = self.aff_int[i].1.wrapping_add(pd);
        }
    }
}

fn read_affine_refs(bus: &Bus, i: usize) -> (i32, i32) {
    let base = 0x0400_0020 + (i as u32) * 0x10;
    let sx = sign28(bus.read32(base + 8));
    let sy = sign28(bus.read32(base + 12));
    (sx, sy)
}

/// Sign-extend a 28-bit fixed-point (20.8) reference coordinate.
fn sign28(v: u32) -> i32 {
    ((v << 4) as i32) >> 4
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

fn pal_raw(bus: &Bus, index: usize) -> u16 {
    u16::from_le_bytes([bus.palette[index * 2], bus.palette[index * 2 + 1]]) & 0x7FFF
}

// ---------------------------------------------------------------------------
// Layer rows + compositor
// ---------------------------------------------------------------------------

/// One background pixel: raw 15-bit colour, or None for transparent.
type Row = [Option<u16>; SCREEN_W];

#[derive(Clone, Copy)]
struct ObjPx {
    color: u16,
    prio: u16,
    semi: bool,
}

struct ObjRow {
    px: [Option<ObjPx>; SCREEN_W],
    window: [bool; SCREEN_W],
}

fn render_scanline(bus: &Bus, fb: &mut [u32; SCREEN_W * SCREEN_H], y: usize, aff: [(i32, i32); 2]) {
    let dispcnt = bus.read16(0x0400_0000);
    let row_start = y * SCREEN_W;
    if dispcnt & (1 << 7) != 0 {
        // Forced blank: the whole line is white.
        for px in &mut fb[row_start..row_start + SCREEN_W] {
            *px = rgb555(0x7FFF);
        }
        return;
    }
    let mode = dispcnt & 0x7;
    let mosaic = bus.read16(0x0400_004c);
    let bg_mosaic_h = usize::from(mosaic & 0xF) + 1;
    let bg_mosaic_v = usize::from((mosaic >> 4) & 0xF) + 1;

    // --- background rows ---
    let mut bg_rows: [Row; 4] = [[None; SCREEN_W]; 4];
    let mut bg_prio = [0u16; 4];
    let mut bg_on = [false; 4];
    for bg in 0..4usize {
        let cnt = bus.read16(0x0400_0008 + (bg as u32) * 2);
        bg_prio[bg] = cnt & 0x3;
        let enabled = dispcnt & (1 << (8 + bg)) != 0;
        let usable = match mode {
            0 => true,
            1 => bg <= 2,
            2 => bg >= 2,
            3 | 4 | 5 => bg == 2,
            _ => false,
        };
        if !enabled || !usable {
            continue;
        }
        bg_on[bg] = true;
        let mosaic_on = cnt & (1 << 6) != 0;
        let y_eff = if mosaic_on { y - (y % bg_mosaic_v) } else { y };
        match mode {
            0 | 1 if bg < 2 || mode == 0 => {
                render_text_bg(bus, bg, cnt, y_eff, &mut bg_rows[bg]);
            }
            1 | 2 => {
                let base = aff[bg - 2];
                render_affine_bg(bus, bg, cnt, base, &mut bg_rows[bg]);
            }
            3 | 4 | 5 => {
                let base = aff[0];
                render_bitmap_bg(bus, mode, dispcnt, base, &mut bg_rows[bg]);
            }
            _ => {}
        }
        if mosaic_on && bg_mosaic_h > 1 {
            apply_h_mosaic(&mut bg_rows[bg], bg_mosaic_h);
        }
    }

    // --- object row ---
    let mut obj = ObjRow { px: [None; SCREEN_W], window: [false; SCREEN_W] };
    if dispcnt & (1 << 12) != 0 {
        render_objs(bus, y, dispcnt, mosaic, &mut obj);
    }
    let obj_on = dispcnt & (1 << 12) != 0;

    // --- compositing ---
    let windows_on = dispcnt & 0xE000 != 0;
    let bldcnt = bus.read16(0x0400_0050);
    let bldalpha = bus.read16(0x0400_0052);
    let eva = (bldalpha & 0x1F).min(16);
    let evb = ((bldalpha >> 8) & 0x1F).min(16);
    let evy = (bus.read16(0x0400_0054) & 0x1F).min(16);
    let backdrop = pal_raw(bus, 0);

    for x in 0..SCREEN_W {
        // Window mask for this pixel: bits 0-3 BG, 4 OBJ, 5 effects.
        let mask: u16 = if !windows_on {
            0x3F
        } else {
            window_mask(bus, dispcnt, x, y, &obj.window)
        };
        // Candidate selection front-to-back: sort key is (priority,
        // layer order BG0 < BG1 < BG2 < BG3 < OBJ < backdrop).
        let mut top: Option<(u16, usize)> = None; // (color, layer id)
        let mut second: Option<(u16, usize)> = None;
        let mut best_key = (u16::MAX, usize::MAX);
        let mut second_key = (u16::MAX, usize::MAX);
        for bg in 0..4usize {
            if !bg_on[bg] || mask & (1 << bg) == 0 {
                continue;
            }
            if let Some(c) = bg_rows[bg][x] {
                let key = (bg_prio[bg], bg);
                if key < best_key {
                    second = top;
                    second_key = best_key;
                    top = Some((c, bg));
                    best_key = key;
                } else if key < second_key {
                    second = Some((c, bg));
                    second_key = key;
                }
            }
        }
        let mut top_semi = false;
        if obj_on && mask & (1 << 4) != 0 {
            if let Some(op) = obj.px[x] {
                let key = (op.prio, 4usize);
                if key < best_key {
                    second = top;
                    top = Some((op.color, 4));
                    top_semi = op.semi;
                } else if key < second_key {
                    second = Some((op.color, 4));
                }
            }
        }
        let (top_color, top_layer) = match top {
            Some(t) => t,
            None => (backdrop, 5usize),
        };
        // If the OBJ was pushed to second place, its semi flag does
        // not matter; if nothing won, top is the backdrop.
        let mut out = top_color;
        let effects_on = mask & (1 << 5) != 0;
        if effects_on {
            let mode_fx = (bldcnt >> 6) & 0x3;
            let in_t1 = bldcnt & (1 << top_layer) != 0;
            if top_semi && top_layer == 4 {
                // Semi-transparent OBJ: always alpha-blend with what
                // is below it (if that layer is a 2nd target).
                if let Some((c2, l2)) = second {
                    if bldcnt & (1 << (8 + l2)) != 0 {
                        out = blend_alpha(top_color, c2, eva, evb);
                    }
                }
            } else if mode_fx == 1 && in_t1 {
                if let Some((c2, l2)) = second {
                    if bldcnt & (1 << (8 + l2)) != 0 {
                        out = blend_alpha(top_color, c2, eva, evb);
                    }
                } else if bldcnt & (1 << (8 + 5)) != 0 {
                    // Below everything is the backdrop.
                    out = blend_alpha(top_color, backdrop, eva, evb);
                }
            } else if mode_fx == 2 && in_t1 {
                out = blend_bright(top_color, evy);
            } else if mode_fx == 3 && in_t1 {
                out = blend_dark(top_color, evy);
            }
        }
        fb[row_start + x] = rgb555(out);
    }
}

fn apply_h_mosaic(row: &mut Row, size: usize) {
    let mut x = 0;
    while x < SCREEN_W {
        let v = row[x];
        for k in 1..size {
            if x + k < SCREEN_W {
                row[x + k] = v;
            }
        }
        x += size;
    }
}

fn blend_alpha(a: u16, b: u16, eva: u16, evb: u16) -> u16 {
    let mix = |shift: u16| -> u16 {
        let ca = (a >> shift) & 0x1F;
        let cb = (b >> shift) & 0x1F;
        ((ca * eva + cb * evb) / 16).min(31)
    };
    mix(0) | (mix(5) << 5) | (mix(10) << 10)
}

fn blend_bright(a: u16, evy: u16) -> u16 {
    let f = |shift: u16| -> u16 {
        let c = (a >> shift) & 0x1F;
        c + ((31 - c) * evy) / 16
    };
    f(0) | (f(5) << 5) | (f(10) << 10)
}

fn blend_dark(a: u16, evy: u16) -> u16 {
    let f = |shift: u16| -> u16 {
        let c = (a >> shift) & 0x1F;
        c - (c * evy) / 16
    };
    f(0) | (f(5) << 5) | (f(10) << 10)
}

/// Window mask byte (WININ/WINOUT layout) for one pixel.
fn window_mask(bus: &Bus, dispcnt: u16, x: usize, y: usize, obj_window: &[bool; SCREEN_W]) -> u16 {
    let winin = bus.read16(0x0400_0048);
    let winout = bus.read16(0x0400_004a);
    if dispcnt & (1 << 13) != 0 {
        let h = bus.read16(0x0400_0040);
        let v = bus.read16(0x0400_0044);
        if in_window(x, y, h, v) {
            return winin & 0xFF;
        }
    }
    if dispcnt & (1 << 14) != 0 {
        let h = bus.read16(0x0400_0042);
        let v = bus.read16(0x0400_0046);
        if in_window(x, y, h, v) {
            return (winin >> 8) & 0xFF;
        }
    }
    if dispcnt & (1 << 15) != 0 && obj_window[x] {
        return (winout >> 8) & 0xFF;
    }
    winout & 0xFF
}

fn in_window(x: usize, y: usize, h: u16, v: u16) -> bool {
    let x1 = usize::from(h >> 8);
    let x2 = usize::from(h & 0xFF);
    let y1 = usize::from(v >> 8);
    let y2 = usize::from(v & 0xFF);
    let in_x = if x1 <= x2 { x >= x1 && x < x2 } else { x >= x1 || x < x2 };
    let in_y = if y1 <= y2 { y >= y1 && y < y2 } else { y >= y1 || y < y2 };
    in_x && in_y
}

// ---------------------------------------------------------------------------
// Background renderers (one scanline each)
// ---------------------------------------------------------------------------

fn render_text_bg(bus: &Bus, bg: usize, cnt: u16, y: usize, row: &mut Row) {
    let hofs = u32::from(bus.read16(0x0400_0010 + (bg as u32) * 4) & 0x1FF);
    let vofs = u32::from(bus.read16(0x0400_0012 + (bg as u32) * 4) & 0x1FF);
    let char_base = usize::from((cnt >> 2) & 0x3) * 0x4000;
    let screen_base = usize::from((cnt >> 8) & 0x1F) * 0x800;
    let wide = cnt & (1 << 14) != 0;
    let tall = cnt & (1 << 15) != 0;
    let eight_bpp = cnt & (1 << 7) != 0;

    let sy = (y as u32 + vofs) & if tall { 511 } else { 255 };
    for (x, px) in row.iter_mut().enumerate() {
        let sx = (x as u32 + hofs) & if wide { 511 } else { 255 };
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
        *px = if eight_bpp {
            let off = char_base + tile * 64 + ty * 8 + tx;
            let idx = bus.vram[off] as usize;
            if idx != 0 { Some(pal_raw(bus, idx)) } else { None }
        } else {
            let off = char_base + tile * 32 + ty * 4 + tx / 2;
            let byte = bus.vram[off];
            let nib = if tx % 2 == 0 { byte & 0xF } else { byte >> 4 } as usize;
            if nib != 0 { Some(pal_raw(bus, pal_bank * 16 + nib)) } else { None }
        };
    }
}

fn render_affine_bg(bus: &Bus, bg: usize, cnt: u16, base_ref: (i32, i32), row: &mut Row) {
    let i = bg - 2;
    let reg = 0x0400_0020 + (i as u32) * 0x10;
    let pa = i32::from(bus.read16(reg) as i16);
    let pc = i32::from(bus.read16(reg + 4) as i16);
    let char_base = usize::from((cnt >> 2) & 0x3) * 0x4000;
    let screen_base = usize::from((cnt >> 8) & 0x1F) * 0x800;
    let wrap = cnt & (1 << 13) != 0;
    let tiles_side = [16i32, 32, 64, 128][usize::from((cnt >> 14) & 0x3)];
    let map_px = tiles_side * 8;
    for (x, px) in row.iter_mut().enumerate() {
        let tx = (base_ref.0 + pa * (x as i32)) >> 8;
        let ty = (base_ref.1 + pc * (x as i32)) >> 8;
        let (tx, ty) = if wrap {
            (tx.rem_euclid(map_px), ty.rem_euclid(map_px))
        } else if !(0..map_px).contains(&tx) || !(0..map_px).contains(&ty) {
            continue;
        } else {
            (tx, ty)
        };
        let entry_off = screen_base + (ty as usize / 8) * (tiles_side as usize) + (tx as usize / 8);
        let tile = usize::from(bus.vram[entry_off]);
        let off = char_base + tile * 64 + (ty as usize % 8) * 8 + (tx as usize % 8);
        let idx = bus.vram[off] as usize;
        if idx != 0 {
            *px = Some(pal_raw(bus, idx));
        }
    }
}

fn render_bitmap_bg(bus: &Bus, mode: u16, dispcnt: u16, base_ref: (i32, i32), row: &mut Row) {
    // Bitmap modes are BG2 with affine transform over a bitmap
    // "map": Mode 3 = 240x160 direct colour, Mode 4 = 240x160 8bpp
    // (page via DISPCNT bit 4), Mode 5 = 160x128 direct colour.
    let reg = 0x0400_0020u32;
    let pa = i32::from(bus.read16(reg) as i16);
    let pc = i32::from(bus.read16(reg + 4) as i16);
    let (bw, bh) = match mode {
        5 => (160i32, 128i32),
        _ => (240, 160),
    };
    let page = if (mode == 4 || mode == 5) && dispcnt & (1 << 4) != 0 { 0xA000usize } else { 0 };
    for (x, px) in row.iter_mut().enumerate() {
        let tx = (base_ref.0 + pa * (x as i32)) >> 8;
        let ty = (base_ref.1 + pc * (x as i32)) >> 8;
        if !(0..bw).contains(&tx) || !(0..bh).contains(&ty) {
            continue;
        }
        *px = match mode {
            3 | 5 => {
                let off = page + ((ty * bw + tx) as usize) * 2;
                Some(u16::from_le_bytes([bus.vram[off], bus.vram[off + 1]]) & 0x7FFF)
            }
            _ => {
                let idx = bus.vram[page + (ty * bw + tx) as usize] as usize;
                if idx != 0 { Some(pal_raw(bus, idx)) } else { None }
            }
        };
    }
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

const OBJ_SIZES: [[(u8, u8); 4]; 4] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
    [(8, 8), (16, 16), (32, 32), (64, 64)],
];

fn obj_tile_pixel(
    bus: &Bus,
    one_dimensional: bool,
    tile_base: usize,
    w: usize,
    tx: usize,
    ty: usize,
    eight_bpp: bool,
    pal_bank: usize,
    vram_base: usize,
) -> Option<u16> {
    let tile_index = if eight_bpp {
        if one_dimensional {
            tile_base + (ty / 8) * (w / 4) + (tx / 8) * 2
        } else {
            tile_base + (ty / 8) * 32 + (tx / 8) * 2
        }
    } else if one_dimensional {
        tile_base + (ty / 8) * (w / 8) + (tx / 8)
    } else {
        tile_base + (ty / 8) * 32 + (tx / 8)
    };
    let px = tx % 8;
    let py = ty % 8;
    if eight_bpp {
        let off = vram_base + tile_index * 32 + py * 8 + px;
        if off >= 0x18000 { return None; }
        let v = bus.vram[off] as usize;
        if v != 0 { Some(pal_raw(bus, 256 + v)) } else { None }
    } else {
        let off = vram_base + tile_index * 32 + py * 4 + px / 2;
        if off >= 0x18000 { return None; }
        let byte = bus.vram[off];
        let nib = if px % 2 == 0 { byte & 0xF } else { byte >> 4 } as usize;
        if nib != 0 { Some(pal_raw(bus, 256 + pal_bank * 16 + nib)) } else { None }
    }
}

fn render_objs(bus: &Bus, y: usize, dispcnt: u16, mosaic: u16, out: &mut ObjRow) {
    let one_dimensional = dispcnt & (1 << 6) != 0;
    let mode = dispcnt & 0x7;
    let vram_base = if mode >= 3 { 0x14000usize } else { 0x10000 };
    let obj_mosaic_h = usize::from((mosaic >> 8) & 0xF) + 1;
    let obj_mosaic_v = usize::from((mosaic >> 12) & 0xF) + 1;
    // Front-most OBJ = lowest OAM index; draw low to high and only
    // fill pixels nobody claimed yet.
    for i in 0..128usize {
        let base = i * 8;
        let a0 = u16::from_le_bytes([bus.oam[base], bus.oam[base + 1]]);
        let a1 = u16::from_le_bytes([bus.oam[base + 2], bus.oam[base + 3]]);
        let a2 = u16::from_le_bytes([bus.oam[base + 4], bus.oam[base + 5]]);
        let affine = a0 & (1 << 8) != 0;
        let disable_or_double = a0 & (1 << 9) != 0;
        if !affine && disable_or_double {
            continue; // OBJ disabled
        }
        let obj_mode = (a0 >> 10) & 0x3;
        let mosaic_on = a0 & (1 << 12) != 0;
        let eight_bpp = a0 & (1 << 13) != 0;
        let shape = usize::from((a0 >> 14) & 0x3);
        let size = usize::from((a1 >> 14) & 0x3);
        let (sw, sh) = OBJ_SIZES[shape][size];
        let (sw, sh) = (usize::from(sw), usize::from(sh));
        let (box_w, box_h) = if affine && disable_or_double { (sw * 2, sh * 2) } else { (sw, sh) };
        let obj_y = usize::from(a0 & 0xFF);
        let obj_x_raw = i32::from(a1 & 0x1FF);
        let obj_x = if obj_x_raw >= 256 { obj_x_raw - 512 } else { obj_x_raw };
        // Distance from the sprite's top edge to this scanline
        // (Y wraps at 256; box may extend to 256+128 via double size).
        let ly_full = (y + 256 - obj_y) % 256;
        if ly_full >= box_h {
            continue;
        }
        let prio = (a2 >> 10) & 0x3;
        let semi = obj_mode == 1;
        let is_window = obj_mode == 2;
        let tile_base = usize::from(a2 & 0x3FF);
        let pal_bank = usize::from((a2 >> 12) & 0xF);

        if affine {
            let group = usize::from((a1 >> 9) & 0x1F);
            // Affine parameters live in the attr3 slots of the four
            // OAM entries the group spans: byte offsets +6, +14,
            // +22, +30 from the group base.
            let gbase = group * 32;
            let rd = |off: usize| i32::from(u16::from_le_bytes([bus.oam[gbase + off], bus.oam[gbase + off + 1]]) as i16);
            let (pa, pb, pc, pd) = (rd(6), rd(14), rd(22), rd(30));
            let ly_m = if mosaic_on { ly_full - (ly_full % obj_mosaic_v) } else { ly_full };
            let cy = ly_m as i32 - (box_h as i32) / 2;
            for sx in 0..box_w {
                let screen_x = obj_x + sx as i32;
                if !(0..SCREEN_W as i32).contains(&screen_x) {
                    continue;
                }
                let sx_m = if mosaic_on { sx - (sx % obj_mosaic_h) } else { sx };
                let cx = sx_m as i32 - (box_w as i32) / 2;
                let tx = ((pa * cx + pb * cy) >> 8) + (sw as i32) / 2;
                let ty = ((pc * cx + pd * cy) >> 8) + (sh as i32) / 2;
                if !(0..sw as i32).contains(&tx) || !(0..sh as i32).contains(&ty) {
                    continue;
                }
                let color = obj_tile_pixel(bus, one_dimensional, tile_base, sw, tx as usize, ty as usize, eight_bpp, pal_bank, vram_base);
                plot_obj(out, screen_x as usize, color, prio, semi, is_window);
            }
        } else {
            let hflip = a1 & (1 << 12) != 0;
            let vflip = a1 & (1 << 13) != 0;
            let ly = if mosaic_on { ly_full - (ly_full % obj_mosaic_v) } else { ly_full };
            let tile_y = if vflip { sh - 1 - ly } else { ly };
            for sx in 0..box_w {
                let screen_x = obj_x + sx as i32;
                if !(0..SCREEN_W as i32).contains(&screen_x) {
                    continue;
                }
                let mut tile_x = if hflip { sw - 1 - sx } else { sx };
                if mosaic_on {
                    tile_x -= tile_x % obj_mosaic_h;
                }
                let color = obj_tile_pixel(bus, one_dimensional, tile_base, sw, tile_x, tile_y, eight_bpp, pal_bank, vram_base);
                plot_obj(out, screen_x as usize, color, prio, semi, is_window);
            }
        }
    }
}

fn plot_obj(out: &mut ObjRow, x: usize, color: Option<u16>, prio: u16, semi: bool, is_window: bool) {
    if is_window {
        if color.is_some() {
            out.window[x] = true;
        }
        return;
    }
    if let Some(c) = color {
        if out.px[x].is_none() {
            out.px[x] = Some(ObjPx { color: c, prio, semi });
        }
    }
}
