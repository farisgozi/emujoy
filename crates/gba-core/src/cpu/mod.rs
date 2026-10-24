//! ARM7TDMI CPU — Phase 1 interpreter.
//!
//! Written from the ARM7TDMI Technical Reference Manual (ARM DDI 0029G)
//! and the ARM Architecture Reference Manual (ARMv4T). No existing
//! emulator's code was consulted while writing this module; per-
//! subsystem behaviour notes live in `docs/hardware-notes.md`.
//!
//! Pipeline model (deliberately simple, Phase 4 makes timing precise):
//! - `regs[15]` always holds the address of the NEXT instruction to
//!   fetch. It is advanced past the current instruction BEFORE execute.
//! - Reading R15 during execution therefore yields
//!   `regs[15] + 4` in ARM state and `regs[15] + 2` in THUMB state,
//!   matching the real pipeline-visible values (addr + 8 / addr + 4).
//! - Cycle counts returned are first approximations (1 per instruction
//!   plus flat extras for memory ops). They exist so the Scheduler has
//!   something sane to advance; they are NOT yet cycle-accurate.

pub mod arm;
pub mod thumb;

use crate::bus::Bus;

// CPSR bit positions.
pub const FLAG_N: u32 = 1 << 31;
pub const FLAG_Z: u32 = 1 << 30;
pub const FLAG_C: u32 = 1 << 29;
pub const FLAG_V: u32 = 1 << 28;
pub const BIT_I: u32 = 1 << 7;
pub const BIT_F: u32 = 1 << 6;
pub const BIT_T: u32 = 1 << 5;

pub const MODE_USER: u32 = 0x10;
pub const MODE_FIQ: u32 = 0x11;
pub const MODE_IRQ: u32 = 0x12;
pub const MODE_SVC: u32 = 0x13;
pub const MODE_ABT: u32 = 0x17;
pub const MODE_UND: u32 = 0x1B;
pub const MODE_SYS: u32 = 0x1F;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpu {
    /// Active register view. Banked registers are swapped in/out of
    /// this array on mode changes.
    regs: [u32; 16],
    cpsr: u32,
    // Banked shadows.
    usr_regs: [u32; 7], // R8..R14 in USR/SYS mode (the shared bank)
    fiq_regs: [u32; 7], // R8..R14 in FIQ mode
    svc_regs: [u32; 2], // R13, R14
    abt_regs: [u32; 2],
    und_regs: [u32; 2],
    irq_regs: [u32; 2],
    spsr_fiq: u32,
    spsr_svc: u32,
    spsr_abt: u32,
    spsr_und: u32,
    spsr_irq: u32,
    /// Diagnostics (not architectural state): last SWI comment and a
    /// count of undefined instructions, so Phase 2 can wire BIOS HLE
    /// and exception handling properly.
    pub last_swi: Option<u32>,
    pub undefined_count: u64,
}

impl Default for Cpu {
    fn default() -> Self {
        Self {
            regs: [0; 16],
            // Supervisor, ARM state, IRQ+FIQ disabled: the conventional
            // post-reset defined state for Phase 1.
            cpsr: MODE_SVC | BIT_I | BIT_F,
            usr_regs: [0; 7],
            fiq_regs: [0; 7],
            svc_regs: [0; 2],
            abt_regs: [0; 2],
            und_regs: [0; 2],
            irq_regs: [0; 2],
            spsr_fiq: 0,
            spsr_svc: 0,
            spsr_abt: 0,
            spsr_und: 0,
            spsr_irq: 0,
            last_swi: None,
            undefined_count: 0,
        }
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cpsr(&self) -> u32 {
        self.cpsr
    }

    pub fn mode(&self) -> u32 {
        self.cpsr & 0x1F
    }

    pub fn thumb_state(&self) -> bool {
        self.cpsr & BIT_T != 0
    }

    pub fn flag(&self, flag: u32) -> bool {
        self.cpsr & flag != 0
    }

    pub fn set_flag(&mut self, flag: u32, on: bool) {
        if on {
            self.cpsr |= flag;
        } else {
            self.cpsr &= !flag;
        }
    }

    /// SPSR of the current mode. User/System have none; reading yields
    /// CPSR (the conventional fallback used by debuggers).
    pub fn spsr(&self) -> u32 {
        match self.mode() {
            MODE_FIQ => self.spsr_fiq,
            MODE_SVC => self.spsr_svc,
            MODE_ABT => self.spsr_abt,
            MODE_UND => self.spsr_und,
            MODE_IRQ => self.spsr_irq,
            _ => self.cpsr,
        }
    }

    pub fn set_spsr(&mut self, value: u32) {
        match self.mode() {
            MODE_FIQ => self.spsr_fiq = value,
            MODE_SVC => self.spsr_svc = value,
            MODE_ABT => self.spsr_abt = value,
            MODE_UND => self.spsr_und = value,
            MODE_IRQ => self.spsr_irq = value,
            _ => {}
        }
    }

    /// Read a register with pipeline semantics for R15.
    pub fn reg(&self, n: usize) -> u32 {
        if n == 15 {
            let extra = if self.thumb_state() { 2 } else { 4 };
            self.regs[15].wrapping_add(extra)
        } else {
            self.regs[n]
        }
    }

    /// Raw R15 (next fetch address) — tests & save states use this.
    pub fn pc(&self) -> u32 {
        self.regs[15]
    }

    pub fn set_pc(&mut self, addr: u32) {
        self.regs[15] = addr;
    }

    /// Write a register. Writing R15 branches; the target is aligned to
    /// the current instruction set (BX & friends mask/set T separately
    /// before calling this).
    pub fn set_reg(&mut self, n: usize, value: u32) {
        if n == 15 {
            self.regs[15] = if self.thumb_state() {
                value & !1
            } else {
                value & !3
            };
        } else {
            self.regs[n] = value;
        }
    }

    /// Snapshot the architectural registers (raw view, R15 = next
    /// fetch address) + CPSR, for save states.
    pub fn snapshot_regs(&self) -> ([u32; 16], u32) {
        (self.regs, self.cpsr)
    }

    pub fn restore_regs(&mut self, regs: [u32; 16], cpsr: u32) {
        self.regs = regs;
        // Direct assignment: restoring a snapshot must not trigger
        // bank swapping halfway through.
        self.cpsr = cpsr;
    }

    /// Full register-file snapshot for save-state format v3: the
    /// active view, CPSR, every banked shadow and every SPSR, in a
    /// fixed order (44 words). `restore_regs` alone is NOT enough for
    /// a state taken mid-game: the IRQ/SVC banked stacks live in the
    /// shadows, and losing them (zeroed shadows on a fresh core)
    /// sends the CPU into unmapped space on the first IRQ return.
    pub fn snapshot_full(&self) -> [u32; 44] {
        let mut out = [0u32; 44];
        out[0..16].copy_from_slice(&self.regs);
        out[16] = self.cpsr;
        out[17..24].copy_from_slice(&self.usr_regs);
        out[24..31].copy_from_slice(&self.fiq_regs);
        out[31..33].copy_from_slice(&self.svc_regs);
        out[33..35].copy_from_slice(&self.abt_regs);
        out[35..37].copy_from_slice(&self.und_regs);
        out[37..39].copy_from_slice(&self.irq_regs);
        out[39] = self.spsr_fiq;
        out[40] = self.spsr_svc;
        out[41] = self.spsr_abt;
        out[42] = self.spsr_und;
        out[43] = self.spsr_irq;
        out
    }

    pub fn restore_full(&mut self, words: &[u32; 44]) {
        self.regs.copy_from_slice(&words[0..16]);
        self.cpsr = words[16];
        self.usr_regs.copy_from_slice(&words[17..24]);
        self.fiq_regs.copy_from_slice(&words[24..31]);
        self.svc_regs.copy_from_slice(&words[31..33]);
        self.abt_regs.copy_from_slice(&words[33..35]);
        self.und_regs.copy_from_slice(&words[35..37]);
        self.irq_regs.copy_from_slice(&words[37..39]);
        self.spsr_fiq = words[39];
        self.spsr_svc = words[40];
        self.spsr_abt = words[41];
        self.spsr_und = words[42];
        self.spsr_irq = words[43];
    }

    /// Change the low (control) byte of CPSR, swapping banked registers
    /// if the mode changes. Used by MSR and exception entry/return.
    pub fn set_cpsr_control(&mut self, new_cpsr: u32) {
        let old_mode = self.mode();
        let new_mode = new_cpsr & 0x1F;
        if old_mode != new_mode {
            self.swap_out(old_mode);
            self.cpsr = (self.cpsr & !0xFF) | (new_cpsr & 0xFF);
            self.swap_in(new_mode);
        } else {
            self.cpsr = (self.cpsr & !0xFF) | (new_cpsr & 0xFF);
        }
        // T bit may ride along in the same write.
        if new_cpsr & BIT_T != self.cpsr & BIT_T {
            // Already assigned above if bit 5 in low byte — nothing to do.
        }
    }

    /// Full CPSR write with bank handling (exception returns use this
    /// via `set_reg(15)` + S-bit paths; MSR uses field masks first).
    pub fn set_cpsr_full(&mut self, new_cpsr: u32) {
        let old_mode = self.mode();
        let new_mode = new_cpsr & 0x1F;
        if old_mode != new_mode {
            self.swap_out(old_mode);
            self.cpsr = new_cpsr;
            self.swap_in(new_mode);
        } else {
            self.cpsr = new_cpsr;
        }
    }

    fn swap_out(&mut self, mode: u32) {
        match mode {
            MODE_FIQ => {
                self.fiq_regs.copy_from_slice(&self.regs[8..15]);
            }
            MODE_USER | MODE_SYS => {
                self.usr_regs.copy_from_slice(&self.regs[8..15]);
            }
            // Privileged modes share R8..R12 with the user bank:
            // refresh that part of the user shadow on the way out too.
            MODE_SVC => {
                self.usr_regs[..5].copy_from_slice(&self.regs[8..13]);
                self.svc_regs.copy_from_slice(&self.regs[13..15]);
            }
            MODE_ABT => {
                self.usr_regs[..5].copy_from_slice(&self.regs[8..13]);
                self.abt_regs.copy_from_slice(&self.regs[13..15]);
            }
            MODE_UND => {
                self.usr_regs[..5].copy_from_slice(&self.regs[8..13]);
                self.und_regs.copy_from_slice(&self.regs[13..15]);
            }
            MODE_IRQ => {
                self.usr_regs[..5].copy_from_slice(&self.regs[8..13]);
                self.irq_regs.copy_from_slice(&self.regs[13..15]);
            }
            _ => {}
        }
    }

    fn swap_in(&mut self, mode: u32) {
        match mode {
            MODE_FIQ => {
                self.regs[8..15].copy_from_slice(&self.fiq_regs);
            }
            MODE_USER | MODE_SYS => {
                self.regs[8..15].copy_from_slice(&self.usr_regs);
            }
            MODE_SVC => {
                self.regs[8..13].copy_from_slice(&self.usr_regs[..5]);
                self.regs[13..15].copy_from_slice(&self.svc_regs);
            }
            MODE_ABT => {
                self.regs[8..13].copy_from_slice(&self.usr_regs[..5]);
                self.regs[13..15].copy_from_slice(&self.abt_regs);
            }
            MODE_UND => {
                self.regs[8..13].copy_from_slice(&self.usr_regs[..5]);
                self.regs[13..15].copy_from_slice(&self.und_regs);
            }
            MODE_IRQ => {
                self.regs[8..13].copy_from_slice(&self.usr_regs[..5]);
                self.regs[13..15].copy_from_slice(&self.irq_regs);
            }
            _ => {}
        }
    }

    /// Read a register as seen in the user bank (for LDM/STM with the
    /// S bit, which transfer user registers from any mode).
    pub fn user_reg(&self, n: usize) -> u32 {
        match n {
            0..=7 => self.regs[n],
            8..=14 => {
                if matches!(self.mode(), MODE_USER | MODE_SYS) {
                    self.regs[n]
                } else {
                    self.usr_regs[n - 8]
                }
            }
            _ => self.reg(15),
        }
    }

    /// Write a register in the user bank (LDM with S bit).
    pub fn set_user_reg(&mut self, n: usize, value: u32) {
        match n {
            0..=7 => self.regs[n] = value,
            8..=14 => {
                if matches!(self.mode(), MODE_USER | MODE_SYS) {
                    self.regs[n] = value;
                } else {
                    self.usr_regs[n - 8] = value;
                }
            }
            _ => {}
        }
    }

    /// Enter an exception: bank in the target mode, save return address
    /// to its LR, save CPSR to its SPSR, disable IRQ, switch to ARM
    /// state, jump to the vector.
    pub fn exception(&mut self, mode: u32, lr_value: u32, vector: u32) {
        let old_cpsr = self.cpsr;
        let old_mode = self.mode();
        if old_mode != mode {
            self.swap_out(old_mode);
            self.swap_in(mode);
        }
        self.cpsr = (old_cpsr & !0x1F) | mode;
        self.cpsr |= BIT_I;
        self.cpsr &= !BIT_T;
        self.set_spsr(old_cpsr);
        self.regs[14] = lr_value;
        self.regs[15] = vector;
    }

    // -- Flag arithmetic -------------------------------------------------

    /// Addition with carry: returns (result, carry_out, overflow).
    pub fn add_flags(a: u32, b: u32, carry: bool) -> (u32, bool, bool) {
        let wide = u64::from(a) + u64::from(b) + u64::from(carry);
        let result = wide as u32;
        let carry_out = wide >> 32 != 0;
        let overflow = (!(a ^ b) & (a ^ result)) >> 31 != 0;
        (result, carry_out, overflow)
    }

    /// Subtraction via addition of the two's complement, as the ARM ARM
    /// prescribes: a - b - !carry.
    pub fn sub_flags(a: u32, b: u32, carry: bool) -> (u32, bool, bool) {
        Self::add_flags(a, !b, carry)
    }

    pub fn set_nz(&mut self, result: u32) {
        self.set_flag(FLAG_N, result >> 31 != 0);
        self.set_flag(FLAG_Z, result == 0);
    }

    pub fn set_nzc(&mut self, result: u32, carry: bool) {
        self.set_nz(result);
        self.set_flag(FLAG_C, carry);
    }

    pub fn set_nzcv(&mut self, result: u32, carry: bool, overflow: bool) {
        self.set_nzc(result, carry);
        self.set_flag(FLAG_V, overflow);
    }

    // -- Execution -------------------------------------------------------

    /// Execute exactly one instruction. Returns approximate cycles.
    pub fn step(&mut self, bus: &mut Bus) -> u32 {
        if self.thumb_state() {
            thumb::step_thumb(self, bus)
        } else {
            arm::step_arm(self, bus)
        }
    }
}

/// Barrel shifter shared by ARM data processing and load/store offsets.
/// Returns (value, carry_out). `amount_is_register` matters for the
/// amount == 0 vs amount == 256 edge cases in register-specified shifts.
pub fn barrel_shift(
    value: u32,
    shift_type: u32,
    amount: u32,
    carry_in: bool,
    amount_is_register: bool,
) -> (u32, bool) {
    match shift_type {
        // LSL
        0 => {
            if amount == 0 {
                (value, carry_in)
            } else if amount < 32 {
                ((value << amount), (value >> (32 - amount)) & 1 != 0)
            } else if amount == 32 {
                (0, value & 1 != 0)
            } else {
                (0, false)
            }
        }
        // LSR
        1 => {
            let amt = if !amount_is_register && amount == 0 { 32 } else { amount };
            if amt == 0 {
                (value, carry_in)
            } else if amt < 32 {
                ((value >> amt), (value >> (amt - 1)) & 1 != 0)
            } else if amt == 32 {
                (0, value >> 31 != 0)
            } else {
                (0, false)
            }
        }
        // ASR
        2 => {
            let amt = if !amount_is_register && amount == 0 { 32 } else { amount };
            if amt == 0 {
                (value, carry_in)
            } else if amt < 32 {
                let signed = value as i32;
                (((signed >> amt) as u32), (value >> (amt - 1)) & 1 != 0)
            } else {
                let sign = value >> 31 != 0;
                (if sign { u32::MAX } else { 0 }, sign)
            }
        }
        // ROR / RRX
        _ => {
            if !amount_is_register && amount == 0 {
                // RRX: rotate right through carry by 1.
                let out = (value >> 1) | ((carry_in as u32) << 31);
                (out, value & 1 != 0)
            } else if amount_is_register && amount == 0 {
                // Register-specified shift of exactly 0: value and
                // carry both pass through untouched (jsmolka arm 166
                // / thumb 68). This differs from amount 32/64..., where
                // the value also passes through but carry becomes
                // bit 31 of the value.
                (value, carry_in)
            } else {
                let amt = amount & 31;
                if amt == 0 {
                    (value, value >> 31 != 0)
                } else {
                    (value.rotate_right(amt), (value >> (amt - 1)) & 1 != 0)
                }
            }
        }
    }
}

/// Condition-code evaluation.
pub fn condition_passed(cond: u32, cpsr: u32) -> bool {
    let n = cpsr & FLAG_N != 0;
    let z = cpsr & FLAG_Z != 0;
    let c = cpsr & FLAG_C != 0;
    let v = cpsr & FLAG_V != 0;
    match cond {
        0x0 => z,              // EQ
        0x1 => !z,             // NE
        0x2 => c,              // CS/HS
        0x3 => !c,             // CC/LO
        0x4 => n,              // MI
        0x5 => !n,             // PL
        0x6 => v,              // VS
        0x7 => !v,             // VC
        0x8 => c && !z,        // HI
        0x9 => !c || z,        // LS
        0xA => n == v,         // GE
        0xB => n != v,         // LT
        0xC => !z && (n == v), // GT
        0xD => z || (n != v),  // LE
        0xE => true,           // AL
        _ => false,            // 0xF: NV on ARMv4 (BLX arrives v5)
    }
}
