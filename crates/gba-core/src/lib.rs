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

pub mod apu;
pub mod bus;
pub mod cpu;
pub mod dma;
pub mod host;
pub mod ppu;
pub mod save;
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
/// Sentinel "return address" for the emulated BIOS IRQ stub: an
/// unused word near the top of BIOS space. Never executed — the
/// core intercepts the PC before the fetch (see step_instruction).
const BIOS_STUB_RETURN: u32 = 0x0000_03FC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HleWait {
    VBlank,
    AnyIrq,
}

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
    /// BIOS-less HLE wait state (IntrWait / VBlankIntrWait / Halt):
    /// while set, the CPU is parked and only devices advance.
    hle_wait: Option<HleWait>,
    /// True while a game IRQ handler runs under the emulated BIOS
    /// stub (see `dispatch_irq`): the stub epilogue runs when the PC
    /// reaches `BIOS_STUB_RETURN`.
    bios_stub_active: bool,
    /// BIOS HLE is a boot policy, not a CPU behaviour: it activates
    /// when the machine is booted via `boot_cartridge`/`boot_bios`
    /// without a BIOS dump, so bare-CPU tests still see the raw SWI
    /// exception semantics.
    hle_enabled: bool,
    /// Sound: PSG channels + Direct Sound FIFOs (Phase 4).
    pub apu: apu::Apu,
    /// Stereo samples produced since the last frame, pushed to the
    /// host at present time (interleaved L,R i16 @ 32768 Hz).
    audio_pending: alloc::vec::Vec<i16>,
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
            hle_wait: None,
            bios_stub_active: false,
            hle_enabled: false,
            apu: apu::Apu::new(),
            audio_pending: alloc::vec::Vec::new(),
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

    /// The cartridge save contents (.sav image) for the frontend to
    /// persist. Kind/size were fixed at ROM load by string detection.
    pub fn save_data(&self) -> &[u8] {
        &self.bus.save.data[..self.bus.save.contents_len()]
    }

    /// Load a previously persisted .sav image into the save chip.
    pub fn load_save_data(&mut self, bytes: &[u8]) {
        self.bus.save.load_contents(bytes);
    }

    /// True when the save chip changed since the last load/ack —
    /// frontends write the .sav file when this is set, then call
    /// `ack_save_written`.
    pub fn save_dirty(&self) -> bool {
        self.bus.save.dirty
    }

    pub fn ack_save_written(&mut self) {
        self.bus.save.dirty = false;
    }

    /// Device-state snapshot for the save-state format (v2 section).
    pub(crate) fn device_snapshot(&self) -> ([u8; 2], [(u16, u16); 4], (u16, u64)) {
        let mut timers = [(0u16, 0u16); 4];
        for (i, t) in self.timers.iter().enumerate() {
            timers[i] = (t.counter, t.reload);
        }
        (
            self.pending_irq.to_le_bytes(),
            timers,
            self.ppu.timing_state(),
        )
    }

    pub(crate) fn device_restore(
        &mut self,
        pending_irq: u16,
        timers: [(u16, u16); 4],
        ppu_timing: (u16, u64),
    ) {
        self.pending_irq = pending_irq;
        for (i, (counter, reload)) in timers.iter().enumerate() {
            self.timers[i].counter = *counter;
            self.timers[i].reload = *reload;
            self.timers[i].displayed = *counter;
        }
        self.ppu.restore_timing(ppu_timing.0, ppu_timing.1);
    }

    /// Set up the register state the real BIOS leaves behind and jump
    /// straight to the cartridge entry point (0x08000000). Stack
    /// addresses and mode layout follow GBATEK's description of the
    /// BIOS-to-cartridge handoff.
    pub fn boot_cartridge(&mut self) {
        use cpu::{BIT_F, BIT_I, MODE_IRQ, MODE_SVC, MODE_SYS};
        self.hle_enabled = !self.bus.bios_loaded;
        self.cpu.set_cpsr_full(MODE_IRQ | BIT_I | BIT_F);
        self.cpu.set_reg(13, 0x0300_7FA0);
        self.cpu.set_cpsr_full(MODE_SVC | BIT_I | BIT_F);
        self.cpu.set_reg(13, 0x0300_7FE0);
        self.cpu.set_cpsr_full(MODE_SYS);
        self.cpu.set_reg(13, 0x0300_7F00);
        self.cpu.set_pc(0x0800_0000);
        self.set_keys(0);
        // The real BIOS leaves the affine matrices at identity after
        // its logo animation, and software relies on it — the
        // jsmolka test suites, for instance, draw their Mode 4 text
        // screen without ever programming BG2PA/BG2PD. A zero matrix
        // would collapse the bitmap onto texel (0,0).
        for reg in [0x0400_0020u32, 0x0400_0026, 0x0400_0030, 0x0400_0036] {
            self.set_io16(reg, 0x0100);
        }
    }

    /// Boot into a loaded BIOS at address 0 (it will hand off to the
    /// cartridge itself when it is done).
    pub fn boot_bios(&mut self) {
        use cpu::{BIT_F, BIT_I, MODE_SVC};
        self.cpu.set_cpsr_full(MODE_SVC | BIT_I | BIT_F);
        self.cpu.set_reg(13, 0x0300_7FE0);
        self.cpu.set_pc(0);
    }

    /// If the next instruction is a BIOS SWI and no BIOS dump is
    /// loaded, return its number: the core HLEs the call instead of
    /// vectoring into empty memory (Phase 3 BIOS handling).
    fn peek_bios_swi(&self) -> Option<u32> {
        if self.bus.bios_loaded || !self.hle_enabled {
            return None;
        }
        let pc = self.cpu.pc();
        if self.cpu.thumb_state() {
            let instr = self.bus.read16(pc & !1);
            if instr & 0xFF00 == 0xDF00 {
                return Some(u32::from(instr & 0xFF));
            }
        } else {
            let instr = self.bus.read32(pc & !3);
            if instr & 0x0F00_0000 == 0x0F00_0000 {
                let imm = instr & 0x00FF_FFFF;
                return Some(if imm < 0x1_0000 { imm & 0xFF } else { imm >> 16 });
            }
        }
        None
    }

    /// Execute exactly one CPU instruction, then advance every device
    /// by its (Phase 1 approximate) cycle cost and service interrupts.
    pub fn step_instruction(&mut self) -> u32 {
        // Emulated BIOS stub epilogue: a game IRQ handler invoked
        // through the stub returns to this sentinel address; pop the
        // registers the stub pushed and resume the interrupted code
        // (SUBS PC, LR, #4 semantics with SPSR restore).
        if self.bios_stub_active && self.cpu.pc() == BIOS_STUB_RETURN {
            self.bios_stub_active = false;
            let sp = self.cpu.reg(13);
            let vals: [u32; 6] = core::array::from_fn(|i| self.bus.read32(sp + (i as u32) * 4));
            self.cpu.set_reg(0, vals[0]);
            self.cpu.set_reg(1, vals[1]);
            self.cpu.set_reg(2, vals[2]);
            self.cpu.set_reg(3, vals[3]);
            self.cpu.set_reg(12, vals[4]);
            self.cpu.set_reg(13, sp.wrapping_add(24));
            let spsr = self.cpu.spsr();
            self.cpu.set_cpsr_full(spsr);
            self.cpu.set_pc(vals[5].wrapping_sub(4));
            let now = self.scheduler.now();
            self.scheduler.advance_to(now + 3);
            self.advance_devices(3);
            self.set_io16(REG_IF, self.pending_irq);
            self.dispatch_irq();
            return 3;
        }
        // Parked by a BIOS wait call (HLE): advance devices only.
        if let Some(wait) = self.hle_wait {
            let chunk = 64;
            let now = self.scheduler.now();
            self.scheduler.advance_to(now + chunk);
            self.advance_devices(chunk);
            self.set_io16(REG_IF, self.pending_irq);
            let wake = match wait {
                HleWait::VBlank => self.ppu.frame_ready,
                HleWait::AnyIrq => {
                    let ie = self.bus.read16(REG_IE);
                    ie & self.pending_irq != 0
                }
            };
            if wake {
                self.hle_wait = None;
                if wait == HleWait::VBlank {
                    self.ppu.frame_ready = false;
                }
            }
            self.dispatch_irq();
            return chunk as u32;
        }
        if let Some(swi) = self.peek_bios_swi() {
            let size = if self.cpu.thumb_state() { 2 } else { 4 };
            let ret = self.cpu.pc().wrapping_add(size);
            self.cpu.set_pc(ret);
            self.exec_bios_hle(swi);
            let now = self.scheduler.now();
            self.scheduler.advance_to(now + 8);
            self.advance_devices(8);
            self.set_io16(REG_IF, self.pending_irq);
            self.dispatch_irq();
            return 8;
        }
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
        let mut timer_overflows = [0u32; 4];
        for ch in 0..4 {
            let overflows = self.tick_timer(ch, cycles, cascade_overflows);
            timer_overflows[ch] = overflows;
            if overflows > 0 {
                let ctrl = self.bus.read16(0x0400_0102 + (ch as u32) * 4);
                if ctrl & (1 << 6) != 0 {
                    self.pending_irq |= 1 << (3 + ch);
                }
            }
            cascade_overflows = overflows;
        }
        self.advance_audio(cycles, timer_overflows);

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

    /// APU service: register sync, trigger delivery, FIFO clocking
    /// from the sound timers, sound DMA refills, sample production.
    fn advance_audio(&mut self, cycles: u64, timer_overflows: [u32; 4]) {
        // Bytes pushed to the FIFO ports since last time.
        for w in 0..2 {
            let bytes = core::mem::take(&mut self.bus.fifo_in[w]);
            if !bytes.is_empty() {
                self.apu.push_fifo(w, &bytes);
            }
        }
        // FIFO reset bits in SOUNDCNT_H are self-clearing on HW.
        let cnth = self.bus.read16(0x0400_0082);
        if cnth & 0x8800 != 0 {
            if cnth & (1 << 11) != 0 {
                self.apu.reset_fifo(0);
            }
            if cnth & (1 << 15) != 0 {
                self.apu.reset_fifo(1);
            }
            self.bus.write16(0x0400_0082, cnth & !0x8800);
        }
        self.apu.sync(&self.bus.io[..]);
        // PSG trigger edges recorded by the bus.
        let trig = self.bus.snd_trigger_mask;
        if trig != 0 {
            self.bus.snd_trigger_mask = 0;
            for ch in 0..4 {
                if trig & (1 << ch) != 0 {
                    self.apu.trigger(ch, &self.bus.io[..]);
                }
            }
        }
        // FIFO playback ticks: each FIFO is clocked by timer 0 or 1
        // (SOUNDCNT_H bits 10 and 14).
        let cnth = self.bus.read16(0x0400_0082);
        for w in 0..2 {
            let timer_ch = if cnth & (1 << (10 + w * 4)) != 0 { 1 } else { 0 };
            for _ in 0..timer_overflows[timer_ch] {
                self.apu.fifo_tick(w);
            }
        }
        // Sound DMA refills (DMA1/DMA2 in Special timing with the
        // destination on a FIFO port).
        for w in 0..2 {
            if !self.apu.fifo_dma_needed[w] {
                continue;
            }
            let port = 0x0400_00A0 + (w as u32) * 4;
            for ch in 1..=2usize {
                let chan = dma::latch(&self.bus, ch);
                if chan.enabled() && chan.timing() == dma::DmaTiming::Special && chan.dst & !3 == port {
                    if dma::run_sound_transfer(&mut self.bus, &chan) {
                        self.pending_irq |= 1 << (8 + ch);
                    }
                    // The transfer wrote into bus.fifo_in; forward.
                    let bytes = core::mem::take(&mut self.bus.fifo_in[w]);
                    if !bytes.is_empty() {
                        self.apu.push_fifo(w, &bytes);
                    }
                    break;
                }
            }
        }
        self.apu.advance(cycles, &self.bus.io[..]);
        if !self.apu.out.is_empty() {
            self.audio_pending.append(&mut self.apu.out);
        }
        // SOUNDCNT_X status bits mirror the channel-running state.
        let cur = self.bus.read16(0x0400_0084);
        let want = (cur & 0xFFF0) | self.apu.status();
        if cur != want {
            self.bus.write16(0x0400_0084, want);
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

    /// High-level emulation of the BIOS calls games actually use,
    /// active only when no BIOS dump is loaded. Register conventions
    /// follow GBATEK. Unknown calls are skipped (return immediately),
    /// which is strictly better than vectoring into empty memory.
    fn exec_bios_hle(&mut self, swi: u32) {
        match swi {
            // SoftReset: re-enter the cartridge.
            0x00 => self.boot_cartridge(),
            // RegisterRamReset: clear the selected memory/registers.
            0x01 => {
                let flags = self.cpu.reg(0);
                if flags & 0x01 != 0 {
                    self.bus.ewram.fill(0);
                }
                if flags & 0x02 != 0 {
                    self.bus.iwram.fill(0);
                }
                if flags & 0x04 != 0 {
                    self.bus.palette.fill(0);
                    self.bus.vram.fill(0);
                    self.bus.oam.fill(0);
                }
                if flags & 0x08 != 0 {
                    self.set_io16(0x0400_0000, 0);
                }
            }
            // Halt / IntrWait / VBlankIntrWait: park the CPU.
            0x02 => self.hle_wait = Some(HleWait::AnyIrq),
            0x04 => self.hle_wait = Some(HleWait::AnyIrq),
            0x05 => self.hle_wait = Some(HleWait::VBlank),
            // Div: R0 = R0/R1, R1 = remainder, R3 = |quotient|.
            0x06 => {
                let num = self.cpu.reg(0) as i32;
                let den = self.cpu.reg(1) as i32;
                let (q, r) = if den == 0 {
                    (if num >= 0 { 1 } else { -1 }, num)
                } else if num == i32::MIN && den == -1 {
                    (i32::MIN, 0) // overflow case per GBATEK
                } else {
                    (num / den, num % den)
                };
                self.cpu.set_reg(0, q as u32);
                self.cpu.set_reg(1, r as u32);
                self.cpu.set_reg(3, q.unsigned_abs());
            }
            // Sqrt: R0 = floor(sqrt(R0)).
            0x08 => {
                let v = self.cpu.reg(0);
                self.cpu.set_reg(0, isqrt(v));
            }
            // CpuSet (0x0B) / CpuFastSet (0x0C).
            0x0B | 0x0C => {
                let mut src = self.cpu.reg(0);
                let mut dst = self.cpu.reg(1);
                let ctrl = self.cpu.reg(2);
                let fill = ctrl & (1 << 24) != 0;
                let wide = ctrl & (1 << 26) != 0;
                let mut count = ctrl & 0x001F_FFFF;
                if swi == 0x0C {
                    count *= 8; // fast set counts 8-word blocks
                }
                for _ in 0..count {
                    if wide {
                        let v = self.bus.read32(src & !3);
                        self.bus.write32(dst & !3, v);
                        if !fill {
                            src = src.wrapping_add(4);
                        }
                        dst = dst.wrapping_add(4);
                    } else {
                        let v = self.bus.read16(src & !1);
                        self.bus.write16(dst & !1, v);
                        if !fill {
                            src = src.wrapping_add(2);
                        }
                        dst = dst.wrapping_add(2);
                    }
                }
            }
            // LZ77UnCompWram (0x11) / LZ77UnCompVram (0x12): the one
            // decompressor test ROMs and many games lean on.
            0x11 | 0x12 => {
                let src = self.cpu.reg(0);
                let dst = self.cpu.reg(1);
                lz77_decompress(&mut self.bus, src, dst);
            }
            _ => {}
        }
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
            // No BIOS: reproduce the real BIOS IRQ stub, which is the
            // contract game handlers are actually written against
            // (FOMT's handler, for instance, silently corrupts its
            // return flow if entered any other way): the stub pushes
            // R0-R3, R12 and LR onto the IRQ stack, calls the handler
            // stored at 0x03007FFC with LR pointing back into the
            // stub, and on return pops everything and resumes with
            // SUBS PC, LR, #4. Here the "stub" is emulated: LR is a
            // sentinel in unused BIOS space and `step_instruction`
            // performs the epilogue when the handler reaches it.
            let handler = self.bus.read32(IRQ_HANDLER_PTR);
            if handler == 0 {
                return; // nobody to call; the BIOS would spin here too
            }
            let saved = [
                self.cpu.reg(0),
                self.cpu.reg(1),
                self.cpu.reg(2),
                self.cpu.reg(3),
                self.cpu.reg(12),
            ];
            self.cpu.exception(cpu::MODE_IRQ, return_addr.wrapping_add(4), handler);
            let lr = self.cpu.reg(14);
            let sp = self.cpu.reg(13).wrapping_sub(24);
            for (i, v) in saved.iter().enumerate() {
                self.bus.write32(sp + (i as u32) * 4, *v);
            }
            self.bus.write32(sp + 20, lr);
            self.cpu.set_reg(13, sp);
            self.cpu.set_reg(14, BIOS_STUB_RETURN);
            self.bios_stub_active = true;
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
        if !self.audio_pending.is_empty() {
            let samples = core::mem::take(&mut self.audio_pending);
            host.push_audio(&samples);
        }
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

fn isqrt(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let mut r = v;
    let mut y = (r + 1) / 2;
    while y < r {
        r = y;
        y = (r + v / r) / 2;
    }
    r
}

/// The BIOS LZ77 format (header word: 0x10 | length<<8, then blocks
/// of 8 items flagged by one byte; literal = 1 byte, back-reference =
/// 12-bit offset + 3-bit length+3). Writes go through the bus, so the
/// VRAM variant inherits VRAM's write behaviour.
fn lz77_decompress(bus: &mut Bus, mut src: u32, mut dst: u32) {
    let header = bus.read32(src);
    src = src.wrapping_add(4);
    if header & 0xFF != 0x10 {
        return;
    }
    let total = header >> 8;
    let mut written: u32 = 0;
    while written < total {
        let flags = bus.read8(src);
        src = src.wrapping_add(1);
        for bit in (0..8).rev() {
            if written >= total {
                break;
            }
            if flags & (1 << bit) == 0 {
                let b = bus.read8(src);
                src = src.wrapping_add(1);
                bus.write8(dst, b);
                dst = dst.wrapping_add(1);
                written += 1;
            } else {
                let b0 = bus.read8(src);
                let b1 = bus.read8(src.wrapping_add(1));
                src = src.wrapping_add(2);
                let len = u32::from(b0 >> 4) + 3;
                let off = (u32::from(b0 & 0xF) << 8) | u32::from(b1);
                let mut from = dst.wrapping_sub(off + 1);
                for _ in 0..len {
                    if written >= total {
                        break;
                    }
                    let b = bus.read8(from);
                    from = from.wrapping_add(1);
                    bus.write8(dst, b);
                    dst = dst.wrapping_add(1);
                    written += 1;
                }
            }
        }
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
