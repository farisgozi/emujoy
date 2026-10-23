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
//! What does NOT exist yet: CPU execution (Phase 1), PPU/APU rendering.
//! `Core::run_frame` advances time and fires scheduled events only.
#![no_std]

extern crate alloc;

pub mod bus;
pub mod host;
pub mod savestate;
pub mod scheduler;

pub use bus::Bus;
pub use host::{Host, NullHost};
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

/// CPU register file placeholder. Phase 1 replaces `step` with a real
/// ARM7TDMI interpreter written from the ARM TRM; the layout (16 GPRs +
/// CPSR) is already the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuState {
    pub regs: [u32; 16],
    pub cpsr: u32,
}

impl Default for CpuState {
    fn default() -> Self {
        // Supervisor mode, IRQ/FIQ disabled — the state the BIOS leaves
        // behind is Phase 2 work; this is a defined starting point.
        Self { regs: [0; 16], cpsr: 0x0000_00D3 }
    }
}

/// The whole machine. Owns everything by value; create as many as you
/// like, they cannot interfere with each other.
pub struct Core {
    pub cpu: CpuState,
    pub scheduler: Scheduler,
    pub bus: Bus,
    /// Framebuffer the PPU will draw into (Phase 3). Allocated once.
    pub framebuffer: Box<[u32; SCREEN_W * SCREEN_H]>,
    /// How many frames have been presented. Handy in tests.
    pub frames_presented: u64,
}

impl Core {
    pub fn new() -> Self {
        Self {
            cpu: CpuState::default(),
            scheduler: Scheduler::new(),
            bus: Bus::new(),
            framebuffer: Box::new([0; SCREEN_W * SCREEN_H]),
            frames_presented: 0,
        }
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
