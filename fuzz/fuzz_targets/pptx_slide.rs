#![no_main]

use almiladoc_fuzz::{PPTX_SLIDE, fuzz_target};

fuzz_target!(|data: &[u8]| PPTX_SLIDE.check(data));
