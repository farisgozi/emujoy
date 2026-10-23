//! Phase 3 acceptance tests: save hardware (SRAM, Flash 64K/128K),
//! save-type detection, and save survival across repeated
//! program/erase + save-state round trips.

use gba_core::save::{detect_kind, SaveKind, SaveMemory};
use gba_core::savestate;
use gba_core::Core;

const SAVE_BASE: u32 = 0x0E00_0000;

fn flash_with_string(marker: &str) -> (Core, Vec<u8>) {
    // Build a minimal valid ROM carrying a save-type string.
    let mut rom = vec![0u8; 0x400];
    rom[0xA0..0xA6].copy_from_slice(b"PANEN!");
    rom[0xAC..0xB0].copy_from_slice(b"TEST");
    rom[0xB0..0xB2].copy_from_slice(b"01");
    rom[0xB2] = 0x96;
    let mut sum: u8 = 0x19;
    for &b in &rom[0xA0..=0xBC] {
        sum = sum.wrapping_add(b);
    }
    rom[0xBD] = 0u8.wrapping_sub(sum);
    let s = format!("{marker}_V123");
    rom[0x100..0x100 + s.len()].copy_from_slice(s.as_bytes());
    let mut core = Core::new();
    core.load_rom(&rom).unwrap();
    (core, rom)
}

fn cmd(bus: &mut gba_core::Bus, addr: u32, value: u8) {
    bus.write8(SAVE_BASE + addr, value);
}
fn unlock(bus: &mut gba_core::Bus) {
    cmd(bus, 0x5555, 0xAA);
    cmd(bus, 0x2AAA, 0x55);
}
fn program_byte(bus: &mut gba_core::Bus, addr: u32, value: u8) {
    unlock(bus);
    cmd(bus, 0x5555, 0xA0);
    cmd(bus, addr, value);
}
fn erase_sector(bus: &mut gba_core::Bus, addr: u32) {
    unlock(bus);
    cmd(bus, 0x5555, 0x80);
    unlock(bus);
    cmd(bus, addr, 0x30);
}

#[test]
fn detection_from_rom_strings() {
    assert_eq!(detect_kind(b"..FLASH1M_V103.."), SaveKind::Flash128);
    assert_eq!(detect_kind(b"..FLASH512_V131.."), SaveKind::Flash64);
    assert_eq!(detect_kind(b"..FLASH_V124.."), SaveKind::Flash64);
    assert_eq!(detect_kind(b"..SRAM_V110.."), SaveKind::Sram);
    assert_eq!(detect_kind(b"nothing here"), SaveKind::Sram);
    let (core, _) = flash_with_string("FLASH1M");
    assert_eq!(core.bus.save.kind, SaveKind::Flash128);
    assert_eq!(core.save_data().len(), 128 * 1024);
}

#[test]
fn sram_is_plain_byte_memory() {
    let mut mem = SaveMemory::new(SaveKind::Sram);
    mem.write(0x0E00_0010, 0x42);
    assert_eq!(mem.read(0x0E00_0010), 0x42);
    assert!(mem.dirty);
    // 16-bit bus view duplicates the byte across lanes.
    let mut core = Core::new();
    core.bus.write8(0x0E00_0020, 0xAB);
    assert_eq!(core.bus.read16(0x0E00_0020), 0xABAB);
}

#[test]
fn flash_program_and_bit_clearing_semantics() {
    let (mut core, _) = flash_with_string("FLASH");
    let bus = &mut core.bus;
    assert_eq!(bus.read8(SAVE_BASE + 0x123), 0xFF, "fresh flash is erased");
    program_byte(bus, 0x123, 0x3C);
    assert_eq!(bus.read8(SAVE_BASE + 0x123), 0x3C);
    // Programming can only clear bits: 0x3C & 0xF0 = 0x30.
    program_byte(bus, 0x123, 0xF0);
    assert_eq!(bus.read8(SAVE_BASE + 0x123), 0x30);
    // Writes without the unlock sequence are ignored.
    bus.write8(SAVE_BASE + 0x124, 0x00);
    assert_eq!(bus.read8(SAVE_BASE + 0x124), 0xFF);
}

#[test]
fn flash_id_mode_and_sector_erase() {
    let (mut core, _) = flash_with_string("FLASH512");
    let bus = &mut core.bus;
    unlock(bus);
    cmd(bus, 0x5555, 0x90); // enter ID mode
    assert_eq!(bus.read8(SAVE_BASE), 0xC2, "Macronix maker id");
    assert_eq!(bus.read8(SAVE_BASE + 1), 0x1C, "64K device id");
    unlock(bus);
    cmd(bus, 0x5555, 0xF0); // exit ID mode
    assert_eq!(bus.read8(SAVE_BASE), 0xFF, "ID mode off: data visible again");

    program_byte(bus, 0x0800, 0x55);
    program_byte(bus, 0x1800, 0x66);
    erase_sector(bus, 0x0800);
    assert_eq!(bus.read8(SAVE_BASE + 0x0800), 0xFF, "sector 0 erased");
    assert_eq!(bus.read8(SAVE_BASE + 0x1800), 0x66, "sector 1 untouched");
}

#[test]
fn flash128_bank_switching() {
    let (mut core, _) = flash_with_string("FLASH1M");
    let bus = &mut core.bus;
    program_byte(bus, 0x0010, 0x11); // bank 0
    unlock(bus);
    cmd(bus, 0x5555, 0xB0);
    cmd(bus, 0x0000, 0x01); // select bank 1
    assert_eq!(bus.read8(SAVE_BASE + 0x0010), 0xFF, "bank 1 starts erased");
    program_byte(bus, 0x0010, 0x22);
    assert_eq!(bus.read8(SAVE_BASE + 0x0010), 0x22);
    unlock(bus);
    cmd(bus, 0x5555, 0xB0);
    cmd(bus, 0x0000, 0x00); // back to bank 0
    assert_eq!(bus.read8(SAVE_BASE + 0x0010), 0x11, "bank 0 data intact");
    let (bank, _) = bus.save.state_words();
    assert_eq!(bank, 0);
}

#[test]
fn save_survival_twenty_cycles() {
    // The FOMT criterion in proxy form: 20 rounds of (write a fresh
    // pattern through the real flash protocol, snapshot a save state,
    // wipe the machine, restore, verify byte-for-byte).
    let (mut core, rom) = flash_with_string("FLASH1M");
    for cycle in 0..20u8 {
        let pattern = |i: usize| cycle ^ (i as u8).wrapping_mul(37);
        // Program 64 bytes in bank 0 and 64 in bank 1.
        for i in 0..64u32 {
            program_byte(&mut core.bus, 0x2000 + i, pattern(i as usize));
        }
        let snapshot = savestate::save(&core);
        let saved_bytes = core.save_data().to_vec();

        // Fresh machine, same ROM, restore the state.
        let mut fresh = Core::new();
        fresh.load_rom(&rom).unwrap();
        savestate::load(&mut fresh, &snapshot).unwrap();
        assert_eq!(fresh.save_data(), &saved_bytes[..], "cycle {cycle}: save contents");
        for i in 0..64u32 {
            assert_eq!(
                fresh.bus.read8(SAVE_BASE + 0x2000 + i),
                pattern(i as usize),
                "cycle {cycle} byte {i}"
            );
        }
        // Erase for the next cycle through the protocol.
        erase_sector(&mut core.bus, 0x2000);
    }
}

#[test]
fn savestate_v2_restores_timer_and_pending_irq() {
    let (mut core, rom) = flash_with_string("SRAM");
    core.bus.write16(0x0400_0100, 0x1234); // TM0 reload visible counter path
    core.pending_irq = 0x0009;
    let blob = savestate::save(&core);
    let mut fresh = Core::new();
    fresh.load_rom(&rom).unwrap();
    savestate::load(&mut fresh, &blob).unwrap();
    assert_eq!(fresh.pending_irq, 0x0009);
}

// -- Phase 4c: EEPROM serial protocol --------------------------------

#[test]
fn eeprom_write_then_read_roundtrip() {
    let mut m = gba_core::save::SaveMemory::new(gba_core::save::SaveKind::Eeprom);
    let send = |m: &mut gba_core::save::SaveMemory, bits: &[u8]| {
        for &b in bits {
            m.eeprom_write_bit(b);
        }
    };
    // Write frame: start(1), write(1), addr 0b000101 (5), 64 data
    // bits (0x0123456789ABCDEF MSB-first), stop(0).
    let mut frame = vec![1u8, 1];
    for i in (0..6).rev() {
        frame.push(((5 >> i) & 1) as u8);
    }
    let value: u64 = 0x0123_4567_89AB_CDEF;
    for i in (0..64).rev() {
        frame.push(((value >> i) & 1) as u8);
    }
    frame.push(0);
    send(&mut m, &frame);
    assert_eq!(&m.data[40..48], &value.to_be_bytes());
    assert!(m.dirty);
    // Busy while programming, then ready.
    assert_eq!(m.eeprom_read_bit(), 0);
    for _ in 0..1100 {
        m.eeprom_read_bit();
    }
    // Read frame: start(1), read(0), addr, stop(0).
    let mut rframe = vec![1u8, 0];
    for i in (0..6).rev() {
        rframe.push(((5 >> i) & 1) as u8);
    }
    rframe.push(0);
    send(&mut m, &rframe);
    // 4 dummy zero bits, then the 64 data bits MSB-first.
    let mut got: u64 = 0;
    for i in 0..68 {
        let bit = m.eeprom_read_bit();
        if i >= 4 {
            got = (got << 1) | u64::from(bit);
        } else {
            assert_eq!(bit, 0, "dummy bits are 0");
        }
    }
    assert_eq!(got, value);
    // After the transfer the line idles at 1.
    assert_eq!(m.eeprom_read_bit(), 1);
}
