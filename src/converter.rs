//! The conversion API: a [`Converter`] built once from a [`Config`], and the
//! calls that walk one [`Input`] on its per-call pool.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use crate::Format;
use crate::config::{Config, ConfigError};
use crate::ctx::ParseCtx;
use crate::diagnostics::{Diagnostics, OnLoss};
use crate::error::ConvertError;
use crate::formats;
use crate::identity::EngineIdentity;
use crate::ledger::{CancelToken, Ledger};
use crate::limits::Limit;
use crate::model::{Document, Unit};
use crate::pool::run_on_pool;
use crate::render::markdown::{Meter, Unread, check_nesting, render, render_losses};

/// Converts documents under one validated [`Config`]; share it across threads
/// and calls.
#[derive(Debug, Clone)]
pub struct Converter {
    config: Arc<Config>,
    identity: Arc<EngineIdentity>,
    #[cfg(test)]
    detections: Arc<std::sync::atomic::AtomicUsize>,
}

impl Converter {
    /// A converter for `config`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a limit or the thread count is out of range.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{Config, ConfigError, Converter};
    ///
    /// let converter = Converter::new(Config::default())?;
    ///
    /// let mut config = Config::default();
    /// config.threads = 0;
    /// assert!(matches!(Converter::new(config), Err(ConfigError::Threads { .. })));
    /// # Ok::<(), ConfigError>(())
    /// ```
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        let identity = Arc::new(EngineIdentity::of(&config));
        Ok(Self {
            config: Arc::new(config),
            identity,
            #[cfg(test)]
            detections: Arc::default(),
        })
    }

    /// The version and configuration fingerprint that fix this converter's
    /// output.
    #[must_use]
    pub fn identity(&self) -> &EngineIdentity {
        &self.identity
    }

    /// The format `bytes` are, detected as [`Format::sniff`] detects it but
    /// under this converter's limits and `cancel`, once `bytes` are admitted
    /// as [`Converter::convert`] admits an input; `None` when the content
    /// names no format.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Cancelled`] when `cancel` is cancelled, and
    /// [`ConvertError::ResourceLimit`] when `bytes` pass
    /// [`Limits::max_input_bytes`](crate::Limits::max_input_bytes) or
    /// detection crosses a limit; any other failure reads as `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{CancelToken, Config, ConvertError, Converter, Format};
    ///
    /// let converter = Converter::new(Config::default())?;
    /// assert_eq!(converter.sniff(br"{\rtf1 Hello}", None)?, Some(Format::Rtf));
    /// assert_eq!(converter.sniff(b"name,year\nAda,1843\n", None)?, None);
    ///
    /// let token = CancelToken::new();
    /// token.cancel();
    /// let refused = converter.sniff(br"{\rtf1 Hello}", Some(&token));
    /// assert!(matches!(refused, Err(ConvertError::Cancelled)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn sniff(
        &self,
        bytes: &[u8],
        cancel: Option<&CancelToken>,
    ) -> Result<Option<Format>, ConvertError> {
        self.admit(bytes, cancel)?;
        self.on_pool(|| formats::detect::detect(&self.parse_ctx(cancel), bytes))
    }

    /// Parse `input` into the document model, with the diagnostics of what it
    /// could not read and what rendering its document drops; losses fail only
    /// when [`Parsed::render`] finishes it.
    ///
    /// # Errors
    ///
    /// [`ConvertError`] when no complete document could be parsed. A PDF has
    /// no document-model form and fails
    /// [`UnsupportedKind::PdfDocumentModel`](crate::UnsupportedKind::PdfDocumentModel),
    /// or [`UnsupportedKind::FeatureDisabled`](crate::UnsupportedKind::FeatureDisabled)
    /// in a build without the `pdf` feature.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::model::Block;
    /// use almiladoc::{Config, ConvertError, Converter, Format, Input, UnsupportedKind};
    ///
    /// let converter = Converter::new(Config::default())?;
    /// let rtf = br"{\rtf1 Hello}";
    ///
    /// let parsed = converter.parse(Input::new(rtf, Format::Rtf))?;
    /// assert!(matches!(parsed.document().blocks.as_slice(), [Block::Paragraph(_)]));
    /// assert!(parsed.diagnostics().is_empty());
    ///
    /// let mislabelled = converter.parse(Input::new(rtf, Format::Docx));
    /// assert!(matches!(
    ///     mislabelled,
    ///     Err(ConvertError::Unsupported {
    ///         kind: UnsupportedKind::FormatMismatch { detected: Format::Rtf, .. },
    ///         ..
    ///     })
    /// ));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn parse(&self, input: Input<'_>) -> Result<Parsed, ConvertError> {
        let bytes = input.bytes;
        let (document, diagnostics, fuel) = self.prepare(input, move |parse_ctx, format| {
            let document = parse_document(&parse_ctx, bytes, format)?;
            let (diagnostics, fuel) = report_of(parse_ctx, &document, self.config.images)?;
            Ok((document, diagnostics, fuel))
        })?;
        Ok(Parsed { document, diagnostics, fuel, config: Arc::clone(&self.config) })
    }

    /// Convert `input` to Markdown: [`Converter::parse`] then
    /// [`Parsed::render`] on one pool, and a PDF through its own extraction.
    ///
    /// # Errors
    ///
    /// [`ConvertError`] when no complete conversion was possible, and
    /// [`ConvertError::Incomplete`] when content was lost under
    /// [`OnLoss::Fail`].
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{Config, ConvertError, Converter, Format, Input};
    ///
    /// let converter = Converter::new(Config::default())?;
    ///
    /// let csv = b"name,year\nAda,1843\n";
    /// let conversion = converter.convert(Input::new(csv, Format::Csv))?;
    /// assert_eq!(conversion.markdown(), "| name | year |\n| --- | --- |\n| Ada | 1843 |\n");
    /// assert!(conversion.is_complete());
    ///
    /// let truncated = br"{\rtf1 a";
    /// let lossy = converter.convert(Input::new(truncated, Format::Rtf));
    /// assert!(matches!(lossy, Err(ConvertError::Incomplete { .. })));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn convert(&self, input: Input<'_>) -> Result<Conversion, ConvertError> {
        let bytes = input.bytes;
        let limits = &self.config.limits;
        self.prepare(input, move |parse_ctx, format| {
            let (markdown, units, diagnostics) = match format {
                #[cfg(feature = "pdf")]
                Format::Pdf => {
                    let markdown = formats::pdf::to_markdown(&parse_ctx, bytes)?;
                    if markdown.len() as u64 > limits.max_output_bytes {
                        return Err(ConvertError::limit(
                            Limit::OutputBytes,
                            limits.max_output_bytes,
                        ));
                    }
                    let (diagnostics, ..) = parse_ctx.into_report();
                    (markdown, Vec::new(), diagnostics)
                }
                format => {
                    let document = parse_document(&parse_ctx, bytes, format)?;
                    let meter = Meter::new(parse_ctx.ledger(), limits.max_output_bytes);
                    let rendered = render(&document, &meter, self.config.images)?;
                    (
                        rendered.markdown,
                        rendered.units,
                        diagnostics_of(parse_ctx, &document, self.config.images)?,
                    )
                }
            };
            conclude(self.config.on_loss, markdown, units, diagnostics)
        })
    }

    fn admit(&self, bytes: &[u8], cancel: Option<&CancelToken>) -> Result<(), ConvertError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(ConvertError::Cancelled);
        }
        let max = self.config.limits.max_input_bytes;
        if bytes.len() as u64 > max {
            return Err(ConvertError::limit(Limit::InputBytes, max));
        }
        Ok(())
    }

    fn prepare<T: Send>(
        &self,
        input: Input<'_>,
        work: impl FnOnce(ParseCtx<'_>, Format) -> Result<T, ConvertError> + Send,
    ) -> Result<T, ConvertError> {
        let Input { bytes, format, cancel } = input;
        self.admit(bytes, cancel.as_ref())?;
        self.on_pool(|| {
            let parse_ctx = self.parse_ctx(cancel.as_ref());
            let format = format.resolve(&parse_ctx, bytes)?;
            work(parse_ctx, format)
        })
    }

    fn parse_ctx<'a>(&'a self, cancel: Option<&'a CancelToken>) -> ParseCtx<'a> {
        let parse_ctx =
            ParseCtx::new(&self.config.limits).with_images(self.config.images).with_cancel(cancel);
        #[cfg(test)]
        let parse_ctx = parse_ctx.with_detection_counter(&self.detections);
        parse_ctx
    }

    fn on_pool<T: Send>(
        &self,
        work: impl FnOnce() -> Result<T, ConvertError> + Send,
    ) -> Result<T, ConvertError> {
        run_on_pool(&self.config, work)
    }
}

/// One document's bytes, the format they are parsed as, and the token that
/// may cancel its conversion.
#[derive(Clone)]
pub struct Input<'a> {
    bytes: &'a [u8],
    format: FormatSelection,
    cancel: Option<CancelToken>,
}

#[derive(Debug, Clone, Copy)]
enum FormatSelection {
    Explicit(Format),
    Detect { fallback: Option<Format> },
}

impl FormatSelection {
    fn resolve(self, parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Format, ConvertError> {
        match self {
            Self::Explicit(format) => {
                formats::detect::check_label(parse_ctx, bytes, format)?;
                Ok(format)
            }
            Self::Detect { fallback } => formats::detect::detect(parse_ctx, bytes)?
                .or(fallback)
                .ok_or_else(|| ConvertError::from(crate::UnsupportedKind::UnknownFormat)),
        }
    }
}

impl<'a> Input<'a> {
    /// `bytes` parsed as `format`, which [`Format::sniff`] or
    /// [`Format::from_extension`] may supply.
    #[must_use]
    pub fn new(bytes: &'a [u8], format: Format) -> Self {
        Self { bytes, format: FormatSelection::Explicit(format), cancel: None }
    }

    /// Detect the input's format under the conversion's own limits, using `fallback` only
    /// when its content names no format.
    ///
    /// ```
    /// use almiladoc::{Config, Converter, Input};
    /// let converter = Converter::new(Config::default())?;
    /// assert_eq!(converter.convert(Input::auto(br"{\rtf1 Hello}", None))?.markdown(), "Hello\n");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn auto(bytes: &'a [u8], fallback: Option<Format>) -> Self {
        Self { bytes, format: FormatSelection::Detect { fallback }, cancel: None }
    }

    /// This input, its conversion failing [`ConvertError::Cancelled`] once
    /// `token` is cancelled; a PDF is checked only before extraction starts.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{CancelToken, Config, ConvertError, Converter, Format, Input};
    ///
    /// let converter = Converter::new(Config::default())?;
    /// let token = CancelToken::new();
    /// let csv = b"name,year\nAda,1843\n";
    ///
    /// let input = Input::new(csv, Format::Csv).with_cancel(token.clone());
    /// assert!(converter.convert(input).is_ok());
    ///
    /// token.cancel();
    /// let input = Input::new(csv, Format::Csv).with_cancel(token);
    /// let error = converter.convert(input).unwrap_err();
    /// assert!(matches!(error, ConvertError::Cancelled));
    /// assert_eq!(error.code(), "cancelled");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn with_cancel(self, token: CancelToken) -> Self {
        Self { cancel: Some(token), ..self }
    }
}

impl fmt::Debug for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Input")
            .field("len", &self.bytes.len())
            .field("format", &self.format)
            .finish()
    }
}

/// A parsed document and its diagnostics, keeping the [`Config`] it was
/// parsed under and the work its parse charged for [`Parsed::render`].
#[derive(Debug, Clone)]
pub struct Parsed {
    document: Document,
    diagnostics: Diagnostics,
    fuel: u64,
    config: Arc<Config>,
}

impl Parsed {
    /// The document model, with the bytes of every embedded asset.
    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The document model, dropping the rest.
    #[must_use]
    pub fn into_document(self) -> Document {
        self.document
    }

    /// What the parse could not carry into the document, and what rendering
    /// it drops.
    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Render the document to Markdown on a pool built from its [`Config`],
    /// carrying the parse's diagnostics and charging its work on from the
    /// parse's, as [`Converter::convert`] does.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Incomplete`] when content was lost under
    /// [`OnLoss::Fail`]; [`ConvertError::ResourceLimit`] when the document
    /// nests deeper than [`Limits::max_nesting_depth`](crate::Limits::max_nesting_depth),
    /// its Markdown passes [`Limits::max_output_bytes`](crate::Limits::max_output_bytes),
    /// or the parse and the render together charge more than
    /// [`Limits::max_work`](crate::Limits::max_work);
    /// [`ConvertError::Internal`] when the pool cannot start or rendering
    /// panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{Config, ConvertError, Converter, Format, Input};
    ///
    /// let converter = Converter::new(Config::default())?;
    ///
    /// let csv = b"name,year\nAda,1843\n";
    /// let parsed = converter.parse(Input::new(csv, Format::Csv))?;
    /// assert_eq!(parsed.render()?.markdown(), "| name | year |\n| --- | --- |\n| Ada | 1843 |\n");
    ///
    /// let truncated = br"{\rtf1 a";
    /// let parsed = converter.parse(Input::new(truncated, Format::Rtf))?;
    /// assert!(parsed.diagnostics().has_loss());
    /// assert!(matches!(parsed.render(), Err(ConvertError::Incomplete { .. })));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn render(&self) -> Result<Conversion, ConvertError> {
        self.on_pool(|| {
            let ledger = self.resumed()?;
            let rendered = render(
                &self.document,
                &Meter::new(&ledger, self.config.limits.max_output_bytes),
                self.config.images,
            )?;
            conclude(
                self.config.on_loss,
                rendered.markdown,
                rendered.units,
                self.diagnostics.clone(),
            )
        })
    }

    /// The document and its diagnostics as one JSON object,
    /// `{schema_version, identity, document, diagnostics}`, written on a pool
    /// built from its [`Config`] and charging its work on from the parse's.
    ///
    /// A data enum is an object whose `kind` names the variant and whose
    /// `value` holds the variant's data, absent when it has none; a field-less
    /// enum is a string, `None` is `null`, and an asset carries `byte_len` in
    /// place of its bytes. A loss does not fail it: the diagnostics say what
    /// the document lacks. Each level of
    /// [`Limits::max_nesting_depth`](crate::Limits::max_nesting_depth) nests
    /// the JSON up to seven levels deeper (a table in a table's cell), five
    /// for a list in a list, so the default limit already admits JSON deeper
    /// than the 1000 levels or so that `json.loads` accepts in Python 3.11.
    ///
    /// # Errors
    ///
    /// [`ConvertError::ResourceLimit`] when the document nests deeper than
    /// [`Limits::max_nesting_depth`](crate::Limits::max_nesting_depth),
    /// its JSON passes [`Limits::max_output_bytes`](crate::Limits::max_output_bytes),
    /// or the parse and the serialisation together charge more than
    /// [`Limits::max_work`](crate::Limits::max_work);
    /// [`ConvertError::Internal`] when the pool cannot start or serialisation
    /// panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{Config, Converter, Format, Input, JSON_SCHEMA_VERSION};
    ///
    /// let converter = Converter::new(Config::default())?;
    ///
    /// let truncated = br"{\rtf1 a";
    /// let json = converter.parse(Input::new(truncated, Format::Rtf))?.to_json()?;
    /// assert!(json.starts_with(&format!(r#"{{"schema_version":{JSON_SCHEMA_VERSION},"#)));
    /// assert!(json.ends_with(
    ///     r#""diagnostics":[{"code":"unclosed_structure","class":"lost","region":"body","unit":null,"count":1}]}"#
    /// ));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[cfg(feature = "serde")]
    pub fn to_json(&self) -> Result<String, ConvertError> {
        self.on_pool(|| {
            let ledger = self.resumed()?;
            let meter = Meter::new(&ledger, self.config.limits.max_output_bytes);
            crate::json::write(
                &EngineIdentity::of(&self.config),
                &self.document,
                &self.diagnostics,
                &meter,
            )
        })
    }

    fn resumed(&self) -> Result<Ledger<'static>, ConvertError> {
        let limits = &self.config.limits;
        check_nesting(&self.document, limits.max_nesting_depth)?;
        Ok(Ledger::new(limits.max_work, None).starting_at(self.fuel))
    }

    fn on_pool<T: Send>(
        &self,
        work: impl FnOnce() -> Result<T, ConvertError> + Send,
    ) -> Result<T, ConvertError> {
        run_on_pool(&self.config, work)
    }
}

fn parse_document(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    format: Format,
) -> Result<Document, ConvertError> {
    let document = formats::parse(parse_ctx, bytes, format)?;
    check_nesting(&document, parse_ctx.limits().max_nesting_depth)?;
    Ok(document)
}

/// What the parse reported, and what rendering `document` loses, read on the parse's ledger.
pub(crate) fn diagnostics_of(
    parse_ctx: ParseCtx<'_>,
    document: &Document,
    images: crate::ImagePolicy,
) -> Result<Diagnostics, ConvertError> {
    report_of(parse_ctx, document, images).map(|(diagnostics, _)| diagnostics)
}

/// [`diagnostics_of`] and the work the parse's ledger has spent once the loss walk ends.
fn report_of(
    parse_ctx: ParseCtx<'_>,
    document: &Document,
    images: crate::ImagePolicy,
) -> Result<(Diagnostics, u64), ConvertError> {
    let (mut diagnostics, targets, ledger) = parse_ctx.into_report();
    let unread = Unread::of(&diagnostics).with_targets(&targets);
    let meter = Meter::new(&ledger, u64::MAX);
    for (code, region, unit, count) in render_losses(document, unread, images, &meter)? {
        diagnostics.add(code, region, unit.map(|u| u.ordinal), count);
    }
    Ok((diagnostics, ledger.spent()))
}

fn conclude(
    on_loss: OnLoss,
    markdown: String,
    units: Vec<UnitSpan>,
    diagnostics: Diagnostics,
) -> Result<Conversion, ConvertError> {
    match on_loss {
        OnLoss::Fail if diagnostics.has_loss() => Err(ConvertError::Incomplete { diagnostics }),
        _ => Ok(Conversion { markdown, units, diagnostics }),
    }
}

/// The result of a conversion: its Markdown and its diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct Conversion {
    markdown: String,
    units: Vec<UnitSpan>,
    diagnostics: Diagnostics,
}

/// A source unit's UTF-8 byte range in the converted Markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct UnitSpan {
    /// The original source unit.
    pub unit: Unit,
    /// Its content range, excluding unit anchors, separators, boundary whitespace, the final
    /// newline and appended note definitions.
    pub bytes: Range<usize>,
}

impl Conversion {
    /// Source units in order, including empty units as zero-length ranges.
    ///
    /// ```
    /// use almiladoc::{Config, Converter, Format, Input};
    /// let result = Converter::new(Config::default())?.convert(Input::new(b"a,b\n", Format::Csv))?;
    /// let span = &result.units()[0];
    /// assert!(result.markdown()[span.bytes.clone()].contains("a"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn units(&self) -> &[UnitSpan] {
        &self.units
    }

    /// What the conversion could not carry into its output.
    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// True when no content was [`Lost`](crate::LossClass::Lost); only
    /// [`OnLoss::Report`] returns a conversion for which it is false.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.diagnostics.has_loss()
    }

    /// The GitHub-Flavored Markdown.
    #[must_use]
    pub fn markdown(&self) -> &str {
        &self.markdown
    }

    /// The GitHub-Flavored Markdown, owned.
    #[must_use]
    pub fn into_markdown(self) -> String {
        self.markdown
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "the tests read fixtures and run conversions on threads of their own"
)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticCode, Region};
    use crate::model::{Block, Inline};
    use crate::shared::test_support::zip_package;
    use std::assert_matches;
    use std::sync::atomic::Ordering::Relaxed;

    const CSV: &[u8] = b"a,b\n1,2\n";

    fn converter(threads: usize) -> Converter {
        Converter::new(Config { threads, ..Config::default() }).unwrap()
    }

    #[test]
    fn new_refuses_an_invalid_config() {
        let refused = Converter::new(Config { threads: 0, ..Config::default() });
        assert_eq!(refused.unwrap_err(), ConfigError::Threads { max: Config::MAX_THREADS });
    }

    #[test]
    fn every_call_runs_on_a_pool_of_its_configured_threads() {
        for count in [1, 3] {
            let converter = converter(count);
            assert_eq!(converter.on_pool(|| Ok(rayon::current_num_threads())).ok(), Some(count));
            let parsed = converter.parse(Input::new(CSV, Format::Csv)).unwrap();
            drop(converter);
            assert_eq!(parsed.on_pool(|| Ok(rayon::current_num_threads())).ok(), Some(count));
        }
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn docx(body: &str, parts: &[(&str, &str)]) -> Vec<u8> {
        let document = format!("<w:document {W}><w:body>{body}</w:body></w:document>");
        zip_package(&[&[("word/document.xml", document.as_str())], parts].concat())
    }

    fn unreadable_notes_docx() -> Vec<u8> {
        let notes = format!("<w:footnotes {W}><w:footnote w:id=\"2\">");
        docx("<w:p><w:r><w:t>kept</w:t></w:r></w:p>", &[("word/footnotes.xml", &notes)])
    }

    fn quoted(levels: usize, text: &str) -> Vec<Block> {
        (0..levels).fold(vec![Block::Paragraph(vec![Inline::plain(text)])], |blocks, _| {
            vec![Block::BlockQuote(blocks)]
        })
    }

    fn parsed(blocks: Vec<Block>, config: Config) -> Parsed {
        Parsed {
            document: Document { blocks, ..Document::default() },
            diagnostics: Diagnostics::default(),
            fuel: 0,
            config: Arc::new(config),
        }
    }

    #[test]
    fn parse_and_convert_report_what_rendering_loses_alike() {
        let body = concat!(
            r#"<w:p><w:hyperlink w:anchor="nowhere"><w:r><w:t>label</w:t></w:r></w:hyperlink>"#,
            r#"<w:r><w:footnoteReference w:id="7"/></w:r></w:p>"#,
        );
        let rels = |part: &str| {
            format!(
                concat!(
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
                    r#"<Relationship Id="rId1" Target="{0}.xml" "#,
                    r#"Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{0}"/>"#,
                    "</Relationships>",
                ),
                part
            )
        };
        let (footnotes, numbering) = (rels("footnotes"), rels("numbering"));
        let cases: [(&[(&str, &str)], &[_]); 3] = [
            (&[], &[(DiagnosticCode::DanglingReference, Region::Body, None, 2)]),
            (
                &[("word/_rels/document.xml.rels", &footnotes)],
                &[
                    (DiagnosticCode::PartUnreadable, Region::Note, None, 1),
                    (DiagnosticCode::DanglingReference, Region::Body, None, 1),
                ],
            ),
            (
                &[("word/_rels/document.xml.rels", &numbering)],
                &[
                    (DiagnosticCode::PartUnreadable, Region::Body, None, 1),
                    (DiagnosticCode::DanglingReference, Region::Body, None, 2),
                ],
            ),
        ];
        for (parts, expected) in cases {
            let bytes = docx(body, parts);
            let converted = reporting().convert(Input::new(&bytes, Format::Docx)).unwrap();
            let parsed = reporting().parse(Input::new(&bytes, Format::Docx)).unwrap();
            assert_eq!(converted.markdown(), "label\n");
            assert_eq!(parsed.render().unwrap(), converted);
            let reported: Vec<_> = converted
                .diagnostics()
                .iter()
                .map(|d| (d.code, d.region, d.unit, d.count))
                .collect();
            assert_eq!(reported, expected, "{parts:?}");
        }
    }

    fn reporting() -> Converter {
        Converter::new(Config { on_loss: OnLoss::Report, ..Config::default() }).unwrap()
    }

    #[test]
    fn a_reported_loss_converts_equally_through_parse_then_render() {
        let bytes = unreadable_notes_docx();
        let converted = reporting().convert(Input::new(&bytes, Format::Docx)).unwrap();
        let parsed = reporting().parse(Input::new(&bytes, Format::Docx)).unwrap();
        assert_eq!(parsed.render().unwrap(), converted);
        assert_eq!(converted.markdown(), "kept\n");
        assert!(!converted.is_complete());
        let codes: Vec<_> = converted.diagnostics().iter().map(|d| d.code).collect();
        assert_eq!(codes, [DiagnosticCode::PartUnreadable]);

        let failing = converter(1);
        let parsed = failing.parse(Input::new(&bytes, Format::Docx)).unwrap();
        assert_eq!(parsed.diagnostics(), converted.diagnostics());
        for result in [failing.convert(Input::new(&bytes, Format::Docx)), parsed.render()] {
            match result {
                Err(ConvertError::Incomplete { diagnostics }) => {
                    assert_eq!(&diagnostics, converted.diagnostics());
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn only_lost_content_fails_and_only_under_fail() {
        for &code in &[
            DiagnosticCode::PartUnreadable,
            DiagnosticCode::HeaderOmitted,
            DiagnosticCode::StylePartUnreadable,
        ] {
            let mut diagnostics = Diagnostics::default();
            diagnostics.add(code, Region::Body, None, 1);
            let lost = code.class() == crate::diagnostics::LossClass::Lost;
            let reported =
                conclude(OnLoss::Report, "md".to_string(), vec![], diagnostics.clone()).unwrap();
            assert_eq!(reported.is_complete(), !lost, "{code}");
            assert_eq!(reported.diagnostics(), &diagnostics);
            match conclude(OnLoss::Fail, "md".to_string(), vec![], diagnostics.clone()) {
                Err(ConvertError::Incomplete { diagnostics: raised }) => {
                    assert!(lost, "{code}");
                    assert_eq!(raised, diagnostics);
                }
                Ok(conversion) => {
                    assert!(!lost, "{code}");
                    assert_eq!(conversion, reported);
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        assert!(
            conclude(OnLoss::Fail, String::new(), vec![], Diagnostics::default())
                .unwrap()
                .is_complete()
        );
    }

    #[test]
    fn parse_reads_its_limits_from_the_config() {
        let bytes = nested_tables(4);
        let mut config = Config::default();
        config.limits.max_nesting_depth = 8;
        let error = Converter::new(config).unwrap().parse(Input::new(&bytes, Format::Docx));
        assert_matches!(
            error,
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 8 })
        );
        converter(1).parse(Input::new(&bytes, Format::Docx)).unwrap();
    }

    #[test]
    fn render_reads_the_nesting_limit_from_the_config() {
        let nested = |max| {
            let mut config = Config::default();
            config.limits.max_nesting_depth = max;
            parsed(quoted(5, "x"), config)
        };
        nested(5).render().unwrap();
        let error = nested(4).render();
        assert_matches!(
            error,
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 4 })
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn to_json_refuses_a_document_nested_past_the_limit_before_writing() {
        let nested = |max| {
            let mut config = Config::default();
            config.limits.max_nesting_depth = max;
            config.limits.max_output_bytes = 1;
            parsed(quoted(5, "x"), config)
        };
        assert_matches!(
            nested(4).to_json(),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 4 })
        );
        assert_matches!(
            nested(5).to_json(),
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, max: 1 })
        );
    }

    #[test]
    fn the_identity_is_the_configs() {
        assert_eq!(converter(3).identity(), &EngineIdentity::of(&Config::default()));
        assert_eq!(
            reporting().identity(),
            &EngineIdentity::of(&Config { on_loss: OnLoss::Report, ..Config::default() })
        );
    }

    #[test]
    fn convert_equals_parse_then_render() {
        let converter = converter(1);
        let converted = converter.convert(Input::new(CSV, Format::Csv)).unwrap();
        let parsed = converter.parse(Input::new(CSV, Format::Csv)).unwrap();
        assert_eq!(parsed.render().unwrap(), converted);
        assert_eq!(converted.markdown(), "| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        assert_eq!(converted.clone().into_markdown(), converted.markdown());
        assert_eq!(parsed.document().blocks.len(), 2);
        assert_eq!(parsed.into_document().blocks.len(), 2);
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn a_pdf_reports_into_the_calls_context() {
        let sentence = "The board approved the plan for the coming season after a long review.";
        let bytes = formats::pdf::tests::composite_font_pdf(sentence, &[0x0200]);
        let expected = [(
            DiagnosticCode::GarbledFontEncoding,
            crate::diagnostics::LossClass::Lost,
            Region::Body,
        )];
        let reported = |diagnostics: &Diagnostics| -> Vec<_> {
            diagnostics.iter().map(|d| (d.code, d.class, d.region)).collect()
        };
        match converter(1).convert(Input::new(&bytes, Format::Pdf)) {
            Err(ConvertError::Incomplete { diagnostics }) => {
                assert_eq!(reported(&diagnostics), expected);
            }
            other => panic!("{other:?}"),
        }
        let conversion = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert!(!conversion.is_complete());
        assert!(conversion.markdown().starts_with(sentence), "{}", conversion.markdown());
        assert_eq!(reported(conversion.diagnostics()), expected);
    }

    #[test]
    fn errors_pass_through() {
        let error = converter(1).convert(Input::new(b"not a package", Format::Docx)).unwrap_err();
        assert_matches!(error, ConvertError::Malformed { .. });
        let error = converter(1).parse(Input::new(b"not a package", Format::Docx)).unwrap_err();
        assert_matches!(error, ConvertError::Malformed { .. });
    }

    fn on_a_small_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
        std::thread::scope(|scope| {
            let caller = std::thread::Builder::new().stack_size(64 * 1024);
            caller.spawn_scoped(scope, work).unwrap().join().unwrap()
        })
    }

    fn nested_tables(depth: usize) -> Vec<u8> {
        let open = "<w:tbl><w:tr><w:tc>".repeat(depth);
        let close = "<w:p/></w:tc></w:tr></w:tbl>".repeat(depth);
        docx(&format!("{open}<w:p/>{close}"), &[])
    }

    #[test]
    fn parse_and_convert_recurse_on_the_pool_not_the_callers_stack() {
        let bytes = nested_tables(60);
        let converter = converter(1);
        on_a_small_stack(|| {
            converter.parse(Input::new(&bytes, Format::Docx)).unwrap();
            converter.convert(Input::new(&bytes, Format::Docx)).unwrap();
        });
    }

    #[test]
    fn render_by_hand_recurses_on_the_pool_not_the_callers_stack() {
        let parsed = parsed(quoted(200, "deep"), Config::default());
        let rendered = on_a_small_stack(|| parsed.render());
        assert!(rendered.is_ok_and(|conversion| conversion.markdown().starts_with('>')));
        #[cfg(feature = "serde")]
        on_a_small_stack(|| parsed.to_json()).unwrap();
    }

    #[test]
    fn debug_shows_no_input_bytes() {
        let shown = format!("{:?}", Input::new(b"secret", Format::Csv));
        assert_eq!(shown, "Input { len: 6, format: Explicit(Csv) }");
    }

    fn corpus(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                corpus(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    #[test]
    fn equal_input_charges_equal_fuel() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut files = Vec::new();
        corpus(&root, &mut files);
        let mut charged = 0;
        for path in files.iter().filter(|path| !path.starts_with(root.join("abuse"))) {
            let bytes = std::fs::read(path).unwrap();
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let format = Format::sniff(&bytes).or_else(|| Format::from_extension(extension));
            let Some(format) = format.filter(|format| *format != Format::Pdf) else {
                continue;
            };
            let fuel = || {
                let parse_ctx = ParseCtx::default();
                let parsed = formats::parse(&parse_ctx, &bytes, format).is_ok();
                (parsed, parse_ctx.fuel())
            };
            let first = fuel();
            assert_eq!(first, fuel(), "{}", path.display());
            charged += usize::from(first.1 > 0);
        }
        assert!(charged > 40, "{charged} fixtures charged");
    }

    #[test]
    fn the_parse_context_charges_against_the_inputs_token() {
        let converter = converter(1);
        let token = CancelToken::new();
        let input = Input::new(CSV, Format::Csv).with_cancel(token.clone());
        converter
            .prepare(input, |ctx, _| {
                ctx.charge(1).unwrap();
                token.cancel();
                assert_matches!(ctx.charge(1), Err(ConvertError::Cancelled));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_token_cancelled_from_another_thread_stops_a_running_parse() {
        let flood = [b"{\\rtf1 ".as_slice(), &b"{}".repeat(8 * 1024 * 1024)].concat();
        let limits = crate::Limits { max_work: u64::MAX, ..crate::Limits::default() };
        let token = CancelToken::new();
        let result = std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                let parse_ctx = ParseCtx::new(&limits).with_cancel(Some(&token));
                formats::parse(&parse_ctx, &flood, Format::Rtf).map(drop)
            });
            token.cancel();
            running.join().unwrap()
        });
        assert_matches!(result, Err(ConvertError::Cancelled));
    }

    fn unidentified_package() -> Vec<u8> {
        let rels = format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>"#,
            "<!-- padding -->".repeat(128)
        );
        zip_package(&[("_rels/.rels", rels)])
    }

    #[test]
    fn sniff_detects_under_the_converters_limits() {
        let bytes = unidentified_package();
        assert_eq!(Format::sniff(&bytes), None);
        assert_matches!(converter(1).sniff(&bytes, None), Ok(None));
        let mut config = Config::default();
        config.limits.max_work = 1;
        let starved = Converter::new(config).unwrap();
        assert_matches!(
            starved.sniff(&bytes, None),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 1 })
        );
        let docx = docx("<w:p/>", &[]);
        assert_matches!(converter(1).sniff(&docx, None), Ok(Some(Format::Docx)));
    }

    #[test]
    fn sniff_admits_before_it_detects() {
        let token = CancelToken::new();
        token.cancel();
        let mut config = Config::default();
        config.limits.max_input_bytes = 1;
        let small = Converter::new(config).unwrap();
        assert_matches!(small.sniff(b"", Some(&token)), Err(ConvertError::Cancelled));
        assert_matches!(small.sniff(CSV, Some(&token)), Err(ConvertError::Cancelled));
        assert_matches!(
            small.sniff(CSV, None),
            Err(ConvertError::ResourceLimit { limit: Limit::InputBytes, max: 1 })
        );
        assert_matches!(small.sniff(b"a", None), Ok(None));
    }

    #[test]
    fn a_pre_cancelled_input_is_refused_before_its_size() {
        let token = CancelToken::new();
        token.cancel();
        let mut config = Config::default();
        config.limits.max_input_bytes = 1;
        let converter = Converter::new(config).unwrap();
        let input = Input::new(CSV, Format::Csv).with_cancel(token);
        assert_matches!(converter.convert(input), Err(ConvertError::Cancelled));
    }
    #[test]
    fn each_input_resolves_once_on_the_parse_context() {
        for input in
            [Input::auto(br"{\rtf1 hello}", None), Input::new(br"{\rtf1 hello}", Format::Rtf)]
        {
            let converter = converter(1);
            converter
                .prepare(input, |ctx, format| {
                    assert_eq!(format, Format::Rtf);
                    let document = parse_document(&ctx, br"{\rtf1 hello}", format)?;
                    let rendered = render(
                        &document,
                        &Meter::new(ctx.ledger(), u64::MAX),
                        crate::ImagePolicy::Placeholder,
                    )?;
                    assert_eq!(rendered.markdown, "hello\n");
                    Ok(())
                })
                .unwrap();
            assert_eq!(converter.detections.load(Relaxed), 1);
        }
    }

    #[test]
    fn exact_unread_targets_survive_diagnostic_folding_without_hiding_other_targets() {
        use crate::model::{LinkTarget, Unit, UnitKind};
        use std::num::NonZeroU32;
        let ctx = ParseCtx::default();
        for position in 0..65 {
            let _scope = ctx.begin_unit(UnitKind::Slide, position).unwrap();
            ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
        }
        let target = |kind, ordinal| Inline::Link {
            content: vec![Inline::plain("target")],
            target: LinkTarget::Unit(Unit::new(kind, NonZeroU32::new(ordinal).unwrap())),
        };
        let document = Document {
            blocks: vec![Block::Paragraph(vec![
                target(UnitKind::Slide, 65),
                target(UnitKind::Slide, 99),
                target(UnitKind::Sheet, 65),
                Inline::Link {
                    content: vec![Inline::plain("independent")],
                    target: LinkTarget::anchor("missing".into()),
                },
            ])],
            ..Document::default()
        };
        let report = diagnostics_of(ctx, &document, crate::ImagePolicy::Placeholder).unwrap();
        assert_eq!(
            report
                .iter()
                .filter(|d| d.code == DiagnosticCode::UnitUnreadable)
                .map(|d| d.count)
                .sum::<u64>(),
            65
        );
        assert_eq!(
            report
                .iter()
                .filter(|d| d.code == DiagnosticCode::DanglingReference)
                .map(|d| d.count)
                .sum::<u64>(),
            3
        );
    }

    #[test]
    fn detection_parse_and_render_share_their_byte_and_work_budgets() {
        let content = format!(
            "<w:document {W}><w:body><w:p><w:r><w:t>{}</w:t></w:r></w:p></w:body></w:document>",
            "x".repeat(1024)
        );
        let bytes = zip_package(&[("word/document.xml", content.as_str())]);
        let detecting = ParseCtx::default();
        assert_eq!(formats::detect::detect(&detecting, &bytes).unwrap(), Some(Format::Docx));
        let frontend = ParseCtx::default();
        formats::parse(&frontend, &bytes, Format::Docx).unwrap();
        let converter = converter(1);
        let (total, work, length) = converter
            .prepare(Input::auto(&bytes, None), |ctx, format| {
                let document = parse_document(&ctx, &bytes, format)?;
                let rendered = render(
                    &document,
                    &Meter::new(ctx.ledger(), u64::MAX),
                    crate::ImagePolicy::Placeholder,
                )?;
                Ok((ctx.archive_bytes(), ctx.fuel(), rendered.markdown.len() as u64))
            })
            .unwrap();
        assert_eq!(converter.detections.load(Relaxed), 1);
        assert!(detecting.archive_bytes() > 0 && frontend.archive_bytes() > 0);
        assert_eq!(total, detecting.archive_bytes() + frontend.archive_bytes());
        let run = 3;
        assert_eq!(work, detecting.fuel() + frontend.fuel() + run + length / 64);
        for (limit, max) in [
            (Limit::TotalBytes, total - 1),
            (Limit::Work, work - 1),
            (Limit::OutputBytes, length - 1),
        ] {
            let mut config = Config::default();
            match limit {
                Limit::TotalBytes => config.limits.max_total_bytes = max,
                Limit::Work => config.limits.max_work = max,
                Limit::OutputBytes => config.limits.max_output_bytes = max,
                _ => panic!("unexpected limit"),
            }
            let converter = Converter::new(config).unwrap();
            for input in [Input::auto(&bytes, None), Input::new(&bytes, Format::Docx)] {
                assert_matches!(converter.convert(input), Err(ConvertError::ResourceLimit { limit: actual, max: actual_max }) if actual == limit && actual_max == max);
            }
        }
    }
    #[test]
    fn public_calls_detect_once_and_rendering_a_parsed_document_never_detects() {
        let converter = converter(1);
        let bytes = br"{\rtf1 hello}";
        converter.convert(Input::auto(bytes, None)).unwrap();
        assert_eq!(converter.detections.load(Relaxed), 1);
        converter.convert(Input::new(bytes, Format::Rtf)).unwrap();
        assert_eq!(converter.detections.load(Relaxed), 2);
        let parsed = converter.parse(Input::auto(bytes, None)).unwrap();
        assert_eq!(converter.detections.load(Relaxed), 3);
        parsed.render().unwrap();
        assert_eq!(converter.detections.load(Relaxed), 3);
        assert_eq!(converter.sniff(bytes, None).unwrap(), Some(Format::Rtf));
        assert_eq!(converter.detections.load(Relaxed), 4);
    }
}
