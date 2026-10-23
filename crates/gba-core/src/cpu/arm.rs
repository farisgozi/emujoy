//! ARM state instruction execution (ARMv4T).
//!
//! Decode strategy: the classic top-bits dispatch used by every
//! interpreter — bits 27..=25 first, refined by bits 7..=4 where the
//! encoding space overlaps (multiplies, halfword transfers, SWP).

use super::*;
use crate::bus::Bus;

pub fn step_arm(cpu: &mut Cpu, bus: &mut Bus) -> u32 {
    let addr = cpu.regs[15];
    let instr = bus.read32(addr & !3);
    // Advance PC past this instruction BEFORE execute (pipeline model).
    cpu.regs[15] = addr.wrapping_add(4);

    let cond = instr >> 28;
    if cond != 0xE && !condition_passed(cond, cpu.cpsr) {
        return 1;
    }
    if cond == 0xF {
        // BLX immediate is ARMv5; on ARM7TDMI this space is undefined.
        return undefined(cpu, addr);
    }

    match (instr >> 25) & 0x7 {
        0b000 => {
            // Could be: multiply / SWP / extra load-store (bits 7 & 4),
            // PSR transfer, data processing, BX.
            if instr & 0x0FFF_FFF0 == 0x012F_FF10 {
                // BX Rm
                let rm = cpu.reg((instr & 0xF) as usize);
                cpu.cpsr = (cpu.cpsr & !BIT_T) | ((rm & 1) << 5);
                cpu.regs[15] = if rm & 1 != 0 { rm & !1 } else { rm & !3 };
                return 3;
            }
            if instr & 0x0FBF_0FFF == 0x010F_0000 {
                // MRS Rd, CPSR/SPSR (mask excludes the Rd field)
                let rd = ((instr >> 12) & 0xF) as usize;
                let val = if instr & (1 << 22) != 0 { cpu.spsr() } else { cpu.cpsr };
                cpu.set_reg(rd, val);
                return 1;
            }
            if instr & 0x0FB0_FFF0 == 0x0120_F000 {
                // MSR CPSR/SPSR, Rm (register form)
                return exec_msr_reg(cpu, instr);
            }
            if (instr >> 4) & 0xF == 0b1001 {
                // Bits 7..4 = 1001 exactly: multiply (bits 27..24 = 0)
                // or SWP (bits 27..24 = 1, routed to the extra handler).
                if (instr >> 24) & 0xF == 0 {
                    return exec_multiply(cpu, instr);
                }
                return exec_extra_load_store(cpu, bus, instr);
            }
            if instr & 0x90 == 0x90 && (instr >> 5) & 0x3 != 0 {
                // Extra load/stores: bit7=1, SH (bits 6..5) != 00, bit4=1.
                return exec_extra_load_store(cpu, bus, instr);
            }
            exec_data_processing(cpu, instr)
        }
        0b001 => {
            // MSR immediate hides inside the TEQ/CMN opcode space with
            // S=0 and Rn=Rd=1111: 001 10R1 1111 mask 1111 rot imm8.
            let op = (instr >> 21) & 0xF;
            if (op == 0x9 || op == 0xB)
                && instr & (1 << 20) == 0
                && (instr >> 16) & 0xF == 0xF
                && (instr >> 12) & 0xF == 0xF
            {
                return exec_msr_imm(cpu, instr);
            }
            exec_data_processing(cpu, instr)
        }
        0b010 => exec_load_store(cpu, bus, instr, false),
        0b011 => {
            if instr & (1 << 4) != 0 {
                // Media / undefined space on ARMv4T.
                undefined(cpu, addr)
            } else {
                exec_load_store(cpu, bus, instr, true)
            }
        }
        0b100 => exec_block_transfer(cpu, bus, instr),
        0b101 => {
            // B / BL
            let link = instr & (1 << 24) != 0;
            let offset = sign_extend(instr & 0x00FF_FFFF, 24) << 2;
            if link {
                cpu.regs[14] = addr.wrapping_add(4);
            }
            // Target = (addr + 8) + signed_offset, per the pipeline model.
            cpu.regs[15] = addr.wrapping_add(8).wrapping_add(offset as u32);
            3
        }
        0b110 => undefined(cpu, addr), // coprocessor load/store: none on GBA
        _ => {
            // 0b111: SWI or coprocessor ops.
            if instr & 0x0F00_0000 == 0x0F00_0000 {
                let comment = instr & 0x00FF_FFFF;
                cpu.last_swi = Some(comment);
                let lr = cpu.regs[15]; // address of next instruction
                cpu.exception(MODE_SVC, lr, 0x08);
                3
            } else {
                undefined(cpu, addr)
            }
        }
    }
}

fn undefined(cpu: &mut Cpu, addr: u32) -> u32 {
    cpu.undefined_count += 1;
    let lr = addr.wrapping_add(4);
    cpu.exception(MODE_UND, lr, 0x04);
    3
}

fn sign_extend(value: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

// ---------------------------------------------------------------------------
// Data processing
// ---------------------------------------------------------------------------

fn shifter_operand(cpu: &Cpu, instr: u32, i_bit: bool) -> (u32, bool) {
    let carry = cpu.flag(FLAG_C);
    if i_bit {
        let imm8 = instr & 0xFF;
        let rotate = ((instr >> 8) & 0xF) * 2;
        if rotate == 0 {
            (imm8, carry)
        } else {
            let v = imm8.rotate_right(rotate);
            (v, v >> 31 != 0)
        }
    } else {
        let rm = cpu.reg((instr & 0xF) as usize);
        let shift_type = (instr >> 5) & 0x3;
        if instr & (1 << 4) != 0 {
            // Shift by register: amount is Rs low byte; PC reads are
            // +4 further on real hardware when Rm/Rs is R15 — our reg()
            // already models the fetch offset; the extra +4 for R15 as
            // operand is an acceptable Phase 1 simplification noted in
            // hardware-notes.
            let amount = cpu.reg(((instr >> 8) & 0xF) as usize) & 0xFF;
            // Register-specified LSL with amount 0 keeps carry.
            barrel_shift(rm, shift_type, amount, carry, true)
        } else {
            let amount = (instr >> 7) & 0x1F;
            barrel_shift(rm, shift_type, amount, carry, false)
        }
    }
}

fn exec_data_processing(cpu: &mut Cpu, instr: u32) -> u32 {
    let i_bit = instr & (1 << 25) != 0;
    let opcode = (instr >> 21) & 0xF;
    let s_bit = instr & (1 << 20) != 0;
    let rn = ((instr >> 16) & 0xF) as usize;
    let rd = ((instr >> 12) & 0xF) as usize;

    // MSR register form intercepted earlier; MSR immediate is caught by
    // test-opcode-with-1111 patterns below: TST/TEQ/CMP/CMN with Rd=1111
    // and S=0 encode MSR flag fields — rare; Phase 1 treats S=0 test
    // opcodes as their (architecturally unused) compare anyway.

    let a = cpu.reg(rn);
    let (b, shifter_carry) = shifter_operand(cpu, instr, i_bit);
    let carry_in = cpu.flag(FLAG_C);

    let is_test = matches!(opcode, 0x8 | 0x9 | 0xA | 0xB);
    let result: u32;
    let mut arith: Option<(bool, bool)> = None; // (carry, overflow)

    match opcode {
        0x0 => result = a & b,                       // AND
        0x1 => result = a ^ b,                       // EOR
        0x2 => { let (r, c, v) = Cpu::sub_flags(a, b, true); result = r; arith = Some((c, v)); } // SUB
        0x3 => { let (r, c, v) = Cpu::sub_flags(b, a, true); result = r; arith = Some((c, v)); } // RSB
        0x4 => { let (r, c, v) = Cpu::add_flags(a, b, false); result = r; arith = Some((c, v)); } // ADD
        0x5 => { let (r, c, v) = Cpu::add_flags(a, b, carry_in); result = r; arith = Some((c, v)); } // ADC
        0x6 => { let (r, c, v) = Cpu::sub_flags(a, b, carry_in); result = r; arith = Some((c, v)); } // SBC
        0x7 => { let (r, c, v) = Cpu::sub_flags(b, a, carry_in); result = r; arith = Some((c, v)); } // RSC
        0x8 => result = a & b,                       // TST
        0x9 => result = a ^ b,                       // TEQ
        0xA => { let (r, c, v) = Cpu::sub_flags(a, b, true); result = r; arith = Some((c, v)); } // CMP
        0xB => { let (r, c, v) = Cpu::add_flags(a, b, false); result = r; arith = Some((c, v)); } // CMN
        0xC => result = a | b,                       // ORR
        0xD => result = b,                           // MOV
        0xE => result = a & !b,                      // BIC
        _ => result = !b,                            // MVN
    }

    if is_test || s_bit {
        match arith {
            Some((c, v)) => cpu.set_nzcv(result, c, v),
            None => cpu.set_nzc(result, shifter_carry),
        }
    }
    if !is_test {
        if rd == 15 && s_bit {
            // Rd=PC with S: restore CPSR from SPSR (exception return).
            let spsr = cpu.spsr();
            cpu.set_reg(rd, result);
            cpu.set_cpsr_full(spsr);
        } else {
            cpu.set_reg(rd, result);
        }
    }
    if rd == 15 && !is_test { 3 } else { 1 }
}

// ---------------------------------------------------------------------------
// Multiply
// ---------------------------------------------------------------------------

fn exec_multiply(cpu: &mut Cpu, instr: u32) -> u32 {
    let s_bit = instr & (1 << 20) != 0;
    let rm = cpu.reg((instr & 0xF) as usize);
    let rs = cpu.reg(((instr >> 8) & 0xF) as usize);
    let opcode = (instr >> 21) & 0xF;

    if opcode <= 0b001 {
        // MUL (000) / MLA (001)
        let rd = ((instr >> 16) & 0xF) as usize;
        let rn = cpu.reg(((instr >> 12) & 0xF) as usize);
        let mut result = rm.wrapping_mul(rs);
        if opcode == 0b001 {
            result = result.wrapping_add(rn);
        }
        cpu.set_reg(rd, result);
        if s_bit {
            cpu.set_nz(result); // MUL/MLA set N,Z only on ARMv4
        }
        return 2;
    }
    // Long multiplies, opcode = bits 24..21 = 0,U,A,S-ish layout:
    // UMULL 0100, UMLAL 0101, SMULL 0110, SMLAL 0111 in this field.
    // Signed is the field's 0b010 bit; accumulate is 0b001.
    let rd_lo = ((instr >> 12) & 0xF) as usize;
    let rd_hi = ((instr >> 16) & 0xF) as usize;
    let accumulate = opcode & 1 != 0;
    let signed = opcode & 0b010 != 0;
    let existing: u64 = (u64::from(cpu.reg(rd_hi)) << 32) | u64::from(cpu.reg(rd_lo));
    let product: u64 = if signed {
        (i64::from(rm as i32) * i64::from(rs as i32)) as u64
    } else {
        u64::from(rm) * u64::from(rs)
    };
    let result = if accumulate { existing.wrapping_add(product) } else { product };
    cpu.set_reg(rd_lo, result as u32);
    cpu.set_reg(rd_hi, (result >> 32) as u32);
    if s_bit {
        cpu.set_flag(FLAG_N, result >> 63 != 0);
        cpu.set_flag(FLAG_Z, result == 0);
    }
    4
}

// ---------------------------------------------------------------------------
// PSR transfer (MSR)
// ---------------------------------------------------------------------------

fn msr_apply(cpu: &mut Cpu, instr: u32, value: u32) -> u32 {
    let to_spsr = instr & (1 << 22) != 0;
    let field_mask = (instr >> 16) & 0xF;
    let mut mask: u32 = 0;
    if field_mask & 1 != 0 { mask |= 0x0000_00FF; } // control
    if field_mask & 2 != 0 { mask |= 0x0000_FF00; } // extension
    if field_mask & 4 != 0 { mask |= 0x00FF_0000; } // status
    if field_mask & 8 != 0 { mask |= 0xFF00_0000; } // flags
    // In User mode only the flags field may be written to CPSR.
    if !to_spsr && cpu.mode() == MODE_USER {
        mask &= 0xFF00_0000;
    }
    if to_spsr {
        let cur = cpu.spsr();
        cpu.set_spsr((cur & !mask) | (value & mask));
    } else if mask & 0xFF != 0 {
        // Control field may change the mode: go through full handling.
        let cur = cpu.cpsr;
        let new_val = (cur & !mask) | (value & mask);
        cpu.set_cpsr_full(new_val);
    } else {
        cpu.cpsr = (cpu.cpsr & !mask) | (value & mask);
    }
    1
}

fn exec_msr_reg(cpu: &mut Cpu, instr: u32) -> u32 {
    let value = cpu.reg((instr & 0xF) as usize);
    msr_apply(cpu, instr, value)
}

fn exec_msr_imm(cpu: &mut Cpu, instr: u32) -> u32 {
    let imm8 = instr & 0xFF;
    let rotate = ((instr >> 8) & 0xF) * 2;
    let value = imm8.rotate_right(rotate);
    msr_apply(cpu, instr, value)
}

// ---------------------------------------------------------------------------
// Single load/store (word & byte)
// ---------------------------------------------------------------------------

fn exec_load_store(cpu: &mut Cpu, bus: &mut Bus, instr: u32, reg_offset: bool) -> u32 {
    let p = instr & (1 << 24) != 0;
    let u = instr & (1 << 23) != 0;
    let b = instr & (1 << 22) != 0;
    let w = instr & (1 << 21) != 0;
    let l = instr & (1 << 20) != 0;
    let rn = ((instr >> 16) & 0xF) as usize;
    let rd = ((instr >> 12) & 0xF) as usize;

    let offset: u32 = if reg_offset {
        let rm = cpu.reg((instr & 0xF) as usize);
        let (v, _) = barrel_shift(rm, (instr >> 5) & 0x3, (instr >> 7) & 0x1F, cpu.flag(FLAG_C), false);
        v
    } else {
        instr & 0xFFF
    };

    let base = cpu.reg(rn);
    let transfer_addr = if p {
        if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) }
    } else {
        base
    };

    if l {
        let value = if b {
            u32::from(bus.read8(transfer_addr))
        } else {
            let raw = bus.read32(transfer_addr & !3);
            raw.rotate_right((transfer_addr & 3) * 8)
        };
        cpu.set_reg(rd, value);
    } else {
        let value = cpu.reg(rd);
        if b {
            bus.write8(transfer_addr, value as u8);
        } else {
            bus.write32(transfer_addr & !3, value);
        }
    }

    if !p {
        // Post-indexed: always writes back.
        let new_base = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        cpu.set_reg(rn, new_base);
    } else if w {
        let new_base = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        cpu.set_reg(rn, new_base);
    }
    3
}

// ---------------------------------------------------------------------------
// Extra load/store: halfword & signed byte/halfword, and SWP
// ---------------------------------------------------------------------------

fn exec_extra_load_store(cpu: &mut Cpu, bus: &mut Bus, instr: u32) -> u32 {
    // SWP: bits 27..23 = 0b00010, bit 22 = byte, bits 11..4 = 0b0000_1001..
    if (instr >> 23) & 0x1F == 0b00010 && (instr >> 4) & 0xF == 0b1001 {
        let byte = instr & (1 << 22) != 0;
        let rn = cpu.reg(((instr >> 16) & 0xF) as usize);
        let rd = ((instr >> 12) & 0xF) as usize;
        let rm = cpu.reg((instr & 0xF) as usize);
        if byte {
            let old = bus.read8(rn);
            bus.write8(rn, rm as u8);
            cpu.set_reg(rd, u32::from(old));
        } else {
            let raw = bus.read32(rn & !3);
            let old = raw.rotate_right((rn & 3) * 8);
            bus.write32(rn & !3, rm);
            cpu.set_reg(rd, old);
        }
        return 4;
    }

    let p = instr & (1 << 24) != 0;
    let u = instr & (1 << 23) != 0;
    let i = instr & (1 << 22) != 0; // immediate offset flag in this space
    let w = instr & (1 << 21) != 0;
    let l = instr & (1 << 20) != 0;
    let rn = ((instr >> 16) & 0xF) as usize;
    let rd = ((instr >> 12) & 0xF) as usize;
    let sh = (instr >> 5) & 0x3; // 01=H, 10=signed byte, 11=signed half

    let offset: u32 = if i {
        ((instr >> 4) & 0xF0) | (instr & 0xF)
    } else {
        cpu.reg((instr & 0xF) as usize)
    };

    let base = cpu.reg(rn);
    let addr = if p {
        if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) }
    } else {
        base
    };

    if l {
        let value = match sh {
            0b01 => u32::from(bus.read16(addr & !1)),
            0b10 => u32::from(bus.read8(addr) as i8 as i32 as u32),
            _ => u32::from(bus.read16(addr & !1) as i16 as i32 as u32),
        };
        cpu.set_reg(rd, value);
    } else {
        // STRH is the only store here (SH=01 with L=0).
        let value = cpu.reg(rd);
        bus.write16(addr & !1, value as u16);
    }

    if !p || w {
        let new_base = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        cpu.set_reg(rn, new_base);
    }
    3
}

// ---------------------------------------------------------------------------
// Block transfer
// ---------------------------------------------------------------------------

fn exec_block_transfer(cpu: &mut Cpu, bus: &mut Bus, instr: u32) -> u32 {
    let p = instr & (1 << 24) != 0;
    let u = instr & (1 << 23) != 0;
    let s = instr & (1 << 22) != 0;
    let w = instr & (1 << 21) != 0;
    let l = instr & (1 << 20) != 0;
    let rn = ((instr >> 16) & 0xF) as usize;
    let list = instr & 0xFFFF;

    let count = list.count_ones();
    let base = cpu.reg(rn);
    // Compute start address & writeback value per addressing mode.
    let (start, writeback) = match (p, u) {
        (false, true) => (base, base.wrapping_add(count * 4)),                    // IA
        (true, true) => (base.wrapping_add(4), base.wrapping_add(count * 4)),    // IB
        (false, false) => (base.wrapping_sub(count * 4).wrapping_add(4), base.wrapping_sub(count * 4)), // DA
        (true, false) => (base.wrapping_sub(count * 4), base.wrapping_sub(count * 4)),                  // DB
    };

    if count == 0 {
        // Empty list: R15 is transferred on ARMv4 and base adjusts by 0x40.
        let adj = if u { base.wrapping_add(0x40) } else { base.wrapping_sub(0x40) };
        if l {
            let v = bus.read32(start & !3);
            cpu.regs[15] = v & !3;
        } else {
            bus.write32(start & !3, cpu.reg(15));
        }
        if w {
            cpu.set_reg(rn, adj);
        }
        return 4;
    }

    let mut addr = start;
    for reg in 0..16usize {
        if list & (1 << reg) == 0 {
            continue;
        }
        if l {
            let v = bus.read32(addr & !3);
            if reg == 15 {
                cpu.regs[15] = v & !3;
                if s {
                    // Load PC + S: restore CPSR from SPSR.
                    let spsr = cpu.spsr();
                    cpu.set_cpsr_full(spsr);
                }
            } else {
                cpu.set_reg(reg, v);
            }
        } else {
            // Store: R15 stores the pipeline value (addr of instr + 12
            // on real hardware from a base of +8 model: our reg(15)
            // returns +8 relative to the instruction; hardware stores
            // +12 — noted as a Phase 1 simplification).
            let v = cpu.reg(reg);
            bus.write32(addr & !3, v);
        }
        addr = addr.wrapping_add(4);
    }
    if w && !(l && list & (1 << rn) != 0) {
        cpu.set_reg(rn, writeback);
    }
    2 + count
}
