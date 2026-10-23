//! DMA — Phase 2: all four channels, immediate + VBlank + HBlank
//! start timing, increment/decrement/fixed/reload addressing, 16/32-bit
//! units, end-of-transfer IRQ. FIFO (sound) mode is Phase 3 with the APU.

use crate::bus::Bus;

pub const DMA_BASE: u32 = 0x0400_00B0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaTiming {
    Immediate,
    VBlank,
    HBlank,
    Special,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DmaChannel {
    pub src: u32,
    pub dst: u32,
    pub count: u32,
    pub control: u16,
}

impl DmaChannel {
    pub fn enabled(&self) -> bool {
        self.control & (1 << 15) != 0
    }
    pub fn timing(&self) -> DmaTiming {
        match (self.control >> 12) & 0x3 {
            0 => DmaTiming::Immediate,
            1 => DmaTiming::VBlank,
            2 => DmaTiming::HBlank,
            _ => DmaTiming::Special,
        }
    }
}

/// Read channel registers out of the IO page. Called on demand — CPU
/// writes land in plain IO memory, so channels are re-latched whenever
/// the core samples them.
pub fn latch(bus: &Bus, ch: usize) -> DmaChannel {
    let base = DMA_BASE + (ch as u32) * 12;
    DmaChannel {
        src: bus.read32(base) & 0x0FFF_FFFF,
        dst: bus.read32(base + 4) & 0x0FFF_FFFF,
        count: u32::from(bus.read16(base + 8)),
        control: bus.read16(base + 10),
    }
}

fn write_control(bus: &mut Bus, ch: usize, control: u16) {
    let addr = DMA_BASE + (ch as u32) * 12 + 10;
    bus.write16(addr, control);
}

/// Run one transfer. Returns true when the channel's IRQ is enabled and
/// the transfer finished (the core then raises the DMA IRQ bit).
pub fn run_transfer(bus: &mut Bus, ch: usize, chan: &DmaChannel) -> bool {
    let ctrl = chan.control;
    let word = ctrl & (1 << 10) != 0;
    let unit: u32 = if word { 4 } else { 2 };
    let max_count: u32 = if ch == 3 { 0x1_0000 } else { 0x4000 };
    let count = if chan.count == 0 { max_count } else { u32::from(chan.count) };

    let dst_mode = (ctrl >> 5) & 0x3;
    let src_mode = (ctrl >> 7) & 0x3;
    let step = |mode: u16, is_src: bool, addr: u32| -> u32 {
        match mode {
            0 => addr.wrapping_add(unit),
            1 => addr.wrapping_sub(unit),
            2 => addr,
            _ => {
                if is_src {
                    addr // src reload is not a hardware behaviour
                } else {
                    addr.wrapping_add(unit) // dst reload handled by re-latch per trigger
                }
            }
        }
    };

    let mut src = chan.src;
    let mut dst = chan.dst;
    for _ in 0..count {
        if word {
            let v = bus.read32(src & !3);
            bus.write32(dst & !3, v);
        } else {
            let v = bus.read16(src & !1);
            bus.write16(dst & !1, v);
        }
        src = step(src_mode, true, src);
        dst = step(dst_mode, false, dst);
    }

    let irq = ctrl & (1 << 14) != 0;
    let repeat = ctrl & (1 << 9) != 0;
    if !repeat || chan.timing() == DmaTiming::Immediate {
        write_control(bus, ch, ctrl & !(1 << 15));
    }
    irq
}
