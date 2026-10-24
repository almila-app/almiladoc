# 0002. OCR as gaps an engine or the caller fills

Status: accepted
Date: 2026-10-14, revised 2026-10-24

## Context

- One page needing OCR fails a whole PDF; ADR 0001 makes it a `Block::Gap`. Required: no network
  unless configured, no global state, explicit configuration, no truncated success.
- The primary consumer converts in a child process without network and OCRs in its parent through a
  vision model behind an OpenAI-compatible server, which returns Markdown.

## Decision

1. **Port.** A synchronous `OcrEngine: Send + Sync`, passed per call, maps `OcrRequest`s (a gap's
   `GapId`, origin, reasons and PNG or JPEG `OcrImage`) to one `OcrOutcome` each: `Fill(GapFill)`,
   `Failed(OcrFailure)` or `Unattempted(OcrFailure)`, a failure holding at most an HTTP status.
   `Parsed::ocr(&dyn OcrEngine, gaps)` runs it on the export in batches of `batch_size()`, applying
   each outcome as `fill_gap` or `fail_gap` and stopping after a batch with a gap unattempted.
2. **Two phases.** Between `parse` and `render` the caller calls `Parsed::fill_gap(GapId, GapFill)`
   or `fail_gap(GapId)`, which records `ocr_failed`. `ocr_pages: PageSelection` makes each selected
   PDF page a gap with reason `caller_requested`, its text discarded without loss.
   `Parsed::ocr_requests(&RasterSpec, gaps)` exports requests, which pickle, for a parent's engine.
3. **Routing.** `ocr_pages`, the page verdicts and `ImageOcr` decide which gaps exist; one `ocr`
   call runs one engine on a subset, and a later one gets only what it left unfilled.
4. **Images.** Under opt-in `ImageOcr::All` each embedded PNG or JPEG gains an `Image(AssetId)` gap
   after its innermost block, its request carrying the asset's bytes undecoded; other images count
   `image_not_ocred`. Image text follows its block, `<br>`-joined in a cell.
5. **Rendering.** A page gap's image is its crop box, an sRGB PNG unpdf paints from the parser
   `Parsed` keeps, so the page rendered is the one extracted, with no system font read. Pages render
   on the call's pool at the largest dpi up to the spec's that fits `max_pixels`, charged one work
   unit per 128 pixels and per 16 PNG bytes; that cap and those charges alone bound painting, which
   cancellation and `max_work` reach between pages. Unpainted text runs, images, inline images and
   undecodable content streams report `raster_incomplete`; shadings and soft masks, none.
6. **Raster spec.** `RasterSpec` defaults to 150 dpi and MiMo-VL's cap of 12 845 056 pixels; it
   comes from the engine or the caller and stays outside `Config`, as it shapes requests, not fills.
7. **`ocr-http`.** `OcrHttp` (feature `ocr-http`) posts to OpenAI-compatible `chat/completions` one
   image per request, `concurrency` at once on `pool.rs`'s scoped workers, a leading `<think>` block
   stripped. 408, 429, 500 and 502-504 retry with backoff, which a cancel ends; a rejected reply
   fails its gap, any other failure leaves it and the rest of its batch unattempted. ureq on rustls
   and ring follows no redirect, caps the body, uses no proxy and leaves no thread behind a call. A
   key needs TLS or loopback; `new` opens no socket.
8. **Local OCR.** A separate crate wrapping oar-ocr or ocrs, when demanded; ocrs waits for rten runs
   off the global pool (https://github.com/robertknight/rten/issues/1188).
9. **Weights.** The local crate SHA-256-checks its weight files and downloads none for the caller.
10. **Identity.** ADR 0001 decision 15 holds over fills; engines stay outside `config_fingerprint`.
11. **Untrusted output.** `GapFill`, `#[non_exhaustive]`, is `Markdown(String)`, plain text
    included, within `max_ocr_text_bytes` (1 MiB) and `max_nesting_depth`, each event charged; raw
    HTML becomes escaped text, a link its label, an image its alt; NFC. A refused fill changes
    nothing: a fill limit is `GapError::Refused { limit }`, never fatal like `max_work`.
12. **Containment.** An engine sees only its requests, each with the parse's fill limit; its panic
    is `Internal`. Python passes only the crate's engines; its own OCR fills through `ocr_requests`
    and `fill_gap`. No error, diagnostic or `Debug` holds OCR text, a response body, a key, a
    prompt or a URL's userinfo or query. Sockets exist only in `src/ocr/http/`.
13. **Tests.** Engines run as fakes and `ocr-http` on loopback, then live before the first release.

## Consequences

- The rasterizer adds 1.02 MB to the wheel and `ocr-http` 0.88 MB, ring's C build needing clang on
  `windows-11-arm`. A PDF's `Parsed` holds unpdf's parser.
- Glyphs a stand-in face lacks, annotation appearances and Lab or DeviceN colour paint wrong
  unreported.

## Alternatives considered

- hayro: +2.82 MB, a second parser, silent on unpainted text. PDFium, MuPDF: native. reqwest: tokio.
  Located lines grouped in the crate: vision models return Markdown, and `GapFill` can gain lines.
- An engine in `Config` or a registry: `Config` loses `Eq`, or global state. A white gap to the
  engine: a model may invent text. tract-onnx: +20.6 MB, 1.8× slower.
