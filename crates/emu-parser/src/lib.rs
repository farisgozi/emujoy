//! Hardened parsers for files an emulator loads from untrusted sources.
//!
//! Design rules (Phase 0, see docs/decisions/0003-save-state-format-v1.md
//! and the security audit that motivated this project):
//! - Every length comes from the file, therefore every length is hostile
//!   until it has been checked against the actual input size.
//! - No allocation is sized by an unchecked value from the file.
//! - Parsers never panic on bad input: they return `Err`.
//! - This crate forbids `unsafe` outright.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Hard upper bound for one path inside a CUE sheet.
///
/// CVE-2025-9809 (libretro-common, CVSS 8.4) was an out-of-bounds write
/// caused by copying a CUE track path into a fixed buffer with no length
/// check. In Rust the copy itself cannot overflow, but we still reject
/// absurd paths at parse time: a parser that accepts anything defers the
/// failure to a worse place.
pub const MAX_CUE_PATH_LEN: usize = 1024;

/// Smallest input that can contain a complete GBA header (0x00..=0xBD).
pub const GBA_HEADER_LEN: usize = 0xC0;
/// GBA ROMs are at most 32 MiB.
pub const GBA_MAX_ROM_LEN: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    TooShort { needed: usize, got: usize },
    TooLarge { limit: usize, got: usize },
    BadFixedValue { offset: usize, expected: u8, got: u8 },
    BadChecksum { expected: u8, got: u8 },
    NonAscii { offset: usize },
    PathTooLong { limit: usize, got: usize },
    MalformedLine { line: usize },
    MissingField(&'static str),
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl core::error::Error for ParseError {}

/// Parsed fields of a GBA cartridge header (GBATEK "Cartridge Header").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GbaHeader {
    /// Bytes 0xA0..0xAC, ASCII, NUL-padded in the ROM.
    pub title: String,
    /// Bytes 0xAC..0xB0, e.g. "BPEE" style game code.
    pub game_code: String,
    /// Bytes 0xB0..0xB2.
    pub maker_code: String,
    /// Byte 0xBC.
    pub software_version: u8,
}

/// Parse and validate a GBA ROM header.
///
/// Validates, in order: total size bounds, the fixed value 0x96 at 0xB2,
/// and the header complement check at 0xBD
/// (`chk = 0 - 0x19 - sum(bytes 0xA0..=0xBC)`, per GBATEK).
pub fn parse_gba_header(rom: &[u8]) -> Result<GbaHeader, ParseError> {
    if rom.len() < GBA_HEADER_LEN {
        return Err(ParseError::TooShort { needed: GBA_HEADER_LEN, got: rom.len() });
    }
    if rom.len() > GBA_MAX_ROM_LEN {
        return Err(ParseError::TooLarge { limit: GBA_MAX_ROM_LEN, got: rom.len() });
    }

    let fixed = rom[0xB2];
    if fixed != 0x96 {
        return Err(ParseError::BadFixedValue { offset: 0xB2, expected: 0x96, got: fixed });
    }

    let mut sum: u8 = 0x19;
    for &b in &rom[0xA0..=0xBC] {
        sum = sum.wrapping_add(b);
    }
    let expected = 0u8.wrapping_sub(sum);
    let got = rom[0xBD];
    if got != expected {
        return Err(ParseError::BadChecksum { expected, got });
    }

    Ok(GbaHeader {
        title: ascii_field(&rom[0xA0..0xAC], 0xA0)?,
        game_code: ascii_field(&rom[0xAC..0xB0], 0xAC)?,
        maker_code: ascii_field(&rom[0xB0..0xB2], 0xB0)?,
        software_version: rom[0xBC],
    })
}

fn ascii_field(bytes: &[u8], base_offset: usize) -> Result<String, ParseError> {
    let mut out = String::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == 0 {
            break;
        }
        if !b.is_ascii() {
            return Err(ParseError::NonAscii { offset: base_offset + i });
        }
        out.push(b as char);
    }
    Ok(out)
}

/// One `FILE` entry of a CUE sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueFile {
    pub path: String,
    pub tracks: Vec<CueTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueTrack {
    pub number: u8,
    /// e.g. "AUDIO", "MODE1/2352", "MODE2/2352"
    pub mode: String,
}

/// Parse the subset of the CUE format this project needs
/// (`FILE` / `TRACK` lines), with hard limits.
///
/// Any line we do not understand is ignored (real CUE files carry REM
/// and INDEX lines), but structurally broken FILE/TRACK lines are errors.
pub fn parse_cue_sheet(input: &[u8]) -> Result<Vec<CueFile>, ParseError> {
    let text = core::str::from_utf8(input).map_err(|_| ParseError::NonAscii { offset: 0 })?;
    let mut files: Vec<CueFile> = Vec::new();

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = strip_keyword(line, "FILE") {
            let path = parse_quoted(rest).ok_or(ParseError::MalformedLine { line: lineno })?;
            if path.len() > MAX_CUE_PATH_LEN {
                return Err(ParseError::PathTooLong { limit: MAX_CUE_PATH_LEN, got: path.len() });
            }
            if path.is_empty() {
                return Err(ParseError::MissingField("FILE path"));
            }
            files.push(CueFile { path: String::from(path), tracks: Vec::new() });
        } else if let Some(rest) = strip_keyword(line, "TRACK") {
            let current = files.last_mut().ok_or(ParseError::MalformedLine { line: lineno })?;
            let mut parts = rest.split_whitespace();
            let number: u8 = parts
                .next()
                .and_then(|p| p.parse().ok())
                .ok_or(ParseError::MalformedLine { line: lineno })?;
            let mode = parts.next().ok_or(ParseError::MalformedLine { line: lineno })?;
            current.tracks.push(CueTrack { number, mode: String::from(mode) });
        }
    }

    if files.is_empty() {
        return Err(ParseError::MissingField("FILE"));
    }
    Ok(files)
}

fn strip_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    // Keyword must be a whole word: next char is whitespace.
    match rest.chars().next() {
        Some(c) if c.is_whitespace() => Some(rest.trim_start()),
        _ => None,
    }
}

fn parse_quoted(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(&inner[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header_rom() -> alloc::vec::Vec<u8> {
        let mut rom = alloc::vec![0u8; 0x200];
        rom[0xA0..0xA6].copy_from_slice(b"PANEN!");
        rom[0xAC..0xB0].copy_from_slice(b"TEST");
        rom[0xB0..0xB2].copy_from_slice(b"01");
        rom[0xB2] = 0x96;
        rom[0xBC] = 3;
        let mut sum: u8 = 0x19;
        for &b in &rom[0xA0..=0xBC] {
            sum = sum.wrapping_add(b);
        }
        rom[0xBD] = 0u8.wrapping_sub(sum);
        rom
    }

    #[test]
    fn parses_valid_header() {
        let rom = valid_header_rom();
        let h = parse_gba_header(&rom).unwrap();
        assert_eq!(h.title, "PANEN!");
        assert_eq!(h.game_code, "TEST");
        assert_eq!(h.maker_code, "01");
        assert_eq!(h.software_version, 3);
    }

    #[test]
    fn rejects_short_and_bad_checksum() {
        assert!(matches!(parse_gba_header(&[0u8; 8]), Err(ParseError::TooShort { .. })));
        let mut rom = valid_header_rom();
        rom[0xBD] ^= 0xFF;
        assert!(matches!(parse_gba_header(&rom), Err(ParseError::BadChecksum { .. })));
    }

    #[test]
    fn parses_cue_and_rejects_monster_path() {
        let cue = b"FILE \"game.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n";
        let files = parse_cue_sheet(cue).unwrap();
        assert_eq!(files[0].path, "game.bin");
        assert_eq!(files[0].tracks[0].number, 1);

        // The CVE-2025-9809 shape: an over-long path must be an error,
        // never a silent truncation or an overflow.
        let mut evil = String::from("FILE \"");
        for _ in 0..(MAX_CUE_PATH_LEN + 1) {
            evil.push('A');
        }
        evil.push_str("\" BINARY\n");
        assert!(matches!(
            parse_cue_sheet(evil.as_bytes()),
            Err(ParseError::PathTooLong { .. })
        ));
    }

    /// Mutation smoke test: stand-in for the cargo-fuzz target in `fuzz/`
    /// until nightly + cargo-fuzz are set up. Parsers must never panic,
    /// whatever bytes they are fed.
    #[test]
    fn parsers_never_panic_on_mutated_input() {
        let mut seed: u32 = 0x1234_5678;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let base = valid_header_rom();
        for _ in 0..20_000 {
            let mut mutated = base.clone();
            for _ in 0..8 {
                let i = (next() as usize) % mutated.len();
                mutated[i] = next() as u8;
            }
            let _ = parse_gba_header(&mutated);
            let _ = parse_cue_sheet(&mutated);
        }
    }
}
