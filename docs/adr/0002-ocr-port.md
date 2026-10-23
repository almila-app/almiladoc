# 0002. OCR as gaps the caller fills

Status: accepted
Date: 2026-10-14

## Context

- One page needing OCR fails a whole PDF; ADR 0001 makes it a `Block::Gap`.
- Required: no network by default, no global state, explicit configuration, no truncated success.
- The primary consumer rasterizes with PDFium and OCRs in its parent process; its conversion child has no network.

## Decision

1. **Port.** Deferred: an in-crate `OcrEngine` lands with its first adapter.

2. **Two phases.**
   - Between `parse` and `render`, the caller OCRs each gap and calls `Parsed::fill_gap(gap, GapFill)`, or `Parsed::fail_gap(gap)`, which records `ocr_failed`.
   - `ocr_pages: PageSelection` (`None` by default, `All` or listed pages) makes each selected PDF page a `Page` gap with reason `caller_requested` whatever its text layer; its extracted text is discarded, never merged, and reports no loss.
   - A rasterizer (`Parsed::rasterizer()`) and `Config::with_ocr` are deferred until a caller lacks its own rasterizer.

3. **Routing.** Deferred with decision 1.

4. **Images.**
   - Under opt-in `ImageOcr::All`, each embedded PNG or JPEG gains an `Image(AssetId)` gap after the innermost block holding the image, within that block's container: body, list item, quote, note or cell; other images count `image_not_ocred`. The crate decodes no image.
   - Image text follows its block, `<br>`-joined in a table cell; page text replaces its gap.

5. **Rendering.** Deferred with the rasterizer.

6. **Raster spec.** Deferred with the rasterizer.

7. **`ocr-http`.** Deferred until a caller needs OCR over the network inside the crate; it is then an OpenAI-compatible vision adapter.

8. **Local OCR.** Offline OCR, when demanded, is a separate crate wrapping oar-ocr or ocrs, never this one. An ocrs wrapper waits for an rten release whose runs never reach rten's or rayon's global pool (https://github.com/robertknight/rten/issues/1188).

9. **Weights.** The local crate SHA-256-checks every weight file against pinned commits or a caller's manifest; nothing downloads them for the caller.

10. **Identity.** Deferred with decision 1.

11. **Untrusted output.**
    - `GapFill::Lines(Vec<OcrLine>)`: lines become plain NFC `Inline::Text`, grouped geometrically, never links or images, at most `max_ocr_lines` (3000).
    - `GapFill::Markdown(String)`: `max_ocr_text_bytes` is checked on its length before parsing; GFM tables, emphasis, strikethrough, headings, lists and code only; raw HTML becomes escaped text, a link keeps its label, an image its alt; each parser event is charged and nesting checked against `max_nesting_depth` before the content is admitted; NFC.
    - A refused fill returns `GapError` and leaves the gap and the ledger as they were.
    - Errors and diagnostics carry no OCR text.

12. **Containment.** Deferred with decision 1.

13. **Tests.** Deferred with decision 8.

## Consequences

- No OCR engine, image decoder, network client or model weights enter the crate or its wheel.
- Stripped size if the adapters return: hayro +6.6 MB, rten +2.66 MB, against ADR 0003 decision 6's wheel bound.
- Owed before a rasterizer or the local crate ships: hayro's `LAB.icc` licence and asset clarifications, and upstream issues on hayro's compiled-out loss reports and rten's pool paths.

## Alternatives considered

- In-crate rasterizer and engines now: the consumer already rasterizes and OCRs outside the conversion, and they would add 9 MB and a network path to every wheel.
- Lines only: a vision model's tables and headings would be lost.
- `Converter::rasterizer(input)`: a second extraction load, on bytes the caller may change.
- A white gap to the engine: a model may invent text.
- tract-onnx in place of rten: +20.6 MB, 1.8× slower, and not the engine asked for.
