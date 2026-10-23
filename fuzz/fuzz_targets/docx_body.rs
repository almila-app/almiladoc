#![no_main]

use almiladoc_fuzz::{DOCX_BODY, fuzz_target};

fuzz_target!(|data: &[u8]| DOCX_BODY.check(data));
