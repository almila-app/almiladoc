use std::time::SystemTime;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OcrFailure {
    /// What kept the request from text.
    pub kind: OcrFailureKind,
    /// The HTTP status of the last attempt, when it had one.
    pub status: Option<u16>,
    /// The longest wait a server asked for over the request's attempts, a later attempt's wait
    /// of the other form replacing it.
    pub retry_after: Option<RetryAfter>,
}

impl OcrFailure {
    /// A failure of `kind` with the HTTP status of its last attempt, if any.
    #[must_use]
    pub const fn new(kind: OcrFailureKind, status: Option<u16>) -> Self {
        Self { kind, status, retry_after: None }
    }

    /// This failure with the wait a server asked for.
    #[must_use]
    pub const fn with_retry_after(self, retry_after: Option<RetryAfter>) -> Self {
        Self { retry_after, ..self }
    }
}

/// What kept a request from text: closed, so it carries no text of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OcrFailureKind {
    /// The engine or server refused the request as sent: an image it cannot decode, or 400,
    /// 422 or another 4xx.
    Rejected,
    /// The image or the reply was too large (413, or past a cap).
    TooLarge,
    /// The model refused the image, or a content filter stopped it.
    Refused,
    /// The model stopped before it finished.
    Unfinished,
    /// The engine read no text, or the reply held no string content or only an unclosed `<think>`.
    NoText,
    /// The reply could not be read as an answer, or redirected.
    Protocol,
    /// The reply came from a model other than the one asked for.
    WrongModel,
    /// The engine or server could not take the request yet: another call holds the engine, the
    /// system granted no thread, or 408, 429 or 5xx.
    Busy,
    /// No answer came: the engine faulted, or the server was unreachable, timed out or dropped
    /// the connection.
    Unreachable,
    /// The token was cancelled first.
    Cancelled,
}

impl OcrFailureKind {
    /// Every kind, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Rejected,
        Self::TooLarge,
        Self::Refused,
        Self::Unfinished,
        Self::NoText,
        Self::Protocol,
        Self::WrongModel,
        Self::Busy,
        Self::Unreachable,
        Self::Cancelled,
    ];

    /// The kind's stable `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::TooLarge => "too_large",
            Self::Refused => "refused",
            Self::Unfinished => "unfinished",
            Self::NoText => "no_text",
            Self::Protocol => "protocol",
            Self::WrongModel => "wrong_model",
            Self::Busy => "busy",
            Self::Unreachable => "unreachable",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A `Retry-After` value as the server sent it, read with no clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetryAfter {
    /// Seconds to wait from the reply, saturating.
    Seconds(u64),
    /// The time to wait until.
    At(SystemTime),
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
    /// Gaps left unfilled because no image could be made of them, which no engine saw: a
    /// page that cannot be painted, or an image over [`RasterSpec::max_bytes`].
    pub refused: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_kind_has_its_own_snake_case_name() {
        assert_eq!(
            OcrFailureKind::ALL.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
            [
                "rejected",
                "too_large",
                "refused",
                "unfinished",
                "no_text",
                "protocol",
                "wrong_model",
                "busy",
                "unreachable",
                "cancelled",
            ]
        );
    }
}
