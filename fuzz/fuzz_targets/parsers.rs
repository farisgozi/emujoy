#![no_main]
// cargo-fuzz target (requires nightly + cargo-fuzz):
//   cargo install cargo-fuzz
//   cargo fuzz run parsers -- -max_total_time=3600
// Until that is set up, emu-parser's mutation smoke test in CI covers
// the same property: parsers must never panic on arbitrary bytes.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = emu_parser::parse_gba_header(data);
    let _ = emu_parser::parse_cue_sheet(data);
});
