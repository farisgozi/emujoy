//! Phase 4 APU tests: PSG square channel output and the Direct
//! Sound FIFO + sound-DMA refill path, observed through the Host
//! audio callback exactly as a frontend receives it.

use gba_core::{Core, Host};

#[derive(Default)]
struct AudioHost {
    samples: Vec<i16>,
}

impl Host for AudioHost {
    fn present_frame(&mut self, _frame: &[u32; 240 * 160]) {}
    fn push_audio(&mut self, samples: &[i16]) {
        self.samples.extend_from_slice(samples);
    }
}

fn halt_core() -> (Core, AudioHost) {
    let mut core = Core::new();
    core.bus.write32(0x0200_0000, 0xEAFF_FFFE);
    core.cpu.set_cpsr_full(0x1F);
    core.cpu.set_reg(13, 0x0300_7F00);
    core.cpu.set_pc(0x0200_0000);
    (core, AudioHost::default())
}

fn run(core: &mut Core, host: &mut AudioHost, frames: usize) {
    for _ in 0..frames {
        core.run_frame_cpu(host);
    }
}

#[test]
fn square_channel_1_produces_a_tone() {
    let (mut core, mut host) = halt_core();
    // Master on, PSG 100%, channel 1 routed L+R at full volume.
    core.bus.write16(0x0400_0084, 0x0080); // SOUNDCNT_X
    core.bus.write16(0x0400_0082, 0x0002); // SOUNDCNT_H: PSG ratio 100%
    core.bus.write16(0x0400_0080, 0x1177); // SOUNDCNT_L
    // Ch1: duty 50%, envelope initial 15 (no sweep of volume).
    core.bus.write16(0x0400_0062, 0xF080); // SOUND1CNT_H
    // ~440 Hz + trigger.
    core.bus.write16(0x0400_0064, 0x86D6); // SOUND1CNT_X
    run(&mut core, &mut host, 10);
    let s = &host.samples;
    assert!(s.len() > 8000, "expected ~10.9k samples for 10 frames, got {}", s.len());
    let nonzero = s.iter().filter(|&&v| v != 0).count();
    assert!(nonzero > s.len() / 2, "tone should fill most samples ({nonzero}/{})", s.len());
    assert!(s.iter().any(|&v| v > 1000), "positive half-cycles present");
    assert!(s.iter().any(|&v| v < -1000), "negative half-cycles present");
    // SOUNDCNT_X must report channel 1 running.
    assert_eq!(core.bus.read16(0x0400_0084) & 0x1, 1);
}

#[test]
fn direct_sound_fifo_plays_dma_fed_bytes() {
    let (mut core, mut host) = halt_core();
    // Pattern in EWRAM: a loud ramp the FIFO DMA will stream.
    for i in 0..64u32 {
        core.bus.write8(0x0200_0100 + i, (i * 4) as u8);
    }
    core.bus.write16(0x0400_0084, 0x0080); // master on
    // SOUNDCNT_H: DS A 100%, A->L and A->R, timer 0; pulse FIFO reset.
    core.bus.write16(0x0400_0082, 0x0834 | 0x0800);
    core.bus.write16(0x0400_0082, 0x0034);
    core.bus.write16(0x0400_0080, 0x0077);
    // Timer 0: overflow every 512 cycles (32768 Hz playback clock).
    core.bus.write16(0x0400_0100, 0xFE00);
    core.bus.write16(0x0400_0102, 0x0080);
    // DMA1 -> FIFO_A, special timing, 32-bit units.
    core.bus.write32(0x0400_00BC, 0x0200_0100); // SAD
    core.bus.write32(0x0400_00C0, 0x0400_00A0); // DAD
    core.bus.write16(0x0400_00C4, 0); // CNT_L (count ignored for sound)
    core.bus.write16(0x0400_00C6, 0xB400); // CNT_H: enable | special | word
    run(&mut core, &mut host, 30);
    let s = &host.samples;
    assert!(s.len() > 20000, "30 frames of audio, got {}", s.len());
    let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    assert!(peak > 2000, "FIFO bytes should be audible, peak {peak}");
    let nonzero = s.iter().filter(|&&v| v != 0).count();
    assert!(nonzero > 500, "sustained FIFO playback, nonzero {nonzero}");
}
