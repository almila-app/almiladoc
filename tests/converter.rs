mod common;

use almiladoc::{
    Config, ConfigError, ConvertError, Converter, Format, Input, Limit, Limits, UnsupportedKind,
};
use common::{converter, fixture_root, format_of, walk};

const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<Converter>();

#[test]
fn every_fixture_converts_identically_on_one_and_three_threads() {
    let root = fixture_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let (one, three) = (converter(1), converter(3));
    let mut pdfs = 0;
    for path in files {
        if path.strip_prefix(&root).unwrap().starts_with("abuse") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let format = format_of(&path, &bytes).unwrap();
        pdfs += usize::from(format == Format::Pdf);
        let single = one.convert(Input::new(&bytes, format)).map_err(|e| e.to_string());
        let several = three.convert(Input::new(&bytes, format)).map_err(|e| e.to_string());
        assert_eq!(single, several, "{}", path.display());
    }
    assert!(pdfs > 0, "the corpus holds PDF fixtures");
}

#[test]
fn a_config_out_of_range_is_refused() {
    let mut config = Config::default();
    config.limits.max_pages = 0;
    assert!(matches!(
        Converter::new(config),
        Err(ConfigError::ZeroLimit { limit: Limit::Pages, .. })
    ));

    let mut config = Config::default();
    config.limits.max_nesting_depth = Limits::MAX_NESTING_CEILING + 1;
    assert!(matches!(
        Converter::new(config),
        Err(ConfigError::AboveCeiling { limit: Limit::NestingDepth, ceiling: 1024, .. })
    ));

    for threads in [0, Config::MAX_THREADS + 1] {
        let mut config = Config::default();
        config.threads = threads;
        assert!(matches!(Converter::new(config), Err(ConfigError::Threads { max: 64, .. })));
    }
}

fn pdf() -> Vec<u8> {
    std::fs::read(fixture_root().join("pdf").join("text.pdf")).unwrap()
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_converts_but_has_no_document_model() {
    let bytes = pdf();
    let converter = converter(1);
    assert!(!converter.convert(Input::new(&bytes, Format::Pdf)).unwrap().markdown().is_empty());
    let error = converter.parse(Input::new(&bytes, Format::Pdf)).unwrap_err();
    assert!(
        matches!(error, ConvertError::Unsupported { kind: UnsupportedKind::PdfDocumentModel, .. }),
        "{error:?}"
    );
}

#[cfg(not(feature = "pdf"))]
#[test]
fn a_build_without_pdf_refuses_every_pdf() {
    let bytes = pdf();
    let converter = converter(1);
    let errors = [
        converter.convert(Input::new(&bytes, Format::Pdf)).unwrap_err(),
        converter.parse(Input::new(&bytes, Format::Pdf)).unwrap_err(),
    ];
    for error in errors {
        assert!(
            matches!(
                error,
                ConvertError::Unsupported {
                    kind: UnsupportedKind::FeatureDisabled { feature: "pdf", .. },
                    ..
                }
            ),
            "{error:?}"
        );
    }
    assert_eq!(Format::sniff(&bytes), Some(Format::Pdf));
}
