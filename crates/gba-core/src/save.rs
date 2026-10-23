//! Cartridge save hardware — Phase 3.
//!
//! Three kinds, selected by the save-type string embedded in the ROM
//! (the same detection commercial cartridges rely on):
//! - **SRAM** (32 KiB): plain byte-wide memory.
//! - **Flash 64 KiB / 128 KiB**: the full command state machine from
//!   GBATEK — unlock sequence (0xAA@0x5555, 0x55@0x2AAA), program byte
//!   (bits can only be cleared; erase restores 0xFF), chip/sector
//!   erase, ID mode, and 128 KiB bank switching.
//! - EEPROM is deliberately not here: FOMT does not use it; it lands
//!   when a target game needs it.
//!
//! All accesses are byte-wide, like the hardware: the bus special-cases
//! the 0x0E000000 region so 16/32-bit accesses collapse to the single
//! byte the chip would actually see.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveKind {
    Sram,
    Flash64,
    Flash128,
}

impl SaveKind {
    pub fn size(self) -> usize {
        match self {
            SaveKind::Sram => 32 * 1024,
            SaveKind::Flash64 => 64 * 1024,
            SaveKind::Flash128 => 128 * 1024,
        }
    }
    fn maker_id(self) -> u8 {
        match self {
            // Macronix for flash; SRAM has no ID protocol.
            SaveKind::Flash64 | SaveKind::Flash128 => 0xC2,
            SaveKind::Sram => 0,
        }
    }
    fn device_id(self) -> u8 {
        match self {
            SaveKind::Flash64 => 0x1C,  // MX29L512-class
            SaveKind::Flash128 => 0x09, // MX29L010-class
            SaveKind::Sram => 0,
        }
    }
}

/// Scan a ROM image for the embedded save-type string.
pub fn detect_kind(rom: &[u8]) -> SaveKind {
    let find = |needle: &[u8]| {
        rom.windows(needle.len()).any(|w| w == needle)
    };
    if find(b"FLASH1M_V") {
        SaveKind::Flash128
    } else if find(b"FLASH512_V") || find(b"FLASH_V") {
        SaveKind::Flash64
    } else {
        // SRAM_V, EEPROM_V (unimplemented), or no string: fall back to
        // SRAM, the most permissive behaviour.
        SaveKind::Sram
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlashSeq {
    Ready,
    Unlock1,      // got 0xAA @ 0x5555
    Unlock2,      // got 0x55 @ 0x2AAA, next write is a command
    Program,      // next write stores a byte
    EraseSetup,   // got 0x80, expect a fresh unlock then erase command
    EraseUnlock1,
    EraseUnlock2, // next write: 0x10 chip erase / 0x30 sector erase
    BankSwitch,   // next write to 0x0000 selects the bank
}

pub struct SaveMemory {
    pub kind: SaveKind,
    pub data: Vec<u8>,
    pub dirty: bool,
    seq: FlashSeq,
    id_mode: bool,
    bank: u8,
}

impl SaveMemory {
    pub fn new(kind: SaveKind) -> Self {
        Self {
            kind,
            data: vec![0xFF; kind.size()],
            dirty: false,
            seq: FlashSeq::Ready,
            id_mode: false,
            bank: 0,
        }
    }

    pub fn load_contents(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.data.len());
        self.data[..n].copy_from_slice(&bytes[..n]);
        self.dirty = false;
    }

    fn window(&self, addr: u32) -> usize {
        let off = (addr & 0xFFFF) as usize;
        match self.kind {
            SaveKind::Sram => (addr as usize) & 0x7FFF,
            SaveKind::Flash64 => off,
            SaveKind::Flash128 => usize::from(self.bank) * 0x10000 + off,
        }
    }

    pub fn read(&self, addr: u32) -> u8 {
        if self.id_mode && matches!(self.kind, SaveKind::Flash64 | SaveKind::Flash128) {
            match addr & 0xFFFF {
                0 => return self.kind.maker_id(),
                1 => return self.kind.device_id(),
                _ => {}
            }
        }
        self.data[self.window(addr)]
    }

    pub fn write(&mut self, addr: u32, value: u8) {
        match self.kind {
            SaveKind::Sram => {
                let off = self.window(addr);
                if self.data[off] != value {
                    self.data[off] = value;
                    self.dirty = true;
                }
            }
            SaveKind::Flash64 | SaveKind::Flash128 => self.flash_write(addr & 0xFFFF, value),
        }
    }

    fn flash_write(&mut self, addr: u32, value: u8) {
        use FlashSeq::*;
        match self.seq {
            Ready => {
                if addr == 0x5555 && value == 0xAA {
                    self.seq = Unlock1;
                }
            }
            Unlock1 => {
                self.seq = if addr == 0x2AAA && value == 0x55 { Unlock2 } else { Ready };
            }
            Unlock2 => {
                if addr != 0x5555 {
                    self.seq = Ready;
                    return;
                }
                match value {
                    0x90 => {
                        self.id_mode = true;
                        self.seq = Ready;
                    }
                    0xF0 => {
                        self.id_mode = false;
                        self.seq = Ready;
                    }
                    0xA0 => self.seq = Program,
                    0x80 => self.seq = EraseSetup,
                    0xB0 => self.seq = BankSwitch,
                    _ => self.seq = Ready,
                }
            }
            Program => {
                // addr is the 16-bit offset within the current bank.
                let target = match self.kind {
                    SaveKind::Flash128 => usize::from(self.bank) * 0x10000 + addr as usize,
                    _ => addr as usize,
                };
                let old = self.data[target];
                let new = old & value; // programming can only clear bits
                if new != old {
                    self.data[target] = new;
                    self.dirty = true;
                }
                self.seq = Ready;
            }
            EraseSetup => {
                self.seq = if addr == 0x5555 && value == 0xAA { EraseUnlock1 } else { Ready };
            }
            EraseUnlock1 => {
                self.seq = if addr == 0x2AAA && value == 0x55 { EraseUnlock2 } else { Ready };
            }
            EraseUnlock2 => {
                match value {
                    0x10 if addr == 0x5555 => {
                        // Chip erase: current bank for 128K (the other
                        // bank is erased by switching to it, as games do).
                        let base = match self.kind {
                            SaveKind::Flash128 => usize::from(self.bank) * 0x10000,
                            _ => 0,
                        };
                        for b in &mut self.data[base..base + 0x10000] {
                            *b = 0xFF;
                        }
                        self.dirty = true;
                    }
                    0x30 => {
                        // Sector erase: 4 KiB sector containing addr.
                        let base = match self.kind {
                            SaveKind::Flash128 => usize::from(self.bank) * 0x10000,
                            _ => 0,
                        };
                        let sector = base + (addr as usize & !0xFFF);
                        for b in &mut self.data[sector..sector + 0x1000] {
                            *b = 0xFF;
                        }
                        self.dirty = true;
                    }
                    _ => {}
                }
                self.seq = Ready;
            }
            BankSwitch => {
                if addr == 0x0000 && self.kind == SaveKind::Flash128 {
                    self.bank = value & 1;
                }
                self.seq = Ready;
            }
        }
    }

    /// Snapshot pieces the save-state format needs beyond the raw data.
    pub fn state_words(&self) -> (u8, bool) {
        (self.bank, self.id_mode)
    }

    pub fn restore_state(&mut self, bank: u8, id_mode: bool, data: &[u8]) {
        self.bank = bank & 1;
        self.id_mode = id_mode;
        self.seq = FlashSeq::Ready;
        self.load_contents(data);
    }
}
