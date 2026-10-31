use crate::FileRole;

/// A failure containing no paths, OCR text, native messages or source-error chain.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The caller cancelled between native operations.
    #[error("OCR was cancelled")]
    Cancelled,
    /// A configuration field is outside its supported range.
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    /// A supplied file could not be read as a regular file.
    #[error("cannot read {0:?} file")]
    FileUnreadable(FileRole),
    /// A supplied file exceeds its configured byte bound.
    #[error("{0:?} file exceeds its byte bound")]
    FileTooLarge(FileRole),
    /// Supplied bytes differ from their expected SHA-256.
    #[error("{0:?} file digest does not match")]
    DigestMismatch(FileRole),
    /// The supplied dictionary must contain one non-NUL Unicode scalar per line.
    #[error("invalid character dictionary")]
    Dictionary,
    /// The native runtime could not be loaded or initialized.
    #[error("cannot initialize ONNX Runtime")]
    Runtime,
    /// This process already has incompatible runtime initialization.
    #[error("ONNX Runtime was already initialized")]
    RuntimeConflict,
    /// The private worker pool could not be created.
    #[error("cannot create OCR worker pool")]
    Pool,
    /// The verified model bytes could not create an OCR pipeline.
    #[error("cannot initialize OCR models")]
    Model,
    /// The image bytes are neither PNG nor JPEG.
    #[error("OCR requires PNG or JPEG bytes")]
    ImageFormat,
    /// The encoded bytes could not be decoded.
    #[error("cannot decode OCR image")]
    ImageDecode,
    /// The image exceeds an encoded, dimensional, pixel or buffer bound.
    #[error("OCR image exceeds its configured bounds")]
    ImageLimit,
    /// The OCR pipeline did not complete recognition.
    #[error("OCR recognition failed")]
    Recognition,
    /// A detected region yielded no text; whether it was a false detection is unknown.
    #[error("OCR detected a region but read no text")]
    EmptyRecognition,
    /// The pipeline returned invalid geometry or an incomplete region.
    #[error("OCR returned an invalid text region")]
    InvalidOutput,
    /// A cropped image, recognized text or region count exceeds its configured bound.
    #[error("OCR output exceeds its configured bounds")]
    OutputLimit,
}
