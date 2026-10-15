#![no_main]

use almiladoc::{Config, Converter, Format, Input};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Conversion may fail with a typed error; it must never panic, hang,
    // or exhaust memory.
    let Ok(converter) = Converter::new(Config::default()) else {
        return;
    };
    let _ = converter.convert(Input::new(data, Format::Doc));
});
