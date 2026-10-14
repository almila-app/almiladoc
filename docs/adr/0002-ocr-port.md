# 0002. OCR as a port with feature-gated adapters

Status: accepted
Date: 2026-10-14

## Context

- One page needing OCR fails a whole PDF; ADR 0001 makes it a `Block::Gap`.
- Required: no network by default, no global state, explicit configuration, no truncated success (https://github.com/firecrawl/anydoc/issues/187).

## Decision

1. **Port.**
   - Object-safe `OcrEngine: Send + Sync`: `identity()`, `recognize(&Raster, OcrOrigin, OcrLimits) -> Result<Vec<OcrLine>, OcrError>`; the core rechecks `OcrLimits`.
   - `Raster` is `Rgb8` or `Gray8`; `OcrOrigin` is `Page(NonZeroU32)` or `Image(AssetId)`; `OcrLine` is text, optional quad and confidence.
   - `OcrError`: `Unavailable` (alone `retryable()`), `Refused`, `Engine`, `Limit { limit, max }`, `Poisoned`.

2. **Two phases.**
   - `Parsed::rasterizer()` (`render`) has hayro parse a serialisation of the repaired, decrypted document extraction read; `Rasterizer: Send + Sync` renders by `&self` into `Rendered { raster, spec, diagnostics }` or `Unreadable { diagnostics }`.
   - `Parsed::fill_gap` takes gap, lines, identity and the `Rendered` spec and diagnostics, refusing an identity whose `RasterSpec` differs.
   - `Parsed::fail_gap(gap, kind, diagnostics)` records `ocr_failed`, so hand-run phases equal `convert`.
   - `Config::with_ocr(Arc<dyn OcrEngine>)` (`render`) lets `convert` run both phases per gap.

3. **Routing.** `Unavailable` and `Poisoned` abort as `ConvertError::Ocr { gap, origin, kind }`; `Limit` becomes `ResourceLimit`; `Refused`, `Engine` and `Unreadable` (as `Refused`) go through `fail_gap`.

4. **Images.**
   - Under opt-in `ImageOcr::All`, embedded PNG and JPEG images become gaps; others stay images, `Omitted`.
   - png and zune-jpeg decode unscaled to 8-bit on white within `max_ocr_pixels`, checked from headers; a bounded EXIF reader applies orientation, which neither decoder does.
   - Image text follows its block, `<br>`-joined in a table cell; page text replaces its gap.

5. **Rendering.**
   - hayro draws rotated pages on white, single-threaded, on a vello_cpu context at baseline SIMD.
   - Scale comes from `RasterSpec` alone; a raster over `max_ocr_pixels` or hayro's `u16` canvas is `ResourceLimit`.
   - A page-count mismatch or invalid file is `Malformed`, a decryption failure `Encrypted`, a degenerate page box `Unreadable`.
   - An unresolved annotation appearance is `Lost`; any other hayro warning makes the gap `Unreadable`.
   - A white page raster fills with no lines and an `Info` diagnostic, without an engine call, only on positive evidence that the page was drawn: hayro decoded its content stream and every image it drew decoded without a warning; otherwise `Unreadable`. A cleanly decoded all-white embedded image fills the same way.

6. **Raster spec.** `RasterSpec { dpi, max_long_edge_px, colour }` is measured for `local`, caller-set for `http`.

7. **`ocr-http`.**
   - Our own documented wire contract: PNG in; plain text or `{"lines": [...]}` out.
   - ureq on rustls with ring: explicit endpoint and auth, required timeout, no redirects, no environment proxy, https unless explicitly allowed, request and response caps.
   - Transport errors, timeouts, 429 and 5xx are `Unavailable`; an exceeded cap is `Limit`; anything else is `Refused`. No retries.

8. **`ocr-local`.**
   - PP-OCRv6 on rten; `rten-gemm` and `rten-simd`, pinned with it, report the kernel class.
   - `recognize` runs on a per-call pool of `LocalOcr`'s `threads`, outside ADR 0001 decision 3's, as rten overflows stacks inside another rayon pool (https://github.com/robertknight/rten/issues/1188).
   - **Release gate:** an rten release, proven in a fresh process, whose runs and load-time optimisation never reach rten's or rayon's global pool, neither by the eager fallback beside a passed pool nor by a failed per-call pool build; our code avoids the `Operators` helpers and `prepack_weights`.
   - Lacking one, `ocr-local` waits, or a published fork of rten carrying only that fix follows ADR 0001 decision 6's pattern.
   - `OcrParams` are PaddleX's pipeline values for every tier, except decision 13's measured detector side limit.

9. **Weights.**
   - `LocalOcr::load(dir, &ModelManifest, config)` SHA-256-checks every file it reads against pinned Hugging Face commits or a caller's manifest, taking only the recognition dictionary from `inference.yml`.
   - Explicit `fetch_models` (`ocr-fetch`) downloads them over HTTPS from Hugging Face only, size-capped and hash-checked.

10. **Identity.**
    - `OcrIdentity` records adapter, measured or declared provenance, runtime version and `RasterSpec`, never threads, endpoint or credentials.
    - `local` adds weight hashes, `OcrParams` and the run-time kernel class; any other run-time SIMD choice enters unless cross-ISA hashes prove it invariant.
    - `http` adds the declared model and revision; services may be nondeterministic, so ADR 0001 decision 15 covers only rendering equal lines.
    - `EngineIdentity.ocr` carries it with the hayro version.

11. **Untrusted output.**
    - Lines become plain NFC `Inline::Text`, grouped geometrically, never links or images.
    - `Limits` gains `max_ocr_pixels`, `max_ocr_text_bytes` and `max_ocr_lines`, the last defaulting to the detector's 3000 candidates; more detector contours fail as `Limit`, where PaddleOCR truncates.
    - Logs and errors carry no OCR text, pixels or reply bytes.

12. **Containment.**
    - `catch_unwind` wraps `Rasterizer::render` and `recognize`; a panic fails as `Internal` and poisons both.
    - `Poisoned` is an engine's own refusal, as the binding's engines' after `fork`. `HttpOcr` hides its key and endpoint and refuses pickling.
    - Python mirrors both phases; its `Parsed` owns a copy of what extraction read, within `max_input_bytes`.

13. **Tests.**
    - **Pull requests:** `FakeOcr` checks routing, blank scans, poisoning and hand-run phases against `convert`; fuzzing covers untrusted input.
    - **Scheduled and release** (ADR 0003 decision 5): goldens per kernel class on `ubuntu-24.04`, `ubuntu-24.04-arm` and `macos-26` demand exact text at 1 and 3 threads and no surviving pool thread; a class without a golden reports not run, and publishing needs every class to pass. Raster hashes add `windows-2025`; PaddleX parity skips document preprocessing.
    - **Measurement** on a public Turkish and English scanned corpus picks the cheapest tier, DPI and detector side limit within 1 CER point of the best, with Turkish-diacritic recall ≥ 0.95, p95 ≤ 2 s per page, RSS ≤ 1 GiB.

## Consequences

- Stripped size: hayro +6.6 MB, rten +2.66 MB; a compressed wheel over 12 MB reopens this.
- `deny.toml` owes BSD-3-Clause, CC0-1.0, ISC, Unicode-3.0, CDLA-Permissive-2.0 and hayro asset clarifications, THIRD-PARTY-LICENSES ring's; hayro's `LAB.icc` needs a licence before `render` ships.
- Residual risk: hayro reports some losses only to a compiled-out logger, so a partly drawn gap is OCRed undiagnosed; upstream issues are owed on it and rten's five pool paths.

## Alternatives considered

- `Converter::rasterizer(input)`: a second extraction load, on bytes the caller may change.
- A white gap to the engine: a model may invent text.
- hayro's `logging`: a global logger.
- tract-onnx in place of rten: +20.6 MB, 1.8× slower, and not the engine asked for; switching engines is the owner's call.
- An OpenAI-chat HTTP profile: added on demand.
