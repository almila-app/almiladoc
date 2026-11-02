# almiladoc

Rust library and Python binding that convert 12 document formats to GitHub-Flavored Markdown in-process. There is no CLI.

- No OCR model ships: scanned pages go to an OpenAI-compatible vision server (`OcrHttp`) or to your own OCR.
- No network access unless you pass `OcrHttp`.
- Fair Source (source-available), under the [licence terms](#licence). A release candidate is on TestPyPI; PyPI and crates.io receive the first final, and the API may change before then.

## Formats

| `format=` name | Family | Extensions |
|---|---|---|
| `doc` | Word 97-2003 | `.doc` |
| `docx` | Word | `.docx`, `.docm` |
| `odt` | OpenDocument text | `.odt` |
| `pdf` | PDF | `.pdf` |
| `ppt` | PowerPoint 97-2003 | `.ppt`, `.pps`, `.pot` |
| `pptx` | PowerPoint | `.pptx`, `.pptm`, `.ppsx`, `.ppsm` |
| `rtf` | Rich Text Format | `.rtf` |
| `epub` | EPUB 2 and 3 | `.epub` |
| `xlsx` | Excel, every generation | `.xlsx`, `.xlsm`, `.xlsb`, `.xls` |
| `ods` | OpenDocument spreadsheet | `.ods` |
| `odp` | OpenDocument presentation | `.odp` |
| `csv` | CSV | `.csv` |

- Content decides the format; CSV has no signature and must be named.
- What each format keeps and loses: [Format support](https://github.com/almila-app/almiladoc/blob/main/docs/support.md).

## Install

```sh
pip install --pre --index-url https://test.pypi.org/simple/ almiladoc
pip install git+https://github.com/almila-app/almiladoc
cargo add almiladoc --git https://github.com/almila-app/almiladoc --features ocr-http,serde
```

- Python 3.11 or newer. The first line installs the release candidate; the second builds from source and needs Rust 1.99 or newer.
- The wheel has every Cargo feature.

| Cargo feature | Default | Adds |
|---|---|---|
| `pdf` | yes | PDF input; without it a PDF fails `feature_disabled` |
| `raster` | yes | Page images for OCR (implies `pdf`); without it PDF gaps get no image |
| `serde` | no | `Parsed::to_json` and `JSON_SCHEMA_VERSION` |
| `ocr-http` | no | `OcrHttp`, the OpenAI-compatible OCR engine |

## Quickstart

```python
from pathlib import Path

import almiladoc

converter = almiladoc.Converter()
print(converter.convert_path("report.docx").markdown)
print(converter.convert(Path("table.csv").read_bytes(), "csv").markdown)
```

```rust
use almiladoc::{Config, Converter, Format, Input};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let converter = Converter::new(Config::default())?;
    let csv = b"name,year\nAda,1843\n";
    let markdown = converter.convert(Input::new(csv, Format::Csv))?.into_markdown();
    assert_eq!(markdown, "| name | year |\n| --- | --- |\n| Ada | 1843 |\n");
    Ok(())
}
```

## Rules for coding agents

Copy these into the agent instructions of a project that uses almiladoc.

- Build one `Converter` per `Config` and share it; get throughput from concurrent calls.
- Default `OnLoss.FAIL` raises `NeedsOcrError` for an unfilled PDF page, then `IncompleteError` for lost content. `OnLoss.REPORT` returns Markdown with `is_complete` false and the diagnostics.
- Branch on `error.code` and `kind`, never on messages.
- CSV is never detected: pass `"csv"`. Format names are exact and lower case; every Excel file is `"xlsx"`.
- A scanned PDF needs `ocr=` or the gap loop of the OCR guide; otherwise it raises `NeedsOcrError`.
- Convert untrusted PDFs in a memory-capped worker process and recycle it after `InternalError`.
- Read signatures from the installed `_almiladoc.pyi` (Python) or `cargo doc --all-features` (Rust).

## Guides

- [Convert documents](https://github.com/almila-app/almiladoc/blob/main/docs/guide/convert.md): calls, detection, output conventions, content policies.
- [Errors and losses](https://github.com/almila-app/almiladoc/blob/main/docs/guide/errors.md): every error code and what to do.
- [OCR for scans and images](https://github.com/almila-app/almiladoc/blob/main/docs/guide/ocr.md): routes, `OcrHttp` options, failures.
- [Run almiladoc in a service](https://github.com/almila-app/almiladoc/blob/main/docs/guide/integrate.md): limits, isolation, cancellation, caching, recipes.
- [Format support](https://github.com/almila-app/almiladoc/blob/main/docs/support.md): what each format keeps and loses.
- [Offline OCR adapter](https://github.com/almila-app/almiladoc/blob/main/ocr-local/README.md): the Rust crate for OCR without a network.

## Licence

- This repository's own work is under the [Business Source License 1.1](https://github.com/almila-app/almiladoc/blob/main/LICENSE) (BUSL-1.1). Each version becomes available under the MIT License four years after it is first made publicly available.
- The portions [REUSE.toml](https://github.com/almila-app/almiladoc/blob/main/REUSE.toml) marks MIT are under the [MIT License](https://github.com/almila-app/almiladoc/blob/main/LICENSES/MIT.txt), with the copyright line it records; REUSE.toml records the copyright and licence of each file.

A commercial licence covers any other production use: USD 1,000 per project per year for a licensee whose gross revenue and outside funding are each at most USD 5 million, a project being one product, service or internal application that uses almiladoc; above that, priced per company. Write to almilaapp@gmail.com.

### Licensing

- **What is a Competing Use?** Making almiladoc available to others in a commercial product or service that substitutes for it or offers the same or substantially similar functionality. [LICENSE](https://github.com/almila-app/almiladoc/blob/main/LICENSE) is the binding text.
- **What is free?** Any non-production use, such as evaluation, development and testing, and production use other than a Competing Use by an individual for personal, non-commercial purposes, for non-commercial teaching or academic research, or by anyone who, with their affiliates, had less than USD 100,000 in gross revenue in the preceding 12 months and has raised less than USD 100,000 in outside funding in total.
- **When does a version become MIT?** On the fourth anniversary of the date that version is first made publicly available; from then on, that version is under the MIT License alone.

## Contributing

See [CONTRIBUTING.md](https://github.com/almila-app/almiladoc/blob/main/CONTRIBUTING.md), [SECURITY.md](https://github.com/almila-app/almiladoc/blob/main/SECURITY.md) and the [code of conduct](https://github.com/almila-app/almiladoc/blob/main/CODE_OF_CONDUCT.md).
