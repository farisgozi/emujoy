//! GBA memory bus — Phase 0 skeleton.
//!
//! Address decoding follows the GBATEK memory map. Timing (waitstates,
//! prefetch) is deliberately NOT modelled yet; Phase 2 introduces a
//! `cycles_for(addr, width, sequential)` cost function and Phase 4 makes
//! it precise. What Phase 0 guarantees is the shape: all memory lives in
//! buffers owned by `Bus`, reads/writes are total functions (unmapped
//! space returns open-bus 0 instead of panicking), and ROM is loaded
//! through one validated entry point.

use alloc::boxed::Box;
use alloc::vec::Vec;

pub const EWRAM_LEN: usize = 256 * 1024;
pub const IWRAM_LEN: usize = 32 * 1024;
pub const VRAM_LEN: usize = 96 * 1024;
pub const OAM_LEN: usize = 1024;
pub const PALETTE_LEN: usize = 1024;
pub const IO_LEN: usize = 0x400;

pub const BIOS_LEN: usize = 16 * 1024;

pub struct Bus {
    pub bios: Box<[u8; BIOS_LEN]>,
    pub bios_loaded: bool,
    pub ewram: Box<[u8; EWRAM_LEN]>,
    pub iwram: Box<[u8; IWRAM_LEN]>,
    pub vram: Box<[u8; VRAM_LEN]>,
    pub oam: Box<[u8; OAM_LEN]>,
    pub palette: Box<[u8; PALETTE_LEN]>,
    pub io: Box<[u8; IO_LEN]>,
    rom: Vec<u8>,
    /// Cartridge save hardware (SRAM / Flash). Region 0x0E accesses are
    /// routed to the chip model, never to a plain buffer.
    pub save: crate::save::SaveMemory,
    /// Interrupt Flag register (0x04000202) is write-one-to-clear on
    /// hardware. Writes to it are intercepted in `write8` and
    /// accumulated here instead of being stored, so the core can tell
    /// a CPU acknowledgement apart from device-raised flags.
    pub if_clear_mask: u16,
    /// Bytes written to the Direct Sound FIFO ports (0x040000A0/A4),
    /// queued for the APU. DMA and CPU writes both land here; the
    /// FIFO ports are write-only, so nothing is stored in `io`.
    pub fifo_in: [alloc::vec::Vec<u8>; 2],
    /// Bitmask of PSG channels whose trigger bit (CNT_X bit 15) was
    /// written since the last drain — write detection for the APU,
    /// which cannot see trigger edges by polling the register value.
    pub snd_trigger_mask: u8,
}

impl Bus {
    pub fn new() -> Self {
        Self {
            bios: Box::new([0; BIOS_LEN]),
            bios_loaded: false,
            ewram: Box::new([0; EWRAM_LEN]),
            iwram: Box::new([0; IWRAM_LEN]),
            vram: Box::new([0; VRAM_LEN]),
            oam: Box::new([0; OAM_LEN]),
            palette: Box::new([0; PALETTE_LEN]),
            io: Box::new([0; IO_LEN]),
            rom: Vec::new(),
            save: crate::save::SaveMemory::new(crate::save::SaveKind::Sram),
            if_clear_mask: 0,
            fifo_in: [alloc::vec::Vec::new(), alloc::vec::Vec::new()],
            snd_trigger_mask: 0,
        }
    }

    /// Note a write to a PSG control register's high byte: CNT_X
    /// trigger bits (bit 15 = byte bit 7 at 0x..65/6D/75/7D) are
    /// recorded for the APU. Also queue FIFO port bytes.
    /// Returns true when the address was fully handled here.
    fn snd_hook(&mut self, addr: u32, value: u8) -> bool {
        match addr {
            0x0400_00A0..=0x0400_00A7 => {
                let idx = if addr < 0x0400_00A4 { 0 } else { 1 };
                self.fifo_in[idx].push(value);
                true
            }
            0x0400_0065 | 0x0400_006D | 0x0400_0075 | 0x0400_007D => {
                if value & 0x80 != 0 {
                    let bit = match addr {
                        0x0400_0065 => 0,
                        0x0400_006D => 1,
                        0x0400_0075 => 2,
                        _ => 3,
                    };
                    self.snd_trigger_mask |= 1 << bit;
                }
                false
            }
            _ => false,
        }
    }

    /// Drain the accumulated IF write-to-clear mask (see field docs).
    pub fn take_if_clear_mask(&mut self) -> u16 {
        let m = self.if_clear_mask;
        self.if_clear_mask = 0;
        m
    }

    pub fn load_rom(&mut self, rom: &[u8]) {
        self.rom.clear();
        self.rom.extend_from_slice(rom);
        // Save chip is chosen by the ROM's embedded save-type string.
        // Existing contents survive a same-kind re-detect.
        let kind = crate::save::detect_kind(rom);
        if kind != self.save.kind {
            self.save = crate::save::SaveMemory::new(kind);
        }
    }

    /// Copy a BIOS dump (max 16 KiB) into the BIOS region. The image is
    /// the user's own dump; the core never ships one.
    pub fn load_bios(&mut self, bios: &[u8]) {
        let n = bios.len().min(BIOS_LEN);
        self.bios[..n].copy_from_slice(&bios[..n]);
        self.bios_loaded = n > 0;
    }

    pub fn rom_is_empty(&self) -> bool {
        self.rom.is_empty()
    }

    pub fn rom_hash(&self) -> u64 {
        // FNV-1a; stored inside save states so a state can refuse to
        // load against a different game.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in &self.rom {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    pub fn save_bytes(&self) -> &[u8] {
        &self.save.data
    }

    /// Decode an address to (region slice, offset). Mirroring follows
    /// GBATEK: EWRAM/IWRAM/VRAM/palette/OAM repeat across their windows.
    fn decode(&self, addr: u32) -> Option<(&[u8], usize)> {
        let a = addr as usize;
        match addr >> 24 {
            0x00 | 0x01 => {
                if self.bios_loaded {
                    Some((&*self.bios, a % BIOS_LEN))
                } else {
                    None
                }
            }
            0x02 => Some((&*self.ewram, a % EWRAM_LEN)),
            0x03 => Some((&*self.iwram, a % IWRAM_LEN)),
            0x04 => Some((&*self.io, (a & 0x3FF) % IO_LEN)),
            0x05 => Some((&*self.palette, a % PALETTE_LEN)),
            0x06 => Some((&*self.vram, vram_offset(a))),
            0x07 => Some((&*self.oam, a % OAM_LEN)),
            0x08..=0x0D => {
                if self.rom.is_empty() {
                    None
                } else {
                    // The whole 32 MiB GamePak window mirrors the ROM
                    // (jsmolka memory test 007 reads data through the
                    // 0x0A000000 mirror), so decode by window offset,
                    // not by region base.
                    Some((&self.rom, (a & 0x01FF_FFFF) % self.rom.len()))
                }
            }
            // 0x0E (save region) is served by the chip model in
            // read8/write8, not by a slice.
            _ => None,
        }
    }

    fn decode_mut(&mut self, addr: u32) -> Option<(&mut [u8], usize)> {
        let a = addr as usize;
        match addr >> 24 {
            0x02 => Some((&mut *self.ewram, a % EWRAM_LEN)),
            0x03 => Some((&mut *self.iwram, a % IWRAM_LEN)),
            0x04 => Some((&mut *self.io, (a & 0x3FF) % IO_LEN)),
            0x05 => Some((&mut *self.palette, a % PALETTE_LEN)),
            0x06 => {
                let off = vram_offset(a);
                Some((&mut *self.vram, off))
            }
            0x07 => Some((&mut *self.oam, a % OAM_LEN)),
            // ROM space and the save region are not slice-writable.
            _ => None,
        }
    }

    /// EEPROM carts expose their chip in the 0x0D window (serial
    /// protocol, one bit per access, upper bits read back as 1s).
    fn in_eeprom_window(&self, addr: u32) -> bool {
        self.save.kind == crate::save::SaveKind::Eeprom && addr >> 24 == 0x0D
    }

    pub fn read8(&self, addr: u32) -> u8 {
        if matches!(addr >> 24, 0x0E..=0x0F) {
            return self.save.read(addr);
        }
        if self.in_eeprom_window(addr) {
            return 0xFE | self.save.eeprom_read_bit();
        }
        match self.decode(addr) {
            Some((region, off)) => region.get(off).copied().unwrap_or(0),
            None => 0,
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        if self.in_eeprom_window(addr) {
            // One bit per bus access, whatever the width.
            return 0xFFFE | u16::from(self.save.eeprom_read_bit());
        }
        if matches!(addr >> 24, 0x0E..=0x0F) {
            // Byte-wide chip on a 16-bit bus: the byte repeats.
            let b = u16::from(self.save.read(addr));
            return b | (b << 8);
        }
        let lo = u16::from(self.read8(addr));
        let hi = u16::from(self.read8(addr.wrapping_add(1)));
        lo | (hi << 8)
    }

    pub fn read32(&self, addr: u32) -> u32 {
        if self.in_eeprom_window(addr) {
            return 0xFFFF_FFFE | u32::from(self.save.eeprom_read_bit());
        }
        if matches!(addr >> 24, 0x0E..=0x0F) {
            let b = u32::from(self.save.read(addr));
            return b * 0x0101_0101;
        }
        let lo = u32::from(self.read16(addr));
        let hi = u32::from(self.read16(addr.wrapping_add(2)));
        lo | (hi << 16)
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        if self.snd_hook(addr, value) {
            return;
        }
        // IF (0x04000202/3): write-one-to-clear, never stored.
        if addr == 0x0400_0202 {
            self.if_clear_mask |= u16::from(value);
            return;
        }
        if addr == 0x0400_0203 {
            self.if_clear_mask |= u16::from(value) << 8;
            return;
        }
        // Video memory byte-write rules (GBATEK; jsmolka memory
        // video_strb tests): OAM ignores byte stores entirely;
        // palette and BG VRAM duplicate the byte into both halves of
        // the addressed halfword; OBJ VRAM ignores byte stores.
        match addr >> 24 {
            0x07 => return,
            0x05 => {
                let off = (addr as usize & 0x3FE) % PALETTE_LEN;
                self.palette[off] = value;
                self.palette[off + 1] = value;
                return;
            }
            0x06 => {
                let off = vram_offset(addr as usize);
                if off < 0x1_0000 {
                    let aligned = off & !1;
                    self.vram[aligned] = value;
                    self.vram[aligned + 1] = value;
                }
                return;
            }
            _ => {}
        }
        if matches!(addr >> 24, 0x0E..=0x0F) {
            self.save.write(addr, value);
            return;
        }
        if self.in_eeprom_window(addr) {
            self.save.eeprom_write_bit(value & 1);
            return;
        }
        if let Some((region, off)) = self.decode_mut(addr) {
            if let Some(slot) = region.get_mut(off) {
                *slot = value;
            }
        }
    }

    /// Raw byte store with NO special-region semantics — the path
    /// 16/32-bit stores decompose into (a palette STR must not act
    /// like two palette byte-stores).
    fn write8_raw(&mut self, addr: u32, value: u8) {
        if self.snd_hook(addr, value) {
            return;
        }
        if addr == 0x0400_0202 {
            self.if_clear_mask |= u16::from(value);
            return;
        }
        if addr == 0x0400_0203 {
            self.if_clear_mask |= u16::from(value) << 8;
            return;
        }
        if matches!(addr >> 24, 0x0E..=0x0F) {
            self.save.write(addr, value);
            return;
        }
        if let Some((region, off)) = self.decode_mut(addr) {
            if let Some(slot) = region.get_mut(off) {
                *slot = value;
            }
        }
    }

    pub fn write16(&mut self, addr: u32, value: u16) {
        if self.in_eeprom_window(addr) {
            // One bit per bus access, whatever the width.
            self.save.eeprom_write_bit((value & 1) as u8);
            return;
        }
        // NOTE: the IF intercept and save region live in write8_raw /
        // the branch below; byte-store video rules do NOT apply here.
        if matches!(addr >> 24, 0x0E..=0x0F) {
            // The save chip is byte-wide on a 16-bit bus: a halfword
            // store lands exactly one byte — the low byte at even
            // addresses, the high byte at odd ones (GBATEK; jsmolka
            // flash test 006 stores 0xAABB at +0 and +1 and reads
            // back 0xBB then 0xAA).
            let byte = if addr & 1 == 0 { value as u8 } else { (value >> 8) as u8 };
            self.save.write(addr, byte);
            return;
        }
        // Halfword accesses ignore address bit 0 outside the
        // byte-wide save region (ARM7 behaviour; jsmolka arm test
        // 407 stores a halfword at an odd EWRAM address and reads it
        // back from the aligned address).
        let a = addr & !1;
        self.write8_raw(a, value as u8);
        self.write8_raw(a.wrapping_add(1), (value >> 8) as u8);
    }

    pub fn write32(&mut self, addr: u32, value: u32) {
        if self.in_eeprom_window(addr) {
            self.save.eeprom_write_bit((value & 1) as u8);
            return;
        }
        if matches!(addr >> 24, 0x0E..=0x0F) {
            // Byte-wide chip: the byte written is selected by the
            // unaligned address bits (jsmolka flash test 008 stores a
            // word at +0..+3 and reads back DD CC BB AA).
            let shift = (addr & 3) * 8;
            self.save.write(addr, (value >> shift) as u8);
            return;
        }
        // Word stores align their address down on ARM7; doing it here
        // (not in the CPU) lets the save region above see the
        // original unaligned address.
        let a = addr & !3;
        self.write16(a, value as u16);
        self.write16(a.wrapping_add(2), (value >> 16) as u16);
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

/// VRAM has a quirk: the 96 KiB maps as 128 KiB with the top 32 KiB
/// mirrored (0x06010000.. repeats 0x06018000.. style aliasing is handled
/// by masking into the real buffer).
fn vram_offset(addr: usize) -> usize {
    let off = addr - 0x0600_0000;
    let off = off % (128 * 1024);
    if off >= VRAM_LEN {
        off - 32 * 1024
    } else {
        off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_writable_and_mirrored() {
        let mut bus = Bus::new();
        bus.write32(0x0200_0100, 0xDEAD_BEEF);
        assert_eq!(bus.read32(0x0200_0100), 0xDEAD_BEEF);
        // EWRAM mirror: +4 MiB window start decodes to the same cell.
        assert_eq!(bus.read32(0x0240_0100 & 0xFFFF_FFFF), 0xDEAD_BEEF);
        bus.write16(0x0300_0020, 0xBEEF);
        assert_eq!(bus.read16(0x0300_0020), 0xBEEF);
    }

    #[test]
    fn unmapped_and_rom_writes_are_safe() {
        let mut bus = Bus::new();
        assert_eq!(bus.read8(0x0100_0000), 0); // BIOS region: Phase 0 reads 0
        bus.write8(0x0800_0000, 0xAA); // ROM space: ignored, no panic
        assert_eq!(bus.read8(0x0800_0000), 0);
    }
}
