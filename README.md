# almiladoc

Converts documents (Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF) to GitHub-Flavored Markdown, from a Rust core with a Python binding.

almiladoc is derived from [anydoc](https://github.com/firecrawl/anydoc) and continues it for scientific text: superscript, subscript and underline kept, a placeholder for each image, typed errors instead of truncated output, structured diagnostics, a page model reported before conversion, and OCR as a pluggable port with no network by default.

## Status

Pre-release. `main` starts from anydoc 0.2.4 while that work lands. Nothing is published yet; the crate and the Python package will be published as `almiladoc`.

## Licence

almiladoc is Fair Source software under the [Functional Source License 1.1, MIT Future License](LICENSE) (FSL-1.1-MIT). You may use, modify and redistribute it for any purpose except a Competing Use: making it available to others in a commercial product or service that substitutes for it or offers substantially similar functionality. Internal use, non-commercial education and research, and use inside a product that does not compete with it are permitted. Each release becomes MIT two years after it is published.

The portions derived from anydoc remain under the [MIT License](LICENSES/MIT.txt), Copyright (c) 2026 Sideguide Technologies Inc.; [NOTICE](NOTICE) and [REUSE.toml](REUSE.toml) record which files carry which licence. almiladoc is not affiliated with or endorsed by Firecrawl.

For other licensing terms, write to almilaapp@gmail.com.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and the [code of conduct](CODE_OF_CONDUCT.md).
