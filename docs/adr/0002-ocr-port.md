# 0002. OCR gaps and engine port

Accepted. The consumer parses without network in a child process, then OCRs in its parent through an OpenAI-compatible vision server returning Markdown; gaps avoid failing an otherwise readable PDF or returning truncated success.

## Decision

1. **Port.** Per-call synchronous `OcrEngine: Send + Sync` maps each `OcrRequest` (gap id, origin, reasons, PNG/JPEG image) to `Fill(GapFill)`, `Failed` or `Unattempted`, failures carrying at most HTTP status. `Parsed::ocr(engine, gaps)` exports `batch_size()` batches, applies fills/failures and stops after a batch with an unattempted gap.
2. **Two phases.** Between parse/render, `fill_gap` supplies content and `fail_gap` records `ocr_failed`; `ocr_requests(&RasterSpec, gaps)` exports pickleable requests. `ocr_pages: PageSelection` forces selected PDF pages to gaps (`caller_requested`), discarding their text without loss.
3. **Routing.** Page verdicts, explicit page selection and `ImageOcr` create gaps; each OCR call uses one engine/subset, and later calls receive only unsettled gaps.
4. **Images.** Opt-in `ImageOcr::All` adds embedded PNG/JPEG gaps after their innermost block, carrying asset bytes undecoded; other images report `image_not_ocred`. Filled text follows the block (`<br>` in cells); unsettled image gaps write nothing and never cause page-only `NeedsOcr`.
5. **Painting.** Reuse the retained unpdf parser, without system fonts, for crop-box sRGB PNGs at the largest dpi within the media-box pixel cap, on the call pool. Charge one unit/128 pixels and /16 PNG bytes; `render_page_with` charges each operation before it runs against a ledger shared by concurrent pages, rounding covered-pixel work up per 128, and stops on resource limit/cancellation.
   - Unpainted runs/images/inline images/undecodable streams report `raster_incomplete`; shadings and soft masks do not. Hidden/off-page unpaintable runs currently overcount until [unpdf-clip](../roadmap.md).
6. **Raster spec.** Default 150 dpi, 12 845 056 pixels (MiMo-VL cap); engine/caller supplied, outside `Config` because it shapes requests rather than fills.
7. **HTTP.** Feature `ocr-http` posts one image to `chat/completions`, at configured concurrency on the calling thread and scoped pool workers, stripping a leading `<think>` block. Retry 408/429/500/502–504 with cancellable backoff; rejected/empty replies fail, other failures leave a gap unattempted and stop taking requests in that batch.
   - ureq/rustls uses ring unless another process provider is installed, asks before sending bodies, refuses redirects, caps replies, uses only explicit proxies and leaves no workers behind. Caller PEM replaces Mozilla roots for endpoint/HTTPS proxy; keys require an HTTPS endpoint or direct loopback, and construction opens no socket.
8. **Local OCR.** Separately locked `ocr-local/` wraps Oar core on private CPU workers/sessions outside core/Python graphs; verified ONNX Runtime API 27 initialization must precede any other ORT use and cannot retry native failure.
9. **Files/bounds.** Local OCR verifies caller model/dictionary/runtime SHA-256s and downloads nothing; native work has no hard cancellation, memory or total-thread cap, so stronger bounds require caller-owned processes.
10. **Identity.** ADR 0001 decision 15 includes fills; engines stay outside `config_fingerprint`.
11. **Untrusted fills.** Non-exhaustive `GapFill` supports `Markdown`, `Lines(Vec<OcrLine>)` and `MarkdownTables`; every fill is NFC and atomic, limited by `max_ocr_text_bytes` (default 1 MiB), nesting and charged events. Lines have normalized horizontal boxes, are ordered/grouped by the core, stay literal and cost 32 additional bytes/box; Markdown escapes HTML and keeps only link labels/image alt text.
    - Tables opt top-level gaps into standalone text-only `table`/`tr`/`td` HTML with positive spans and safe merged cells; unsupported markup/nested contexts fail `InvalidTable`. Per-fill limits return `GapError::Refused { limit }`, unlike fatal work exhaustion.
12. **Containment.** Engines see only requests plus fill limits; panics become `Internal`, Python accepts only built-in engines and exports requests for custom OCR. Errors, diagnostics and Debug exclude OCR text, response bodies, keys, prompts and URL userinfo/query; ureq trace leaks keys, so Python compiles `log` out; sockets exist only in `src/ocr/http/`.
13. **Tests.** Fake engines and loopback HTTP suffice; the owner accepts the consumer's existing Xiaomi endpoint/settings without another live run before the first release.

## Limits and rejected routes

- Raster/HTTP add 1.02/0.88 MB to the wheel; ring requires clang on `windows-11-arm`. Missing stand-in glyphs, annotation appearances and Lab/DeviceN colours paint incorrectly without a diagnostic.
- hayro adds 2.82 MB and a second parser while hiding unpainted text; PDFium/MuPDF require native dependencies; reqwest requires tokio; tract-onnx adds 20.6 MB and runs 1.8× slower.
- Vision engines return Markdown, so located-line grouping belongs in the core rather than an engine. Putting an engine in Config loses Eq; a registry adds global state; white gap images invite invented text.
