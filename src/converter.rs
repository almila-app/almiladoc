//! The conversion API: a [`Converter`] built once from a [`Config`], and the
//! calls that walk one [`Input`] on its per-call pool.

use std::fmt;
use std::sync::Arc;

use crate::Format;
use crate::config::{Config, ConfigError};
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::formats;
use crate::ledger::CancelToken;
use crate::limits::Limit;
use crate::model::Document;
use crate::pool::run_on_pool;
use crate::render::markdown::document_to_markdown;

/// Converts documents under one validated [`Config`]; share it across threads
/// and calls.
#[derive(Debug, Clone)]
pub struct Converter {
    config: Arc<Config>,
}

impl Converter {
    /// A converter for `config`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a limit or the thread count is out of range.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Converter { config: Arc::new(config) })
    }

    /// Parse `input` into the document model.
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
        let document =
            self.on_pool(|| formats::parse(&self.parse_ctx(&input), input.bytes, input.format))?;
        Ok(Parsed { document, config: Arc::clone(&self.config) })
    }

    /// Convert `input` to Markdown: [`Converter::parse`] then
    /// [`Parsed::render`] on one pool, and a PDF through its own extraction.
    ///
    /// # Errors
    ///
    /// [`ConvertError`] when no complete conversion was possible.
    pub fn convert(&self, input: Input<'_>) -> Result<Conversion, ConvertError> {
        self.admit(&input)?;
        self.on_pool(|| {
            let markdown = match input.format {
                #[cfg(feature = "pdf")]
                Format::Pdf => formats::pdf::to_markdown(input.bytes)?,
                format => document_to_markdown(&formats::parse(
                    &self.parse_ctx(&input),
                    input.bytes,
                    format,
                )?),
            };
            Ok(Conversion { markdown })
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

/// A parsed document, keeping the [`Config`] it was parsed under for
/// [`Parsed::render`].
#[derive(Debug, Clone)]
pub struct Parsed {
    document: Document,
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

    /// Render the document to Markdown on a pool built from its [`Config`].
    ///
    /// # Errors
    ///
    /// [`ConvertError::Internal`] when the pool cannot start or rendering
    /// panics.
    pub fn render(&self) -> Result<Conversion, ConvertError> {
        self.on_pool(|| Ok(Conversion { markdown: document_to_markdown(&self.document) }))
    }

    fn on_pool<T: Send>(
        &self,
        work: impl FnOnce() -> Result<T, ConvertError> + Send,
    ) -> Result<T, ConvertError> {
        run_on_pool(&self.config, work)
    }
}

/// The result of a conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Conversion {
    markdown: String,
}

impl Conversion {
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
        let parsed = Parsed { document, config: Arc::new(Config::default()) };
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
