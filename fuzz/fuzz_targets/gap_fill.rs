#![no_main]

use almiladoc::{Format, GapFill, Input};
use almiladoc_fuzz::{converter, fuzz_target};

const PDF: &[u8] = include_bytes!("../../tests/fixtures/pdf/handmade-mixed.pdf");

fuzz_target!(|data: &[u8]| {
    let mut parsed = converter().parse(Input::new(PDF, Format::Pdf)).expect("the fixture parses");
    let gap = parsed.gaps().next().expect("the fixture has a gap").id;
    let fill = GapFill::Markdown(String::from_utf8_lossy(data).into_owned());
    if parsed.fill_gap(gap, fill).is_ok() {
        let _: Result<_, _> = parsed.render();
    }
});
