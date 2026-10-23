//! The host interface: everything the core needs from the outside
//! world, and nothing else.
//!
//! Inspired by libretro's callback model but designed for this project:
//! the core is a pure library. It owns no window, no file descriptor,
//! no thread, no socket and no wall clock. The frontend (Linux /
//! Android / Web shell) implements this trait; tests implement
//! [`NullHost`]. Virtual file access goes through the frontend too, so
//! save data can be sandboxed, synced, or held in memory per platform.

use crate::{SCREEN_H, SCREEN_W};

pub trait Host {
    /// Present one finished frame (240x160, XRGB u32 pixels).
    fn present_frame(&mut self, frame: &[u32; SCREEN_W * SCREEN_H]);

    /// Samples are interleaved stereo i16 at the core's output rate.
    /// Phase 0 produces no audio; the signature is fixed now so the APU
    /// (Phase 3) never has to change the interface.
    fn push_audio(&mut self, _samples: &[i16]) {}

    /// Current gamepad state as a bitmask (bit set = pressed).
    /// Bit order follows the GBA keypad: A, B, Select, Start,
    /// Right, Left, Up, Down, R, L.
    fn poll_input(&mut self) -> u16 {
        0
    }

    /// Read a virtual file (save data, RTC state) by name.
    /// `None` means "does not exist", not an error.
    fn read_file(&mut self, _name: &str) -> Option<alloc::vec::Vec<u8>> {
        None
    }

    /// Write a virtual file. The frontend decides what that means:
    /// a real file on Linux, SAF-backed storage on Android,
    /// IndexedDB on Web.
    fn write_file(&mut self, _name: &str, _data: &[u8]) {}
}

/// A host that discards video/audio and counts frames — for tests and
/// for the headless harness.
#[derive(Debug, Default)]
pub struct NullHost {
    pub frames_seen: u64,
    pub last_frame_hash: u64,
}

impl Host for NullHost {
    fn present_frame(&mut self, frame: &[u32; SCREEN_W * SCREEN_H]) {
        self.frames_seen += 1;
        // Same FNV-1a as Core::framebuffer_hash, computed on what the
        // host actually received.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &px in frame.iter() {
            for byte in px.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        self.last_frame_hash = hash;
    }
}
