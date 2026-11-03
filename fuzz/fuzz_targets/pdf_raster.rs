#![no_main]

use almiladoc::{ConvertError, Format, Input, OcrRefusal, PageSelection, RasterSize, RasterSpec};
use almiladoc_fuzz::{fuzz_target, painting_converter};
use std::sync::Arc;

type Paints = Result<Vec<Result<Arc<[u8]>, OcrRefusal>>, ConvertError>;

fuzz_target!(|data: &[u8]| {
    let converter = painting_converter();
    let mut spec = RasterSpec::default();
    let exact = data.last().is_some_and(|byte| byte & 1 == 1);
    let size = if exact { RasterSize::LongEdge(64) } else { RasterSize::Dpi(50) };
    (spec.size, spec.max_pixels) = (size, 1 << 20);
    let paint = || -> Paints {
        let input = Input::new(data, Format::Pdf).with_ocr_pages(PageSelection::All);
        let mut parsed = converter.parse(input)?;
        let requests = parsed.ocr_requests(&spec, None)?;
        Ok(requests.into_iter().map(|made| made.map(|request| request.image.bytes)).collect())
    };
    assert_eq!(paint(), paint(), "painting the pages again gives other bytes");
});
