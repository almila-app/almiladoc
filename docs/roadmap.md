# Roadmap

Open work only; [ADRs](adr/) own decisions. Landing an item removes its section and Waves row; partial completion narrows it, and evidence correcting a wave updates it; item/wave ids stay here, outside rustdoc, README, commits and test names.

## Item rules

- **Proof:** leave `make ci` green; new tests fail before the change. Limit/context tests cover each forwarding/reading site and kill a default-substitution mutant; independent admission/in-flight checks each need a test that kills their own mutant.
- **Limits:** default-limit inputs prove abuse resistance; lowered limits prove charge sites only. First enforcement owns field rustdoc; committed abuse inputs own `tests/it/limits.rs` rows; model fields/types/diagnostics ship only with their first producer, declared once with fixed loss class and additive on non-exhaustive types.
- **Size:** about 800 hand-written changed lines maximum, excluding generated files, fixtures, snapshots and locks; split larger items at a named or natural seam.
- **Ownership:** parallel wave items share no file except REUSE path lists and `tests/it/main.rs` mod lines, whose conflicts retain both sides; other conflicts stop landing. Manifest/lock writers run serially; one item/wave touches `python/`, and each fixture generator has one owner.
- **API/fixtures:** use `change-core-api` and `add-fixture`; JSON schema bumps only for removal, rename or meaning changes. Abuse names are `<limit>--<shape>--errors.<ext>`; generate inputs over 1 MiB in tests and foreign containers through `zip::ZipWriter`/`cfb`; no third-party bytes; update only owned/named format snapshots, with separate diagnostic snapshots.
- **Support:** changes to reads/skips/reports update [support](support.md); hygiene checks the diagnostic code set both ways, not format attribution.
- **Refactors:** preserve Markdown, snapshots, diagnostics, errors, charges, API and binding; necessary behavior changes need explicit acceptance/regressions, never disguised cleanup or automatic deferral. Compact tests retain every case/defect.
- **Risk:** `Tier` is T0 mechanical (for example locks), T1 non-decision prose/new tests, T2 ordinary code, T3 public contracts/gates/design; [ADR 0004](adr/0004-maintainer-landing.md) owns landing and trailers.
- **Updates:** adopt latest stable dependencies/toolchains/runners/tools immediately on a green candidate, outside waves, as signed-off T0 commits closing Dependabot PRs with links (ADR 0003 decision 8).

## Waves

`∥` means disjoint parallel work; `then` binds tighter and means serial. Unlisted items land separately before the first wave sharing their files.

| Wave | Items | Placement |
|---|---|---|
| W8 | exact-long-edge then garbled-pages then scan-stamp | |

Before first publication prove native PDF and real-consumer integration together, and measure pool stack costs (ADR 0001).

## Items

### W8

#### exact-long-edge: OCR images with an exact long edge

- `RasterSpec.dpi` becomes `size: RasterSize { Dpi, LongEdge }`: `LongEdge(L)` paints L pixels on the long edge of the rotated crop box unpdf 0.53.0's `PdfParser::page_size` reports, the short side by `PageSize::pixels` arithmetic (`round().max(1)`), and refuses an exact size over `max_pixels` as `OverPixels` instead of shrinking it; `Dpi` keeps its down-fit, sized on the crop box. Embedded PNG/JPEG requests read width/height from their headers; Python mirrors `long_edge` (with `dpi`, a raster `ConfigError`) and the `OcrHttpConfig` raster kwargs.
- Files: `src/ocr/`, `src/formats/pdf.rs`, `python/`, ADR 0002, `docs/support.md`, `docs/guide/ocr.md`; T2.

#### garbled-pages: a mostly garbled page becomes an OCR gap

- `reasons()` adds `GarbledText` when the lost codes (unpdf 0.53.0's `Page::suppressed_text_codes` less the `UnreadableFont::codes` of `unmapped_codes` runs, plus codes left undecoded) reach `lost * 2 >= painted_text_chars + lost` and a named minimum; no new control or API. The threshold is measured on the ADR 0001 535-page corpus with zero new gaps (without it, a `PdfScanPolicy` rule instead of a default), and support.md states that such a gap left unfilled under Report, or unpaintable, drops the page's readable text and reports only `ocr_gap`.
- Files: `src/formats/pdf.rs`, `tests/fixturegen/pdf.py`, PDF fixtures and snapshots, `docs/support.md`; T2.

#### scan-stamp: a stamp over a page scan leaves the page an OCR gap

- A page that unpdf 0.53.0's `Page::bilevel_image_coverage` shows a black-and-white scan covers becomes `ImageBacked` despite a one-off native stamp, which keeps it Text today; photos and figures covering as much stay Text. Threshold and guards are measured; support.md's stamp line and `ImageBacked` row change in place.
- Files: `src/formats/pdf.rs`, `tests/fixturegen/pdf.py`, PDF fixtures and snapshots, `docs/support.md`; T2.

## External tracks

- **unpdf image coverage:** union area is exact up to 256 image paints and sampled beyond, so crafted strips can keep a page under `CoverageFirst`'s half, leaving it to the default rule; adopt the release that measures it exactly (a sweep line).
- **Offline OCR quality:** evaluate native OCR on representative consumer scans once a labelled corpus has paragraph ground truth and marked headings.
- **Consumer proof:** after the candidate wheel reaches TestPyPI/PyPI, the consumer replaces its old converter without shims, asserts `importlib.metadata.version("almiladoc")` equals the candidate, asserts no old-converter import remains and passes its suite; first proof covers all formats and PDF through the model, every later 0.x minor repeats it, and 1.0 proves its own candidate.
- **Consumer agreement:** at proof start agree PDF scope, `Incomplete`/`Internal` handling, two-phase transport, prompt and concurrency; existing Xiaomi endpoint/settings need no additional live run before the first final tag (owner decision).

## Maintainer steps

As each release step becomes due: green consumer proof → final 0.x tag. At the first final, switch the README install lines to PyPI and crates.io. Before first final, supply `CRATES_IO_PUBLISH_NEW_TOKEN` (`publish-new`) in `crates-io`; before the next final, add the previous final's row to `docs/release-dates.tsv` (ADR 0003 decisions 7/9).
