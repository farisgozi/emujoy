//! THUMB state instruction execution (ARMv4T, formats 1–19).

use super::*;
use crate::bus::Bus;

pub fn step_thumb(cpu: &mut Cpu, bus: &mut Bus) -> u32 {
    let addr = cpu.regs[15];
    let instr = bus.read16(addr & !1);
    cpu.regs[15] = addr.wrapping_add(2);
    // PC as seen by the instruction: addr + 4.
    let pc = addr.wrapping_add(4);

    match instr >> 13 {
        0b000 => {
            if (instr >> 11) & 0x3 == 0b11 {
                thumb_add_sub(cpu, instr)
            } else {
                thumb_shift(cpu, instr)
            }
        }
        0b001 => thumb_imm_op(cpu, instr),
        0b010 => {
            match (instr >> 10) & 0x7 {
                0b000 => thumb_alu_hi(cpu, bus, instr, false), // ALU ops (010000)
                0b001 => thumb_alu_hi(cpu, bus, instr, true),  // Hi-reg / BX (010001)
                0b010 | 0b011 => {
                    // PC-relative load: 01001 Rd, imm8*4
                    let rd = ((instr >> 8) & 0x7) as usize;
                    let target = (pc & !2).wrapping_add(u32::from(instr & 0xFF) * 4);
                    let v = bus.read32(target & !3);
                    cpu.set_reg(rd, v);
                    3
                }
                _ => {
                    // 0101: load/store with register offset (0101 L B 0), sign-extended variants (0101 H1 H2 1)
                    if instr & (1 << 9) == 0 {
                        thumb_ls_reg(cpu, bus, instr)
                    } else {
                        thumb_ls_sign(cpu, bus, instr)
                    }
                }
            }
        }
        0b011 => thumb_ls_imm(cpu, bus, instr),
        0b100 => {
            if instr & (1 << 12) == 0 {
                // 1000: halfword imm offset
                let l = instr & (1 << 11) != 0;
                let rd = (instr & 0x7) as usize;
                let rb = ((instr >> 3) & 0x7) as usize;
                let off = u32::from((instr >> 6) & 0x1F) * 2;
                let a = cpu.reg(rb).wrapping_add(off);
                if l {
                    let v = bus.read16(a & !1);
                    cpu.set_reg(rd, u32::from(v).rotate_right((a & 1) * 8));
                } else {
                    bus.write16(a, cpu.reg(rd) as u16);
                }
                3
            } else {
                // 1001: SP-relative load/store
                let l = instr & (1 << 11) != 0;
                let rd = ((instr >> 8) & 0x7) as usize;
                let off = u32::from(instr & 0xFF) * 4;
                let a = cpu.reg(13).wrapping_add(off);
                if l {
                    let v = bus.read32(a & !3);
                    cpu.set_reg(rd, v.rotate_right((a & 3) * 8));
                } else {
                    bus.write32(a, cpu.reg(rd));
                }
                3
            }
        }
        0b101 => {
            if (instr >> 12) & 0xF == 0b1010 {
                // Format 12: ADD Rd, PC (bit 11 = 0) / ADD Rd, SP (bit 11 = 1)
                let rd = ((instr >> 8) & 0x7) as usize;
                let off = u32::from(instr & 0xFF) * 4;
                if instr & (1 << 11) == 0 {
                    let base = pc & !2;
                    cpu.set_reg(rd, base.wrapping_add(off));
                } else {
                    let sp = cpu.reg(13);
                    cpu.set_reg(rd, sp.wrapping_add(off));
                }
                1
            } else if (instr >> 8) & 0xF == 0b0000 {
                // 1011 0000 Siiiiiii: adjust SP by imm7*4 (sign at bit 7)
                let off = u32::from(instr & 0x7F) * 4;
                let sp = cpu.reg(13);
                if instr & (1 << 7) != 0 {
                    cpu.set_reg(13, sp.wrapping_sub(off));
                } else {
                    cpu.set_reg(13, sp.wrapping_add(off));
                }
                1
            } else if (instr >> 9) & 0x3 == 0b10 {
                // PUSH (1011 010R) / POP (1011 110R): bits 10..9 = 10
                thumb_push_pop(cpu, bus, instr)
            } else {
                // Other 1011 misc (BKPT etc. on later arch): nop for Phase 1.
                1
            }
        }
        0b110 => {
            if instr & (1 << 12) == 0 {
                thumb_block(cpu, bus, instr)
            } else if (instr >> 8) & 0xF == 0xF {
                // SWI
                let comment = u32::from(instr & 0xFF);
                cpu.last_swi = Some(comment);
                let lr = cpu.regs[15];
                cpu.exception(MODE_SVC, lr, 0x08);
                3
            } else {
                // Conditional branch
                let cond = u32::from((instr >> 8) & 0xF);
                if condition_passed(cond, cpu.cpsr) {
                    let signed = (instr & 0xFF) as u8 as i8 as i32 * 2;
                    cpu.regs[15] = (pc as i32).wrapping_add(signed) as u32;
                    3
                } else {
                    1
                }
            }
        }
        _ => {
            // 0b111
            if instr & (1 << 12) == 0 {
                // Unconditional branch 11100
                let signed = sign11(instr & 0x7FF);
                cpu.regs[15] = (pc as i32).wrapping_add(signed * 2) as u32;
                3
            } else if instr & (1 << 11) == 0 {
                // BL first half (11110): LR = PC + (sign-extended imm11 << 12)
                let signed = sign11(instr & 0x7FF);
                cpu.regs[14] = pc.wrapping_add((signed << 12) as u32);
                1
            } else {
                // BL second half (11111): branch to LR + imm11*2, link
                let target = cpu.regs[14].wrapping_add(u32::from(instr & 0x7FF) * 2);
                cpu.regs[14] = addr.wrapping_add(2) | 1;
                cpu.regs[15] = target & !1;
                3
            }
        }
    }
}

fn sign11(v: u16) -> i32 {
    let v = i32::from(v);
    if v & 0x400 != 0 { v - 0x800 } else { v }
}

fn thumb_shift(cpu: &mut Cpu, instr: u16) -> u32 {
    let op = (instr >> 11) & 0x3;
    let amount = u32::from((instr >> 6) & 0x1F);
    let rs = cpu.reg(((instr >> 3) & 0x7) as usize);
    let rd = (instr & 0x7) as usize;
    let carry = cpu.flag(FLAG_C);
    let shift_type = match op {
        0 => 0, // LSL
        1 => 1, // LSR (amount 0 means 32 in immediate form)
        _ => 2, // ASR
    };
    let (result, carry_out) = barrel_shift(rs, shift_type, amount, carry, false);
    cpu.set_reg(rd, result);
    cpu.set_nzc(result, carry_out);
    1
}

fn thumb_add_sub(cpu: &mut Cpu, instr: u16) -> u32 {
    let i_bit = instr & (1 << 10) != 0;
    let sub = instr & (1 << 9) != 0;
    let rs = cpu.reg(((instr >> 3) & 0x7) as usize);
    let operand = if i_bit {
        u32::from((instr >> 6) & 0x7)
    } else {
        cpu.reg(((instr >> 6) & 0x7) as usize)
    };
    let rd = (instr & 0x7) as usize;
    let (result, c, v) = if sub {
        Cpu::sub_flags(rs, operand, true)
    } else {
        Cpu::add_flags(rs, operand, false)
    };
    cpu.set_reg(rd, result);
    cpu.set_nzcv(result, c, v);
    1
}

fn thumb_imm_op(cpu: &mut Cpu, instr: u16) -> u32 {
    let op = (instr >> 11) & 0x3;
    let rd = ((instr >> 8) & 0x7) as usize;
    let imm = u32::from(instr & 0xFF);
    let a = cpu.reg(rd);
    match op {
        0 => {
            cpu.set_reg(rd, imm);
            cpu.set_nz(imm);
        }
        1 => {
            let (r, c, v) = Cpu::sub_flags(a, imm, true);
            cpu.set_nzcv(r, c, v);
        }
        2 => {
            let (r, c, v) = Cpu::add_flags(a, imm, false);
            cpu.set_reg(rd, r);
            cpu.set_nzcv(r, c, v);
        }
        _ => {
            let (r, c, v) = Cpu::sub_flags(a, imm, true);
            cpu.set_reg(rd, r);
            cpu.set_nzcv(r, c, v);
        }
    }
    1
}

fn thumb_alu_hi(cpu: &mut Cpu, bus: &mut Bus, instr: u16, hi: bool) -> u32 {
    if !hi {
        // Format 4: ALU operations on low registers.
        let op = (instr >> 6) & 0xF;
        let rs = cpu.reg(((instr >> 3) & 0x7) as usize);
        let rd = (instr & 0x7) as usize;
        let a = cpu.reg(rd);
        let carry = cpu.flag(FLAG_C);
        match op {
            0x0 => { let r = a & rs; cpu.set_reg(rd, r); cpu.set_nz(r); }
            0x1 => { let r = a ^ rs; cpu.set_reg(rd, r); cpu.set_nz(r); }
            0x2 => { let (r, c) = barrel_shift(a, 0, rs & 0xFF, carry, true); cpu.set_reg(rd, r); cpu.set_nzc(r, c); }
            0x3 => { let (r, c) = barrel_shift(a, 1, rs & 0xFF, carry, true); cpu.set_reg(rd, r); cpu.set_nzc(r, c); }
            0x4 => { let (r, c) = barrel_shift(a, 2, rs & 0xFF, carry, true); cpu.set_reg(rd, r); cpu.set_nzc(r, c); }
            0x5 => { let (r, c, v) = Cpu::add_flags(a, rs, carry); cpu.set_reg(rd, r); cpu.set_nzcv(r, c, v); }
            0x6 => { let (r, c, v) = Cpu::sub_flags(a, rs, carry); cpu.set_reg(rd, r); cpu.set_nzcv(r, c, v); }
            0x7 => { let (r, c) = barrel_shift(a, 3, rs & 0xFF, carry, true); cpu.set_reg(rd, r); cpu.set_nzc(r, c); }
            0x8 => { let r = a & rs; cpu.set_nz(r); } // TST
            0x9 => { let (r, c, v) = Cpu::sub_flags(0, rs, true); cpu.set_reg(rd, r); cpu.set_nzcv(r, c, v); } // NEG
            0xA => { let (r, c, v) = Cpu::sub_flags(a, rs, true); cpu.set_nzcv(r, c, v); } // CMP
            0xB => { let (r, c, v) = Cpu::add_flags(a, rs, false); cpu.set_nzcv(r, c, v); } // CMN
            0xC => { let r = a | rs; cpu.set_reg(rd, r); cpu.set_nz(r); }
            0xD => { let r = a.wrapping_mul(rs); cpu.set_reg(rd, r); cpu.set_nz(r); } // MUL: C/V unaffected-ish (Phase 1)
            0xE => { let r = a & !rs; cpu.set_reg(rd, r); cpu.set_nz(r); }
            _ => { let r = !rs; cpu.set_reg(rd, r); cpu.set_nz(r); } // MVN
        }
        let _ = bus;
        return 1;
    }
    // Format 5: hi register operations / BX.
    let op = (instr >> 8) & 0x3;
    let h1 = if instr & (1 << 7) != 0 { 8 } else { 0 };
    let h2 = if instr & (1 << 6) != 0 { 8 } else { 0 };
    let rs_idx = (((instr >> 3) & 0x7) + h2) as usize;
    let rd_idx = ((instr & 0x7) + h1) as usize;
    match op {
        0 => { let r = cpu.reg(rd_idx).wrapping_add(cpu.reg(rs_idx)); cpu.set_reg(rd_idx, r); }
        1 => { let (r, c, v) = Cpu::sub_flags(cpu.reg(rd_idx), cpu.reg(rs_idx), true); cpu.set_nzcv(r, c, v); }
        2 => { let v = cpu.reg(rs_idx); cpu.set_reg(rd_idx, v); }
        _ => {
            // BX Rs
            let target = cpu.reg(rs_idx);
            cpu.cpsr = (cpu.cpsr & !BIT_T) | ((target & 1) << 5);
            cpu.regs[15] = if target & 1 != 0 { target & !1 } else { target & !3 };
        }
    }
    1
}

fn thumb_ls_reg(cpu: &mut Cpu, bus: &mut Bus, instr: u16) -> u32 {
    let l = instr & (1 << 11) != 0;
    let b = instr & (1 << 10) != 0;
    let ro = cpu.reg(((instr >> 6) & 0x7) as usize);
    let rb = cpu.reg(((instr >> 3) & 0x7) as usize);
    let rd = (instr & 0x7) as usize;
    let a = rb.wrapping_add(ro);
    if l {
        if b {
            cpu.set_reg(rd, u32::from(bus.read8(a)));
        } else {
            let raw = bus.read32(a & !3);
            cpu.set_reg(rd, raw.rotate_right((a & 3) * 8));
        }
    } else if b {
        bus.write8(a, cpu.reg(rd) as u8);
    } else {
        bus.write32(a, cpu.reg(rd));
    }
    3
}

fn thumb_ls_sign(cpu: &mut Cpu, bus: &mut Bus, instr: u16) -> u32 {
    let h = instr & (1 << 11) != 0; // STRH/LDRH selector combined w/ S
    let s = instr & (1 << 10) != 0;
    let ro = cpu.reg(((instr >> 6) & 0x7) as usize);
    let rb = cpu.reg(((instr >> 3) & 0x7) as usize);
    let rd = (instr & 0x7) as usize;
    let a = rb.wrapping_add(ro);
    match (h, s) {
        (false, false) => {
            // STRH — the bus applies the odd-address region rule.
            bus.write16(a, cpu.reg(rd) as u16);
        }
        (false, true) => {
            // LDRSB
            cpu.set_reg(rd, u32::from(bus.read8(a) as i8 as i32 as u32));
        }
        (true, false) => {
            // LDRH: misaligned reads rotate the aligned halfword.
            let v = bus.read16(a & !1);
            cpu.set_reg(rd, u32::from(v).rotate_right((a & 1) * 8));
        }
        _ => {
            // LDRSH: odd address behaves like LDRSB.
            if a & 1 != 0 {
                cpu.set_reg(rd, u32::from(bus.read8(a) as i8 as i32 as u32));
            } else {
                let v = bus.read16(a & !1) as i16;
                cpu.set_reg(rd, v as i32 as u32);
            }
        }
    }
    3
}

fn thumb_ls_imm(cpu: &mut Cpu, bus: &mut Bus, instr: u16) -> u32 {
    let b = instr & (1 << 12) != 0;
    let l = instr & (1 << 11) != 0;
    let off5 = u32::from((instr >> 6) & 0x1F);
    let rb = cpu.reg(((instr >> 3) & 0x7) as usize);
    let rd = (instr & 0x7) as usize;
    let a = rb.wrapping_add(if b { off5 } else { off5 * 4 });
    if l {
        if b {
            cpu.set_reg(rd, u32::from(bus.read8(a)));
        } else {
            let raw = bus.read32(a & !3);
            cpu.set_reg(rd, raw.rotate_right((a & 3) * 8));
        }
    } else if b {
        bus.write8(a, cpu.reg(rd) as u8);
    } else {
        bus.write32(a, cpu.reg(rd));
    }
    3
}

fn thumb_push_pop(cpu: &mut Cpu, bus: &mut Bus, instr: u16) -> u32 {
    let pop = instr & (1 << 11) != 0;
    let r_bit = instr & (1 << 8) != 0;
    let list = instr & 0xFF;
    let count = list.count_ones() + u32::from(r_bit);
    let sp = cpu.reg(13);
    if !pop {
        // PUSH: full descending.
        let mut addr = sp.wrapping_sub(count * 4);
        let start = addr;
        for reg in 0..8usize {
            if list & (1 << reg) != 0 {
                bus.write32(addr & !3, cpu.reg(reg));
                addr = addr.wrapping_add(4);
            }
        }
        if r_bit {
            bus.write32(addr & !3, cpu.regs[14]);
        }
        cpu.set_reg(13, start);
    } else {
        let mut addr = sp;
        for reg in 0..8usize {
            if list & (1 << reg) != 0 {
                let v = bus.read32(addr & !3);
                cpu.set_reg(reg, v);
                addr = addr.wrapping_add(4);
            }
        }
        if r_bit {
            let v = bus.read32(addr & !3);
            cpu.regs[15] = v & !1;
            addr = addr.wrapping_add(4);
        }
        cpu.set_reg(13, addr);
    }
    2 + count
}

fn thumb_block(cpu: &mut Cpu, bus: &mut Bus, instr: u16) -> u32 {
    let l = instr & (1 << 11) != 0;
    let rb = ((instr >> 8) & 0x7) as usize;
    let list = instr & 0xFF;
    let base = cpu.reg(rb);
    let mut addr = base;
    if list == 0 {
        // Empty list: PC is transferred and base advances 0x40 (ARMv4 behaviour).
        if l {
            let v = bus.read32(addr & !3);
            cpu.regs[15] = v & !1;
        } else {
            // THUMB STM stores PC as seen two bytes further along
            // than a plain PC read (jsmolka thumb test 229 compares
            // the stored value against a following MOV Rd, PC).
            bus.write32(addr & !3, cpu.reg(15).wrapping_add(2));
        }
        cpu.set_reg(rb, base.wrapping_add(0x40));
        return 4;
    }
    let lowest = (0..8usize).find(|&r| list & (1 << r) != 0).unwrap_or(0);
    for reg in 0..8usize {
        if list & (1 << reg) == 0 {
            continue;
        }
        if l {
            let v = bus.read32(addr & !3);
            cpu.set_reg(reg, v);
        } else if reg == rb && reg != lowest {
            // Base in the list but not first: stores the writeback
            // value (base + 4*count), per ARMv4 (thumb test 230).
            bus.write32(addr, base.wrapping_add(list.count_ones() * 4));
        } else {
            bus.write32(addr, cpu.reg(reg));
        }
        addr = addr.wrapping_add(4);
    }
    cpu.set_reg(rb, addr);
    2 + list.count_ones()
}
