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
//! Recovery and skipped-content events are reported through the [`log`]
//! facade (debug/warn level); logging never changes conversion behavior and
//! its messages are not a stable API.

#![warn(missing_docs)]

pub mod model;

mod config;
mod converter;
mod ctx;
mod error;
mod formats;
mod ledger;
mod limits;
mod package;
mod pool;
mod render;
mod shared;

pub use config::{Config, ConfigError};
pub use converter::{Conversion, Converter, Input, Parsed};
pub use error::{ConvertError, PartName, UnsupportedKind};
pub use ledger::CancelToken;
pub use limits::{Limit, Limits};

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
    /// [pdf-inspector]: https://github.com/firecrawl/pdf-inspector
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
    /// Detect the format from the content itself: the signature and identity
    /// each container specification designates (PDF header, RTF open group,
    /// OLE stream names, ZIP package mimetype/content types). Plain-text
    /// formats (CSV) carry no signature and return `None`; so does anything
    /// unrecognized.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Format> {
        formats::detect::from_bytes(bytes)
    }

    /// The format a bare extension names (no leading dot), matched
    /// case-insensitively. `None` for anything unrecognized.
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Format> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "doc" => Format::Doc,
            "docx" | "docm" => Format::Docx,
            "odt" => Format::Odt,
            "pdf" => Format::Pdf,
            "pptx" | "pptm" | "ppsx" | "ppsm" => Format::Pptx,
            "ppt" | "pps" | "pot" => Format::Ppt,
            "rtf" => Format::Rtf,
            "epub" => Format::Epub,
            "xlsx" | "xlsm" | "xlsb" | "xls" => Format::Excel,
            "ods" => Format::Ods,
            "odp" => Format::Odp,
            "csv" => Format::Csv,
            _ => return None,
        })
    }
}
