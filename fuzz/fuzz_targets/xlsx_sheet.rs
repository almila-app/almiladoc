#![no_main]

use almiladoc_fuzz::{XLSX_SHEET, fuzz_target};

fuzz_target!(|data: &[u8]| XLSX_SHEET.check(data));
