#![no_main]

use almiladoc::{ConvertError, Format, Input, PageSelection, RasterSpec};
use almiladoc_fuzz::{fuzz_target, painting_converter};
use std::sync::Arc;

fuzz_target!(|data: &[u8]| {
    let converter = painting_converter();
    let mut spec = RasterSpec::default();
    (spec.dpi, spec.max_pixels) = (50, 1 << 20);
    let paint = || -> Result<Vec<Arc<[u8]>>, ConvertError> {
        let input = Input::new(data, Format::Pdf).with_ocr_pages(PageSelection::All);
        let mut parsed = converter.parse(input)?;
        let requests = parsed.ocr_requests(&spec, None)?;
        Ok(requests.into_iter().map(|request| request.image.bytes).collect())
    };
    assert_eq!(paint(), paint(), "painting the pages again gives other bytes");
});
