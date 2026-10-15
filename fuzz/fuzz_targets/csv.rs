#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Conversion may fail with a typed error; it must never panic, hang,
    // or exhaust memory.
    let _ = almiladoc::to_markdown_bytes(data, almiladoc::Format::Csv);
});
