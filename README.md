# almiladoc

Rust library and Python binding converting Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF to GitHub-Flavored Markdown. Runs in-process; network access requires a caller-configured `OcrHttp` engine.

Fair Source (source-available), under the [licence terms](#licence). No release yet: TestPyPI receives the first candidate after the OCR stack; PyPI and crates.io receive the first final, and the API may change before then.

## Formats

| Format | Extensions |
|---|---|
| Word | `.doc`, `.docx`, `.docm` |
| PowerPoint | `.ppt`, `.pps`, `.pot`, `.pptx`, `.pptm`, `.ppsx`, `.ppsm` |
| Excel | `.xls`, `.xlsx`, `.xlsm`, `.xlsb` |
| OpenDocument | `.odt`, `.ods`, `.odp` |
| Rich Text Format | `.rtf` |
| EPUB | `.epub` |
| CSV | `.csv` |
| PDF | `.pdf` |

Content determines the format; CSV has no signature and must be named or supplied as a fallback. [Support](docs/support.md) lists retained content, losses and OCR markers per format.

## Install

Before release, build from source with a Rust toolchain:

```sh
pip install git+https://github.com/almila-app/almiladoc
```

## Python

```python
import json
from pathlib import Path

import almiladoc

converter = almiladoc.Converter()
report = converter.convert_path("report.docx")
markdown = report.markdown

table = Path("table.csv").read_bytes()
markdown = converter.convert(table, "csv").markdown

parsed = converter.parse(Path("report.docx").read_bytes())
model = json.loads(parsed.to_json())
assets = parsed.assets()
assert parsed.render().markdown == report.markdown

config = almiladoc.Config(
    limits=almiladoc.Limits(max_input_bytes=64 << 20),
    threads=4,
    on_loss=almiladoc.OnLoss.REPORT,
)
lenient = almiladoc.Converter(config)
for diagnostic in lenient.convert_path("report.docx").diagnostics:
    print(diagnostic.code, diagnostic.loss_class, diagnostic.region, diagnostic.count)

token = almiladoc.CancelToken()
markdown = converter.convert(table, "csv", cancel=token).markdown

letter = Path("letter.rtf").read_bytes()
try:
    converter.convert(letter, "doc")
except almiladoc.UnsupportedError as error:
    markdown = converter.convert(letter, error.detected).markdown

try:
    converter.convert_path("scan.pdf")
except almiladoc.NeedsOcrError as error:
    print(error.pages, error.page_count)
```

- Share a frozen-config `Converter`; calls release the GIL, borrow `bytes`, admit other contiguous buffers before copying and check path size before reading. Omitted format detects content, paths fall back to extension, and mismatches expose `UnsupportedError.detected` for explicit retry.
- A cross-thread `CancelToken` stops work at the next charged unit, between PDF pages and before painting steps; Ctrl-C is unseen during calls.
- `parse` → `Parsed.gaps` → fill/fail → render supports external OCR; `ocr_pages="all"` or 1-based pages forces PDF gaps (ignored for other formats), `Config(pdf_scans=PdfScanPolicy.COVERAGE_FIRST)` also gaps every page images cover at least half of, whatever text it holds, and `Config(image_ocr=ImageOcr.ALL)` adds embedded PNG/JPEG gaps after their blocks, naming the asset index.
- `fill_gap` accepts Markdown, `fill_gap_lines` accepts literal `OcrLine(text, (left, top, right, bottom))` with normalized bounds and no inferred headings/tables, and `fill_gap_tables` accepts standalone text-only table/tr/td HTML at top-level gaps. All refusals are atomic: unknown/settled gaps raise KeyError/ValueError and per-fill limits GapRefused with limit.
- Pickleable `ocr_requests(gaps=None, *, dpi=150, max_pixels=12_845_056, max_bytes=None)` return an `OcrRequest` (embedded image or PDF PNG, with `width`, `height` and per-kind `RasterFidelity` for a painted page) or an `OcrRefusal` (`kind` `unpaintable` or `over_bytes`) per gap; return text with `fill_gap(request.gap, text)`, or use `Parsed.ocr(engine)`/`convert(..., ocr=engine)`. `OcrHttp.recognize(requests)` returns pickleable `OcrResult`s (text or `OcrFailure` with kind, status and `retry_after`, plus token usage, realized tier and attempts) for requests from another process.
- `Conversion.units` contains `(kind, ordinal, start, end)` offsets into Python Unicode Markdown; `Parsed.to_json()` uses the Rust schema and `assets()` returns `(id, media_type, data)`.

The [typed binding](python/almiladoc/_almiladoc.pyi) owns signatures, exception fields and configuration defaults. Conversion failures derive from `ConvertError` with stable snake_case codes; invalid fields raise `ConfigError(ValueError)` naming `limit` (`raster` for dpi/pixels/bytes), unknown formats ValueError, invalid buffers TypeError and unreadable paths/directories/Unix FIFOs/devices OSError.

## Rust

```rust
use almiladoc::{Config, Converter, Format, Input};

fn convert(report: &[u8], csv: &[u8]) -> Result<(String, String), Box<dyn std::error::Error>> {
    let converter = Converter::new(Config::default())?;
    let report = converter.convert(Input::auto(report, None))?.into_markdown();
    let table = converter.convert(Input::new(csv, Format::Csv))?.into_markdown();
    Ok((report, table))
}
```

- Pass bytes through checked `Input::new` or `Input::auto(bytes, fallback)`, which uses fallback only for unrecognized content. Share a validated converter; calls use private thread pools (default 1), while parse retains configuration, assets, cancellation and diagnostics for render; [converter rustdoc](src/converter.rs) owns API details.
- [Limits](src/limits.rs) admit input before parsing and charge deterministic work; cancellation continues through render, JSON, fills, OCR and request export. A PDF page selection past the last page fails `PageOutOfRange` (`PageOutOfRangeError`); inspect supports PDF only.
- Diagnostics carry code/class/region/unit/count, never input text: Omitted is policy, Deferred a placeholder/gap, Lost unread content and Info formatting/repair. Parse retains losses; default Fail renders Lost as Incomplete and unfilled/failed pages as NeedsOcr first; Report returns diagnostics with is_complete false ([support](docs/support.md)).
- [Typed errors](src/error.rs) contain only escaped/capped input part names; boundary panics become Internal, requiring worker recycling. [Unit ranges](src/converter.rs) are UTF-8 bytes, while Python ranges count characters; empty readable units keep original ordinals and zero-length ranges.
- `serde` adds model/diagnostic/identity/conversion serialization and `Parsed::to_json`, including losses under output/work limits; [JSON rustdoc](src/converter.rs) owns the envelope and [schema version](src/json.rs) changes only for removal, rename or changed meaning, so readers ignore additions. Images distinguish unread/unavailable and carry source origins, anchors carry destination units, tables have no kind and gap ids allow null units.
- Default pdf/raster features use [unpdf](https://docs.rs/unpdf); without pdf, PDF is Unsupported. Page decoding has no caller budget inside a page: isolate PDF work in memory-capped workers recycled after failure.
- Optional `ocr-http` adds `OcrHttp` for OpenAI-compatible chat/completions; construction opens no socket, redirects are refused and workers finish before return. Keys require HTTPS or direct loopback (HTTPS endpoint through proxies), only explicit proxies are used, and replacement PEM roots apply to endpoint/HTTPS proxy; a cancel closes requests in flight; [HTTP rustdoc](src/ocr/http/mod.rs) owns configuration, message layouts, service tiers, batches/retries and recognize_images, [ADR 0002](docs/adr/0002-ocr-port.md) owns engine/fill constraints.

### Identity

`Converter::identity()` fixes version and SHA-256 `config_fingerprint` of output-affecting fields (excluding limits and threads). With committed `Cargo.lock`, equal identity, input and gap fills produce byte-identical output across calls, threads and, except PDF, architectures.

The optional [offline OCR adapter](ocr-local/) uses verified caller files and the core's located-line layout.

## Development

`make ci` runs the CI gates using `rust-toolchain.toml`; prerequisites are GNU make, rustup and [uv](https://docs.astral.sh/uv/), which installs locked tools and CPython 3.11/3.15. [ADR 0003](docs/adr/0003-ci-and-release.md) owns gate, tool and release policy; [CLAUDE.md](CLAUDE.md) owns contributor-agent instructions; unavailable targets report `NOT RUN` with a reason.

## Licence

- This repository's own work is under the [Business Source License 1.1](LICENSE) (BUSL-1.1). Each version becomes available under the MIT License four years after it is first made publicly available.
- The portions [REUSE.toml](REUSE.toml) marks MIT are under the [MIT License](LICENSES/MIT.txt), with the copyright line it records; REUSE.toml records the copyright and licence of each file.

A commercial licence covers any other production use: USD 1,000 per project per year for a licensee whose gross revenue and outside funding are each at most USD 5 million, a project being one product, service or internal application that uses almiladoc; above that, priced per company. Write to almilaapp@gmail.com.

### Licensing

- **What is a Competing Use?** Making almiladoc available to others in a commercial product or service that substitutes for it or offers the same or substantially similar functionality. [LICENSE](LICENSE) is the binding text.
- **What is free?** Any non-production use, such as evaluation, development and testing, and production use other than a Competing Use by an individual for personal, non-commercial purposes, for non-commercial teaching or academic research, or by anyone who, with their affiliates, had less than USD 100,000 in gross revenue in the preceding 12 months and has raised less than USD 100,000 in outside funding in total.
- **When does a version become MIT?** On the fourth anniversary of the date that version is first made publicly available; from then on, that version is under the MIT License alone.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and the [code of conduct](CODE_OF_CONDUCT.md).
