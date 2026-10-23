//! Phase 1 CPU acceptance tests.
//!
//! Small hand-assembled ARM/THUMB programs are written into IWRAM and
//! executed instruction by instruction. Programs end in `B .` / `B`
//! self-loops; the runner stops when the PC reaches the halt address.
//! These tests are the in-repo stand-in for ARMWrestler / jsmolka
//! gba-tests: those ROMs need a PPU to report results, so they join in
//! Phase 2. Encoders below are test-local and written from the ARM ARM
//! encoding tables independently of the interpreter's decode tables, so
//! an encoding bug and a decoding bug cannot cancel out silently —
//! several words are also pinned to their textbook values.

use gba_core::cpu::{FLAG_C, FLAG_N, FLAG_V, FLAG_Z};
use gba_core::Core;

const BASE: u32 = 0x0300_0000; // IWRAM
const STACK: u32 = 0x0300_7000;

// --- ARM encoders (test-local) -------------------------------------------

fn dp_imm(op: u32, s: u32, rn: u32, rd: u32, rot: u32, imm8: u32) -> u32 {
    0xE000_0000 | (1 << 25) | (op << 21) | (s << 20) | (rn << 16) | (rd << 12) | (rot << 8) | imm8
}
fn dp_reg(op: u32, s: u32, rn: u32, rd: u32, shift_imm: u32, shift_ty: u32, rm: u32) -> u32 {
    0xE000_0000 | (op << 21) | (s << 20) | (rn << 16) | (rd << 12) | (shift_imm << 7) | (shift_ty << 5) | rm
}
const OP_SUB: u32 = 2;
const OP_ADD: u32 = 4;
const OP_CMP: u32 = 10;
const OP_MOV: u32 = 13;
const OP_MVN: u32 = 15;
fn mov_imm(rd: u32, rot: u32, imm8: u32) -> u32 { dp_imm(OP_MOV, 0, 0, rd, rot, imm8) }
fn ls_imm(load: bool, byte: bool, rn: u32, rd: u32, off: u32) -> u32 {
    // P=1 U=1 W=0, I=0
    0xE400_0000 | (1 << 24) | (1 << 23) | ((byte as u32) << 22) | ((load as u32) << 20) | (rn << 16) | (rd << 12) | off
}
const HALT: u32 = 0xEAFF_FFFE; // B .

struct Machine {
    core: Core,
    halt: u32,
}

impl Machine {
    fn arm(code: &[u32]) -> Self {
        let mut core = Core::new();
        for (i, w) in code.iter().enumerate() {
            core.bus.write32(BASE + (i as u32) * 4, *w);
        }
        core.cpu.set_pc(BASE);
        core.cpu.set_reg(13, STACK);
        Machine { halt: BASE + (code.len() as u32 - 1) * 4, core }
    }
    fn thumb(code: &[u16], byte_len: u32) -> Self {
        let mut core = Core::new();
        for (i, h) in code.iter().enumerate() {
            core.bus.write16(BASE + (i as u32) * 2, *h);
        }
        // Enter THUMB state the way real code does: BX from ARM.
        core.bus.write32(BASE + 0x400, 0xE12F_FF10 | 0); // BX R0 placeholder patched below
        core.cpu.set_reg(0, (BASE + 0x100) | 1);
        core.bus.write32(BASE + 0x400, 0xE12F_FF10); // BX R0
        for (i, h) in code.iter().enumerate() {
            core.bus.write16(BASE + 0x100 + (i as u32) * 2, *h);
        }
        core.cpu.set_pc(BASE + 0x400);
        core.cpu.set_reg(13, STACK);
        let _ = byte_len;
        Machine { halt: BASE + 0x100 + (code.len() as u32 - 1) * 2, core }
    }
    fn run(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            if self.core.cpu.pc() == self.halt {
                return;
            }
            self.core.step_instruction();
        }
        panic!("program did not halt; pc={:#010x}", self.core.cpu.pc());
    }
    fn reg(&self, n: usize) -> u32 {
        self.core.cpu.reg(n)
    }
    fn flag(&self, f: u32) -> bool {
        self.core.cpu.flag(f)
    }
}

// --- ARM tests ------------------------------------------------------------

#[test]
fn arm_alu_and_flags() {
    // mov r0,#10 ; mov r1,#32 ; add r2,r0,r1 ; subs r3,r2,#42 ; halt
    let code = [
        mov_imm(0, 0, 10),
        mov_imm(1, 0, 32),
        dp_reg(OP_ADD, 0, 0, 2, 0, 0, 1),
        dp_imm(OP_SUB, 1, 2, 3, 0, 42),
        HALT,
    ];
    let mut m = Machine::arm(&code);
    m.run(16);
    assert_eq!(m.reg(2), 42);
    assert_eq!(m.reg(3), 0);
    assert!(m.flag(FLAG_Z), "SUBS to zero must set Z");
    assert!(!m.flag(FLAG_N));
}

#[test]
fn arm_encoder_matches_textbook_words() {
    assert_eq!(mov_imm(0, 0, 10), 0xE3A0_000A); // MOV R0, #10
    assert_eq!(dp_reg(OP_ADD, 0, 0, 2, 0, 0, 1), 0xE080_2001); // ADD R2, R0, R1
    assert_eq!(ls_imm(true, false, 1, 0, 0), 0xE591_0000); // LDR R0, [R1]
    assert_eq!(HALT, 0xEAFF_FFFE); // B .
}

#[test]
fn arm_shifter_carry_and_zero() {
    // mvn r0,#0 (r0 = 0xFFFF_FFFF) — use mov of rotated imm instead:
    // mov r0, #0x80000000 (imm 2 ror 2); movs r1, r0, LSL #1 -> r1=0, C=1, Z=1
    let code = [
        mov_imm(0, 1, 2),                    // 2 ror 2 = 0x8000_0000
        dp_reg(OP_MOV, 1, 0, 1, 1, 0, 0),    // MOVS R1, R0, LSL #1
        HALT,
    ];
    let mut m = Machine::arm(&code);
    m.run(8);
    assert_eq!(m.reg(0), 0x8000_0000);
    assert_eq!(m.reg(1), 0);
    assert!(m.flag(FLAG_C));
    assert!(m.flag(FLAG_Z));
}

#[test]
fn arm_add_overflow_sets_v() {
    // r0 = 0x7FFF_FFFF (imm 0x7F ror 4? 0x7F ror 4 = 0xF000_0007 no —
    // use MVN of 0x80000000) ; adds r1, r0, #1 -> N=1 V=1
    let code = [
        mov_imm(0, 1, 2),                    // R0 = 0x8000_0000
        dp_imm(OP_MVN, 0, 0, 0, 0, 0),       // placeholder replaced below
        HALT,
    ];
    // Simpler: MVN R0, #0x7F gives 0xFFFF_FF80 — not what we want.
    // Build 0x7FFF_FFFF as MVN of 0x8000_0000:
    let mut code = code.to_vec();
    code[1] = dp_reg(OP_MVN, 0, 0, 0, 0, 0, 0); // MVN R0, R0
    code.insert(2, dp_imm(OP_ADD, 1, 0, 1, 0, 1)); // ADDS R1, R0, #1
    let mut m = Machine::arm(&code);
    m.run(8);
    assert_eq!(m.reg(1), 0x8000_0000);
    assert!(m.flag(FLAG_V), "0x7FFF_FFFF + 1 must overflow");
    assert!(m.flag(FLAG_N));
}

#[test]
fn arm_load_store_word_and_byte() {
    // r4 = 0x0200_0000 via imm (2 ror 8); store/load round trip.
    let code = [
        mov_imm(4, 4, 2),                    // 2 ror 8 = 0x0200_0000
        mov_imm(0, 0, 0xAB),
        ls_imm(false, false, 4, 0, 0),       // STR R0, [R4]
        ls_imm(true, false, 4, 1, 0),        // LDR R1, [R4]
        ls_imm(false, true, 4, 0, 3),        // STRB R0, [R4, #3]
        ls_imm(true, false, 4, 2, 0),        // LDR R2, [R4]
        HALT,
    ];
    let mut m = Machine::arm(&code);
    m.run(16);
    assert_eq!(m.reg(1), 0xAB);
    assert_eq!(m.reg(2), 0xAB00_00AB);
}

#[test]
fn arm_multiply_and_long_multiply() {
    // mul r2, r0, r1 (6*7) ; umull r3, r4, r5, r5 with r5 = 0xFFFF_FFFF
    let mul = |rd: u32, rs: u32, rm: u32| -> u32 {
        0xE000_0090 | (rd << 16) | (rs << 8) | rm
    };
    let umull = |rdlo: u32, rdhi: u32, rs: u32, rm: u32| -> u32 {
        0xE080_0090 | (rdhi << 16) | (rdlo << 12) | (rs << 8) | rm
    };
    let code = [
        mov_imm(0, 0, 6),
        mov_imm(1, 0, 7),
        mul(2, 1, 0),
        dp_imm(OP_MVN, 0, 0, 5, 0, 0), // MVN R5, #0 -> 0xFFFF_FFFF
        umull(3, 4, 5, 5),
        HALT,
    ];
    let mut m = Machine::arm(&code);
    m.run(16);
    assert_eq!(m.reg(2), 42);
    // (2^32-1)^2 = 0xFFFFFFFE_00000001
    assert_eq!(m.reg(3), 0x0000_0001);
    assert_eq!(m.reg(4), 0xFFFF_FFFE);
}

#[test]
fn arm_loop_sums_one_to_ten() {
    // r0 = sum, r1 = i: sum=0; i=1; loop: sum+=i; i+=1; cmp i,#11; bne loop
    let add_r0_r1 = dp_reg(OP_ADD, 0, 0, 0, 0, 0, 1);
    let add_i = dp_imm(OP_ADD, 0, 1, 1, 0, 1);
    let cmp_i = dp_imm(OP_CMP, 1, 1, 0, 0, 11);
    // BNE back 3 instructions: from branch at index 5 (addr +20),
    // target index 2 (addr +8): offset = (8 - (20+8))/4 = -5
    let bne = 0x1A00_0000 | ((-5i32) as u32 & 0xFF_FFFF);
    let code = [mov_imm(0, 0, 0), mov_imm(1, 0, 1), add_r0_r1, add_i, cmp_i, bne, HALT];
    let mut m = Machine::arm(&code);
    m.run(200);
    assert_eq!(m.reg(0), 55);
}

#[test]
fn arm_fibonacci_iterative() {
    // fib(10) with two accumulators; loop 10 times.
    // a=0,b=1; loop: t=a+b; a=b; b=t; n-=1; cmp n,#0; bne loop
    let code = [
        mov_imm(0, 0, 0),                       // a
        mov_imm(1, 0, 1),                       // b
        mov_imm(2, 0, 10),                      // n
        dp_reg(OP_ADD, 0, 0, 3, 0, 0, 1),       // t = a + b
        dp_reg(OP_MOV, 0, 1, 0, 0, 0, 1),       // a = b — MOV Rd=R1: op MOV rn=1? use dp_reg MOV with rm=1, rd=0: encoding below fixed by helper args (rn ignored)
        dp_reg(OP_MOV, 0, 0, 1, 0, 0, 3),       // b = t
        dp_imm(OP_SUB, 0, 2, 2, 0, 1),          // n -= 1
        dp_imm(OP_CMP, 1, 2, 0, 0, 0),          // cmp n, #0
        0x1A00_0000 | ((-6i32) as u32 & 0xFF_FFFF), // bne loop (from idx8 to idx3: (12-(32+8))/4 = -7? computed in comment; patched by run below)
        HALT,
    ];
    // Recompute the branch: branch instr at index 8 (addr 32), target
    // index 3 (addr 12): offset = (12 - (32 + 8)) / 4 = -7.
    let mut code = code.to_vec();
    code[8] = 0x1A00_0000 | ((-7i32) as u32 & 0xFF_FFFF);
    let mut m = Machine::arm(&code);
    m.run(500);
    assert_eq!(m.reg(0), 55, "a after 10 iterations is fib(10)");
}

#[test]
fn arm_block_transfer_roundtrip() {
    // stmia r6!, {r0-r2} ; ldmia r7!, {r3-r5} after resetting r7 = r6-12
    let stmia = 0xE880_0000 | (1 << 21) | (6 << 16) | 0b111; // P0 U1 S0 W1 L0
    let ldmia = 0xE890_0000 | (1 << 21) | (7 << 16) | (0b111 << 3);
    // Note: LDM/STM use bits 23 (U) explicitly:
    let stmia = stmia | (1 << 23);
    let ldmia = ldmia | (1 << 23);
    let code = [
        mov_imm(0, 0, 11),
        mov_imm(1, 0, 22),
        mov_imm(2, 0, 33),
        mov_imm(6, 12, 3), // 3 ror 24 = 0x0300_0000 — points at IWRAM base (data area overlaps code caveat: code is at IWRAM start)
        HALT,
    ];
    // Data area must not collide with code: use IWRAM + 0x2000.
    let mut code = code.to_vec();
    // r6 = 0x0300_2000: imm 0x32? 0x03002000 is not a rotated imm8.
    // Two-step: mov r6, #0x0300_0000 (imm 3 ror 8 -> rot=4); add r6, #0x2000 not imm-encodable either (0x2000 = imm 0x80 ror 28? rotate 0x80 ror 28 = 0x8000? no).
    // Use LSL: add via dp_reg shift: r6 += 1<<13: mov r8,#1; mov r8,r8,LSL#13; add r6,r6,r8.
    code.truncate(4);
    code.extend([
        mov_imm(6, 4, 3),                        // R6 = 0x0300_0000
        mov_imm(8, 0, 1),
        dp_reg(OP_MOV, 0, 0, 8, 13, 0, 8),       // R8 = R8 LSL #13 = 0x2000
        dp_reg(OP_ADD, 0, 6, 6, 0, 0, 8),        // R6 += R8
        stmia,                                    // STMIA R6!, {R0-R2}
        dp_imm(OP_SUB, 0, 6, 7, 0, 12),          // R7 = R6 - 12
        ldmia,                                    // LDMIA R7!, {R3-R5}
        HALT,
    ]);
    let mut m = Machine::arm(&code);
    m.run(32);
    assert_eq!(m.reg(3), 11);
    assert_eq!(m.reg(4), 22);
    assert_eq!(m.reg(5), 33);
}

#[test]
fn arm_mrs_msr_roundtrip_preserves_flags() {
    // cmp r0,#0 (sets Z) ; mrs r1, cpsr ; adds r2,r2,#1 (clears Z via flags)
    // ; msr cpsr_f, r1 (restores flags incl Z)
    let mrs = 0xE10F_1000; // MRS R1, CPSR
    let msr_f = 0xE128_F001; // MSR CPSR_f, R1 (field mask = flags only)
    let code = [
        mov_imm(0, 0, 5),
        dp_imm(OP_CMP, 1, 0, 0, 0, 5), // CMP R0, #5 -> Z=1
        mrs,
        dp_imm(OP_ADD, 1, 2, 2, 0, 1), // ADDS R2, R2, #1 -> Z=0
        msr_f,
        HALT,
    ];
    let mut m = Machine::arm(&code);
    m.run(16);
    assert!(m.flag(FLAG_Z), "MSR cpsr_f must restore saved flags");
}

#[test]
fn arm_swi_enters_supervisor_with_spsr() {
    let code = [0xEF00_0005, HALT]; // SWI #5
    let mut m = Machine::arm(&code);
    let before = m.core.cpu.cpsr();
    m.core.step_instruction();
    assert_eq!(m.core.cpu.mode(), 0x13, "SWI must enter SVC mode");
    assert_eq!(m.core.cpu.pc(), 0x08, "SWI vector");
    assert_eq!(m.core.cpu.last_swi, Some(5));
    assert_eq!(m.core.cpu.spsr(), before, "SPSR_svc must hold the old CPSR");
}

// --- THUMB tests ------------------------------------------------------------

fn t_mov_imm(rd: u16, imm: u16) -> u16 { 0x2000 | (rd << 8) | imm }
fn t_add_imm(rd: u16, imm: u16) -> u16 { 0x3000 | (rd << 8) | imm }
fn t_sub_imm(rd: u16, imm: u16) -> u16 { 0x3800 | (rd << 8) | imm }
const T_HALT: u16 = 0xE7FE; // B .

#[test]
fn thumb_loop_sums_one_to_ten_and_flags() {
    // r0=sum=0, r1=i=1; loop: r0+=r1 (format2 reg add: 0x1800|rm<<6|rs<<3|rd)
    // r1+=1; cmp r1,#11 (0x2900|rd<<8|imm -> CMP is 00101)
    // bne loop
    let add_r0_r1: u16 = 0x1800 | (1 << 6) | (0 << 3) | 0;
    let cmp_r1_11: u16 = 0x2800 | (1 << 8) | 11;
    // BNE: cond=1, offset back to index 2 from index 5: pc = addr+4 ->
    // target_idx 2, branch idx 5: offset_bytes = (2-5)*2 - 4 + 2? compute:
    // branch at byte 10, pc seen = 10+4=14 (relative to thumb base),
    // target byte 4: imm = (4 - 14)/2 = -5 = 0xFB
    let bne: u16 = 0xD100 | 0xFB;
    let code = [
        t_mov_imm(0, 0),
        t_mov_imm(1, 1),
        add_r0_r1,
        t_add_imm(1, 1),
        cmp_r1_11,
        bne,
        T_HALT,
    ];
    let mut m = Machine::thumb(&code, 0);
    m.run(300);
    assert_eq!(m.reg(0), 55);
}

#[test]
fn thumb_shift_carry() {
    // movs r0,#0x80 ; lsls r0,r0,#24 -> 0x8000_0000 ; lsls r0,r0,#1 -> 0, C=1, Z=1
    let lsl24: u16 = (24 << 6) | (0 << 3) | 0;
    let lsl1: u16 = (1 << 6) | (0 << 3) | 0;
    let code = [t_mov_imm(0, 0x80), lsl24, lsl1, T_HALT];
    let mut m = Machine::thumb(&code, 0);
    m.run(16);
    assert_eq!(m.reg(0), 0);
    assert!(m.flag(FLAG_C));
    assert!(m.flag(FLAG_Z));
}

#[test]
fn thumb_push_pop_and_sp() {
    // movs r0,#77 ; push {r0} ; movs r0,#0 ; pop {r1} ; halt
    let push_r0: u16 = 0xB401;
    let pop_r1: u16 = 0xBC02;
    let code = [t_mov_imm(0, 77), push_r0, t_mov_imm(0, 0), pop_r1, T_HALT];
    let mut m = Machine::thumb(&code, 0);
    let sp_before = m.core.cpu.reg(13);
    m.run(16);
    assert_eq!(m.reg(1), 77);
    assert_eq!(m.core.cpu.reg(13), sp_before, "SP must be restored after pop");
}

#[test]
fn thumb_bl_and_bx_lr() {
    // main: movs r0,#1 ; bl sub ; halt ; sub: adds r0,#41 ; bx lr
    // BL at byte 2 (after one mov): pc for BL pair starts at idx1.
    // Layout halfwords: [0]=movs, [1]=bl1, [2]=bl2, [3]=halt, [4]=sub adds, [5]=bx lr
    // target byte = 8, BL first half at byte 2: offset = 8 - (2+4) = 2 -> imm22 = 1
    // hi = 0, lo = 1
    let bl1: u16 = 0xF000;
    let bl2: u16 = 0xF800 | 1;
    let bx_lr: u16 = 0x4770;
    let code = [t_mov_imm(0, 1), bl1, bl2, T_HALT, t_add_imm(0, 41), bx_lr];
    let mut m = Machine::thumb(&code, 0);
    // The halt halfword sits at index 3, before the subroutine, so the
    // generic last-index halt address does not apply; stop there.
    let return_addr = BASE + 0x100 + 6;
    for _ in 0..16 {
        if m.core.cpu.pc() == return_addr {
            break;
        }
        m.core.step_instruction();
    }
    assert_eq!(m.core.cpu.pc(), return_addr);
    assert_eq!(m.reg(0), 42);
}

#[test]
fn thumb_sub_imm_negative_flag() {
    let code = [t_mov_imm(0, 3), t_sub_imm(0, 5), T_HALT];
    let mut m = Machine::thumb(&code, 0);
    m.run(8);
    assert_eq!(m.reg(0), 0xFFFF_FFFE);
    assert!(m.flag(FLAG_N));
    assert!(!m.flag(FLAG_C), "3 - 5 borrows, C must clear");
}

#[test]
fn fiq_roundtrip_preserves_user_bank() {
    // Regression: switching SYS -> FIQ -> SYS must restore the user
    // bank's R8..R14 exactly (an earlier swap implementation let the
    // FIQ bank leak into the user view, corrupting SP — this is what
    // jsmolka's psr_transfer tests caught).
    use gba_core::cpu::{MODE_FIQ, MODE_SYS};
    let mut core = gba_core::Core::new();
    core.cpu.set_cpsr_full(MODE_SYS);
    core.cpu.set_reg(8, 0x1111_1111);
    core.cpu.set_reg(13, 0x0300_7F00);
    core.cpu.set_cpsr_full(MODE_FIQ);
    assert_eq!(core.cpu.reg(13), 0, "FIQ bank starts separate");
    core.cpu.set_reg(8, 0x2222_2222);
    core.cpu.set_reg(13, 0x0300_7000);
    core.cpu.set_cpsr_full(MODE_SYS);
    assert_eq!(core.cpu.reg(8), 0x1111_1111, "user R8 restored");
    assert_eq!(core.cpu.reg(13), 0x0300_7F00, "user SP restored");
    core.cpu.set_cpsr_full(MODE_FIQ);
    assert_eq!(core.cpu.reg(8), 0x2222_2222, "FIQ R8 kept");
    assert_eq!(core.cpu.reg(13), 0x0300_7000, "FIQ SP kept");
}
