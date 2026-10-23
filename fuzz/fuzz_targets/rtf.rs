#![no_main]

use almiladoc::Format;
use almiladoc_fuzz::{convert, fuzz_target};

fuzz_target!(|data: &[u8]| convert(data, Format::Rtf));
