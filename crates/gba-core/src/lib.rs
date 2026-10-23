//! Clean-room GBA emulation core — Phase 0 skeleton.
//!
//! What exists in Phase 0 (and nothing more):
//! - [`Scheduler`]: the single source of time. Every subsystem will book
//!   events here instead of counting cycles privately.
//! - [`Bus`]: the GBA memory map as address decoding over owned buffers.
//! - [`Core`]: one struct that owns ALL state by value. There is no
//!   global mutable state anywhere in this crate, so two `Core`s can
//!   live in one process (run-ahead, TAS, headless tests, link play).
//! - [`savestate`]: versioned, checksummed save-state format v1.
//! - [`Host`]: the narrow interface the frontend implements. The core
//!   never touches a real file, socket, thread, or clock.
//!
//! Phase 1 added the ARM7TDMI interpreter (`cpu` module). Phase 2 adds
//! the minimum boot system: timers + IRQ dispatch (with the BIOS
//! handler-pointer contract at 0x03007FFC when no BIOS is loaded),
//! four DMA channels, a scanline PPU (tile Mode 0, bitmap Modes 3/4,
//! basic OBJ), and skip-BIOS cartridge boot. What does NOT exist yet:
//! APU/sound, affine backgrounds, windows/blending, save hardware
//! (SRAM/Flash/EEPROM) — those are Phase 3, driven by FOMT.
#![no_std]

extern crate alloc;

pub mod bus;
pub mod cpu;
pub mod dma;
pub mod host;
pub mod ppu;
pub mod savestate;
pub mod scheduler;

pub use bus::Bus;
pub use cpu::Cpu;
pub use host::{Host, NullHost};
pub use ppu::Ppu;
pub use scheduler::{EventKind, Scheduler};

use alloc::boxed::Box;

/// GBA display size.
pub const SCREEN_W: usize = 240;
pub const SCREEN_H: usize = 160;
/// 16.78 MHz * (228 lines * 1232 cycles) — cycles in one frame.
pub const CYCLES_PER_FRAME: u64 = 280_896;
/// NTSC-like GBA refresh (~59.7275 Hz). Provided for the frontend's
/// frame pacing: locking presentation to exactly 60.000 Hz is a known
/// source of judder/audio crackle on other cores.
pub const FRAME_RATE_MILLIHZ: u32 = 59_727;

/// One hardware timer. The register file lives in IO memory; this
/// struct keeps the parts the register file cannot express (reload
/// value vs visible counter, prescaler phase, CPU-write detection).
#[derive(Debug, Clone, Copy, Default)]
struct Timer {
    counter: u16,
    reload: u16,
    phase: u32,
    displayed: u16,
    was_enabled: bool,
}

/// IO addresses the core mediates.
const REG_IF: u32 = 0x0400_0202;
const REG_IE: u32 = 0x0400_0200;
const REG_IME: u32 = 0x0400_0208;
const REG_DISPSTAT: u32 = 0x0400_0004;
const REG_KEYINPUT: u32 = 0x0400_0130;
const IRQ_HANDLER_PTR: u32 = 0x0300_7FFC;

/// The whole machine. Owns everything by value; create as many as you
/// like, they cannot interfere with each other.
pub struct Core {
    pub cpu: Cpu,
    pub scheduler: Scheduler,
    pub bus: Bus,
    pub ppu: Ppu,
    /// Framebuffer the PPU draws into.
    pub framebuffer: Box<[u32; SCREEN_W * SCREEN_H]>,
    /// How many frames have been presented. Handy in tests.
    pub frames_presented: u64,
    /// Authoritative pending-interrupt bits (mirrored into REG_IF).
    pub pending_irq: u16,
    timers: [Timer; 4],
    dma_prev_control: [u16; 4],
    keys_pressed: u16,
}

impl Core {
    pub fn new() -> Self {
        let mut core = Self {
            cpu: Cpu::new(),
            scheduler: Scheduler::new(),
            bus: Bus::new(),
            ppu: Ppu::new(),
            framebuffer: Box::new([0; SCREEN_W * SCREEN_H]),
            frames_presented: 0,
            pending_irq: 0,
            timers: [Timer::default(); 4],
            dma_prev_control: [0; 4],
            keys_pressed: 0,
        };
        core.set_io16(REG_KEYINPUT, 0x03FF);
        core
    }

    fn set_io16(&mut self, addr: u32, value: u16) {
        let off = (addr - 0x0400_0000) as usize;
        self.bus.io[off] = value as u8;
        self.bus.io[off + 1] = (value >> 8) as u8;
    }

    /// Set the current key state (bit set = pressed, GBA key order).
    /// KEYINPUT in IO memory is the inverse (active-low).
    pub fn set_keys(&mut self, pressed: u16) {
        self.keys_pressed = pressed;
        self.set_io16(REG_KEYINPUT, !pressed & 0x03FF);
    }

    /// Load a BIOS dump supplied by the user (never bundled).
    pub fn load_bios(&mut self, bios: &[u8]) {
        self.bus.load_bios(bios);
    }

    /// Set up the register state the real BIOS leaves behind and jump
    /// straight to the cartridge entry point (0x08000000). Stack
    /// addresses and mode layout follow GBATEK's description of the
    /// BIOS-to-cartridge handoff.
    pub fn boot_cartridge(&mut self) {
        use cpu::{BIT_F, BIT_I, MODE_IRQ, MODE_SVC, MODE_SYS};
        self.cpu.set_cpsr_full(MODE_IRQ | BIT_I | BIT_F);
        self.cpu.set_reg(13, 0x0300_7FA0);
        self.cpu.set_cpsr_full(MODE_SVC | BIT_I | BIT_F);
        self.cpu.set_reg(13, 0x0300_7FE0);
        self.cpu.set_cpsr_full(MODE_SYS);
        self.cpu.set_reg(13, 0x0300_7F00);
        self.cpu.set_pc(0x0800_0000);
        self.set_keys(0);
    }

    /// Boot into a loaded BIOS at address 0 (it will hand off to the
    /// cartridge itself when it is done).
    pub fn boot_bios(&mut self) {
        use cpu::{BIT_F, BIT_I, MODE_SVC};
        self.cpu.set_cpsr_full(MODE_SVC | BIT_I | BIT_F);
        self.cpu.set_reg(13, 0x0300_7FE0);
        self.cpu.set_pc(0);
    }

    /// Execute exactly one CPU instruction, then advance every device
    /// by its (Phase 1 approximate) cycle cost and service interrupts.
    pub fn step_instruction(&mut self) -> u32 {
        let cycles = self.cpu.step(&mut self.bus);
        let now = self.scheduler.now();
        self.scheduler.advance_to(now + u64::from(cycles));

        // CPU acknowledgement of interrupts (write-one-to-clear).
        let clear = self.bus.take_if_clear_mask();
        if clear != 0 {
            self.pending_irq &= !clear;
        }
        self.advance_devices(u64::from(cycles));
        self.set_io16(REG_IF, self.pending_irq);
        self.dispatch_irq();
        cycles
    }

    /// Execute instructions until the device clock has advanced at
    /// least `cycles`.
    pub fn run_cycles(&mut self, cycles: u64) {
        let deadline = self.scheduler.now().saturating_add(cycles);
        while self.scheduler.now() < deadline {
            self.step_instruction();
        }
    }

    fn advance_devices(&mut self, cycles: u64) {
        // Timers first: their overflows feed the IRQ bits below.
        let mut cascade_overflows: u32 = 0;
        for ch in 0..4 {
            let overflows = self.tick_timer(ch, cycles, cascade_overflows);
            if overflows > 0 {
                let ctrl = self.bus.read16(0x0400_0102 + (ch as u32) * 4);
                if ctrl & (1 << 6) != 0 {
                    self.pending_irq |= 1 << (3 + ch);
                }
            }
            cascade_overflows = overflows;
        }

        // PPU timing + rendering (split borrows via replace).
        let mut ppu = core::mem::replace(&mut self.ppu, Ppu::new());
        let events = ppu.advance(cycles, &mut self.bus, &mut self.framebuffer);
        self.ppu = ppu;
        let dispstat = self.bus.read16(REG_DISPSTAT);
        if events.vblank && dispstat & (1 << 3) != 0 {
            self.pending_irq |= 1 << 0;
        }
        if events.hblank && dispstat & (1 << 4) != 0 {
            self.pending_irq |= 1 << 1;
        }
        if events.vcounter_match && dispstat & (1 << 5) != 0 {
            self.pending_irq |= 1 << 2;
        }

        // DMA: VBlank/HBlank-triggered channels, then immediate ones
        // whose enable bit was newly written by the CPU.
        if events.vblank {
            self.run_timed_dma(dma::DmaTiming::VBlank);
        }
        if events.hblank && self.ppu.line < SCREEN_H as u16 {
            self.run_timed_dma(dma::DmaTiming::HBlank);
        }
        for ch in 0..4 {
            let chan = dma::latch(&self.bus, ch);
            let newly_enabled = chan.enabled() && self.dma_prev_control[ch] & (1 << 15) == 0;
            if newly_enabled && chan.timing() == dma::DmaTiming::Immediate {
                if dma::run_transfer(&mut self.bus, ch, &chan) {
                    self.pending_irq |= 1 << (8 + ch);
                }
            }
            self.dma_prev_control[ch] = dma::latch(&self.bus, ch).control;
        }
    }

    fn run_timed_dma(&mut self, timing: dma::DmaTiming) {
        for ch in 0..4 {
            let chan = dma::latch(&self.bus, ch);
            if chan.enabled() && chan.timing() == timing {
                if dma::run_transfer(&mut self.bus, ch, &chan) {
                    self.pending_irq |= 1 << (8 + ch);
                }
            }
            self.dma_prev_control[ch] = dma::latch(&self.bus, ch).control;
        }
    }

    /// One timer's advance. Returns its overflow count (the next
    /// timer's cascade input).
    fn tick_timer(&mut self, ch: usize, cycles: u64, cascade_in: u32) -> u32 {
        let base = 0x0400_0100 + (ch as u32) * 4;
        let counter_addr = base;
        let ctrl_addr = base + 2;
        let control = self.bus.read16(ctrl_addr);
        let t = &mut self.timers[ch];

        // CPU wrote the counter register? That sets the reload value.
        let visible = self.bus.read16(counter_addr);
        if visible != t.displayed {
            t.reload = visible;
            if control & (1 << 7) == 0 {
                t.counter = visible;
            }
        }
        if control & (1 << 7) == 0 {
            t.phase = 0;
            t.was_enabled = false;
            t.displayed = t.counter;
            self.bus.write16(counter_addr, t.counter);
            return 0;
        }
        if !t.was_enabled {
            // Enable rising edge: the counter starts from the reload
            // value, exactly like hardware.
            t.was_enabled = true;
            t.counter = t.reload;
        }
        let cascade = control & (1 << 2) != 0 && ch > 0;
        let ticks: u64 = if cascade {
            u64::from(cascade_in)
        } else {
            let divider = match control & 0x3 {
                0 => 1u64,
                1 => 64,
                2 => 256,
                _ => 1024,
            };
            t.phase += cycles as u32;
            let ticks = u64::from(t.phase) / divider;
            t.phase %= divider as u32;
            ticks
        };
        let mut overflows: u32 = 0;
        for _ in 0..ticks {
            t.counter = t.counter.wrapping_add(1);
            if t.counter == 0 {
                t.counter = t.reload;
                overflows += 1;
            }
        }
        t.displayed = t.counter;
        self.bus.write16(counter_addr, t.counter);
        overflows
    }

    fn dispatch_irq(&mut self) {
        let ie = self.bus.read16(REG_IE);
        let ime = self.bus.read16(REG_IME);
        if ime & 1 == 0 || ie & self.pending_irq == 0 {
            return;
        }
        if self.cpu.flag(cpu::BIT_I) {
            return;
        }
        let return_addr = self.cpu.pc();
        if self.bus.bios_loaded {
            self.cpu.exception(cpu::MODE_IRQ, return_addr.wrapping_add(4), 0x18);
        } else {
            // No BIOS: emulate its dispatcher contract — the game has
            // stored its handler address at 0x03007FFC, and the handler
            // returns with SUBS PC, LR, #4 (our exception() prepared
            // LR and SPSR exactly as the BIOS path expects).
            let handler = self.bus.read32(IRQ_HANDLER_PTR);
            let target = if handler != 0 { handler } else { 0x18 };
            self.cpu.exception(cpu::MODE_IRQ, return_addr.wrapping_add(4), target);
        }
    }

    /// Run the CPU until the PPU completes a frame, then present it.
    /// This is the Phase 2 frame loop frontends should use.
    pub fn run_frame_cpu(&mut self, host: &mut impl Host) {
        self.ppu.frame_ready = false;
        let mut guard: u64 = 0;
        while !self.ppu.frame_ready {
            self.step_instruction();
            guard += 1;
            if guard > 4_000_000 {
                break; // runaway program; present what we have
            }
        }
        self.frames_presented += 1;
        let keys = host.poll_input();
        self.set_keys(keys);
        host.present_frame(&self.framebuffer);
    }

    /// Load a cartridge image after validating its header.
    /// The ROM bytes are copied into the bus; the caller's buffer is
    /// not retained.
    pub fn load_rom(&mut self, rom: &[u8]) -> Result<emu_parser::GbaHeader, emu_parser::ParseError> {
        let header = emu_parser::parse_gba_header(rom)?;
        self.bus.load_rom(rom);
        Ok(header)
    }

    /// Advance the machine by `cycles`, firing every scheduled event
    /// whose time has come, in timestamp order.
    pub fn tick(&mut self, cycles: u64) {
        let target = self.scheduler.now().saturating_add(cycles);
        while let Some(at) = self.scheduler.next_event_time() {
            if at > target {
                break;
            }
            self.scheduler.advance_to(at);
            if let Some(kind) = self.scheduler.pop_due() {
                self.handle_event(kind);
            }
        }
        self.scheduler.advance_to(target);
    }

    fn handle_event(&mut self, kind: EventKind) {
        match kind {
            // Phase 0 only proves the plumbing: a VBlank event re-arms
            // itself one frame later, forever.
            EventKind::VBlank => {
                self.scheduler.schedule(CYCLES_PER_FRAME, EventKind::VBlank);
            }
            // Timer / DMA / audio events arrive in later phases.
            _ => {}
        }
    }

    /// Run one frame's worth of time and present the framebuffer.
    pub fn run_frame(&mut self, host: &mut impl Host) {
        if self.scheduler.next_event_time().is_none() {
            self.scheduler.schedule(CYCLES_PER_FRAME, EventKind::VBlank);
        }
        self.tick(CYCLES_PER_FRAME);
        self.frames_presented += 1;
        host.present_frame(&self.framebuffer);
        let _keys = host.poll_input();
    }

    /// FNV-1a hash of the current framebuffer. The headless harness uses
    /// this as its determinism oracle: same inputs, same hash, always.
    pub fn framebuffer_hash(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &px in self.framebuffer.iter() {
            for byte in px.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        hash
    }
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_instances_are_independent() {
        // The Phase 0 acceptance test from the roadmap: no globals means
        // instance B must not observe instance A's ROM, time or frames.
        let rom = crate::test_rom::fake_rom();
        let mut a = Core::new();
        let b = Core::new();
        a.load_rom(&rom).unwrap();

        let mut host = NullHost::default();
        a.run_frame(&mut host);
        a.run_frame(&mut host);

        assert_eq!(a.frames_presented, 2);
        assert_eq!(b.frames_presented, 0);
        assert_eq!(b.scheduler.now(), 0);
        assert!(b.bus.rom_is_empty());
        assert_eq!(host.frames_seen, 2);
    }

    #[test]
    fn ticking_is_deterministic() {
        let mut a = Core::new();
        let mut b = Core::new();
        for _ in 0..10 {
            a.tick(1_000);
            b.tick(1_000);
        }
        assert_eq!(a.scheduler.now(), b.scheduler.now());
        assert_eq!(a.framebuffer_hash(), b.framebuffer_hash());
    }
}

/// Shared helper for tests in this crate: a minimal ROM with a valid
/// header (checksum included), assembled byte by byte.
#[cfg(test)]
pub(crate) mod test_rom {
    use alloc::vec;
    use alloc::vec::Vec;

    pub fn fake_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[0xA0..0xA6].copy_from_slice(b"PANEN!");
        rom[0xAC..0xB0].copy_from_slice(b"TEST");
        rom[0xB0..0xB2].copy_from_slice(b"01");
        rom[0xB2] = 0x96;
        let mut sum: u8 = 0x19;
        for &b in &rom[0xA0..=0xBC] {
            sum = sum.wrapping_add(b);
        }
        rom[0xBD] = 0u8.wrapping_sub(sum);
        rom
    }
}
