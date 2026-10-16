//! The conversion API: a [`Converter`] built once from a [`Config`], and the
//! calls that walk one [`Input`] on its per-call pool.

use std::fmt;
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
use crate::model::Document;
use crate::pool::run_on_pool;
use crate::render::markdown::{Meter, Unread, check_nesting, render, render_losses};

/// Converts documents under one validated [`Config`]; share it across threads
/// and calls.
#[derive(Debug, Clone)]
pub struct Converter {
    config: Arc<Config>,
    identity: Arc<EngineIdentity>,
}

impl Converter {
    /// A converter for `config`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a limit or the thread count is out of range.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        let identity = Arc::new(EngineIdentity::of(&config));
        Ok(Converter { config: Arc::new(config), identity })
    }

    /// The version and configuration fingerprint that fix this converter's
    /// output.
    #[must_use]
    pub fn identity(&self) -> &EngineIdentity {
        &self.identity
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
    pub fn parse(&self, input: Input<'_>) -> Result<Parsed, ConvertError> {
        self.admit(&input)?;
        let (document, diagnostics, fuel) = self.on_pool(|| {
            let parse_ctx = self.parse_ctx(&input);
            let document = parse_document(&parse_ctx, input.bytes, input.format)?;
            let fuel = parse_ctx.fuel();
            let diagnostics = diagnostics_of(parse_ctx, &document);
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
    pub fn convert(&self, input: Input<'_>) -> Result<Conversion, ConvertError> {
        self.admit(&input)?;
        let limits = &self.config.limits;
        self.on_pool(|| {
            let parse_ctx = self.parse_ctx(&input);
            let (markdown, diagnostics) = match input.format {
                #[cfg(feature = "pdf")]
                Format::Pdf => {
                    let markdown = formats::pdf::to_markdown(&parse_ctx, input.bytes)?;
                    if markdown.len() as u64 > limits.max_output_bytes {
                        return Err(ConvertError::limit(
                            Limit::OutputBytes,
                            limits.max_output_bytes,
                        ));
                    }
                    (markdown, parse_ctx.into_diagnostics())
                }
                format => {
                    let document = parse_document(&parse_ctx, input.bytes, format)?;
                    let meter = Meter::new(parse_ctx.ledger(), limits.max_output_bytes);
                    let markdown = render(&document, &meter)?;
                    (markdown, diagnostics_of(parse_ctx, &document))
                }
            };
            conclude(self.config.on_loss, markdown, diagnostics)
        })
    }

    fn admit(&self, input: &Input<'_>) -> Result<(), ConvertError> {
        if input.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(ConvertError::Cancelled);
        }
        let max = self.config.limits.max_input_bytes;
        if input.bytes.len() as u64 > max {
            return Err(ConvertError::limit(Limit::InputBytes, max));
        }
        Ok(())
    }

    fn parse_ctx<'c>(&'c self, input: &'c Input<'_>) -> ParseCtx<'c> {
        ParseCtx::new(&self.config.limits).with_cancel(input.cancel.as_ref())
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
    format: Format,
    cancel: Option<CancelToken>,
}

impl<'a> Input<'a> {
    /// `bytes` parsed as `format`, which [`Format::sniff`] or
    /// [`Format::from_extension`] may supply.
    #[must_use]
    pub fn new(bytes: &'a [u8], format: Format) -> Self {
        Input { bytes, format, cancel: None }
    }

    /// This input, its conversion failing [`ConvertError::Cancelled`] once
    /// `token` is cancelled; a PDF is checked only before extraction starts.
    #[must_use]
    pub fn with_cancel(self, token: CancelToken) -> Self {
        Input { cancel: Some(token), ..self }
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
    pub fn render(&self) -> Result<Conversion, ConvertError> {
        let limits = &self.config.limits;
        self.on_pool(|| {
            check_nesting(&self.document, limits.max_nesting_depth)?;
            let ledger = Ledger::new(limits.max_work, None).starting_at(self.fuel);
            let markdown = render(&self.document, &Meter::new(&ledger, limits.max_output_bytes))?;
            conclude(self.config.on_loss, markdown, self.diagnostics.clone())
        })
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

/// What the parse reported, and what rendering `document` loses.
fn diagnostics_of(parse_ctx: ParseCtx<'_>, document: &Document) -> Diagnostics {
    let mut diagnostics = parse_ctx.into_diagnostics();
    for (code, region, count) in render_losses(document, Unread::of(&diagnostics)) {
        diagnostics.add(code, region, None, count);
    }
    diagnostics
}

fn conclude(
    on_loss: OnLoss,
    markdown: String,
    diagnostics: Diagnostics,
) -> Result<Conversion, ConvertError> {
    match on_loss {
        OnLoss::Fail if diagnostics.has_loss() => Err(ConvertError::Incomplete { diagnostics }),
        _ => Ok(Conversion { markdown, diagnostics }),
    }
}

/// The result of a conversion: its Markdown and its diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Conversion {
    markdown: String,
    diagnostics: Diagnostics,
}

impl Conversion {
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
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticCode, Region};

    const CSV: &[u8] = b"a,b\n1,2\n";

    fn converter(threads: usize) -> Converter {
        Converter::new(Config { threads, ..Config::default() }).unwrap()
    }

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<Converter>();
    const _: () = assert_send_sync::<Parsed>();

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

    fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, text) in parts {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn recovered_docx() -> Vec<u8> {
        let document = format!("<w:document {W}><w:body><w:p><w:r><w:t>kept</w:t></w:r></w:p>");
        zip_of(&[("word/document.xml", &document)])
    }

    fn docx(body: &str, parts: &[(&str, &str)]) -> Vec<u8> {
        let document = format!("<w:document {W}><w:body>{body}</w:body></w:document>");
        zip_of(&[&[("word/document.xml", document.as_str())], parts].concat())
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
        let bytes = recovered_docx();
        let converted = reporting().convert(Input::new(&bytes, Format::Docx)).unwrap();
        let parsed = reporting().parse(Input::new(&bytes, Format::Docx)).unwrap();
        assert_eq!(parsed.render().unwrap(), converted);
        assert_eq!(converted.markdown(), "kept\n");
        assert!(!converted.is_complete());
        let codes: Vec<_> = converted.diagnostics().iter().map(|d| d.code).collect();
        assert_eq!(codes, [DiagnosticCode::RecoveredXml]);

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
            DiagnosticCode::OcrGap,
            DiagnosticCode::StylePartUnreadable,
        ] {
            let mut diagnostics = Diagnostics::default();
            diagnostics.add(code, Region::Body, None, 1);
            let lost = code.class() == crate::diagnostics::LossClass::Lost;
            let reported = conclude(OnLoss::Report, "md".to_string(), diagnostics.clone()).unwrap();
            assert_eq!(reported.is_complete(), !lost, "{code}");
            assert_eq!(reported.diagnostics(), &diagnostics);
            match conclude(OnLoss::Fail, "md".to_string(), diagnostics.clone()) {
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
            conclude(OnLoss::Fail, String::new(), Diagnostics::default()).unwrap().is_complete()
        );
    }

    #[test]
    fn parse_reads_its_limits_from_the_config() {
        let bytes = nested_tables(4);
        let mut config = Config::default();
        config.limits.max_nesting_depth = 8;
        let error = Converter::new(config).unwrap().parse(Input::new(&bytes, Format::Docx));
        assert!(
            matches!(
                error,
                Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 8 })
            ),
            "{error:?}"
        );
        assert!(converter(1).parse(Input::new(&bytes, Format::Docx)).is_ok());
    }

    #[test]
    fn render_reads_the_nesting_limit_from_the_config() {
        let mut blocks =
            vec![crate::model::Block::Paragraph(vec![crate::model::Inline::plain("x")])];
        for _ in 0..5 {
            blocks = vec![crate::model::Block::BlockQuote(blocks)];
        }
        let parsed = |max| {
            let mut config = Config::default();
            config.limits.max_nesting_depth = max;
            let document = Document { blocks: blocks.clone(), ..Document::default() };
            Parsed {
                document,
                diagnostics: Diagnostics::default(),
                fuel: 0,
                config: Arc::new(config),
            }
        };
        assert!(parsed(5).render().is_ok());
        let error = parsed(4).render();
        assert!(
            matches!(
                error,
                Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 4 })
            ),
            "{error:?}"
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
        assert_eq!(parsed.document().blocks.len(), 1);
        assert_eq!(parsed.into_document().blocks.len(), 1);
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn a_pdf_reports_into_the_calls_context() {
        let sentence = "The board approved the plan for the coming season after a long review.";
        let bytes = crate::formats::pdf::tests::composite_font_pdf(sentence, &[0x0200]);
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
        assert!(matches!(error, ConvertError::Malformed { .. }), "{error:?}");
        let error = converter(1).parse(Input::new(b"not a package", Format::Docx)).unwrap_err();
        assert!(matches!(error, ConvertError::Malformed { .. }), "{error:?}");
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
        let w = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
        let document = format!("<w:document {w}><w:body>{open}<w:p/>{close}</w:body></w:document>");
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zip, document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_and_convert_recurse_on_the_pool_not_the_callers_stack() {
        let bytes = nested_tables(60);
        let converter = converter(1);
        on_a_small_stack(|| {
            assert!(converter.parse(Input::new(&bytes, Format::Docx)).is_ok());
            assert!(converter.convert(Input::new(&bytes, Format::Docx)).is_ok());
        });
    }

    #[test]
    fn render_by_hand_recurses_on_the_pool_not_the_callers_stack() {
        let mut blocks =
            vec![crate::model::Block::Paragraph(vec![crate::model::Inline::plain("deep")])];
        for _ in 0..200 {
            blocks = vec![crate::model::Block::BlockQuote(blocks)];
        }
        let document = Document { blocks, notes: Vec::new(), assets: Vec::new() };
        let parsed = Parsed {
            document,
            diagnostics: Diagnostics::default(),
            fuel: 0,
            config: Arc::new(Config::default()),
        };
        let rendered = on_a_small_stack(|| parsed.render());
        assert!(rendered.is_ok_and(|conversion| conversion.markdown().starts_with(">")));
    }

    #[test]
    fn debug_shows_no_input_bytes() {
        let shown = format!("{:?}", Input::new(b"secret", Format::Csv));
        assert_eq!(shown, "Input { len: 6, format: Csv }");
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
        let parse_ctx = converter.parse_ctx(&input);
        assert!(parse_ctx.charge(1).is_ok());
        token.cancel();
        assert!(matches!(parse_ctx.charge(1), Err(ConvertError::Cancelled)));
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
        assert!(matches!(result, Err(ConvertError::Cancelled)), "{result:?}");
    }

    #[test]
    fn a_pre_cancelled_input_is_refused_before_its_size() {
        let token = CancelToken::new();
        token.cancel();
        let mut config = Config::default();
        config.limits.max_input_bytes = 1;
        let converter = Converter::new(config).unwrap();
        let input = Input::new(CSV, Format::Csv).with_cancel(token);
        assert!(matches!(converter.convert(input), Err(ConvertError::Cancelled)));
    }
}
