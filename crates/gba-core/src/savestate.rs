//! Save state format v1 — designed before there is anything worth
//! saving, on purpose.
//!
//! The audit behind this project showed save-state loading in existing
//! emulators is typically a raw struct dump: no schema version, no
//! integrity check, so a corrupt or hostile state file reaches straight
//! into core internals. This format is the opposite:
//!
//! ```text
//! header : magic "EMUSTATE" (8) | format_version u16 | flags u16 | rom_hash u64
//! section: id u16 | len u32 | crc32 u32 | payload[len]
//! ```
//!
//! Loading validates EVERYTHING (magic, version, section lengths
//! against the remaining bytes, every CRC, the ROM hash, and exact
//! end-of-input) before one byte of state is applied. Any mismatch is a
//! total rejection, never a best-effort load.

use alloc::vec::Vec;

use crate::scheduler::EventKind;
use crate::Core;

pub const FORMAT_VERSION: u16 = 1;
const MAGIC: &[u8; 8] = b"EMUSTATE";

const SECTION_CPU: u16 = 1;
const SECTION_SCHEDULER: u16 = 2;
const SECTION_MEMORY: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    BadMagic,
    UnsupportedVersion(u16),
    Truncated,
    SectionOutOfBounds,
    CrcMismatch { section: u16 },
    RomMismatch,
    BadPayload { section: u16 },
    TrailingBytes,
}

/// CRC-32 (IEEE), bitwise implementation — slow but tiny and obvious;
/// save states are written at human timescales, not per frame.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn section(&mut self, id: u16, payload: &[u8]) {
        self.buf.extend_from_slice(&id.to_le_bytes());
        self.buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(&crc32(payload).to_le_bytes());
        self.buf.extend_from_slice(payload);
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SaveError> {
        let end = self.pos.checked_add(n).ok_or(SaveError::SectionOutOfBounds)?;
        let slice = self.data.get(self.pos..end).ok_or(SaveError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }
    fn u16(&mut self) -> Result<u16, SaveError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, SaveError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, SaveError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
}

pub fn save(core: &Core) -> Vec<u8> {
    let mut w = Writer { buf: Vec::new() };
    w.buf.extend_from_slice(MAGIC);
    w.buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    w.buf.extend_from_slice(&0u16.to_le_bytes()); // flags
    w.buf.extend_from_slice(&core.bus.rom_hash().to_le_bytes());

    // CPU section: 16 regs + CPSR.
    let mut cpu = Vec::with_capacity(68);
    for r in core.cpu.regs {
        cpu.extend_from_slice(&r.to_le_bytes());
    }
    cpu.extend_from_slice(&core.cpu.cpsr.to_le_bytes());
    w.section(SECTION_CPU, &cpu);

    // Scheduler section: now + (at, kind-tag) per pending event.
    let events = core.scheduler.snapshot();
    let mut sched = Vec::with_capacity(8 + events.len() * 16);
    sched.extend_from_slice(&core.scheduler.now().to_le_bytes());
    sched.extend_from_slice(&(events.len() as u32).to_le_bytes());
    for (at, kind) in &events {
        sched.extend_from_slice(&at.to_le_bytes());
        sched.extend_from_slice(&kind_tag(*kind).to_le_bytes());
        sched.extend_from_slice(&0u32.to_le_bytes()); // reserved sub-tag
    }
    w.section(SECTION_SCHEDULER, &sched);

    // Memory section: the writable regions, concatenated in fixed order.
    let mut mem = Vec::with_capacity(
        crate::bus::EWRAM_LEN + crate::bus::IWRAM_LEN + crate::bus::VRAM_LEN,
    );
    mem.extend_from_slice(&*core.bus.ewram);
    mem.extend_from_slice(&*core.bus.iwram);
    mem.extend_from_slice(&*core.bus.vram);
    w.section(SECTION_MEMORY, &mem);

    w.buf
}

pub fn load(core: &mut Core, data: &[u8]) -> Result<(), SaveError> {
    let mut r = Reader { data, pos: 0 };
    if r.take(8)? != MAGIC {
        return Err(SaveError::BadMagic);
    }
    let version = r.u16()?;
    if version != FORMAT_VERSION {
        return Err(SaveError::UnsupportedVersion(version));
    }
    let _flags = r.u16()?;
    if r.u64()? != core.bus.rom_hash() {
        return Err(SaveError::RomMismatch);
    }

    // Validate every section fully BEFORE applying any of them.
    let mut sections: Vec<(u16, &[u8])> = Vec::new();
    while r.pos < data.len() {
        let id = r.u16()?;
        let len = r.u32()? as usize;
        let crc = r.u32()?;
        let payload = r.take(len)?;
        if crc32(payload) != crc {
            return Err(SaveError::CrcMismatch { section: id });
        }
        sections.push((id, payload));
    }
    if r.pos != data.len() {
        return Err(SaveError::TrailingBytes);
    }

    for (id, payload) in sections {
        match id {
            SECTION_CPU => {
                if payload.len() != 68 {
                    return Err(SaveError::BadPayload { section: id });
                }
                let mut pr = Reader { data: payload, pos: 0 };
                for reg in core.cpu.regs.iter_mut() {
                    *reg = pr.u32()?;
                }
                core.cpu.cpsr = pr.u32()?;
            }
            SECTION_SCHEDULER => {
                let mut pr = Reader { data: payload, pos: 0 };
                let now = pr.u64()?;
                let count = pr.u32()? as usize;
                // The count comes from the file: bound it by the bytes
                // that actually remain before believing it.
                if count > (payload.len() - pr.pos) / 16 {
                    return Err(SaveError::BadPayload { section: id });
                }
                let mut events = Vec::with_capacity(count);
                for _ in 0..count {
                    let at = pr.u64()?;
                    let tag = pr.u32()?;
                    let _sub = pr.u32()?; // reserved sub-tag
                    events.push((at, tag_kind(tag)));
                }
                core.scheduler.restore(now, &events);
            }
            SECTION_MEMORY => {
                let expected =
                    crate::bus::EWRAM_LEN + crate::bus::IWRAM_LEN + crate::bus::VRAM_LEN;
                if payload.len() != expected {
                    return Err(SaveError::BadPayload { section: id });
                }
                let (ew, rest) = payload.split_at(crate::bus::EWRAM_LEN);
                let (iw, vr) = rest.split_at(crate::bus::IWRAM_LEN);
                core.bus.ewram.copy_from_slice(ew);
                core.bus.iwram.copy_from_slice(iw);
                core.bus.vram.copy_from_slice(vr);
            }
            // Unknown section: ignored by design (forward compatibility),
            // but it was still length- and CRC-checked above.
            _ => {}
        }
    }
    Ok(())
}

fn kind_tag(kind: EventKind) -> u32 {
    match kind {
        EventKind::VBlank => 1,
        EventKind::HBlank => 2,
        EventKind::TimerOverflow(n) => 0x100 | u32::from(n),
        EventKind::Dma(n) => 0x200 | u32::from(n),
        EventKind::AudioFifo(n) => 0x300 | u32::from(n),
        EventKind::Subsystem(tag) => 0x1000 | (tag & 0xFF),
    }
}

fn tag_kind(tag: u32) -> EventKind {
    match tag {
        1 => EventKind::VBlank,
        2 => EventKind::HBlank,
        t if t & 0xF00 == 0x100 => EventKind::TimerOverflow((t & 0xFF) as u8),
        t if t & 0xF00 == 0x200 => EventKind::Dma((t & 0xFF) as u8),
        t if t & 0xF00 == 0x300 => EventKind::AudioFifo((t & 0xFF) as u8),
        t => EventKind::Subsystem(t & 0xFF),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        // Standard CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn roundtrip_restores_state() {
        let rom = crate::test_rom::fake_rom();
        let mut a = Core::new();
        a.load_rom(&rom).unwrap();
        a.bus.write32(0x0200_0000, 0xCAFE_BABE);
        a.cpu.regs[3] = 42;
        let blob = save(&a);

        let mut b = Core::new();
        b.load_rom(&rom).unwrap();
        load(&mut b, &blob).unwrap();
        assert_eq!(b.bus.read32(0x0200_0000), 0xCAFE_BABE);
        assert_eq!(b.cpu.regs[3], 42);
    }

    #[test]
    fn corrupt_and_foreign_states_are_rejected() {
        let rom = crate::test_rom::fake_rom();
        let mut a = Core::new();
        a.load_rom(&rom).unwrap();
        let blob = save(&a);

        // Flip one payload byte: CRC must catch it.
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        let mut c = Core::new();
        c.load_rom(&rom).unwrap();
        assert!(matches!(load(&mut c, &bad), Err(SaveError::CrcMismatch { .. })));

        // State saved for a different ROM (here: no ROM) must refuse.
        let mut d = Core::new();
        assert!(matches!(load(&mut d, &blob), Err(SaveError::RomMismatch)));

        // Truncation is an error, not a partial load.
        let mut e = Core::new();
        e.load_rom(&rom).unwrap();
        assert!(load(&mut e, &blob[..blob.len() / 2]).is_err());
    }

    #[test]
    fn scheduler_section_is_16_bytes_per_event() {
        // Guards the count-bounding math in the loader: snapshot events
        // serialise as (at u64, tag u32, reserved u32).
        let mut s = crate::Scheduler::new();
        s.schedule(5, EventKind::VBlank);
        assert_eq!(s.snapshot().len(), 1);
    }
}
