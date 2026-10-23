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
            // MSR immediate hides inside the test-opcode space:
            // 0011 0R10 <field mask> 1111 <rot> <imm8>. The field mask
            // (bits 19..16) is part of the instruction, NOT a fixed
            // 1111 — an earlier decode demanded 0xF there and silently
            // dropped MSR #imm into data processing (caught by
            // jsmolka arm test 001: MSR CPSR_f, #FLAG_Z).
            if instr & 0x0FB0_F000 == 0x0320_F000 {
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
        let rm_idx = (instr & 0xF) as usize;
        let shift_type = (instr >> 5) & 0x3;
        if instr & (1 << 4) != 0 {
            // Shift by register: when Rm or Rs is R15, the value read
            // is instruction address + 12 (one pipeline stage further
            // than the usual +8) — jsmolka arm test 224
            // (`mov r0, pc, lsl r0`) pins this down. regs[15] here is
            // already address+4, so +8 more reaches +12.
            let rm = if rm_idx == 15 {
                cpu.regs[15].wrapping_add(8)
            } else {
                cpu.reg(rm_idx)
            };
            let rs_idx = ((instr >> 8) & 0xF) as usize;
            let amount = if rs_idx == 15 {
                cpu.regs[15].wrapping_add(8) & 0xFF
            } else {
                cpu.reg(rs_idx) & 0xFF
            };
            barrel_shift(rm, shift_type, amount, carry, true)
        } else {
            let rm = cpu.reg(rm_idx);
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

    // PC as operand 1 also reads +12 (not +8) when operand 2 is a
    // register-specified shift (jsmolka arm test 225).
    let a = if rn == 15 && !i_bit && instr & (1 << 4) != 0 {
        cpu.regs[15].wrapping_add(8)
    } else {
        cpu.reg(rn)
    };
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

    if rd == 15 && s_bit {
        // Rd=PC with S restores CPSR from SPSR (exception return) —
        // and this applies to the test opcodes too: a CMP with Rd
        // encoded as 15 changes the mode instead of just setting
        // flags (jsmolka arm test 234 relies on exactly this).
        let spsr = cpu.spsr();
        if !is_test {
            cpu.set_reg(rd, result);
        }
        cpu.set_cpsr_full(spsr);
        return 3;
    }
    if is_test || s_bit {
        match arith {
            Some((c, v)) => cpu.set_nzcv(result, c, v),
            None => cpu.set_nzc(result, shifter_carry),
        }
    }
    if !is_test {
        cpu.set_reg(rd, result);
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

    // Writeback value, applied at different points per direction:
    // for loads it lands BEFORE Rd is set, so when Rd == Rn the
    // loaded value wins (jsmolka arm test 360); for stores it lands
    // after, so a stored Rn keeps its original value.
    let new_base = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
    let writeback = !p || w;

    if l {
        if writeback {
            cpu.set_reg(rn, new_base);
        }
        let value = if b {
            u32::from(bus.read8(transfer_addr))
        } else {
            let raw = bus.read32(transfer_addr & !3);
            raw.rotate_right((transfer_addr & 3) * 8)
        };
        cpu.set_reg(rd, value);
    } else {
        // STR of R15 stores instruction address + 12 (jsmolka arm
        // test 356), not the +8 a plain PC read returns.
        let value = if rd == 15 {
            cpu.regs[15].wrapping_add(8)
        } else {
            cpu.reg(rd)
        };
        if b {
            bus.write8(transfer_addr, value as u8);
        } else {
            bus.write32(transfer_addr, value);
        }
    }

    if !l && writeback {
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

    // Same writeback ordering rule as word transfers: loads apply
    // writeback before Rd so Rd == Rn keeps the loaded value
    // (jsmolka arm test 412); stores apply it after.
    let wb_value = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
    let do_wb = !p || w;
    if l {
        if do_wb {
            cpu.set_reg(rn, wb_value);
        }
        let value = match sh {
            // LDRH: misaligned reads return the aligned halfword
            // rotated right by 8 (GBA behaviour, jsmolka thumb 211).
            0b01 => u32::from(bus.read16(addr & !1)).rotate_right((addr & 1) * 8),
            0b10 => u32::from(bus.read8(addr) as i8 as i32 as u32),
            // LDRSH: at an odd address it behaves like LDRSB.
            _ => {
                if addr & 1 != 0 {
                    u32::from(bus.read8(addr) as i8 as i32 as u32)
                } else {
                    u32::from(bus.read16(addr & !1) as i16 as i32 as u32)
                }
            }
        };
        cpu.set_reg(rd, value);
    } else {
        // STRH is the only store here (SH=01 with L=0). The bus
        // applies the region rule for odd addresses: aligned
        // halfword store in normal memory, single high-byte store
        // in the byte-wide save region.
        let value = cpu.reg(rd);
        bus.write16(addr, value as u16);
        if do_wb {
            cpu.set_reg(rn, wb_value);
        }
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
        // Empty list: R15 is transferred on ARMv4 and base adjusts by
        // 0x40; the transfer address is computed as if all 16
        // registers were in the list (test 530: STMDA stores at
        // base-0x3C).
        let adj = if u { base.wrapping_add(0x40) } else { base.wrapping_sub(0x40) };
        let xfer = match (p, u) {
            (false, true) => base,
            (true, true) => base.wrapping_add(4),
            (false, false) => base.wrapping_sub(0x3C),
            (true, false) => base.wrapping_sub(0x40),
        };
        if l {
            let v = bus.read32(xfer & !3);
            cpu.regs[15] = v & !3;
        } else {
            // Stored PC is instruction address + 12 (test 515).
            bus.write32(xfer, cpu.regs[15].wrapping_add(8));
        }
        if w {
            cpu.set_reg(rn, adj);
        }
        return 4;
    }

    let lowest = (0..16usize).find(|&r| list & (1 << r) != 0).unwrap_or(0);
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
            } else if s {
                // Load into the user bank (S bit, no PC in list).
                cpu.set_user_reg(reg, v);
            } else {
                cpu.set_reg(reg, v);
            }
        } else {
            // Store quirks: R15 stores instruction address + 12
            // (regs[15] holds +4 in our model, reg() reads +8). The
            // base register, when in the list but not first, stores
            // its writeback value (jsmolka thumb test 230). With the
            // S bit, values come from the user bank (test 511).
            let v = if s && reg != 15 {
                cpu.user_reg(reg)
            } else if reg == 15 {
                cpu.regs[15].wrapping_add(8)
            } else if reg == rn && reg != lowest {
                writeback
            } else {
                cpu.reg(reg)
            };
            bus.write32(addr, v);
        }
        addr = addr.wrapping_add(4);
    }
    if w && !(l && list & (1 << rn) != 0) {
        cpu.set_reg(rn, writeback);
    }
    2 + count
}
