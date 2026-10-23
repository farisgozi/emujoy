//! Cartridge save hardware — Phase 3.
//!
//! Three kinds, selected by the save-type string embedded in the ROM
//! (the same detection commercial cartridges rely on):
//! - **SRAM** (32 KiB): plain byte-wide memory.
//! - **Flash 64 KiB / 128 KiB**: the full command state machine from
//!   GBATEK — unlock sequence (0xAA@0x5555, 0x55@0x2AAA), program byte
//!   (bits can only be cleared; erase restores 0xFF), chip/sector
//!   erase, ID mode, and 128 KiB bank switching.
//! - **EEPROM** (512 B / 8 KiB): the serial 1-bit protocol in the
//!   0x0D000000 window — request frames (start bit, read/write bit,
//!   fixed-width address, data), busy polling, and 64-bit block
//!   transfers. The address width is a fixed chip property (6 or 14
//!   bits); it cannot be parsed out of the bit stream (a 0 address
//!   bit is indistinguishable from the stop bit), so it follows the
//!   size of the loaded save image.
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
    Eeprom,
}

impl SaveKind {
    pub fn size(self) -> usize {
        match self {
            SaveKind::Sram => 32 * 1024,
            SaveKind::Flash64 => 64 * 1024,
            SaveKind::Flash128 => 128 * 1024,
            // Backing buffer is the largest chip; the used capacity
            // follows the learned address width (see contents_len).
            SaveKind::Eeprom => 8 * 1024,
        }
    }
    fn maker_id(self) -> u8 {
        match self {
            // Macronix for flash; SRAM/EEPROM have no ID protocol.
            SaveKind::Flash64 | SaveKind::Flash128 => 0xC2,
            SaveKind::Sram | SaveKind::Eeprom => 0,
        }
    }
    fn device_id(self) -> u8 {
        match self {
            SaveKind::Flash64 => 0x1C,  // MX29L512-class
            SaveKind::Flash128 => 0x09, // MX29L010-class
            SaveKind::Sram | SaveKind::Eeprom => 0,
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
    } else if find(b"EEPROM_V") {
        SaveKind::Eeprom
    } else {
        // SRAM_V or no string: fall back to SRAM, the most permissive
        // behaviour.
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

/// Serial EEPROM protocol state (see the module docs).
#[derive(Debug, Clone)]
pub struct EepromProto {
    /// Address width in bits — a fixed property of the chip: 6 on
    /// the 512-byte (4 Kbit) part, 14 on the 8 KiB (64 Kbit) part.
    /// The core cannot see the width in the protocol stream (a 0
    /// address bit looks exactly like the stop bit), so it follows
    /// the loaded save image: >512 bytes means the big chip.
    pub addr_bits: u8,
    state: EState,
    addr: u32,
    addr_count: u8,
    data: [u8; 8],
    data_count: u8,
    out_addr: u32,
    out_pos: u8,
    out_active: bool,
    busy_reads: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EState {
    Start,
    Command,
    ReadAddr,
    ReadStop,
    WriteAddr,
    WriteData,
    WriteStop,
}

impl Default for EepromProto {
    fn default() -> Self {
        Self {
            addr_bits: 6,
            state: EState::Start,
            addr: 0,
            addr_count: 0,
            data: [0; 8],
            data_count: 0,
            out_addr: 0,
            out_pos: 0,
            out_active: false,
            busy_reads: 0,
        }
    }
}

pub struct SaveMemory {
    pub kind: SaveKind,
    pub data: Vec<u8>,
    pub dirty: bool,
    seq: FlashSeq,
    id_mode: bool,
    bank: u8,
    /// RefCell so the bus's read path (&self) can advance the serial
    /// output state — every EEPROM read consumes a bit.
    pub eeprom: core::cell::RefCell<EepromProto>,
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
            eeprom: core::cell::RefCell::new(EepromProto::default()),
        }
    }

    /// Length of the meaningful save contents (EEPROM chips expose
    /// only the capacity their address width reaches).
    pub fn contents_len(&self) -> usize {
        match self.kind {
            SaveKind::Eeprom => ((1usize << self.eeprom.borrow().addr_bits) * 8).min(self.data.len()),
            _ => self.data.len(),
        }
    }

    // -- EEPROM serial protocol -------------------------------------

    /// Feed one bit (bit 0 of a write to the 0x0D window).
    pub fn eeprom_write_bit(&mut self, bit: u8) {
        let bit = bit & 1;
        let mut guard = self.eeprom.borrow_mut();
        let e = &mut *guard;
        match e.state {
            EState::Start => {
                if bit == 1 {
                    e.state = EState::Command;
                    e.out_active = false;
                }
            }
            EState::Command => {
                e.addr = 0;
                e.addr_count = 0;
                e.state = if bit == 1 { EState::WriteAddr } else { EState::ReadAddr };
            }
            EState::ReadAddr => {
                e.addr = (e.addr << 1) | u32::from(bit);
                e.addr_count += 1;
                if e.addr_count >= e.addr_bits {
                    e.state = EState::ReadStop;
                }
            }
            EState::ReadStop => {
                // One stop bit (value ignored), then the chip starts
                // driving the output on subsequent reads.
                e.out_addr = e.addr;
                e.out_pos = 0;
                e.out_active = true;
                e.state = EState::Start;
            }
            EState::WriteAddr => {
                e.addr = (e.addr << 1) | u32::from(bit);
                e.addr_count += 1;
                if e.addr_count >= e.addr_bits {
                    e.data = [0; 8];
                    e.data_count = 0;
                    e.state = EState::WriteData;
                }
            }
            EState::WriteData => {
                let idx = (e.data_count / 8) as usize;
                e.data[idx] = (e.data[idx] << 1) | bit;
                e.data_count += 1;
                if e.data_count >= 64 {
                    e.state = EState::WriteStop;
                }
            }
            EState::WriteStop => {
                if bit == 0 {
                    // (Computed from addr_bits directly: contents_len
                    // would re-borrow the RefCell we already hold.)
                    let cap = ((1usize << e.addr_bits) * 8).min(self.data.len());
                    let blocks = ((cap / 8).max(1)) as u32;
                    let off = ((e.addr % blocks) * 8) as usize;
                    if self.data[off..off + 8] != e.data {
                        self.data[off..off + 8].copy_from_slice(&e.data);
                        self.dirty = true;
                    }
                    e.busy_reads = 1000; // programming time, polled by games
                }
                e.state = EState::Start;
            }
        }
    }

    /// Read one bit from the 0x0D window: the busy flag (0 = busy,
    /// 1 = ready) or the next output bit of a read transfer.
    pub fn eeprom_read_bit(&self) -> u8 {
        let mut guard = self.eeprom.borrow_mut();
        let e = &mut *guard;
        if e.busy_reads > 0 {
            e.busy_reads -= 1;
            return 0;
        }
        if !e.out_active {
            return 1;
        }
        let pos = e.out_pos;
        e.out_pos += 1;
        if pos < 4 {
            return 0; // dummy bits before the data
        }
        let bitpos = pos - 4;
        if bitpos >= 64 {
            e.out_active = false;
            return 1;
        }
        let cap = ((1usize << e.addr_bits) * 8).min(self.data.len());
        let blocks = ((cap / 8).max(1)) as u32;
        let off = ((e.out_addr % blocks) * 8) as usize;
        let byte = self.data[off + (bitpos / 8) as usize];
        (byte >> (7 - (bitpos % 8))) & 1
    }

    pub fn load_contents(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.data.len());
        self.data[..n].copy_from_slice(&bytes[..n]);
        self.dirty = false;
        if self.kind == SaveKind::Eeprom && bytes.len() > 512 {
            // A full-size image means the 8 KiB chip (14-bit
            // addresses in its protocol).
            self.eeprom.borrow_mut().addr_bits = 14;
        }
    }

    fn window(&self, addr: u32) -> usize {
        let off = (addr & 0xFFFF) as usize;
        match self.kind {
            SaveKind::Sram => (addr as usize) & 0x7FFF,
            SaveKind::Flash64 => off,
            SaveKind::Flash128 => usize::from(self.bank) * 0x10000 + off,
            SaveKind::Eeprom => 0, // serial protocol; never indexed
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
            SaveKind::Eeprom => {} // the 0x0E window is inert for EEPROM carts
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
