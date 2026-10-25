use crate::ledger::CancelToken;
use crate::ocr::{GapFill, OcrRequest, RasterSpec};

/// Reads the text of the images it is asked about, for
/// [`Parsed::ocr`](crate::Parsed::ocr) to fill their gaps with.
///
/// # Examples
///
/// ```
/// use almiladoc::{CancelToken, GapFill, OcrEngine, OcrOutcome, OcrRequest};
///
/// struct Lengths;
///
/// impl OcrEngine for Lengths {
///     fn batch_size(&self) -> usize {
///         4
///     }
///
///     fn recognize(&self, requests: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
///         let read = |request: &OcrRequest| format!("{} bytes", request.image.bytes.len());
///         requests.iter().map(|r| OcrOutcome::Fill(GapFill::Markdown(read(r)))).collect()
///     }
/// }
/// ```
pub trait OcrEngine: Send + Sync {
    /// The most requests one [`OcrEngine::recognize`] call takes; 0 counts as 1.
    fn batch_size(&self) -> usize {
        1
    }

    /// How large an image of a page it takes.
    fn raster(&self) -> RasterSpec {
        RasterSpec::default()
    }

    /// One outcome for each of `requests`, in their order. `cancel` is the parse's token,
    /// which a long call should watch; a call that returns any other number of outcomes
    /// fails each of its requests' gaps.
    fn recognize(&self, requests: &[OcrRequest], cancel: &CancelToken) -> Vec<OcrOutcome>;
}

/// What an [`OcrEngine`] made of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OcrOutcome {
    /// The text it read, which fills the gap.
    Fill(GapFill),
    /// It tried and failed, which fails the gap.
    Failed(OcrFailure),
    /// It did not try, which leaves the gap unfilled and stops the call after this batch.
    Unattempted(OcrFailure),
}

/// Why an [`OcrEngine`] read nothing for a request, holding no text of the attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct OcrFailure {
    /// The HTTP status of the last attempt, when it had one.
    pub status: Option<u16>,
}

impl OcrFailure {
    /// A failure with the HTTP status of its last attempt, if any.
    #[must_use]
    pub const fn new(status: Option<u16>) -> Self {
        Self { status }
    }
}

/// What one [`Parsed::ocr`](crate::Parsed::ocr) call did to the gaps it could request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct OcrSummary {
    /// Gaps filled.
    pub filled: usize,
    /// Gaps failed: the engine failed them, their fill was refused, or their batch got the
    /// wrong number of outcomes.
    pub failed: usize,
    /// Gaps left unfilled: the engine did not attempt them, or a batch before theirs stopped
    /// the call.
    pub unattempted: usize,
}
