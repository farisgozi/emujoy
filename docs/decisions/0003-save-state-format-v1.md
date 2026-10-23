# 0003 — Save state format v1: versioned, checksummed, validated-first

Date: 2026-10-23 · Status: accepted

## Format
```
header : magic "EMUSTATE" | format_version u16 | flags u16 | rom_hash u64
section: id u16 | len u32 | crc32 u32 | payload[len]   (repeated)
```
Sections: 1 = CPU, 2 = Scheduler (now + pending events), 3 = Memory
(EWRAM+IWRAM+VRAM). Unknown sections are skipped but still CRC-checked
(forward compatibility for newer writers).

## Rules
1. Validate everything — magic, version, lengths against remaining
   bytes, every CRC, ROM hash, exact end-of-input — BEFORE applying.
2. Any mismatch = total rejection. Never a best-effort load.
3. Counts/lengths inside a payload are bounded by the payload's real
   remaining bytes before use (the scheduler event count does this).
4. The ROM hash binds a state to one game; foreign states refuse.

## Why first, not later
Run-ahead, rewind and TAS all stand on save/restore being cheap,
deterministic and safe. Retrofitting integrity onto a raw struct dump
is how the audited cores got stuck; this format is Phase 0 work.
