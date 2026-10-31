use std::sync::Mutex;

use almiladoc::{
    CancelToken, GapFill, OcrEngine, OcrFailure, OcrImage, OcrLine, OcrOutcome, OcrRequest,
    RasterSpec,
};
use image::{ImageFormat, RgbImage};

use crate::{Config, Error, Limits, ModelFiles, OfflineOcr, Page, Runtime};

pub(crate) trait Recognizer: Send {
    fn page(
        &mut self,
        image: RgbImage,
        limits: &Limits,
        cancel: &CancelToken,
    ) -> Result<Page, Error>;
}

impl Recognizer for OfflineOcr {
    fn page(
        &mut self,
        image: RgbImage,
        limits: &Limits,
        cancel: &CancelToken,
    ) -> Result<Page, Error> {
        self.recognize_image(image, limits, &|| cancel.is_cancelled())
    }
}

/// An explicitly initialized CPU OCR engine returning raw located lines to core's layout owner.
///
/// Native calls cannot be interrupted or hard memory-capped; callers needing deadlines isolate it.
/// Worker settings do not cap auxiliary native threads.
///
/// ```no_run
/// use almiladoc::{Converter, Input, RasterSpec};
/// use almiladoc_ocr_local::{Config, FileSpec, Limits, LocalOcr, ModelFiles, Runtime};
///
/// # fn convert(files: ModelFiles, library: FileSpec, input: Input<'_>) -> Result<(), Box<dyn std::error::Error>> {
/// let runtime = Runtime::load(&library, &Limits::default())?;
/// let engine = LocalOcr::new(&runtime, &files, Config::default(), RasterSpec::default())?;
/// let mut parsed = Converter::new(almiladoc::Config::default())?.parse(input)?;
/// parsed.ocr(&engine, None)?;
/// let _converted = parsed.render()?;
/// # Ok(())
/// # }
/// ```
pub struct LocalOcr {
    pub(crate) backend: Mutex<Box<dyn Recognizer>>,
    pub(crate) limits: Limits,
    pub(crate) raster: RasterSpec,
}

impl std::fmt::Debug for LocalOcr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOcr")
            .field("limits", &self.limits)
            .field("raster", &self.raster)
            .finish_non_exhaustive()
    }
}

impl LocalOcr {
    /// Load verified caller models; the runtime and its dependency files must remain immutable.
    ///
    /// # Errors
    /// Refuses invalid configuration, a raster cap above input admission, and producer setup errors.
    /// Encoded-byte and dimension limits still apply; incompressible PNGs may exceed the byte cap.
    pub fn new(
        runtime: &Runtime,
        models: &ModelFiles,
        config: Config,
        raster: RasterSpec,
    ) -> Result<Self, Error> {
        config.validate()?;
        raster.validate().map_err(|_| Error::Config("raster"))?;
        if raster.max_pixels > config.limits.max_pixels
            || crate::decode::painted_buffer_bytes(raster.max_pixels)
                .is_none_or(|bytes| bytes > config.limits.max_decode_bytes)
        {
            return Err(Error::Config("raster"));
        }
        let limits = config.limits.clone();
        Ok(Self {
            backend: Mutex::new(Box::new(OfflineOcr::new(runtime, models, config)?)),
            limits,
            raster,
        })
    }

    pub(crate) fn read(&self, image: &OcrImage, budget: u64, cancel: &CancelToken) -> OcrOutcome {
        if cancel.is_cancelled() {
            return unattempted();
        }
        let result = self.try_read(image, budget, cancel);
        if cancel.is_cancelled() {
            return unattempted();
        }
        match result {
            Ok(lines) => OcrOutcome::Fill(GapFill::Lines(lines)),
            Err(
                Error::ImageFormat
                | Error::ImageDecode
                | Error::ImageLimit
                | Error::EmptyRecognition
                | Error::OutputLimit,
            ) => OcrOutcome::Failed(OcrFailure::default()),
            Err(_) => unattempted(),
        }
    }

    fn try_read(
        &self,
        image: &OcrImage,
        budget: u64,
        cancel: &CancelToken,
    ) -> Result<Vec<OcrLine>, Error> {
        if image.bytes.len() > self.limits.max_image_bytes {
            return Err(Error::ImageLimit);
        }
        let mime = match image::guess_format(&image.bytes).map_err(|_| Error::ImageFormat)? {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            _ => return Err(Error::ImageFormat),
        };
        if image.media_type != mime {
            return Err(Error::ImageFormat);
        }
        let mut backend = self.backend.try_lock().map_err(|_| Error::Runtime)?;
        let mut limits = self.limits.clone();
        limits.max_text_bytes =
            limits.max_text_bytes.min(usize::try_from(budget).unwrap_or(usize::MAX));
        limits.max_lines = limits
            .max_lines
            .min(usize::try_from(budget / OcrLine::BOX_BYTES).unwrap_or(usize::MAX));
        let decoded = crate::runtime::quiet(|| {
            crate::decode::decode_with_cancel(&image.bytes, &limits, &|| cancel.is_cancelled())
        })?;
        let dimensions = decoded.dimensions();
        crate::checkpoint(&|| cancel.is_cancelled())?;
        let page = backend.page(decoded, &limits, cancel)?;
        crate::checkpoint(&|| cancel.is_cancelled())?;
        located(page, dimensions, budget, &limits, cancel)
    }
}

impl OcrEngine for LocalOcr {
    fn raster(&self) -> RasterSpec {
        self.raster
    }

    fn recognize(&self, requests: &[OcrRequest], cancel: &CancelToken) -> Vec<OcrOutcome> {
        let mut stopped = false;
        requests
            .iter()
            .map(|request| {
                let outcome = if stopped {
                    unattempted()
                } else {
                    self.read(&request.image, request.max_text_bytes, cancel)
                };
                stopped |= matches!(outcome, OcrOutcome::Unattempted(_));
                outcome
            })
            .collect()
    }
}

fn unattempted() -> OcrOutcome {
    OcrOutcome::Unattempted(OcrFailure::default())
}

pub(crate) fn located(
    page: Page,
    dimensions: (u32, u32),
    budget: u64,
    limits: &Limits,
    cancel: &CancelToken,
) -> Result<Vec<OcrLine>, Error> {
    if (page.width, page.height) != dimensions || page.width == 0 || page.height == 0 {
        return Err(Error::InvalidOutput);
    }
    if page.lines.len() > limits.max_lines {
        return Err(Error::OutputLimit);
    }
    let mut bytes = u64::try_from(page.lines.len())
        .ok()
        .and_then(|count| count.checked_mul(OcrLine::BOX_BYTES))
        .ok_or(Error::OutputLimit)?;
    let mut text_bytes = 0usize;
    page.lines
        .into_iter()
        .map(|line| {
            crate::checkpoint(&|| cancel.is_cancelled())?;
            let (text, rectangle) = line.into_parts();
            if text.trim().is_empty() {
                return Err(Error::EmptyRecognition);
            }
            text_bytes = text_bytes.checked_add(text.len()).ok_or(Error::OutputLimit)?;
            bytes = bytes
                .checked_add(u64::try_from(text.len()).map_err(|_| Error::OutputLimit)?)
                .ok_or(Error::OutputLimit)?;
            if bytes > budget || text_bytes > limits.max_text_bytes {
                return Err(Error::OutputLimit);
            }
            let width = f64::from(page.width);
            let height = f64::from(page.height);
            OcrLine::new(
                text,
                [
                    f64::from(rectangle.left) / width,
                    f64::from(rectangle.top) / height,
                    f64::from(rectangle.right) / width,
                    f64::from(rectangle.bottom) / height,
                ],
            )
            .map_err(|_| Error::InvalidOutput)
        })
        .collect()
}
