# Offline OCR adapter

Rust crate `almiladoc-ocr-local`: an `OcrEngine` over caller-supplied PP-OCR models and an ONNX Runtime library. It is unpublished (use a git or path dependency), not available from Python, and downloads nothing.

## Usage

```rust,no_run
use almiladoc::{Config as CoreConfig, Converter, Format, Input, RasterSpec};
use almiladoc_ocr_local::{Config, FileSpec, Limits, LocalOcr, ModelFiles, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let library = FileSpec::new("libonnxruntime.so", [0; 32]);
    let models = ModelFiles {
        detector: FileSpec::new("detector.onnx", [0; 32]),
        recognizer: FileSpec::new("recognizer.onnx", [0; 32]),
        dictionary: FileSpec::new("dictionary.txt", [0; 32]),
    };
    let runtime = Runtime::load(&library, &Limits::default())?;
    let engine = LocalOcr::new(&runtime, &models, Config::default(), RasterSpec::default())?;
    let pdf = std::fs::read("scan.pdf")?;
    let mut parsed = Converter::new(CoreConfig::default())?.parse(Input::new(&pdf, Format::Pdf))?;
    parsed.ocr(&engine, None)?;
    println!("{}", parsed.render()?.markdown());
    Ok(())
}
```

- Replace each `[0; 32]` with the SHA-256 of that file; a mismatch is refused.
- Needs an ONNX Runtime shared library that supports API 27, a PP-OCR detector and recognizer as single ONNX files, and the recognizer's UTF-8 dictionary, one character per line.
- Fills are `GapFill::Lines`; the core groups them into columns and paragraphs.
- One call at a time: a concurrent call is left `busy`.
- A call has no hard cancel, memory or thread cap: run it in a worker process.
- `LocalOcr::new` refuses a `RasterSpec` whose `max_pixels` exceeds the adapter's `Limits.max_pixels`, and an image over `Limits.max_image_bytes` encoded bytes is refused. Their defaults:

```rust
let limits = almiladoc_ocr_local::Limits::default();
assert_eq!((limits.max_pixels, limits.max_image_bytes), (16 << 20, 32 << 20));
```

## Tests

- Run each ignored CPU native test in a fresh caller-owned process, with deadline/isolation as needed: `installed_runtime_logger_does_not_forward_native_messages_to_tracing` and `verified_native_runtime_models_vocabulary_and_core_settlement`.
- From repository root: `ALMILADOC_OCR_NATIVE_INPUT=/path/input.json cargo test --locked --manifest-path ocr-local/Cargo.toml --test native <name> -- --ignored --exact --test-threads=1`.
- Private JSON supplies runtime/detector/recognizer/dictionary objects `{ "file": "local-path", "sha256": "64-hex-digits" }`, plus `cases` with those fields and expected transcript `text`; paths are absolute or relative to JSON. Supply one text PNG and one blank control; compare NFC/whitespace-normalized transcripts; never package input/images/models/transcripts, and errors expose no paths/text.
- Dictionary lines contain one non-NUL Unicode scalar each; decoder adds blank/space, and every recognition tensor must match vocabulary before decoding. PNG/JPEG byte/dimension limits still apply after raster admission (the default `max_image_bytes` may refuse an incompressible painted PNG); native work has no hard in-flight cancellation, memory or total-thread cap.
