# almiladoc

Converts documents (Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF) to GitHub-Flavored Markdown, from a Rust core with a Python binding. It runs in-process and makes no network calls.

almiladoc is Fair Source (source-available) software, not open source: see [Licence](#licence). It is derived from [anydoc](https://github.com/firecrawl/anydoc) by Firecrawl and continues it for scientific text.

## Status

Pre-release: nothing is published yet, and the API below will change before the first release.

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
pip install almiladoc
cargo add almiladoc
```

## Python

```python
import almiladoc

markdown = almiladoc.to_markdown("report.docx")
markdown = almiladoc.to_markdown_bytes(data)
markdown = almiladoc.to_markdown_bytes(data, "csv")
document = almiladoc.to_document(data)

try:
    almiladoc.to_markdown("scan.pdf")
except almiladoc.NeedsOcrError as error:
    print(error.pages, error.page_count)
```

Every conversion failure subclasses `almiladoc.ConvertError`: `UnsupportedError`, `NeedsOcrError`, `MalformedError`, `EncryptedError`, `ResourceLimitError` (with `limit` and `max`), `MissingPartError`, `IncompleteError` (with `diagnostics`, a list of `(code, class, region, unit, count)` tuples), `CancelledError` and `InternalError`. An unreadable file raises `OSError`.

## Rust

```rust
use almiladoc::{Config, Converter, Format, Input};

fn convert(report: &[u8], csv: &[u8]) -> Result<(String, String), Box<dyn std::error::Error>> {
    let converter = Converter::new(Config::default())?;
    let format = Format::sniff(report).ok_or("unrecognized format")?;
    let report = converter.convert(Input::new(report, format))?.into_markdown();
    let table = converter.convert(Input::new(csv, Format::Csv))?.into_markdown();
    Ok((report, table))
}
```

The crate reads no file: pass the bytes and name the format, from `Format::sniff` or `Format::from_extension`; CSV carries no signature and has to be named. A `Converter` is built once from a `Config` (its `Limits`, `threads`, default 1, and `on_loss`), which `Converter::new` validates, and is shared across threads. `convert` returns a `Conversion` with the Markdown; `parse` stops at the document model, which also carries the embedded assets, and `Parsed::render` finishes it; on a PDF, `parse` returns `Unsupported`. Each call runs on a pool of its own `threads` threads. `Limits::max_input_bytes` refuses an input before parsing starts, and `Limits::max_work` bounds each call by deterministic units of work. `Input::with_cancel` takes a `CancelToken`, which any thread may cancel: the call then fails `Cancelled` at its next unit of work, or before a PDF's extraction starts. `ConvertError` is `Unsupported`, `NeedsOcr`, `Malformed`, `Encrypted`, `ResourceLimit`, `MissingPart`, `Incomplete`, `Cancelled` or `Internal`; its messages carry no input text beyond an escaped, capped part name. A panic inside a call is `Internal`: recycle the worker that raised it.

What a conversion cannot carry into its output is reported as `Diagnostics`: each `Diagnostic` is a `DiagnosticCode` with its fixed `LossClass` (`Omitted` by a policy, `Deferred` behind a placeholder or gap, `Lost`, or `Info`), the `Region` it sat in, an optional unit and a count, and holds no input text. Under the default `Config::on_loss`, `OnLoss::Fail`, a conversion that lost content fails `Incomplete` with its diagnostics; `OnLoss::Report` returns the `Conversion`, whose `diagnostics()` lists them and whose `is_complete()` is false. `parse` never fails on a loss: `Parsed::diagnostics` lists them and `Parsed::render` decides.

PDF support is the default `pdf` feature; without it, a PDF is `Unsupported`. Limits and cancellation stop at the PDF engine (pdf-inspector and lopdf), whose decoders allocate from stream headers: isolate PDF work in a memory-capped worker that is recycled after a failure.

### Identity

`Converter::identity()` returns the `EngineIdentity` that fixes a conversion's output: the crate `version` and a `config_fingerprint`, the SHA-256 of every `Config` field that shapes output (all but `limits` and `threads`). Under the committed `Cargo.lock`, equal identities and equal input give byte-identical output across threads, calls and, except for PDF, architectures. Identity does not hold while pdf-inspector's `PI_NO_MERGE` or `PDF_INSPECTOR_BCMAPS_DIR` environment variable is set, until a pinned release reads neither.

## Development

`make ci` runs every gate CI runs, with the toolchain `rust-toolchain.toml` pins; it needs GNU make, rustup and [uv](https://docs.astral.sh/uv/), which installs the locked Python tools and CPython 3.11 and 3.15. Each gate is also a target of its own: `fmt`, `clippy`, `test`, `doc`, `features` and `python`, which builds the wheel from the sdist and tests it installed; `policy` (cargo-deny), `locks`, `licences` (regenerates `THIRD-PARTY-LICENSES`), `reuse`, `hygiene`, `actions` (zizmor, online when `GH_TOKEN` is set), `dco`, `provenance` and `fuzz`. `make tools` fetches the sha256-pinned cargo-deny, cargo-about and cargo-fuzz binaries into the build directory. A target the host cannot run reports `NOT RUN` and its reason.

## Licence

- This repository's additions are under the [Functional Source License 1.1, MIT Future License](LICENSE) (FSL-1.1-MIT). Each version becomes available under the MIT License two years after its release.
- The portions taken from anydoc remain under the [MIT License](LICENSES/MIT.txt), Copyright (c) 2026 Sideguide Technologies Inc.
- [REUSE.toml](REUSE.toml) records the copyright and licence of each file; [NOTICE](NOTICE) states the derivation. almiladoc is not affiliated with or endorsed by Firecrawl.

For other licensing terms, write to almilaapp@gmail.com.

### Competing Use

- **What is a Competing Use?** Making almiladoc available to others in a commercial product or service that substitutes for it, substitutes for another product or service the licensor offers using it (one that exists when that version is made available), or offers the same or substantially similar functionality. [LICENSE](LICENSE) is the binding text.
- **What is allowed?** Any other purpose, for example internal use, non-commercial education and research, and using it inside a product or service that does not compete with it, including modifying and redistributing it under the same licence.
- **When does a version become MIT?** On the second anniversary of the date that version is made available; from then on, that version has no Competing Use restriction.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and the [code of conduct](CODE_OF_CONDUCT.md).
