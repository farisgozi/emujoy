//! Headless harness — the Phase 0 acceptance runner.
//!
//! Usage:
//!   cargo run -p headless              # synthetic ROM, 60 frames, twice
//!   cargo run -p headless -- game.gba  # a real ROM file (header check
//!                                      # + frames; CPU arrives in Phase 1)
//!
//! It proves the plumbing end to end: header parsing, ROM loading,
//! scheduler-driven frames, host presentation, and — the important
//! part — determinism: two runs from identical state must produce
//! identical frame hashes.

use gba_core::{Core, Host, NullHost};

/// Host that records the audio stream for EMU_WAV dumps.
#[derive(Default)]
struct WavHost {
    frames: u64,
    samples: Vec<i16>,
}

impl Host for WavHost {
    fn present_frame(&mut self, _frame: &[u32; 240 * 160]) {
        self.frames += 1;
    }
    fn push_audio(&mut self, samples: &[i16]) {
        self.samples.extend_from_slice(samples);
    }
}

fn write_wav(path: &str, samples: &[i16]) {
    let n = samples.len() as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&32768u32.to_le_bytes()); // sample rate
    out.extend_from_slice(&(32768u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(n * 2).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out).expect("write wav");
}

fn synthetic_rom() -> Vec<u8> {
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

fn run_once(rom: &[u8], frames: u64, execute: bool) -> (u64, u64) {
    let mut core = Core::new();
    let header = core.load_rom(rom).expect("ROM header should validate");
    println!(
        "loaded: title={:?} game_code={:?} maker={:?} version={} save={:?}",
        header.title, header.game_code, header.maker_code, header.software_version,
        core.bus.save.kind
    );
    if execute {
        // Real ROM: boot like the BIOS handoff and run the CPU.
        if let Ok(sav) = std::fs::read("game.sav") {
            core.load_save_data(&sav);
        }
        core.boot_cartridge();
    }
    let mut host = NullHost::default();
    for _ in 0..frames {
        if execute {
            core.run_frame_cpu(&mut host);
        } else {
            core.run_frame(&mut host);
        }
    }
    (host.frames_seen, core.framebuffer_hash())
}

/// Validate FOMT's EWRAM heap block chain (used by EMU_HEAPCHECK).
/// Chain head = word at IWRAM 0x03000418; block = {size, flag, prev,
/// next} with 16-byte headers tiling the whole heap span.
fn heap_validate(core: &Core, heap_total: u32) -> Result<usize, String> {
    let ctx = core.bus.read32(0x0300_0418);
    if ctx == 0 {
        return Ok(0);
    }
    let mut node = ctx;
    let mut prev = 0u32;
    let mut total = 0u32;
    let mut count = 0usize;
    loop {
        let size = core.bus.read32(node);
        let p = core.bus.read32(node + 8);
        let n = core.bus.read32(node + 12);
        if p != prev {
            return Err(format!("node {node:#x}: prev {p:#x} != expected {prev:#x}"));
        }
        if size > heap_total && heap_total != 0 {
            return Err(format!("node {node:#x}: size {size:#x} exceeds heap {heap_total:#x}"));
        }
        total = total.wrapping_add(0x10).wrapping_add(size);
        count += 1;
        if n == 0 {
            break;
        }
        let expect = node.wrapping_add(0x10).wrapping_add(size);
        if n != expect {
            return Err(format!(
                "node {node:#x}: next {n:#x} != tiled {expect:#x} (size {size:#x})"
            ));
        }
        prev = node;
        node = n;
        if count > 8192 {
            return Err("runaway chain".to_string());
        }
    }
    if heap_total != 0 && total != heap_total {
        return Err(format!("span {total:#x} != heap size {heap_total:#x}"));
    }
    Ok(count)
}

fn heap_dump(core: &Core) {
    let ctx = core.bus.read32(0x0300_0418);
    println!("heap dump (ctx {ctx:#x}):");
    let mut node = ctx;
    let mut count = 0;
    while node != 0 && count < 64 {
        println!(
            "  @{node:#010x} size={:#x} flag={} prev={:#x} next={:#x}",
            core.bus.read32(node),
            core.bus.read32(node + 4),
            core.bus.read32(node + 8),
            core.bus.read32(node + 12)
        );
        node = core.bus.read32(node + 12);
        count += 1;
    }
}

fn main() {
    if std::env::var("EMU_TRACE").is_ok() {
        let path = std::env::args().nth(1).expect("path");
        let rom = std::fs::read(&path).expect("read rom");
        trace(&rom);
        return;
    }
    let path_arg = std::env::args().nth(1);
    let execute = path_arg.is_some();
    let rom = match &path_arg {
        Some(path) => std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(2);
        }),
        None => synthetic_rom(),
    };

    let frames: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    if let Ok(wav_path) = std::env::var("EMU_WAV") {
        let mut core = Core::new();
        core.load_rom(&rom).expect("wav load");
        if execute {
            core.boot_cartridge();
        }
        let mut host = WavHost::default();
        for _ in 0..frames {
            core.run_frame_cpu(&mut host);
        }
        let peak = host.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        let nonzero = host.samples.iter().filter(|&&s| s != 0).count();
        println!("audio: {} samples, peak {peak}, nonzero {nonzero}", host.samples.len());
        write_wav(&wav_path, &host.samples);
        println!("wav written to {wav_path}");
        return;
    }
    if let Ok(script) = std::env::var("EMU_KEYS") {
        // Scripted play session: EMU_KEYS="frame:KEYS:hold;..." with
        // KEYS as names joined by '+' (A B SELECT START L R UP DOWN
        // LEFT RIGHT) or a hex mask. EMU_SNAP="f1,f2,..." dumps the
        // framebuffer at those frames; EMU_SAV_IN/EMU_SAV_OUT move
        // the battery save in/out; EMU_STATE_OUT/EMU_STATE_IN save
        // and restore a save-state mid-script ("@frame" suffix on
        // EMU_STATE_IN loads it at that frame instead of at boot).
        fn key_mask(names: &str) -> u16 {
            if let Some(hex) = names.strip_prefix("0x") {
                return u16::from_str_radix(hex, 16).unwrap_or(0);
            }
            let mut m = 0u16;
            for n in names.split('+') {
                m |= match n.trim() {
                    "A" => 0x001,
                    "B" => 0x002,
                    "SELECT" => 0x004,
                    "START" => 0x008,
                    "RIGHT" => 0x010,
                    "LEFT" => 0x020,
                    "UP" => 0x040,
                    "DOWN" => 0x080,
                    "R" => 0x100,
                    "L" => 0x200,
                    _ => 0,
                };
            }
            m
        }
        let heapcheck = std::env::var("EMU_HEAPCHECK").is_ok();
        // Heap-integrity checker state (EMU_HEAPCHECK): track calls into
        // the game's allocator family and validate the block chain after
        // each outermost call returns.
        let mut heap_calls: Vec<(u32, u32, u32, u32)> = Vec::new(); // (entry, lr, arg0, arg1)
        let mut heap_call_no = 0u64;
        let mut heap_total = 0u32;
        let mut heap_broken = false;
        let mut events: Vec<(u64, u16, u64)> = Vec::new(); // (frame, mask, hold)
        for part in script.split(';') {
            let f: Vec<&str> = part.split(':').collect();
            if f.len() == 3 {
                events.push((
                    f[0].trim().parse().unwrap_or(0),
                    key_mask(f[1]),
                    f[2].trim().parse().unwrap_or(5),
                ));
            }
        }
        let snaps: Vec<u64> = std::env::var("EMU_SNAP")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        let mut core = Core::new();
        core.load_rom(&rom).expect("play load");
        if let Ok(w) = std::env::var("EMU_WATCH") {
            let addr = u32::from_str_radix(w.trim().trim_start_matches("0x"), 16).unwrap_or(0);
            core.watch_addr = Some(addr);
            core.watch_val = core.bus.read32(addr);
            println!("play: watching {addr:#010x}");
        }
        if let Ok(sav_path) = std::env::var("EMU_SAV_IN") {
            if let Ok(sav) = std::fs::read(&sav_path) {
                core.load_save_data(&sav);
                println!("play: loaded save {sav_path} ({} bytes)", sav.len());
            }
        }
        if execute {
            core.boot_cartridge();
        }
        if let Ok(pokes) = std::env::var("EMU_POKE") {
            // Diagnostic: force-write words after boot ("addr:val,...").
            for part in pokes.split(',') {
                if let Some((a, v)) = part.split_once(':') {
                    let a = u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                    let v = u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                    core.bus.write32(a, v);
                    println!("play: poked {a:#010x} = {v:#010x}");
                }
            }
        }
        let state_in = std::env::var("EMU_STATE_IN").ok();
        let (state_path, state_frame) = match &state_in {
            Some(s) => match s.split_once('@') {
                Some((p, f)) => (Some(p.to_string()), f.parse().unwrap_or(0)),
                None => (Some(s.clone()), 0),
            },
            None => (None, 0),
        };
        if let Some(p) = &state_path {
            if state_frame == 0 {
                let bytes = std::fs::read(p).expect("read state");
                gba_core::savestate::load(&mut core, &bytes).expect("load state");
                println!("play: state loaded from {p}");
            }
        }
        let trace_window: Option<(u64, u64)> = if heapcheck {
            Some((0, frames))
        } else {
            std::env::var("EMU_TRACE_WINDOW").ok().map(|s| {
                let (a, b) = s.split_once('-').unwrap();
                (a.parse().unwrap_or(0), b.parse().unwrap_or(0))
            })
        };
        let mut host = NullHost::default();
        let dump_ppm = |core: &Core, tag: &str| {
            let mut out = String::from("P6\n240 160\n255\n").into_bytes();
            for &px in core.framebuffer.iter() {
                out.push((px >> 16) as u8);
                out.push((px >> 8) as u8);
                out.push(px as u8);
            }
            let dir = std::env::var("EMU_OUT_DIR").unwrap_or_else(|_| "/tmp".to_string());
            let path = format!("{dir}/snap_{tag}.ppm");
            std::fs::write(&path, out).expect("snap");
            println!("play: snap {path}");
        };
        for frame in 0..frames {
            let mut mask = 0u16;
            for &(f, m, hold) in &events {
                if frame >= f && frame < f + hold {
                    mask |= m;
                }
            }
            core.set_keys(mask);
            if std::env::var("EMU_POKE_KEEP").is_ok() {
                if let Ok(pokes) = std::env::var("EMU_POKE") {
                    for part in pokes.split(',') {
                        if let Some((a, v)) = part.split_once(':') {
                            let a = u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                            let v = u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                            core.bus.write32(a, v);
                        }
                    }
                }
            }
            if let Some((w0, w1)) = trace_window {
                if frame >= w0 && frame < w1 {
                    core.ppu.frame_ready = false;
                    let mut last_block = u32::MAX;
                    let mut last_dispcnt = core.bus.read16(0x0400_0000);
                    let mut lines = if heapcheck { 9999 } else { 0 };
                    let mut resolve_prints = 0u32;
                    let mut steps = 0u64;
                    let mut park_run = 0u64;
                    let mut park_pc = 0u32;
                    while !core.ppu.frame_ready && steps < 2_000_000 {
                        let pc = core.cpu.pc();
                        if pc == park_pc {
                            park_run += 1;
                        } else {
                            if park_run > 50 {
                                println!("trace f{}: PARKED x{} at {:#010x}", frame, park_run, park_pc);
                                lines += 1;
                            }
                            park_pc = pc;
                            park_run = 0;
                        }
                        let block = pc >> 8;
                        if block != last_block && lines < 4000 {
                            println!(
                                "trace f{} s{}: pc={:#010x} r0={:#x} r1={:#x} r2={:#x} r3={:#x} lr={:#010x} sp={:#010x} if={:#06x}",
                                frame, steps, pc,
                                core.cpu.reg(0), core.cpu.reg(1), core.cpu.reg(2), core.cpu.reg(3),
                                core.cpu.reg(14), core.cpu.reg(13),
                                core.bus.read16(0x0400_0202)
                            );
                            lines += 1;
                            last_block = block;
                        }
                        if pc == 0x080D_01F8 && resolve_prints < 3 {
                            resolve_prints += 1;
                            println!(
                                "trace f{} s{}: RESOLVE arg={:#x} ctx={:#x} aux={:#x} head0={:#x}",
                                frame, steps, core.cpu.reg(0),
                                core.bus.read32(0x0300_0418), core.bus.read32(0x0300_041C),
                                core.bus.read32(0x0200_1F10)
                            );
                            lines += 1;
                        }
                        if (0x080D_3740..0x080D_3760).contains(&pc) {
                            println!(
                                "trace f{} s{}: SWI-STUB pc={:#x} r0={:#x} r1={:#x} r2={:#x} lr={:#x}",
                                frame, steps, pc,
                                core.cpu.reg(0), core.cpu.reg(1), core.cpu.reg(2), core.cpu.reg(14)
                            );
                        }
                        if pc == 0x080D_3BC0 && resolve_prints <= 3 {
                            println!("trace f{} s{}: DISPATCH arg={:#x} hook={:#x}", frame, steps, core.cpu.reg(0), core.bus.read32(0x0300_0CD4));
                            lines += 1;
                        }
                        if (0x080D_01F8..0x080D_0260).contains(&pc) && resolve_prints > 0 && resolve_prints <= 3 && lines < 6000 {
                            println!(
                                "trace f{} s{}:   exec {:#010x} r0={:#x} r1={:#x} r2={:#x} r4={:#x} r5={:#x}",
                                frame, steps, pc,
                                core.cpu.reg(0), core.cpu.reg(1), core.cpu.reg(2),
                                core.cpu.reg(4), core.cpu.reg(5)
                            );
                            lines += 1;
                        }
                        if heapcheck && !heap_broken {
                            // Complete a tracked allocator call when the
                            // CPU reaches its return address.
                            if let Some(&(entry, lr, a0, a1)) = heap_calls.last() {
                                if pc == lr {
                                    heap_calls.pop();
                                    if heap_calls.is_empty() {
                                        heap_call_no += 1;
                                        let result = core.cpu.reg(0);
                                        match heap_validate(&core, heap_total) {
                                            Ok(n) => {
                                                if heap_call_no % 200 == 0 {
                                                    println!("heap: ok through call #{heap_call_no} ({n} blocks)");
                                                }
                                            }
                                            Err(msg) => {
                                                heap_broken = true;
                                                println!(
                                                    "heap: BROKEN after call #{heap_call_no} fn={entry:#x} arg0={a0:#x} arg1={a1:#x} caller_lr={lr:#x} result={result:#x} frame={frame} step={steps}: {msg}"
                                                );
                                                heap_dump(&core);
                                            }
                                        }
                                    }
                                }
                            }
                            let fam = matches!(pc, 0x080D_01E0 | 0x080D_01F8 | 0x080D_0260 | 0x080D_02B8);
                            let cur_lr = core.cpu.reg(14) & !1;
                            if fam && pc != 0x080D_01E0 {
                                // Validate the chain as the allocator sees
                                // it on entry — catches corruption caused
                                // by any code between allocator calls.
                                if let Err(msg) = heap_validate(&core, heap_total) {
                                    heap_broken = true;
                                    println!(
                                        "heap: BROKEN at entry fn={pc:#x} arg0={:#x} arg1={:#x} caller_lr={cur_lr:#x} frame={frame} step={steps}: {msg}",
                                        core.cpu.reg(0), core.cpu.reg(1)
                                    );
                                    heap_dump(&core);
                                }
                            }
                            if fam && heap_calls.last().map(|&(_, lr, _, _)| lr) != Some(cur_lr) {
                                if pc == 0x080D_01E0 {
                                    heap_total = core.cpu.reg(1);
                                    println!("heap: init base={:#x} size={:#x} (frame {frame})", core.cpu.reg(0), heap_total);
                                }
                                heap_calls.push((pc, cur_lr, core.cpu.reg(0), core.cpu.reg(1)));
                            }
                        }
                        core.step_instruction();
                        steps += 1;
                        let d = core.bus.read16(0x0400_0000);
                        if d != last_dispcnt {
                            println!("trace f{} s{}: DISPCNT {:#06x} -> {:#06x} (pc now {:#010x})", frame, steps, last_dispcnt, d, core.cpu.pc());
                            last_dispcnt = d;
                            lines += 1;
                        }
                    }
                    if park_run > 50 {
                        println!("trace f{}: PARKED x{} at {:#010x} (frame end)", frame, park_run, park_pc);
                    }
                } else {
                    core.run_frame_cpu(&mut host);
                }
            } else {
                core.run_frame_cpu(&mut host);
            }
            if std::env::var("EMU_PLAY_DEBUG").is_ok() && (frame + 1) % 60 == 0 {
                println!(
                    "play@{:5}: pc={:#010x} thumb={} clk={} dispcnt={:#06x} ie={:#06x} if={:#06x} ime={} keyinput={:#06x}",
                    frame + 1,
                    core.cpu.pc(),
                    core.cpu.thumb_state(),
                    core.scheduler.now(),
                    core.bus.read16(0x0400_0000),
                    core.bus.read16(0x0400_0200),
                    core.bus.read16(0x0400_0202),
                    core.bus.read16(0x0400_0208) & 1,
                    core.bus.read16(0x0400_0130),
                );
            }
            if snaps.contains(&(frame + 1)) {
                dump_ppm(&core, &format!("{}", frame + 1));
            }
            if let Some(p) = &state_path {
                if state_frame == frame + 1 && state_frame != 0 {
                    let bytes = std::fs::read(p).expect("read state");
                    gba_core::savestate::load(&mut core, &bytes).expect("load state mid");
                    println!("play: state loaded from {p} at frame {}", frame + 1);
                }
            }
            if let Ok(sp) = std::env::var("EMU_STATE_OUT") {
                if frame + 1 == frames {
                    let bytes = gba_core::savestate::save(&core);
                    std::fs::write(&sp, &bytes).expect("write state");
                    println!("play: state saved to {sp} ({} bytes)", bytes.len());
                }
            }
        }
        println!(
            "play: done {frames} frames, save_dirty={} save_len={}",
            core.save_dirty(),
            core.save_data().len()
        );
        for (t, pc, v) in &core.watch_log {
            println!("watch: t={} (~frame {}) writer_pc={:#010x} new_val={:#010x}", t, t / 280_896, pc, v);
        }
        if std::env::var("EMU_PLAY_DUMP_MEM").is_ok() {
            let dir = std::env::var("EMU_OUT_DIR").unwrap_or_else(|_| "/tmp".to_string());
            std::fs::write(format!("{dir}/play_iwram.bin"), &core.bus.iwram[..]).expect("iwram");
            std::fs::write(format!("{dir}/play_ewram.bin"), &core.bus.ewram[..]).expect("ewram");
            std::fs::write(format!("{dir}/play_vram.bin"), &core.bus.vram[..]).expect("vram");
            println!("play: mem dumped (pc={:#x}, sp={:#x}, lr={:#x})", core.cpu.pc(), core.cpu.reg(13), core.cpu.reg(14));
        }
        if let Ok(out) = std::env::var("EMU_SAV_OUT") {
            let data = core.save_data();
            if !data.is_empty() {
                std::fs::write(&out, data).expect("write sav");
                println!("play: save written to {out}");
            }
        }
        return;
    }
    let (frames_a, hash_a) = run_once(&rom, frames, execute);
    let (frames_b, hash_b) = run_once(&rom, frames, execute);

    println!("run A: {frames_a} frames, framebuffer hash {hash_a:#018x}");
    println!("run B: {frames_b} frames, framebuffer hash {hash_b:#018x}");
    if hash_a == hash_b {
        println!("determinism: OK (identical hashes)");
    } else {
        eprintln!("determinism: FAILED");
        std::process::exit(1);
    }

    // Optional: dump the final frame of a fresh run as a binary PPM
    // (EMU_DUMP_FRAME=out.ppm) for visual inspection of test ROMs.
    if std::env::var("EMU_PROBE").is_ok() {
        let mut core = Core::new();
        core.load_rom(&rom).expect("probe load");
        if execute {
            core.boot_cartridge();
        }
        let mut host = NullHost::default();
        for _ in 0..frames {
            core.run_frame_cpu(&mut host);
        }
        println!("probe: dispcnt={:#06x} bg2pa={:#x} bg2pb={:#x} bg2pc={:#x} bg2pd={:#x} bg2x={:#x} bg2y={:#x} bg2cnt={:#06x} pal0={:#06x} pal1={:#06x}",
            core.bus.read16(0x0400_0000), core.bus.read16(0x0400_0020), core.bus.read16(0x0400_0022),
            core.bus.read16(0x0400_0024), core.bus.read16(0x0400_0026),
            core.bus.read32(0x0400_0028), core.bus.read32(0x0400_002c),
            core.bus.read16(0x0400_000c),
            u16::from_le_bytes([core.bus.palette[0], core.bus.palette[1]]),
            u16::from_le_bytes([core.bus.palette[2], core.bus.palette[3]]));
        // Full display register dump.
        let names = ["bg0cnt", "bg1cnt", "bg2cnt", "bg3cnt"];
        for i in 0..4u32 {
            println!("probe: {}={:#06x} hofs={} vofs={}", names[i as usize],
                core.bus.read16(0x0400_0008 + i * 2),
                core.bus.read16(0x0400_0010 + i * 4) & 0x1FF,
                core.bus.read16(0x0400_0012 + i * 4) & 0x1FF);
        }
        println!("probe: winin={:#06x} winout={:#06x} bldcnt={:#06x} bldalpha={:#06x} bldy={:#06x} mosaic={:#06x}",
            core.bus.read16(0x0400_0048), core.bus.read16(0x0400_004a),
            core.bus.read16(0x0400_0050), core.bus.read16(0x0400_0052),
            core.bus.read16(0x0400_0054), core.bus.read16(0x0400_004c));
        println!("probe: bg3pa={:#x} bg3pd={:#x} bg3x={:#x} bg3y={:#x}",
            core.bus.read16(0x0400_0030), core.bus.read16(0x0400_0036),
            core.bus.read32(0x0400_0038), core.bus.read32(0x0400_003c));
        // OAM dump: every non-disabled sprite with decoded geometry.
        for i in 0..128usize {
            let b = i * 8;
            let rd = |o: usize| u16::from_le_bytes([core.bus.oam[b + o], core.bus.oam[b + o + 1]]);
            let (a0, a1, a2) = (rd(0), rd(2), rd(4));
            if a0 & 0x0300 == 0x0200 {
                continue;
            }
            let affine = a0 & (1 << 8) != 0;
            let grp = (a1 >> 9) & 0x1F;
            let (pa, pb, pc, pd) = if affine {
                let g = (grp as usize) * 32;
                let gr = |o: usize| u16::from_le_bytes([core.bus.oam[g + o], core.bus.oam[g + o + 1]]);
                (gr(6), gr(14), gr(22), gr(30))
            } else {
                (0, 0, 0, 0)
            };
            println!("oam {i:3}: a0={a0:#06x} a1={a1:#06x} a2={a2:#06x} y={} x={} shape={} size={} mode={} bpp8={} affine={} grp={} pa={:#x} pb={:#x} pc={:#x} pd={:#x} tile={} prio={} pal={}",
                a0 & 0xFF, a1 & 0x1FF, (a0 >> 14) & 3, (a1 >> 14) & 3,
                (a0 >> 10) & 3, a0 & (1 << 13) != 0, affine, grp, pa, pb, pc, pd,
                a2 & 0x3FF, (a2 >> 10) & 3, (a2 >> 12) & 0xF);
        }
    }
    if std::env::var("EMU_PROBE_LINES").is_ok() {
        // Per-line scroll snapshot over ONE frame (the last one):
        // catches HBlank-raster effects by recording HOFS/VOFS of
        // every BG at each visible line boundary.
        let mut core = Core::new();
        core.load_rom(&rom).expect("probe-lines load");
        if execute {
            core.boot_cartridge();
        }
        let mut host = NullHost::default();
        for _ in 0..frames.saturating_sub(1) {
            core.run_frame_cpu(&mut host);
        }
        let mut last_line = core.ppu.line;
        let mut guard: u64 = 0;
        let mut seen_zero = false;
        loop {
            core.step_instruction();
            guard += 1;
            if core.ppu.line != last_line {
                last_line = core.ppu.line;
                let l = last_line;
                if l == 0 {
                    seen_zero = true;
                }
                if l == 160 && seen_zero {
                    break;
                }
                if l < 160 {
                    println!("line {:3}: dispcnt={:#06x} hofs=[{},{},{},{}] vofs=[{},{},{},{}] bg2x={:#x} bg2y={:#x} mosaic={:#06x}",
                        l, core.bus.read16(0x0400_0000),
                        core.bus.read16(0x0400_0010) & 0x1FF, core.bus.read16(0x0400_0014) & 0x1FF,
                        core.bus.read16(0x0400_0018) & 0x1FF, core.bus.read16(0x0400_001c) & 0x1FF,
                        core.bus.read16(0x0400_0012) & 0x1FF, core.bus.read16(0x0400_0016) & 0x1FF,
                        core.bus.read16(0x0400_001a) & 0x1FF, core.bus.read16(0x0400_001e) & 0x1FF,
                        core.bus.read32(0x0400_0028), core.bus.read32(0x0400_002c),
                        core.bus.read16(0x0400_004c));
                }
            }
            if guard > 6_000_000 {
                break;
            }
        }
    }
    if std::env::var("EMU_DUMP_MEM").is_ok() {
        let mut core = Core::new();
        core.load_rom(&rom).expect("dump-mem load");
        if execute {
            core.boot_cartridge();
        }
        let mut host = NullHost::default();
        for _ in 0..frames {
            core.run_frame_cpu(&mut host);
        }
        std::fs::write("/tmp/dump_vram.bin", &core.bus.vram[..]).expect("vram");
        std::fs::write("/tmp/dump_oam.bin", &core.bus.oam[..]).expect("oam");
        std::fs::write("/tmp/dump_pal.bin", &core.bus.palette[..]).expect("pal");
        std::fs::write("/tmp/dump_iwram.bin", &core.bus.iwram[..]).expect("iwram");
        std::fs::write("/tmp/dump_ewram.bin", &core.bus.ewram[..]).expect("ewram");
        println!("mem dumped: vram {} oam {} pal {}", core.bus.vram.len(), core.bus.oam.len(), core.bus.palette.len());
        return;
    }
    if let Ok(which) = std::env::var("EMU_LAYER") {
        // Diagnostic: render the final frame with only one layer
        // enabled (0-3 = BG0-3, 4 = OBJ), forcing DISPCNT after every
        // step so the game's own writes can't re-enable the rest.
        let which: u16 = which.parse().unwrap_or(0);
        let mut core = Core::new();
        core.load_rom(&rom).expect("layer load");
        if execute {
            core.boot_cartridge();
        }
        let mut host = NullHost::default();
        for _ in 0..frames.saturating_sub(1) {
            core.run_frame_cpu(&mut host);
        }
        core.ppu.frame_ready = false;
        let mut guard: u64 = 0;
        while !core.ppu.frame_ready {
            core.step_instruction();
            let cur = core.bus.read16(0x0400_0000);
            let keep = if which <= 3 { 1u16 << (8 + which) } else { 1u16 << 12 };
            let forced = (cur & 0x0087) | keep | (cur & 0x0040);
            core.bus.write16(0x0400_0000, forced);
            guard += 1;
            if guard > 8_000_000 {
                break;
            }
        }
        // One more pass stepping until the frame is fully rendered:
        // run_frame_cpu normally stops AT vblank start (line 160),
        // which is exactly a complete frame in the framebuffer.
        let path = format!("/tmp/layer{which}.ppm");
        let mut outb = String::from("P6\n240 160\n255\n").into_bytes();
        for &px in core.framebuffer.iter() {
            outb.push((px >> 16) as u8);
            outb.push((px >> 8) as u8);
            outb.push(px as u8);
        }
        std::fs::write(&path, outb).expect("write layer ppm");
        println!("layer {which} dumped to {path}");
        return;
    }
    if let Ok(out) = std::env::var("EMU_DUMP_FRAME") {
        let mut core = Core::new();
        core.load_rom(&rom).expect("reload for dump");
        if execute {
            core.boot_cartridge();
        }
        let mut host = NullHost::default();
        for _ in 0..frames {
            if execute {
                core.run_frame_cpu(&mut host);
            } else {
                core.run_frame(&mut host);
            }
        }
        let mut ppm = Vec::with_capacity(15 + 240 * 160 * 3);
        ppm.extend_from_slice(b"P6\n240 160\n255\n");
        for px in core.framebuffer.iter() {
            ppm.push((px >> 16) as u8);
            ppm.push((px >> 8) as u8);
            ppm.push(*px as u8);
        }
        std::fs::write(&out, ppm).expect("write ppm");
        println!("frame dumped to {out}");
    }

    // Save-state smoke check through the public API.
    let mut core = Core::new();
    core.load_rom(&rom).expect("reload");
    let blob = gba_core::savestate::save(&core);
    let mut restored = Core::new();
    restored.load_rom(&rom).expect("reload 2");
    gba_core::savestate::load(&mut restored, &blob).expect("save state should load");
    println!("save state: OK ({} bytes, format v{})", blob.len(), gba_core::savestate::FORMAT_VERSION);
}

// Debug entry: EMU_TRACE=1 runs the ROM instruction by instruction and
// reports where the PC settles. Not part of the normal harness flow.
#[allow(dead_code)]
fn trace(rom: &[u8]) {
    let mut core = Core::new();
    core.load_rom(rom).expect("load");
    core.boot_cartridge();
    let mut last_pc = 0u32;
    let mut same = 0u64;
    let mut verbose = std::env::var("EMU_TRACE_FIRST").is_ok();
    let mut history: std::collections::VecDeque<(u64, u32, u32, u32)> = std::collections::VecDeque::new();
    for i in 0..40_000_000u64 {
        if verbose && i < 160 {
            println!("step {i}: pc={:#010x} r12={:#x} cpsr={:#010x}", core.cpu.pc(), core.cpu.reg(12), core.cpu.cpsr());
        }
        let prev_pc = core.cpu.pc();
        let instr_word = core.bus.read32(prev_pc & !3);
        history.push_back((i, prev_pc, instr_word, core.cpu.reg(13)));
        if history.len() > 48 { history.pop_front(); }
        let prev_mode = core.cpu.mode();
        core.step_instruction();
        if std::env::var("EMU_TRACE_IRQ").is_ok() && prev_mode != 0x12 && core.cpu.mode() == 0x12 {
            println!("IRQ ENTRY at step {i}: interrupted pc={prev_pc:#010x} thumb={} lr_irq={:#010x} spsr={:#010x}",
                prev_pc & 1 == 1 || core.cpu.cpsr() & 0x20 != 0, core.cpu.reg(14), core.cpu.spsr());
            if i > 460_000 {
                for (si, sp_pc, iw, sp) in &history {
                    println!("  hist step {si}: pc={sp_pc:#010x} instr={iw:#010x} sp={sp:#010x}");
                }
            }
        }
        if std::env::var("EMU_TRACE_UND").is_ok() && prev_mode != 0x1B && core.cpu.mode() == 0x1B {
            println!("UNDEFINED EXCEPTION at step {i}: faulting pc={prev_pc:#010x} instr={instr_word:#010x} thumb={}", prev_pc & 1 == 1 || core.cpu.cpsr() & 0x20 != 0);
            for (si, sp_pc, iw, sp) in &history {
                println!("  hist step {si}: pc={sp_pc:#010x} instr={iw:#010x} sp={sp:#010x}");
            }
            break;
        }
        let pc = core.cpu.pc();
        if prev_pc == 0x0800_1d70 {
            println!("EVAL REACHED at step {i}: r12={} mode={:#x} sp={:#010x}", core.cpu.reg(12), core.cpu.mode(), core.cpu.reg(13));
        }
        if std::env::var("EMU_TRACE_MODE").is_ok() && core.cpu.mode() == 0x11 && prev_pc >= 0x0800_0000 {
            println!("IN FIQ at step {i}: pc={prev_pc:#010x} instr={instr_word:#010x} sp={:#010x} lr={:#010x}", core.cpu.reg(13), core.cpu.reg(14));
        }
        let sp_now = core.cpu.reg(13);
        if verbose && !(0x0300_0000..0x0300_8000).contains(&sp_now) {
            println!("SP CORRUPT at step {i}: after pc={prev_pc:#010x} instr={instr_word:#010x} sp={sp_now:#010x} r0={:#x} r1={:#x} r2={:#x} r3={:#x} r4={:#x}", core.cpu.reg(0), core.cpu.reg(1), core.cpu.reg(2), core.cpu.reg(3), core.cpu.reg(4));
            verbose = false;
        }
        if verbose && pc < 0x0200_0000 && prev_pc >= 0x0200_0000 {
            println!("LEFT VALID MEMORY at step {i}: {prev_pc:#010x} -> {pc:#010x} r14={:#010x} sp={:#010x} cpsr={:#010x}", core.cpu.reg(14), core.cpu.reg(13), core.cpu.cpsr());
            for (si, sp_pc, iw, sp) in &history {
                println!("  hist step {si}: pc={sp_pc:#010x} instr={iw:#010x} sp={sp:#010x}");
            }
        }
        if pc == last_pc {
            same += 1;
        } else {
            last_pc = pc;
            same = 0;
        }
        if i % 500_000 == 0 || same > 200_000 {
            println!(
                "step {i}: pc={pc:#010x} mode={:#x} cpsr={:#010x} dispstat={:#x} dispcnt={:#x} ie={:#x} if={:#x} ime={:#x} handler_ptr={:#010x}",
                core.cpu.mode(),
                core.cpu.cpsr(),
                core.bus.read16(0x0400_0004),
                core.bus.read16(0x0400_0000),
                core.bus.read16(0x0400_0200),
                core.bus.read16(0x0400_0202),
                core.bus.read16(0x0400_0208),
                core.bus.read32(0x0300_7FFC),
            );
            if same > 200_000 {
                println!("PC settled at {pc:#010x} — dumping surrounding state and stopping");
                break;
            }
        }
    }
}
