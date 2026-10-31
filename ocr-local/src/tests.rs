use std::io::Cursor;
use std::sync::Arc;

use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
use oar_ocr_core::domain::TextRegion;
use oar_ocr_core::processors::{BoundingBox, Point};
use sha2::{Digest, Sha256};

use crate::artifact::read_verified;
use crate::{
    Config, Error, FileRole, FileSpec, Limits, ModelFiles, Rectangle, Runtime, decode, output,
};

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> FileSpec {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    FileSpec::new(path, digest(bytes))
}

fn picture(format: ImageFormat) -> Vec<u8> {
    let pixels = ImageBuffer::from_pixel(3, 2, Rgb([40u8, 80, 120]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(pixels).write_to(&mut encoded, format).unwrap();
    encoded.into_inner()
}

fn region(text: Option<&str>, points: &[(f32, f32)]) -> TextRegion {
    TextRegion::with_recognition(
        BoundingBox::new(points.iter().map(|&(x, y)| Point { x, y }).collect()),
        text.map(Arc::from),
        Some(0.9),
    )
}

fn filled_page(
    regions: Vec<TextRegion>,
    width: u32,
    height: u32,
    limits: &Limits,
) -> Result<crate::Page, Error> {
    output::page(regions.into_iter().map(Ok), width, height, limits)
}

const RECT: [(f32, f32); 4] = [(1.0, 2.0), (7.0, 2.0), (7.0, 5.0), (1.0, 5.0)];

#[test]
fn configuration_refuses_every_zero_bound_and_invalid_thread_count() {
    type Change = fn(&mut Config);
    let cases: &[(&str, Change)] = &[
        ("threads", |c| c.threads = 0),
        ("threads", |c| c.threads = 9),
        ("max_image_bytes", |c| c.limits.max_image_bytes = 0),
        ("max_image_bytes", |c| c.limits.max_image_bytes = usize::MAX),
        ("max_dimension", |c| c.limits.max_dimension = 0),
        ("max_pixels", |c| c.limits.max_pixels = 0),
        ("max_decode_bytes", |c| c.limits.max_decode_bytes = 5),
        ("max_artifact_bytes", |c| c.limits.max_artifact_bytes = 0),
        ("max_artifact_bytes", |c| {
            c.limits.max_artifact_bytes = usize::MAX;
        }),
        ("max_dictionary_bytes", |c| {
            c.limits.max_dictionary_bytes = 0;
        }),
        ("max_dictionary_bytes", |c| {
            c.limits.max_dictionary_bytes = usize::MAX;
        }),
        ("max_lines", |c| c.limits.max_lines = 0),
        ("max_lines", |c| c.limits.max_lines = 100_001),
        ("max_text_bytes", |c| c.limits.max_text_bytes = 0),
        ("max_text_bytes", |c| c.limits.max_text_bytes = usize::MAX),
    ];
    assert_eq!(Config::default().validate(), Ok(()));
    for &(name, change) in cases {
        let mut config = Config::default();
        change(&mut config);
        assert_eq!(config.validate(), Err(Error::Config(name)));
    }
}

#[test]
fn digest_validation_covers_each_file_role_and_uses_exact_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = file(&dir, "private-model-path", b"model-content");
    for role in [FileRole::Runtime, FileRole::Detector, FileRole::Recognizer, FileRole::Dictionary]
    {
        assert_eq!(read_verified(&artifact, role, 13), Ok(b"model-content".to_vec()));
        assert_eq!(read_verified(&artifact, role, 12), Err(Error::FileTooLarge(role)));
        let wrong = FileSpec::new(artifact.path(), digest(b"other-content"));
        assert_eq!(read_verified(&wrong, role, 13), Err(Error::DigestMismatch(role)));
    }
    let loaded = read_verified(&artifact, FileRole::Detector, 13).unwrap();
    std::fs::write(artifact.path(), b"changed-content").unwrap();
    assert_eq!(loaded, b"model-content");
    assert!(!format!("{artifact:?}").contains("private-model-path"));
}

#[test]
fn file_errors_hold_neither_path_nor_native_error_text() {
    let dir = tempfile::tempdir().unwrap();
    for path in [dir.path().join("private-missing-path"), dir.path().to_path_buf()] {
        let error =
            read_verified(&FileSpec::new(path, [0; 32]), FileRole::Detector, 10).unwrap_err();
        assert_eq!(error, Error::FileUnreadable(FileRole::Detector));
        assert_eq!(error.to_string(), "cannot read Detector file");
        assert!(!format!("{error:?}").contains("private"));
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn dictionaries_are_verified_before_utf8_and_content_validation() {
    let dir = tempfile::tempdir().unwrap();
    let detector = file(&dir, "detector", b"detector");
    let recognizer = file(&dir, "recognizer", b"recognizer");
    for text in
        [b"".as_slice(), b"\n\n", b"\xff", b"x\0\ny", b"a\n\nb", b"ab\nc", "e\u{301}\nx".as_bytes()]
    {
        let models = ModelFiles {
            detector: detector.clone(),
            recognizer: recognizer.clone(),
            dictionary: file(&dir, "dictionary", text),
        };
        assert_eq!(models.read(&Limits::default()), Err(Error::Dictionary));
        assert_eq!(
            crate::OfflineOcr::new(&Runtime::test_handle(), &models, Config::default())
                .unwrap_err(),
            Error::Dictionary
        );
    }
    let dictionary = file(&dir, "dictionary", "a\nç\nı\nİ\n".as_bytes());
    let models = ModelFiles { detector, recognizer, dictionary };
    assert_eq!(
        models.read(&Limits::default()),
        Ok(crate::artifact::VerifiedModels {
            detector: b"detector".to_vec(),
            recognizer: b"recognizer".to_vec(),
            dictionary: vec!["a".into(), "ç".into(), "ı".into(), "İ".into()],
        })
    );
    let bounds = Limits { max_dictionary_bytes: 2, ..Limits::default() };
    assert_eq!(models.read(&bounds), Err(Error::FileTooLarge(FileRole::Dictionary)));
}

#[test]
fn runtime_rejects_unverified_and_invalid_libraries_without_exposing_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = file(&dir, "private-runtime-path", b"not-a-dynamic-library");
    let wrong = FileSpec::new(artifact.path(), [0; 32]);
    assert_eq!(
        Runtime::load(&wrong, &Limits::default()).unwrap_err(),
        Error::DigestMismatch(FileRole::Runtime)
    );
    let error = Runtime::load(&artifact, &Limits::default()).unwrap_err();
    assert_eq!(error, Error::Runtime);
    assert_eq!(error.to_string(), "cannot initialize ONNX Runtime");
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn png_and_jpeg_decode_with_dimensions_preserved() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg] {
        let decoded = decode::decode(&picture(format), &Limits::default()).unwrap();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.as_raw().len(), 18);
    }
}

#[test]
fn image_admission_checks_encoded_size_dimensions_pixels_and_buffer_bytes() {
    let encoded = picture(ImageFormat::Png);
    let cases = [
        Limits { max_image_bytes: encoded.len() - 1, ..Limits::default() },
        Limits { max_dimension: 2, ..Limits::default() },
        Limits { max_pixels: 5, ..Limits::default() },
        Limits { max_decode_bytes: 35, ..Limits::default() },
    ];
    for bounds in cases {
        assert_eq!(decode::decode(&encoded, &bounds), Err(Error::ImageLimit));
    }
    assert_eq!(decode::decode(b"GIF89a", &Limits::default()), Err(Error::ImageFormat));
    assert_eq!(decode::decode(b"not image bytes", &Limits::default()), Err(Error::ImageFormat));
    assert_eq!(decode::decode(&encoded[..32], &Limits::default()), Err(Error::ImageDecode));
}

#[test]
fn output_is_literal_redacted_and_keeps_detector_order() {
    let text = "# <script>ı İ ç</script>";
    let input = vec![region(Some("right"), &RECT), region(Some(text), &RECT)];
    let page = filled_page(input, 10, 10, &Limits::default()).unwrap();
    assert_eq!(page.lines.iter().map(crate::Line::text).collect::<Vec<_>>(), ["right", text]);
    assert_eq!(
        page.lines[0].rectangle(),
        Rectangle { left: 1.0, top: 2.0, right: 7.0, bottom: 5.0 }
    );
    assert!(!format!("{page:?}").contains("script"));
    assert_eq!(filled_page(Vec::new(), 10, 10, &Limits::default()).unwrap().lines, []);
}

#[test]
fn malformed_or_missing_regions_refuse_the_entire_output() {
    let mut nan = RECT;
    nan[0].0 = f32::NAN;
    let cases = [
        vec![],
        vec![(1.0, 2.0); 4],
        vec![(0.0, 0.0); 3],
        vec![(-1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (-1.0, 2.0)],
        vec![(1.0, 1.0), (11.0, 1.0), (11.0, 2.0), (1.0, 2.0)],
        nan.to_vec(),
        vec![(f32::INFINITY, 1.0); 4],
    ];
    for points in cases {
        assert_eq!(
            filled_page(vec![region(Some("private-text"), &points)], 10, 10, &Limits::default()),
            Err(Error::InvalidOutput)
        );
    }
    assert_eq!(
        filled_page(vec![region(None, &RECT)], 10, 10, &Limits::default()),
        Err(Error::InvalidOutput)
    );
}

#[test]
fn line_and_utf8_byte_limits_refuse_instead_of_truncating() {
    let input = || vec![region(Some("ç"), &RECT), region(Some("İ"), &RECT)];
    let bounds = Limits { max_lines: 1, ..Limits::default() };
    assert_eq!(filled_page(input(), 10, 10, &bounds), Err(Error::OutputLimit));
    let bounds = Limits { max_text_bytes: 3, ..Limits::default() };
    assert_eq!(filled_page(input(), 10, 10, &bounds), Err(Error::OutputLimit));
    let bounds = Limits { max_text_bytes: 4, ..Limits::default() };
    assert_eq!(filled_page(input(), 10, 10, &bounds).unwrap().lines.len(), 2);
}

#[test]
fn animated_png_is_refused_instead_of_reading_only_its_first_frame() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(2, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[20, 40, 60]).unwrap();
        writer.write_image_data(&[60, 40, 20]).unwrap();
    }
    assert_eq!(decode::decode(&bytes, &Limits::default()), Err(Error::ImageFormat));
}

#[test]
fn transparent_png_pixels_are_composited_against_white() {
    for (alpha, expected) in [(0u8, 255u8), (128, 127), (255, 0)] {
        let pixels = ImageBuffer::from_pixel(1, 1, image::Rgba([0u8, 0, 0, alpha]));
        let mut buffer = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels).write_to(&mut buffer, ImageFormat::Png).unwrap();
        let decoded = decode::decode(&buffer.into_inner(), &Limits::default()).unwrap();
        assert_eq!(decoded.get_pixel(0, 0).0, [expected; 3]);
    }
}

#[test]
fn decode_accounting_holds_exact_source_and_normalization_boundaries() {
    assert_eq!(decode::buffer_bytes(6, 18), Some(60));
    assert_eq!(decode::painted_buffer_bytes(6), Some(60));
    assert_eq!(decode::buffer_bytes(u64::MAX, 1), None);
    assert_eq!(decode::buffer_bytes(1, u64::MAX), None);
    assert_eq!(decode::painted_buffer_bytes(u64::MAX / 10 + 1), None);
    let encoded = picture(ImageFormat::Png);
    let limits = Limits { max_image_bytes: encoded.len(), ..Limits::default() };
    assert_eq!(decode::decode(&encoded, &limits).unwrap().dimensions(), (3, 2));
    for (cap, expected) in [(59, Err(Error::ImageLimit)), (60, Ok((3, 2)))] {
        let limits = Limits { max_decode_bytes: cap, ..Limits::default() };
        assert_eq!(decode::decode(&encoded, &limits).map(|image| image.dimensions()), expected);
    }
    assert_eq!(
        Limits { max_decode_bytes: 7, ..Limits::default() }.validate(),
        Err(Error::Config("max_decode_bytes"))
    );
    assert_eq!(Limits { max_decode_bytes: 8, ..Limits::default() }.validate(), Ok(()));
}
