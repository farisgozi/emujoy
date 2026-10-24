//! Linux desktop frontend.
//!
//! Usage: `emu-frontend <game.gba> [save.sav]`
//!
//! Controls: Arrows = D-pad, Z = B, X = A, A = L, S = R,
//! Enter = Start, Shift = Select, Esc = quit.
//! The .sav file (same path as the ROM with .sav extension unless
//! given) is loaded at boot and rewritten whenever the game saves.
//! Audio plays through cpal on the default output device (resampled
//! from the core's 32768 Hz); with no device it runs silent.

use gba_core::{Core, Host};
use minifb::{Key, Scale, Window, WindowOptions};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SCALE: usize = 3;

/// Core audio output rate (Hz); samples are interleaved stereo i16.
const CORE_RATE: f64 = 32768.0;
/// Queue cap: ~2 seconds of audio. Beyond this the oldest samples are
/// dropped — if the device stalls we resync instead of lagging forever.
const QUEUE_CAP: usize = 32768 * 2 * 2;

#[derive(Clone, Default)]
struct AudioQueue(Arc<Mutex<VecDeque<i16>>>);

impl AudioQueue {
    fn push(&self, samples: &[i16]) {
        let mut q = self.0.lock().unwrap();
        q.extend(samples.iter().copied());
        if q.len() > QUEUE_CAP {
            let excess = q.len() - QUEUE_CAP;
            q.drain(..excess);
        }
    }
}

struct FrontendHost {
    keys: u16,
    audio: AudioQueue,
}

impl Host for FrontendHost {
    fn present_frame(&mut self, _frame: &[u32; gba_core::SCREEN_W * gba_core::SCREEN_H]) {}
    fn push_audio(&mut self, samples: &[i16]) {
        self.audio.push(samples);
    }
    fn poll_input(&mut self) -> u16 {
        self.keys
    }
}

/// One output stream filling `data` from the queue with a
/// nearest-neighbour resample from the core rate to the device rate.
/// Underruns render as silence.
fn make_stream<T: cpal::SizedSample + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: AudioQueue,
    conv: fn(i16) -> T,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    use cpal::traits::DeviceTrait;
    let channels = config.channels as usize;
    let ratio = CORE_RATE / f64::from(config.sample_rate.0);
    let mut src: Vec<i16> = Vec::new();
    let mut phase = 0.0f64;
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels.max(1);
            {
                let mut q = queue.0.lock().unwrap();
                let want = ((frames as f64 * ratio) as usize + 2) * 2;
                let take = want.saturating_sub(src.len()).min(q.len());
                src.extend(q.drain(..take));
            }
            for i in 0..frames {
                let idx = phase as usize * 2;
                let (l, r) = if idx + 1 < src.len() {
                    (src[idx], src[idx + 1])
                } else {
                    (0, 0)
                };
                data[i * channels] = conv(l);
                if channels > 1 {
                    data[i * channels + 1] = conv(r);
                }
                phase += ratio;
            }
            let consumed = (phase as usize) * 2;
            phase -= phase.trunc();
            src.drain(..consumed.min(src.len()));
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )
}

/// Open the default output device. Returns `None` (with a note on
/// stderr) when the machine has no usable audio device — the frontend
/// then simply runs silent.
fn start_audio(queue: AudioQueue) -> Option<cpal::Stream> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host.default_output_device().or_else(|| {
        eprintln!("audio: no default output device; running silent");
        None
    })?;
    let supported = device.default_output_config().ok().or_else(|| {
        eprintln!("audio: output device has no usable config; running silent");
        None
    })?;
    let config: cpal::StreamConfig = supported.clone().into();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            make_stream::<f32>(&device, &config, queue, |s| f32::from(s) / 32768.0)
        }
        cpal::SampleFormat::I16 => make_stream::<i16>(&device, &config, queue, |s| s),
        cpal::SampleFormat::U16 => {
            make_stream::<u16>(&device, &config, queue, |s| (i32::from(s) + 32768) as u16)
        }
        other => {
            eprintln!("audio: unsupported sample format {other:?}; running silent");
            return None;
        }
    };
    match stream {
        Ok(s) => {
            if let Err(e) = s.play() {
                eprintln!("audio: cannot start stream: {e}; running silent");
                return None;
            }
            println!(
                "audio: {} Hz, {} ch, {:?}",
                config.sample_rate.0, config.channels,
                supported.sample_format()
            );
            Some(s)
        }
        Err(e) => {
            eprintln!("audio: cannot build stream: {e}; running silent");
            None
        }
    }
}

fn key_bits(window: &Window) -> u16 {
    // GBA key order: A B Select Start Right Left Up Down R L
    let mut k: u16 = 0;
    let get = |keys: &[Key], bit: u16, k: &mut u16| {
        if window.is_key_down(keys[0]) {
            *k |= bit;
        }
    };
    get(&[Key::X], 1 << 0, &mut k); // A
    get(&[Key::Z], 1 << 1, &mut k); // B
    get(&[Key::RightShift], 1 << 2, &mut k); // Select
    get(&[Key::LeftShift], 1 << 2, &mut k);
    get(&[Key::Enter], 1 << 3, &mut k); // Start
    get(&[Key::Right], 1 << 4, &mut k);
    get(&[Key::Left], 1 << 5, &mut k);
    get(&[Key::Up], 1 << 6, &mut k);
    get(&[Key::Down], 1 << 7, &mut k);
    get(&[Key::S], 1 << 8, &mut k); // R
    get(&[Key::A], 1 << 9, &mut k); // L
    k
}

fn main() {
    let rom_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: emu-frontend <game.gba> [save.sav]");
        std::process::exit(2);
    });
    let sav_path = std::env::args().nth(2).unwrap_or_else(|| {
        let mut p = rom_path.clone();
        if let Some(dot) = p.rfind('.') {
            p.truncate(dot);
        }
        format!("{p}.sav")
    });

    let rom = std::fs::read(&rom_path).unwrap_or_else(|e| {
        eprintln!("cannot read {rom_path}: {e}");
        std::process::exit(2);
    });

    let mut core = Core::new();
    let header = core.load_rom(&rom).unwrap_or_else(|e| {
        eprintln!("invalid GBA ROM: {e:?}");
        std::process::exit(2);
    });
    println!(
        "title={:?} code={:?} save chip={:?} ({} bytes)",
        header.title,
        header.game_code,
        core.bus.save.kind,
        core.save_data().len()
    );
    if let Ok(sav) = std::fs::read(&sav_path) {
        core.load_save_data(&sav);
        println!("loaded save file {sav_path} ({} bytes)", sav.len());
    }
    core.boot_cartridge();

    let mut window = Window::new(
        &format!("emu-core — {}", &header.title),
        gba_core::SCREEN_W,
        gba_core::SCREEN_H,
        WindowOptions { scale: Scale::X4, ..WindowOptions::default() },
    )
    .unwrap_or_else(|e| {
        eprintln!("cannot open window: {e}");
        std::process::exit(1);
    });
    // ~59.7275 Hz pacing comes from the core's frame production; the
    // window just displays what it is given.
    window.set_target_fps(60);

    let queue = AudioQueue::default();
    let _audio_stream = start_audio(queue.clone());
    let mut host = FrontendHost { keys: 0, audio: queue };
    while window.is_open() && !window.is_key_down(Key::Escape) {
        host.keys = key_bits(&window);
        core.run_frame_cpu(&mut host);
        let _ = SCALE;
        window
            .update_with_buffer(&core.framebuffer[..], gba_core::SCREEN_W, gba_core::SCREEN_H)
            .unwrap_or_else(|e| eprintln!("present failed: {e}"));
        if core.save_dirty() {
            match std::fs::write(&sav_path, core.save_data()) {
                Ok(()) => {
                    core.ack_save_written();
                    println!("save written to {sav_path}");
                }
                Err(e) => eprintln!("cannot write {sav_path}: {e}"),
            }
        }
    }
    // Final save flush on exit.
    if core.save_dirty() {
        let _ = std::fs::write(&sav_path, core.save_data());
    }
    println!("bye — {} frames presented", core.frames_presented);
}
