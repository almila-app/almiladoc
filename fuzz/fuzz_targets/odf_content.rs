#![no_main]

use almiladoc_fuzz::{ODF_CONTENT, fuzz_target};

fuzz_target!(|data: &[u8]| ODF_CONTENT.check(data));
