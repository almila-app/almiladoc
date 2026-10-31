# almiladoc

Converts documents (Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF) to GitHub-Flavored Markdown, from a Rust core with a Python binding. It runs in-process and makes no network call unless the caller passes it an `OcrHttp` engine, which posts images to the OCR server it names.

almiladoc is Fair Source (source-available) software, not open source: see [Licence](#licence).

## Status

No release yet. The first release candidate goes to TestPyPI once the OCR stack fills a PDF's gaps; PyPI and crates.io receive the first final release. The API below will change before it.

## Formats

| Format           | Extensions                                                 |
| ---------------- | ---------------------------------------------------------- |
| Word             | `.doc`, `.docx`, `.docm`                                   |
| PowerPoint       | `.ppt`, `.pps`, `.pot`, `.pptx`, `.pptm`, `.ppsx`, `.ppsm` |
| Excel            | `.xls`, `.xlsx`, `.xlsm`, `.xlsb`                          |
| OpenDocument     | `.odt`, `.ods`, `.odp`                                     |
| Rich Text Format | `.rtf`                                                     |
| EPUB             | `.epub`                                                    |
| CSV              | `.csv`                                                     |
| PDF              | `.pdf`                                                     |

The format is detected from the content; CSV carries no signature, so in-memory CSV has to be named. A PDF page that is scanned or image-only becomes a gap awaiting OCR: under the default `OnLoss::Fail` the document fails with `NeedsOcr`, naming the pages, rather than returning partial text, and under `OnLoss::Report` each gap writes `<!-- needs-ocr page N -->`. Between parse and render the caller fills a gap with the Markdown its own OCR read, which renders in the gap's place, or records that its OCR failed, which writes `<!-- ocr-failed page N -->`.

## Install

Until the first release, build the wheel from source, which needs a Rust toolchain:

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

A `Converter` is built once from a frozen `Config` and shared across threads; each call releases the GIL. `bytes` is read in place, any other buffer is copied once after `Limits.max_input_bytes` admits its length, and `convert_path` checks a file's size before reading it. Without a `format`, a call detects it from the content under the converter's limits, as `Converter.sniff` does, and `convert_path` falls back to the extension; a `format` the content contradicts raises `UnsupportedError` with `kind` `format_mismatch` and `detected`, the format to retry with. A `CancelToken` cancelled from any thread stops the call at its next unit of work, with PDF extraction checked between pages and painting before each step; Ctrl-C is not seen while a call runs. `ocr_pages="all"` or a set of 1-based pages makes each selected PDF page a gap with reason `caller_requested`, whatever its text; other formats ignore it. `Parsed.fill_gap(gap, markdown)` fills a row of `Parsed.gaps` with the Markdown OCR read for it, its raw HTML kept as text, a link as its label and an image as its alt, and `Parsed.fail_gap(gap)` records that its OCR failed; `Config(image_ocr=ImageOcr.ALL)` also leaves a gap after each embedded PNG or JPEG, its row naming the image's index in `Parsed.assets()`, whose text renders after the block that shows it; `Parsed.ocr_requests(gaps=None, *, dpi=150, max_pixels=12_845_056)` exports an `OcrRequest`, which pickles, for each such gap, its embedded image or its page painted as a PNG at the largest dpi up to `dpi` at which its media box fits `max_pixels`, for an OCR run in another process whose text comes back through `fill_gap(request.gap, text)`; a gap no row names raises `KeyError`, one already filled or failed `ValueError`, and a fill past `max_ocr_text_bytes`, `max_nesting_depth` or `max_grid_slots` `GapRefused` (a `ValueError` with `limit`), each leaving the document unchanged. `OcrHttp(base_url, model, *, api_key=None, …)` reads gaps through an OpenAI-compatible vision server, as the `ocr-http` feature below does: `Parsed.ocr(engine)` fills a parse's gaps and gives an `OcrSummary`, `convert(…, ocr=engine)` and `convert_path` fill them before rendering, and `engine.recognize(requests)` reads another process's requests, giving for each a `str` or an `OcrFailure`, which pickles. Every conversion failure subclasses `almiladoc.ConvertError` and carries `code`, its snake_case name such as `needs_ocr`: `UnsupportedError` (with `kind`, such as `unknown_format`, and `detected`), `NeedsOcrError` (with `pages` and `page_count`), `MalformedError` and `MissingPartError` (with `part`), `EncryptedError`, `ResourceLimitError` (with `limit` and `max`), `IncompleteError` (with `diagnostics`, a tuple of `Diagnostic`), `InvalidConfigError` (with `page` and `page_count`), `CancelledError` and `InternalError`. A `Limits`, `Config` or `OcrHttp` field out of range raises `ConfigError`, a `ValueError` naming the field as `limit` (`raster` for a refused `dpi` or `max_pixels`); an unknown `format` name raises `ValueError`, an input that is not a contiguous byte buffer `TypeError`, and an unreadable file, a directory, or a FIFO or device on Unix, `OSError`.

`Conversion.units` contains `(kind, ordinal, start, end)` tuples whose offsets slice the Python Markdown string, including non-ASCII characters. Empty readable units keep zero-length ranges and original ordinals.

`Config(images=ImagePolicy.ALT_TEXT)` writes image labels; `ImagePolicy.OMIT` drops images. The default `PLACEHOLDER` preserves asset references and visible markers, and external images become text links.

`Parsed.to_json()` returns the document model and its diagnostics as the JSON the Rust `Parsed::to_json` writes, and `Parsed.assets()` the embedded assets as `(id, media_type, data)` tuples.

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

The crate reads no file: pass bytes through `Input::new(bytes, format)` or `Input::auto(bytes, None)`. Automatic detection shares parsing and rendering’s limits; `Input::auto(bytes, Some(format))` supplies a fallback only when the content is unrecognized. CSV carries no signature and must be named or supplied as the fallback. A `Converter` is built once from a `Config` (its `Limits`, `threads`, default 1, `on_loss`, `images` and `image_ocr`), which `Converter::new` validates, and is shared across threads. `convert` returns a `Conversion` with the Markdown; `parse` stops at the document model, which also carries the embedded assets, and `Parsed::render` finishes it; `Parsed::gaps` lists a PDF's pages that need OCR and, under `ImageOcr::All`, each embedded PNG or JPEG, filled and failed ones included, `Parsed::fill_gap` fills one with a `GapFill::Markdown` and `Parsed::fail_gap` records that its OCR failed, each refusing with a `GapError` (`UnknownGap`, `Settled`, `Refused` naming a fill limit, or `Convert`) that leaves the document unchanged, while an `OcrEngine` fills them in-process: `Parsed::ocr` runs it over the gaps that ask for an image, in batches of its `batch_size`, each `OcrOutcome` filling or failing its gap, `Converter::convert_with_ocr` parses, runs it and renders, and `Parsed::ocr_requests` exports the same `OcrRequest`s for an engine elsewhere; with the default `raster` feature a PDF page's request carries the page painted as a PNG, at the largest dpi up to the engine's `RasterSpec` (150 dpi, 12 845 056 pixels) at which the page's media box fits its pixel cap; and `Converter::inspect` reports a PDF's pages and the verdict ruled for each (`text`, `no_text` or `needs_ocr` with its reasons), any other format being `Unsupported`. Each call runs on a pool of its own `threads` threads. `Limits::max_input_bytes` refuses an input before parsing starts, and `Limits::max_work` bounds each call by deterministic units of work. `Input::with_cancel` takes a `CancelToken`, which any thread may cancel: the call then fails `Cancelled` at its next unit of work, with PDF extraction checked between pages and painting before each step, and so do `render`, `to_json`, `fill_gap`, `ocr` and `ocr_requests` on the `Parsed` it made. `Input::with_ocr_pages` takes a `PageSelection` of PDF pages the caller will OCR, each becoming a gap with reason `CallerRequested` whatever its text, and a page past the last fails `InvalidConfig`; other formats ignore it. `ConvertError` is `Unsupported`, `NeedsOcr`, `Malformed`, `Encrypted`, `ResourceLimit`, `MissingPart`, `Incomplete`, `InvalidConfig`, `Cancelled` or `Internal`; its messages carry no input text beyond an escaped, capped part name. A panic inside a call is `Internal`: recycle the worker that raised it.

What a conversion cannot carry into its output is reported as `Diagnostics`: each `Diagnostic` is a `DiagnosticCode` with its fixed `LossClass` (`Omitted` by a policy, `Deferred` behind a placeholder or gap, `Lost`, or `Info`), the `Region` it sat in, an optional unit and a count, and holds no input text. Under the default `Config::on_loss`, `OnLoss::Fail`, a conversion that lost content fails `Incomplete` with its diagnostics; `OnLoss::Report` returns the `Conversion`, whose `diagnostics()` lists them and whose `is_complete()` is false. `parse` never fails on a loss: `Parsed::diagnostics` lists them and `Parsed::render` decides. [docs/support.md](docs/support.md) states, per format, what each reader leaves out and the code that reports it.

`Conversion::units()` returns typed source units and UTF-8 byte ranges into `markdown()`. Ranges contain unit content, excluding unit anchors, separators, boundary whitespace, the final newline and appended note definitions. Empty readable units keep zero-length ranges and original ordinals; hidden sheets and unreadable units have no range, bar a PPT slide whose slide-list text outlives its lost container. Slides with content are separated by one `---`; empty slides add no separator.

The `serde` feature derives `Serialize` for the model, `Diagnostic`, `EngineIdentity` and `Conversion`, and adds `Parsed::to_json`: one object, `{schema_version, identity, document, diagnostics}`, in which a data enum is `{"kind", "value"}`, a field-less one its `snake_case` name, absent document metadata is omitted, other `None` fields are `null`, and an asset carries `byte_len` instead of its bytes. It returns the JSON when content was lost, since the diagnostics travel in it, and counts it against `Limits::max_output_bytes` and `Limits::max_work`. `JSON_SCHEMA_VERSION` rises only when a field or `kind` is removed, renamed or changes meaning, so a reader ignores those it does not know. Schema 3 distinguishes image source read failures (`unread`) from intentional source-free stand-ins (`unavailable`); every image carries its source `{region, unit}` as `origin`. Anchor targets are `{id, unit}`; `unit` identifies its source destination when known, so unread content suppresses only its own missing links. Schema 4 removes a table's `kind`. PDF pages are `page` units, and a `gap` block, `{id: {unit, ordinal}, origin, reasons}`, stands for a page or an image awaiting OCR. Schema 5 lets a gap's `unit` be `null`, in a document without units.

PDF support is the default `pdf` feature, read with [unpdf](https://docs.rs/unpdf) page by page on the call's own thread; without it, a PDF is `Unsupported`. Limits and cancellation reach the PDF engine between pages and before each step of painting one, since inside a page its stream decoding takes no caller budget: isolate PDF work in a memory-capped worker that is recycled after a failure.

The `ocr-http` feature adds `OcrHttp`, an `OcrEngine` that posts each image to an OpenAI-compatible server's `chat/completions`, as `OcrHttpConfig` sets out: a base URL, a model, an optional key sent only over `https` or directly to loopback, the prompt and sampling fields, and how many requests go out at once (`concurrency`, 1 by default, a batch holding twice as many), on the calling thread and threads of the engine's own that end before the call returns. A request the server cannot take yet is retried, a reply it rejects or that holds no Markdown fails its gap, and any other failure leaves its gap for a later call; `OcrHttp::new` opens no socket, and it follows no redirect. Only an explicit `proxy` is used; `root_certificates_pem` replaces Mozilla roots, also for an HTTPS proxy. A key sent through a proxy requires an HTTPS endpoint. `OcrHttp::recognize_images` reads `OcrImage`s outside a parse, such as another process's requests.

### Identity

`Converter::identity()` returns the `EngineIdentity` that fixes a conversion's output: the crate `version` and a `config_fingerprint`, the SHA-256 of every `Config` field that shapes output (all but `limits` and `threads`). Under the committed `Cargo.lock`, equal identities and equal input give byte-identical output across threads, calls and, except for PDF, architectures.

## Development

`make ci` runs every gate CI runs, with the toolchain `rust-toolchain.toml` pins; it needs GNU make, rustup and [uv](https://docs.astral.sh/uv/), which installs the locked Python tools and CPython 3.11 and 3.15. Each gate is also a target of its own: `fmt`, `clippy`, `test`, `doc`, `features` and `python`, which builds the wheel from the sdist and tests it installed; `policy` (cargo-deny), `locks`, `licences` (regenerates `THIRD-PARTY-LICENSES`), `reuse`, `hygiene`, `actions` (zizmor, online when `GH_TOKEN` is set), `dco` and `fuzz`. `make tools` fetches the sha256-pinned cargo-deny, cargo-about and cargo-fuzz binaries into the build directory. A target the host cannot run reports `NOT RUN` and its reason.

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
