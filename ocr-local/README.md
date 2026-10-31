# Offline OCR

A separate Rust adapter for verified caller-supplied PP-OCR models and an ONNX Runtime library supporting API 27. It downloads no assets and is outside the core and Python dependency graphs. Use `Runtime::load`, then `LocalOcr::new`, and pass the engine to `Parsed::ocr`.

Run each ignored native CPU test in a fresh caller-owned process: `installed_runtime_logger_does_not_forward_native_messages_to_tracing` and `verified_native_runtime_models_vocabulary_and_core_settlement`. From the repository root, select one by name with `ALMILADOC_OCR_NATIVE_INPUT=/path/input.json cargo test --locked --manifest-path ocr-local/Cargo.toml --test native <name> -- --ignored --exact --test-threads=1`; use a deadline and resource isolation when needed.

The private input JSON supplies `runtime`, `detector`, `recognizer` and `dictionary` objects, each `{ "file": "local-path", "sha256": "64-hex-digits" }`, plus `cases`: objects with those same file/digest fields and `text`, the expected transcript. Paths are relative to the input JSON or absolute. Supply exactly one text PNG and one blank PNG control; expected text is compared after NFC and whitespace normalization. Inputs, images, model files and transcripts are not packaged. Errors contain no input paths or recognized text.

Dictionary lines must each contain exactly one non-NUL Unicode scalar. The pinned decoder adds its own blank and space tokens; every recognition tensor must match that vocabulary before decoding. PNG/JPEG byte and dimension limits still apply after raster admission; the default 32 MiB encoded-byte cap can reject a large incompressible painted PNG. Native calls have no hard in-flight cancellation, memory or total-thread cap.
