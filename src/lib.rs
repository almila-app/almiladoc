//! almiladoc converts documents to GitHub-Flavored Markdown.
//!
//! ```
//! use almiladoc::{Config, Converter, Format, Input};
//!
//! let converter = Converter::new(Config::default())?;
//! let csv = b"name,year\nAda,1843\n";
//! let markdown = converter.convert(Input::new(csv, Format::Csv))?.into_markdown();
//! assert_eq!(markdown, "| name | year |\n| --- | --- |\n| Ada | 1843 |\n");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! What a conversion cannot carry into its output is reported as
//! [`Diagnostics`]: content [`Lost`](LossClass::Lost) fails the conversion
//! with [`ConvertError::Incomplete`] under the default [`OnLoss::Fail`], and
//! [`OnLoss::Report`] returns it beside the Markdown. Every piece of input
//! skipped or repaired on the way is reported in [`Diagnostics`] too.

pub mod model;

mod config;
mod converter;
mod ctx;
mod diagnostics;
mod error;
mod formats;
mod identity;
#[cfg(feature = "serde")]
mod json;
mod ledger;
mod limits;
mod package;
mod pool;
mod render;
mod shared;

pub use config::{Config, ConfigError, ImagePolicy};
pub use converter::{Conversion, Converter, Input, Parsed, UnitSpan};
pub use diagnostics::{Diagnostic, DiagnosticCode, Diagnostics, LossClass, OnLoss, Region};
pub use error::{ConvertError, PartName, UnsupportedKind};
pub use identity::EngineIdentity;
#[cfg(feature = "serde")]
pub use json::JSON_SCHEMA_VERSION;
pub use ledger::CancelToken;
pub use limits::{Limit, Limits};

const fn assert_send_sync<T: Send + Sync>() {}
const _: () = {
    assert_send_sync::<ConvertError>();
    assert_send_sync::<ConfigError>();
    assert_send_sync::<Converter>();
    assert_send_sync::<Parsed>();
};

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// Input format. Selects the parser; container variants that share a parser
/// (docm, xlsm, ...) map onto these via [`Format::sniff`] or
/// [`Format::from_extension`].
///
/// New formats are added over time, so a match outside this crate needs a
/// wildcard arm:
///
/// ```compile_fail,E0004
/// fn name(format: almiladoc::Format) -> &'static str {
///     match format {
///         almiladoc::Format::Doc => "doc",
///         almiladoc::Format::Docx => "docx",
///         almiladoc::Format::Odt => "odt",
///         almiladoc::Format::Pdf => "pdf",
///         almiladoc::Format::Ppt => "ppt",
///         almiladoc::Format::Pptx => "pptx",
///         almiladoc::Format::Rtf => "rtf",
///         almiladoc::Format::Epub => "epub",
///         almiladoc::Format::Excel => "xlsx",
///         almiladoc::Format::Ods => "ods",
///         almiladoc::Format::Odp => "odp",
///         almiladoc::Format::Csv => "csv",
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// Binary Word 97-2003 (`.doc`).
    Doc,
    /// WordprocessingML (`.docx`, `.docm`), both Transitional and Strict.
    Docx,
    /// OpenDocument Text (`.odt`).
    Odt,
    /// Converted with [pdf-inspector], which emits Markdown directly:
    /// [`Converter::parse`] is unsupported for PDFs, and a build without the
    /// `pdf` feature converts none. Scanned or image-only pages need OCR,
    /// which almiladoc does not do: the document errors with
    /// [`ConvertError::NeedsOcr`] naming them.
    ///
    /// [pdf-inspector]: https://docs.rs/pdf-inspector
    Pdf,
    /// Binary PowerPoint 97-2003 (`.ppt`, `.pps`, `.pot`).
    Ppt,
    /// PresentationML (`.pptx`, `.pptm`, `.ppsx`, `.ppsm`).
    Pptx,
    /// Rich Text Format (`.rtf`).
    Rtf,
    /// EPUB 2 and 3 (`.epub`).
    Epub,
    /// Excel workbooks: `.xlsx`, `.xlsm`, binary `.xlsb`, and legacy
    /// OLE-based `.xls`.
    Excel,
    /// OpenDocument Spreadsheet (`.ods`).
    Ods,
    /// OpenDocument Presentation (`.odp`).
    Odp,
    /// Delimiter-separated text (`.csv`). Carries no signature, so it has to
    /// be named rather than detected.
    Csv,
}

impl Format {
    pub(crate) const ALL: [Self; 12] = [
        Self::Doc,
        Self::Docx,
        Self::Odt,
        Self::Pdf,
        Self::Ppt,
        Self::Pptx,
        Self::Rtf,
        Self::Epub,
        Self::Excel,
        Self::Ods,
        Self::Odp,
        Self::Csv,
    ];

    /// Detect the format from the content itself: the signature and identity
    /// each container specification designates (PDF header, RTF open group,
    /// OLE stream names, ZIP package mimetype/content types). A package is
    /// read only as far as its identity: 256 bytes of `mimetype`, 1 MiB of
    /// each of its relationships, content types and manifest, and the first
    /// 16 KiB of its main part. Plain-text formats (CSV) carry no signature
    /// and return `None`; so does anything unrecognized.
    ///
    /// [`Converter::parse`] and [`Converter::convert`] run the same detection
    /// on the input and fail [`UnsupportedKind::FormatMismatch`], naming the
    /// detected format, when it is not the format named; a PDF header past
    /// the first byte does not contradict a CSV label.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{Config, Converter, Format, Input};
    ///
    /// let rtf = br"{\rtf1 Hello}";
    /// let format = Format::sniff(rtf).ok_or("unrecognized format")?;
    /// assert_eq!(format, Format::Rtf);
    ///
    /// let converter = Converter::new(Config::default())?;
    /// let markdown = converter.convert(Input::new(rtf, format))?.into_markdown();
    /// assert_eq!(markdown, "Hello\n");
    ///
    /// assert_eq!(Format::sniff(b"name,year\nAda,1843\n"), None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        formats::detect::from_bytes(bytes)
    }

    /// The format's name: the extension that identifies it, `xlsx` for
    /// every Excel workbook.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Pdf => "pdf",
            Self::Ppt => "ppt",
            Self::Pptx => "pptx",
            Self::Rtf => "rtf",
            Self::Epub => "epub",
            Self::Excel => "xlsx",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::Csv => "csv",
        }
    }

    /// The format a bare extension names (no leading dot), matched
    /// case-insensitively. `None` for anything unrecognized.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// use almiladoc::Format;
    ///
    /// let format = Path::new("thesis.DOCM")
    ///     .extension()
    ///     .and_then(|extension| extension.to_str())
    ///     .and_then(Format::from_extension);
    /// assert_eq!(format, Some(Format::Docx));
    ///
    /// assert_eq!(Format::from_extension("xlsb"), Some(Format::Excel));
    /// assert_eq!(Format::from_extension("txt"), None);
    /// ```
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "doc" => Self::Doc,
            "docx" | "docm" => Self::Docx,
            "odt" => Self::Odt,
            "pdf" => Self::Pdf,
            "pptx" | "pptm" | "ppsx" | "ppsm" => Self::Pptx,
            "ppt" | "pps" | "pot" => Self::Ppt,
            "rtf" => Self::Rtf,
            "epub" => Self::Epub,
            "xlsx" | "xlsm" | "xlsb" | "xls" => Self::Excel,
            "ods" => Self::Ods,
            "odp" => Self::Odp,
            "csv" => Self::Csv,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses exactly a [`Format::as_str`] name, case-sensitively; extension
/// aliases such as `docm` belong to [`Format::from_extension`].
///
/// ```
/// use almiladoc::Format;
///
/// assert_eq!("xlsx".parse(), Ok(Format::Excel));
/// assert!("XLSX".parse::<Format>().is_err());
/// assert!("docm".parse::<Format>().is_err());
/// ```
impl std::str::FromStr for Format {
    type Err = ParseFormatError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|format| format.as_str() == name).ok_or(ParseFormatError)
    }
}

/// The error parsing a name that is not exactly one format's
/// [`Format::as_str`].
///
/// ```
/// use almiladoc::Format;
///
/// let error = "docm".parse::<Format>().unwrap_err();
/// assert!(error.to_string().starts_with("unknown format; expected one of doc, docx, odt"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ParseFormatError;

impl std::fmt::Display for ParseFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [first, rest @ ..] = Format::ALL;
        write!(f, "unknown format; expected one of {first}")?;
        for format in rest {
            write!(f, ", {format}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseFormatError {}

#[cfg(test)]
mod tests {
    use super::{Format, ParseFormatError};

    #[test]
    fn every_format_is_named_by_the_extension_that_names_it() {
        let names = [
            (Format::Doc, "doc"),
            (Format::Docx, "docx"),
            (Format::Odt, "odt"),
            (Format::Pdf, "pdf"),
            (Format::Ppt, "ppt"),
            (Format::Pptx, "pptx"),
            (Format::Rtf, "rtf"),
            (Format::Epub, "epub"),
            (Format::Excel, "xlsx"),
            (Format::Ods, "ods"),
            (Format::Odp, "odp"),
            (Format::Csv, "csv"),
        ];
        for (format, name) in names {
            assert_eq!(format.as_str(), name);
            assert_eq!(format.to_string(), name);
            assert_eq!(Format::from_extension(name), Some(format));
            assert_eq!(name.parse(), Ok(format));
        }
    }

    /// Fails to compile when a format is added, and fails when `ALL` drops or repeats one.
    #[test]
    fn all_holds_every_format_once_in_declaration_order() {
        let declared = |format| match format {
            Format::Doc => 0,
            Format::Docx => 1,
            Format::Odt => 2,
            Format::Pdf => 3,
            Format::Ppt => 4,
            Format::Pptx => 5,
            Format::Rtf => 6,
            Format::Epub => 7,
            Format::Excel => 8,
            Format::Ods => 9,
            Format::Odp => 10,
            Format::Csv => 11,
        };
        let order: Vec<usize> = Format::ALL.into_iter().map(declared).collect();
        assert_eq!(order, Vec::from_iter(0..12));
    }

    #[test]
    fn only_a_format_name_parses() {
        for name in ["DOCX", "Docx", "docm", ".docx", "docx ", "", "markdown", "xls", "excel"] {
            assert_eq!(name.parse::<Format>(), Err(ParseFormatError), "{name:?}");
        }
        assert_eq!(
            ParseFormatError.to_string(),
            "unknown format; expected one of doc, docx, odt, pdf, ppt, pptx, rtf, epub, xlsx, ods, \
             odp, csv"
        );
    }
}
