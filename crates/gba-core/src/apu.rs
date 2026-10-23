//! APU — Phase 4: the four legacy PSG channels plus the two Direct
//! Sound FIFOs, mixed to stereo i16 at 32768 Hz (512 CPU cycles per
//! sample frame).
//!
//! Register values live in the bus IO page like all other devices;
//! this struct keeps only the *dynamic* state (phase counters,
//! envelope positions, FIFO contents). Trigger edges are detected
//! by the bus (a register poll cannot see them) and delivered via
//! [`Apu::trigger`]. Wave RAM follows the hardware mapping model:
//! 16 bytes visible at 0x04000090, backed by two internal banks; the
//! bank bit in SOUND3CNT_L selects the mapped/played bank.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub const SAMPLE_CYCLES: u64 = 512; // 16.78 MHz / 512 = 32768 Hz
const SEQ_CYCLES: u64 = 32768; // frame sequencer step = 512 Hz

// IO offsets (from 0x04000000)
const S1_SWEEP: u32 = 0x60;
const S1_CNT_H: u32 = 0x62;
const S1_CNT_X: u32 = 0x64;
const S2_CNT_L: u32 = 0x68;
const S2_CNT_H: u32 = 0x6C;
const S3_CNT_L: u32 = 0x70;
const S3_CNT_H: u32 = 0x72;
const S3_CNT_X: u32 = 0x74;
const S4_CNT_L: u32 = 0x78;
const S4_CNT_H: u32 = 0x7C;
const CNT_L: u32 = 0x80;
const CNT_H: u32 = 0x82;
const CNT_X: u32 = 0x84;
const WAVE_MAP: u32 = 0x90;

const DUTY_PATTERNS: [[i32; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 1, 1, 1],
    [0, 1, 1, 1, 1, 1, 1, 0],
];

#[derive(Default)]
struct Square {
    on: bool,
    freq: u16,
    timer: i32,
    duty_pos: usize,
    volume: u8,
    env_step: u8,
    env_timer: u8,
    length: u16,
    // sweep (channel 1)
    sweep_shift: u8,
    sweep_negate: bool,
    sweep_period: u8,
    sweep_timer: u8,
    sweep_enabled: bool,
    freq_shadow: u16,
}

#[derive(Default)]
struct Wave {
    on: bool,
    freq: u16,
    timer: i32,
    pos: usize, // 0..63 across both banks
    length: u16,
}

#[derive(Default)]
struct Noise {
    on: bool,
    timer: i32,
    lfsr: u16,
    volume: u8,
    env_step: u8,
    env_timer: u8,
    length: u16,
}

pub struct Apu {
    master_on: bool,
    sq1: Square,
    sq2: Square,
    wave: Wave,
    wave_ram: [u8; 32],
    noise: Noise,
    fifo: [VecDeque<u8>; 2],
    fifo_dac: [i8; 2],
    pub fifo_dma_needed: [bool; 2],
    seq_acc: u64,
    seq_step: u8,
    sample_acc: u64,
    /// Accumulated stereo samples (interleaved L,R), drained by core.
    pub out: Vec<i16>,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu {
    pub fn new() -> Self {
        Self {
            master_on: false,
            sq1: Square::default(),
            sq2: Square::default(),
            wave: Wave::default(),
            wave_ram: [0; 32],
            noise: Noise { lfsr: 0x7FFF, ..Default::default() },
            fifo: [VecDeque::new(), VecDeque::new()],
            fifo_dac: [0; 2],
            fifo_dma_needed: [false; 2],
            seq_acc: 0,
            seq_step: 0,
            sample_acc: 0,
            out: Vec::new(),
        }
    }

    fn reg(io: &[u8], off: u32) -> u16 {
        u16::from_le_bytes([io[off as usize], io[off as usize + 1]])
    }

    /// Sync level-sensitive state from the IO page (master enable,
    /// wave RAM mapping). Called once per advance.
    pub fn sync(&mut self, io: &[u8]) {
        let cntx = Self::reg(io, CNT_X);
        let on = cntx & (1 << 7) != 0;
        if on && !self.master_on {
            self.master_on = true;
        } else if !on && self.master_on {
            self.master_on = false;
            self.sq1.on = false;
            self.sq2.on = false;
            self.wave.on = false;
            self.noise.on = false;
        }
        // Wave RAM: the bank selected by SOUND3CNT_L bit 5 is mapped
        // at 0x90..0x9F.
        let cnt3l = Self::reg(io, S3_CNT_L);
        let bank = usize::from((cnt3l >> 5) & 1);
        let dst = bank * 16;
        self.wave_ram[dst..dst + 16].copy_from_slice(&io[WAVE_MAP as usize..WAVE_MAP as usize + 16]);
    }

    /// Channel-running bits for SOUNDCNT_X bits 0-3.
    pub fn status(&self) -> u16 {
        (self.sq1.on as u16)
            | ((self.sq2.on as u16) << 1)
            | ((self.wave.on as u16) << 2)
            | ((self.noise.on as u16) << 3)
    }

    pub fn push_fifo(&mut self, which: usize, bytes: &[u8]) {
        for &b in bytes {
            if self.fifo[which].len() < 32 {
                self.fifo[which].push_back(b);
            }
        }
        if self.fifo[which].len() > 16 {
            self.fifo_dma_needed[which] = false;
        }
    }

    pub fn reset_fifo(&mut self, which: usize) {
        self.fifo[which].clear();
        self.fifo_dac[which] = 0;
        self.fifo_dma_needed[which] = true;
    }

    /// One playback tick from the timer assigned to this FIFO: the
    /// next byte becomes the DAC value; dropping to half-full asks
    /// for a sound DMA refill.
    pub fn fifo_tick(&mut self, which: usize) {
        if let Some(b) = self.fifo[which].pop_front() {
            self.fifo_dac[which] = b as i8;
        }
        if self.fifo[which].len() <= 16 {
            self.fifo_dma_needed[which] = true;
        }
    }

    /// Trigger (re)start of a channel: 0,1 = squares, 2 = wave, 3 =
    /// noise. Parameters are (re)latched from the IO page.
    pub fn trigger(&mut self, ch: usize, io: &[u8]) {
        if !self.master_on {
            return;
        }
        match ch {
            0 => {
                let h = Self::reg(io, S1_CNT_H);
                let x = Self::reg(io, S1_CNT_X);
                let sw = Self::reg(io, S1_SWEEP);
                trigger_square(&mut self.sq1, h, x);
                self.sq1.freq_shadow = self.sq1.freq;
                self.sq1.sweep_shift = (sw & 0x7) as u8;
                self.sq1.sweep_negate = sw & (1 << 3) != 0;
                self.sq1.sweep_period = ((sw >> 4) & 0x7) as u8;
                self.sq1.sweep_timer = self.sq1.sweep_period;
                self.sq1.sweep_enabled = self.sq1.sweep_period > 0 || self.sq1.sweep_shift > 0;
                if self.sq1.sweep_shift > 0 {
                    // Immediate overflow check on trigger.
                    let delta = self.sq1.freq_shadow >> self.sq1.sweep_shift;
                    if self.sq1.freq_shadow + delta > 2047 {
                        self.sq1.on = false;
                    }
                }
            }
            1 => {
                let l = Self::reg(io, S2_CNT_L);
                let h = Self::reg(io, S2_CNT_H);
                trigger_square(&mut self.sq2, l, h);
            }
            2 => {
                let l = Self::reg(io, S3_CNT_L);
                let h = Self::reg(io, S3_CNT_H);
                let x = Self::reg(io, S3_CNT_X);
                if l & (1 << 7) != 0 {
                    self.wave.on = true;
                }
                self.wave.freq = x & 0x7FF;
                self.wave.length = 256 - u16::from(h & 0xFF);
                self.wave.pos = 0;
                self.wave.timer = wave_period(self.wave.freq);
            }
            _ => {
                let l = Self::reg(io, S4_CNT_L);
                let h = Self::reg(io, S4_CNT_H);
                let n = &mut self.noise;
                n.on = true;
                n.volume = ((l >> 12) & 0xF) as u8;
                n.env_step = ((l >> 8) & 0x7) as u8;
                n.env_timer = n.env_step;
                n.length = 64 - (l & 0x3F);
                n.lfsr = 0x7FFF;
                n.timer = noise_period(h);
                let _ = h;
            }
        }
    }

    /// Advance everything by `cycles` CPU cycles, producing samples.
    pub fn advance(&mut self, cycles: u64, io: &[u8]) {
        if !self.master_on {
            return;
        }
        // Frame sequencer (512 Hz): length / sweep / envelope clocks.
        self.seq_acc += cycles;
        while self.seq_acc >= SEQ_CYCLES {
            self.seq_acc -= SEQ_CYCLES;
            self.sequencer_step(io);
        }
        // Frequency timers.
        step_square(&mut self.sq1, cycles);
        step_square(&mut self.sq2, cycles);
        self.step_wave(cycles, io);
        self.step_noise(cycles, io);
        // Sample production at 32768 Hz.
        self.sample_acc += cycles;
        while self.sample_acc >= SAMPLE_CYCLES {
            self.sample_acc -= SAMPLE_CYCLES;
            let (l, r) = self.mix(io);
            self.out.push(l);
            self.out.push(r);
        }
    }

    fn sequencer_step(&mut self, io: &[u8]) {
        let step = self.seq_step;
        self.seq_step = (self.seq_step + 1) % 8;
        if step % 2 == 0 {
            // Length counters (256 Hz).
            clock_length_sq(&mut self.sq1, Self::reg(io, S1_CNT_X));
            clock_length_sq(&mut self.sq2, Self::reg(io, S2_CNT_H));
            if Self::reg(io, S3_CNT_X) & (1 << 14) != 0 && self.wave.on {
                if self.wave.length > 0 {
                    self.wave.length -= 1;
                    if self.wave.length == 0 {
                        self.wave.on = false;
                    }
                }
            }
            if Self::reg(io, S4_CNT_H) & (1 << 14) != 0 && self.noise.on {
                if self.noise.length > 0 {
                    self.noise.length -= 1;
                    if self.noise.length == 0 {
                        self.noise.on = false;
                    }
                }
            }
        }
        if step == 2 || step == 6 {
            // Sweep (128 Hz), channel 1 only.
            let s = &mut self.sq1;
            if s.on && s.sweep_enabled && s.sweep_period > 0 {
                if s.sweep_timer > 0 {
                    s.sweep_timer -= 1;
                }
                if s.sweep_timer == 0 {
                    s.sweep_timer = s.sweep_period;
                    if s.sweep_shift > 0 {
                        let delta = s.freq_shadow >> s.sweep_shift;
                        let new = if s.sweep_negate {
                            s.freq_shadow.wrapping_sub(delta)
                        } else {
                            s.freq_shadow + delta
                        };
                        if new > 2047 {
                            s.on = false;
                        } else {
                            s.freq_shadow = new;
                            s.freq = new;
                            let again = if s.sweep_negate { new.wrapping_sub(new >> s.sweep_shift) } else { new + (new >> s.sweep_shift) };
                            if again > 2047 {
                                s.on = false;
                            }
                        }
                    }
                }
            }
        }
        if step == 7 {
            // Envelopes (64 Hz).
            clock_env_sq(&mut self.sq1, Self::reg(io, S1_CNT_H));
            clock_env_sq(&mut self.sq2, Self::reg(io, S2_CNT_L));
            let l = Self::reg(io, S4_CNT_L);
            let n = &mut self.noise;
            if n.on && n.env_step > 0 {
                if n.env_timer > 0 {
                    n.env_timer -= 1;
                }
                if n.env_timer == 0 {
                    n.env_timer = n.env_step;
                    let inc = l & (1 << 11) != 0;
                    if inc && n.volume < 15 {
                        n.volume += 1;
                    } else if !inc && n.volume > 0 {
                        n.volume -= 1;
                    }
                }
            }
        }
    }

    fn step_wave(&mut self, cycles: u64, io: &[u8]) {
        if !self.wave.on {
            return;
        }
        let x = Self::reg(io, S3_CNT_X);
        self.wave.freq = x & 0x7FF;
        let mut t = i64::from(self.wave.timer) - cycles as i64;
        let period = i64::from(wave_period(self.wave.freq)).max(1);
        while t <= 0 {
            t += period;
            self.wave.pos = (self.wave.pos + 1) % 64;
        }
        self.wave.timer = t as i32;
    }

    fn step_noise(&mut self, cycles: u64, io: &[u8]) {
        if !self.noise.on {
            return;
        }
        let h = Self::reg(io, S4_CNT_H);
        let mut t = i64::from(self.noise.timer) - cycles as i64;
        let period = i64::from(noise_period(h)).max(1);
        let narrow = h & (1 << 3) != 0;
        while t <= 0 {
            t += period;
            let l = &mut self.noise.lfsr;
            let bit = (*l & 1) ^ ((*l >> 1) & 1);
            *l >>= 1;
            *l |= bit << 14;
            if narrow {
                *l = (*l & !(1 << 6)) | (bit << 6);
            }
        }
        self.noise.timer = t as i32;
    }

    fn mix(&self, io: &[u8]) -> (i16, i16) {
        let cntl = Self::reg(io, CNT_L);
        let cnth = Self::reg(io, CNT_H);
        let psg_ratio = [16i32, 32, 64, 16][usize::from(cnth & 0x3)]; // 25%,50%,100%,25% in /64
        let vol_l = i32::from((cntl >> 4) & 0x7) + 1;
        let vol_r = i32::from(cntl & 0x7) + 1;

        // PSG channel values, bipolar approximations in -15..=15.
        let h1 = Self::reg(io, S1_CNT_H);
        let h2 = Self::reg(io, S2_CNT_L);
        let sq1v = square_value(&self.sq1, h1);
        let sq2v = square_value(&self.sq2, h2);
        let wavev = self.wave_value(io);
        let noisev = {
            let n = &self.noise;
            if n.on && n.volume > 0 {
                if n.lfsr & 1 == 0 { i32::from(n.volume) } else { -i32::from(n.volume) }
            } else {
                0
            }
        };
        let chan = [sq1v, sq2v, wavev, noisev];
        let mut psg_l = 0i32;
        let mut psg_r = 0i32;
        for (i, &v) in chan.iter().enumerate() {
            if cntl & (1 << (12 + i)) != 0 {
                psg_l += v;
            }
            if cntl & (1 << (8 + i)) != 0 {
                psg_r += v;
            }
        }
        psg_l = psg_l * psg_ratio * vol_l / 64;
        psg_r = psg_r * psg_ratio * vol_r / 64;

        // Direct Sound: DAC bytes scaled by their volume bits and
        // routed per SOUNDCNT_H.
        let da = i32::from(self.fifo_dac[0]);
        let db = i32::from(self.fifo_dac[1]);
        let scale_a = if cnth & (1 << 2) != 0 { 256 } else { 128 };
        let scale_b = if cnth & (1 << 3) != 0 { 256 } else { 128 };
        let mut ds_l = 0i32;
        let mut ds_r = 0i32;
        if cnth & (1 << 5) != 0 { ds_l += da * scale_a / 4; }
        if cnth & (1 << 4) != 0 { ds_r += da * scale_a / 4; }
        if cnth & (1 << 7) != 0 { ds_l += db * scale_b / 4; }
        if cnth & (1 << 6) != 0 { ds_r += db * scale_b / 4; }

        let l = (psg_l * 24 + ds_l).clamp(-32768, 32767) as i16;
        let r = (psg_r * 24 + ds_r).clamp(-32768, 32767) as i16;
        (l, r)
    }

    fn wave_value(&self, io: &[u8]) -> i32 {
        if !self.wave.on {
            return 0;
        }
        let l = Self::reg(io, S3_CNT_L);
        let h = Self::reg(io, S3_CNT_H);
        if l & (1 << 7) == 0 {
            return 0; // DAC off
        }
        let dimension_two = l & (1 << 6) != 0;
        let play_bank = usize::from((l >> 5) & 1);
        let idx = if dimension_two {
            self.wave.pos // 0..63 across both banks
        } else {
            play_bank * 32 + (self.wave.pos % 32)
        };
        let byte = self.wave_ram[idx / 2];
        let nib = if idx % 2 == 0 { byte >> 4 } else { byte & 0xF } as i32;
        let vol_code = (h >> 13) & 0x3;
        let shifted = match vol_code {
            0 => return 0,        // mute
            1 => nib,              // 100%
            2 => nib >> 1,        // 50%
            _ => nib >> 2,        // 25%
        };
        let v = if h & (1 << 15) != 0 { (nib * 3) >> 2 } else { shifted }; // force 75%
        (v - 8) * 2
    }
}

fn square_value(s: &Square, cnt_h: u16) -> i32 {
    if !s.on || s.volume == 0 {
        return 0;
    }
    let duty = usize::from((cnt_h >> 6) & 0x3);
    let bit = DUTY_PATTERNS[duty][s.duty_pos];
    if bit == 1 { i32::from(s.volume) } else { -i32::from(s.volume) }
}

fn trigger_square(s: &mut Square, cnt_h: u16, cnt_x: u16) {
    s.on = true;
    s.freq = cnt_x & 0x7FF;
    s.volume = ((cnt_h >> 12) & 0xF) as u8;
    s.env_step = ((cnt_h >> 8) & 0x7) as u8;
    s.env_timer = s.env_step;
    s.length = 64 - (cnt_h & 0x3F);
    s.duty_pos = 0;
    s.timer = square_period(s.freq);
    // DAC off (envelope initial volume 0 and no envelope increase)
    // still produces silence but the channel "runs"; keep it simple.
}

fn clock_length_sq(s: &mut Square, cnt_x: u16) {
    if cnt_x & (1 << 14) != 0 && s.on && s.length > 0 {
        s.length -= 1;
        if s.length == 0 {
            s.on = false;
        }
    }
}

fn clock_env_sq(s: &mut Square, cnt_h: u16) {
    if !s.on || s.env_step == 0 {
        return;
    }
    if s.env_timer > 0 {
        s.env_timer -= 1;
    }
    if s.env_timer == 0 {
        s.env_timer = s.env_step;
        let inc = cnt_h & (1 << 11) != 0;
        if inc && s.volume < 15 {
            s.volume += 1;
        } else if !inc && s.volume > 0 {
            s.volume -= 1;
        }
    }
}

fn step_square(s: &mut Square, cycles: u64) {
    if !s.on {
        return;
    }
    let mut t = i64::from(s.timer) - cycles as i64;
    let period = i64::from(square_period(s.freq)).max(1);
    while t <= 0 {
        t += period;
        s.duty_pos = (s.duty_pos + 1) % 8;
    }
    s.timer = t as i32;
}

fn square_period(freq: u16) -> i32 {
    16 * (2048 - i32::from(freq & 0x7FF))
}

fn wave_period(freq: u16) -> i32 {
    8 * (2048 - i32::from(freq & 0x7FF))
}

fn noise_period(cnt_h: u16) -> i32 {
    let div_code = i32::from(cnt_h & 0x7);
    let div = [8, 16, 32, 48, 64, 80, 96, 112][div_code as usize];
    let shift = i32::from((cnt_h >> 4) & 0xF);
    32 * div * (1 << shift)
}
