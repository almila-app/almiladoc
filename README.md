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

Every conversion failure subclasses `almiladoc.ConvertError`: `UnsupportedError`, `NeedsOcrError`, `MalformedError`, `EncryptedError`, `ResourceLimitError` and `MissingPartError`. An unreadable file raises `OSError`.

## Rust

```rust
use almiladoc::{ConvertError, Format};

fn convert(path: &str, csv: &[u8]) -> Result<(String, String), ConvertError> {
    let report = almiladoc::to_markdown(path)?;
    let table = almiladoc::to_markdown_bytes(csv, Format::Csv)?;
    Ok((report, table))
}
```

`to_document` stops at the document model, which also carries the embedded assets; on a PDF it returns `Unsupported`. `ConvertError` is `Unsupported`, `NeedsOcr`, `Malformed`, `Encrypted`, `ResourceLimit`, `MissingPart` or `Io`.

## Development

On Linux, macOS or Windows:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --locked
```

The Python tests run from the repository root against an installed wheel:

```sh
pip install 'maturin>=1.15,<2'
maturin build --release
pip install --no-index --find-links target/wheels almiladoc
python -m unittest discover -s python/tests
```

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
