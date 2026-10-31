# Offline OCR

A standalone Rust CPU pipeline for verified caller-supplied PP-OCR models and an ONNX Runtime library supporting API 27. It downloads no assets and is outside the core and Python dependency graphs. Use `Runtime::load`, then `OfflineOcr::new` and `OfflineOcr::recognize`.

Dictionary lines must each contain exactly one non-NUL Unicode scalar. The pinned decoder adds its own blank and space tokens; every recognition tensor must match that vocabulary before decoding. PNG/JPEG byte and dimension limits still apply after raster admission; the default 32 MiB encoded-byte cap can reject a large incompressible painted PNG. Native calls have no hard in-flight cancellation, memory or total-thread cap.
