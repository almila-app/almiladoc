# almiladoc

Converts documents (Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF) to GitHub-Flavored Markdown, from a Rust core with a Python binding. It runs in-process and makes no network calls.

almiladoc is Fair Source (source-available) software, not open source: see [Licence](#licence).

## Status

Release candidate 0.1.0rc1 is on TestPyPI; PyPI and crates.io receive the first final release. The API below will change before it.

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

The format is detected from the content; CSV carries no signature, so in-memory CSV has to be named. A PDF with scanned or image-only pages fails with `NeedsOcr`, naming the pages, rather than returning partial text.

## Install

```sh
pip install -i https://test.pypi.org/simple/ almiladoc==0.1.0rc1
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

A `Converter` is built once from a frozen `Config` and shared across threads; each call releases the GIL. `bytes` is read in place, any other buffer is copied once after `Limits.max_input_bytes` admits its length, and `convert_path` checks a file's size before reading it. Without a `format`, a call detects it from the content under the converter's limits, as `Converter.sniff` does, and `convert_path` falls back to the extension; a `format` the content contradicts raises `UnsupportedError` with `kind` `format_mismatch` and `detected`, the format to retry with. A `CancelToken` cancelled from any thread stops the call at its next unit of work, a PDF only before its extraction; Ctrl-C is not seen while a call runs. Every conversion failure subclasses `almiladoc.ConvertError` and carries `code`, its snake_case name such as `needs_ocr`: `UnsupportedError` (with `kind`, such as `unknown_format`, and `detected`), `NeedsOcrError` (with `pages` and `page_count`), `MalformedError` and `MissingPartError` (with `part`), `EncryptedError`, `ResourceLimitError` (with `limit` and `max`), `IncompleteError` (with `diagnostics`, a tuple of `Diagnostic`), `CancelledError` and `InternalError`. A `Limits` or `Config` field out of range raises `ConfigError`, a `ValueError` naming the field as `limit`; an unknown `format` name raises `ValueError`, an input that is not a contiguous byte buffer `TypeError`, and an unreadable file, a directory, or a FIFO or device on Unix, `OSError`.

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

The crate reads no file: pass bytes through `Input::new(bytes, format)` or `Input::auto(bytes, None)`. Automatic detection shares parsing and rendering’s limits; `Input::auto(bytes, Some(format))` supplies a fallback only when the content is unrecognized. CSV carries no signature and must be named or supplied as the fallback. A `Converter` is built once from a `Config` (its `Limits`, `threads`, default 1, `on_loss` and `images`), which `Converter::new` validates, and is shared across threads. `convert` returns a `Conversion` with the Markdown; `parse` stops at the document model, which also carries the embedded assets, and `Parsed::render` finishes it; on a PDF, `parse` returns `Unsupported`. Each call runs on a pool of its own `threads` threads. `Limits::max_input_bytes` refuses an input before parsing starts, and `Limits::max_work` bounds each call by deterministic units of work. `Input::with_cancel` takes a `CancelToken`, which any thread may cancel: the call then fails `Cancelled` at its next unit of work, or before a PDF's extraction starts. `ConvertError` is `Unsupported`, `NeedsOcr`, `Malformed`, `Encrypted`, `ResourceLimit`, `MissingPart`, `Incomplete`, `Cancelled` or `Internal`; its messages carry no input text beyond an escaped, capped part name. A panic inside a call is `Internal`: recycle the worker that raised it.

What a conversion cannot carry into its output is reported as `Diagnostics`: each `Diagnostic` is a `DiagnosticCode` with its fixed `LossClass` (`Omitted` by a policy, `Deferred` behind a placeholder or gap, `Lost`, or `Info`), the `Region` it sat in, an optional unit and a count, and holds no input text. Under the default `Config::on_loss`, `OnLoss::Fail`, a conversion that lost content fails `Incomplete` with its diagnostics; `OnLoss::Report` returns the `Conversion`, whose `diagnostics()` lists them and whose `is_complete()` is false. `parse` never fails on a loss: `Parsed::diagnostics` lists them and `Parsed::render` decides. [docs/support.md](docs/support.md) states, per format, what each reader leaves out and the code that reports it.

`Conversion::units()` returns typed source units and UTF-8 byte ranges into `markdown()`. Ranges contain unit content, excluding unit anchors, separators, boundary whitespace, the final newline and appended note definitions. Empty readable units keep zero-length ranges and original ordinals; hidden sheets and unreadable units have no range, bar a PPT slide whose slide-list text outlives its lost container. Slides with content are separated by one `---`; empty slides add no separator.

The `serde` feature derives `Serialize` for the model, `Diagnostic`, `EngineIdentity` and `Conversion`, and adds `Parsed::to_json`: one object, `{schema_version, identity, document, diagnostics}`, in which a data enum is `{"kind", "value"}`, a field-less one its `snake_case` name, `None` is `null` and an asset carries `byte_len` instead of its bytes. It returns the JSON when content was lost, since the diagnostics travel in it, and counts it against `Limits::max_output_bytes` and `Limits::max_work`. `JSON_SCHEMA_VERSION` rises only when a field or `kind` is removed, renamed or changes meaning, so a reader ignores those it does not know. Schema 3 distinguishes image source read failures (`unread`) from intentional source-free stand-ins (`unavailable`); every image carries its source `{region, unit}` as `origin`. Anchor targets are `{id, unit}`; `unit` identifies its source destination when known, so unread content suppresses only its own missing links. Schema 4 removes a table's `kind`.

PDF support is the default `pdf` feature; without it, a PDF is `Unsupported`. Limits and cancellation stop at the PDF engine (pdf-inspector and lopdf), whose decoders allocate from stream headers: isolate PDF work in a memory-capped worker that is recycled after a failure.

### Identity

`Converter::identity()` returns the `EngineIdentity` that fixes a conversion's output: the crate `version` and a `config_fingerprint`, the SHA-256 of every `Config` field that shapes output (all but `limits` and `threads`). Under the committed `Cargo.lock`, equal identities and equal input give byte-identical output across threads, calls and, except for PDF, architectures. Identity does not hold while pdf-inspector's `PI_NO_MERGE` or `PDF_INSPECTOR_BCMAPS_DIR` environment variable is set, until a pinned release reads neither.

## Development

`make ci` runs every gate CI runs, with the toolchain `rust-toolchain.toml` pins; it needs GNU make, rustup and [uv](https://docs.astral.sh/uv/), which installs the locked Python tools and CPython 3.11 and 3.15. Each gate is also a target of its own: `fmt`, `clippy`, `test`, `doc`, `features` and `python`, which builds the wheel from the sdist and tests it installed; `policy` (cargo-deny), `locks`, `licences` (regenerates `THIRD-PARTY-LICENSES`), `reuse`, `hygiene`, `actions` (zizmor, online when `GH_TOKEN` is set), `dco` and `fuzz`. `make tools` fetches the sha256-pinned cargo-deny, cargo-about and cargo-fuzz binaries into the build directory. A target the host cannot run reports `NOT RUN` and its reason.

## Licence

- This repository's own work is under the [Functional Source License 1.1, MIT Future License](LICENSE) (FSL-1.1-MIT). Each version becomes available under the MIT License two years after its release.
- The portions [REUSE.toml](REUSE.toml) marks MIT are under the [MIT License](LICENSES/MIT.txt), with the copyright line it records; REUSE.toml records the copyright and licence of each file.

For other licensing terms, write to almilaapp@gmail.com.

### Competing Use

- **What is a Competing Use?** Making almiladoc available to others in a commercial product or service that substitutes for it, substitutes for another product or service the licensor offers using it (one that exists when that version is made available), or offers the same or substantially similar functionality. [LICENSE](LICENSE) is the binding text.
- **What is allowed?** Any other purpose, for example internal use, non-commercial education and research, and using it inside a product or service that does not compete with it, including modifying and redistributing it under the same licence.
- **When does a version become MIT?** On the second anniversary of the date that version is made available; from then on, that version has no Competing Use restriction.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and the [code of conduct](CODE_OF_CONDUCT.md).
