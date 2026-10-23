//! Linux desktop frontend — Phase 3.
//!
//! Usage: `emu-frontend <game.gba> [save.sav]`
//!
//! Controls: Arrows = D-pad, Z = B, X = A, A = L, S = R,
//! Enter = Start, Shift = Select, Esc = quit.
//! The .sav file (same path as the ROM with .sav extension unless
//! given) is loaded at boot and rewritten whenever the game saves.

use gba_core::{Core, Host};
use minifb::{Key, Scale, Window, WindowOptions};

const SCALE: usize = 3;

struct FrontendHost {
    keys: u16,
}

impl Host for FrontendHost {
    fn present_frame(&mut self, _frame: &[u32; gba_core::SCREEN_W * gba_core::SCREEN_H]) {}
    fn poll_input(&mut self) -> u16 {
        self.keys
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

    let mut host = FrontendHost { keys: 0 };
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
