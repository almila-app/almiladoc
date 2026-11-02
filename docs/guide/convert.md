# Convert documents

Turn files, bytes or OCR text into Markdown, read each part of the result and choose what it keeps.

## Choose the call

| Task | Python | Rust |
|---|---|---|
| File on disk | `Converter.convert_path(path)` | `std::fs::read`, then `Converter::convert` |
| Bytes in memory | `Converter.convert(data, format)` | `Converter::convert(Input)` |
| Assets and JSON | `Converter.parse(data)`, then `Parsed.render()`, `Parsed.assets()`, `Parsed.to_json()` | `Converter::parse`, `Parsed::render`, `Parsed::to_json` |
| Format of bytes | `Converter.sniff(data)` or `sniff(data)` | `Converter::sniff`, `Format::sniff` |
| OCR text of an image outside a document | `Converter.render_ocr(markdown)`, `render_ocr_tables`, `render_ocr_lines(lines)` | `Converter::render_ocr(GapFill, cancel)` |

- Rust: `Input::new(bytes, Format)` checks the name against the content;
  `Input::auto(bytes, fallback)` detects it, using `fallback` only when the content names nothing.
- `Format::from_extension(ext)` is case-insensitive; `str::parse::<Format>` takes only the
  `format=` names of the README table.
- `render_ocr` reads OCR text as a [fill](ocr.md#your-own-ocr) and returns the `Conversion` a
  filled page holds, without `units`; blank text gives `""`. Text past `max_ocr_text_bytes` raises
  `ResourceLimitError` before it is copied, never `GapRefused`, as do the nesting, grid, work and
  output limits; invalid tables raise `MalformedError`, a cancelled `cancel=` token
  `CancelledError`.

```python
import almiladoc

fill = "Rate \\(x_1\\)\n\n<fcel>A<lcel><nl><fcel>b<fcel>c<nl>"
print(almiladoc.Converter().render_ocr(fill).markdown)
```

## Detection

- Content decides. A `format` the content contradicts raises `UnsupportedError` with `kind`
  `format_mismatch`; retry with its `detected` format.
- CSV has no signature: name it (`"csv"`) or give a fallback. `sniff` returns `None` for it and
  for anything unrecognized, and a call without a name raises `UnsupportedError` (`unknown_format`).
- `convert_path` uses the file extension only when the content names no format.
- Names are lower case and exact; `"xlsx"` covers every Excel workbook. Any other name raises
  `ValueError` (Rust: `ParseFormatError`).
- An encrypted OOXML file is not recognized from its content: name it (`"docx"`, `"pptx"`) to get
  `EncryptedError`.

```python
from pathlib import Path

import almiladoc

converter = almiladoc.Converter()
markdown = converter.convert_path("report.docx").markdown
table = converter.convert(Path("table.csv").read_bytes(), "csv").markdown

letter = Path("letter.rtf").read_bytes()
assert almiladoc.sniff(letter) == "rtf"
try:
    converter.convert(letter, "doc")
except almiladoc.UnsupportedError as error:
    assert (error.kind, error.detected) == ("format_mismatch", "rtf")
    markdown = converter.convert(letter, error.detected).markdown
```

## Read the output

| Markdown | Meaning |
|---|---|
| `![label](asset:N)` | Image placeholder; `N` is the `id` of an entry in `Parsed.assets()` |
| `[^N]` and `[^N]: text` | Footnote or endnote, numbered by first reference |
| `[^almiladoc-comment-N]` | Comment N, replies nested in the parent; needs `CommentPolicy.EMIT` |
| `> <a id="almiladoc:header:default:1"></a>Header (default):` | Header or footer block with printed fields as `{page}` `{pages}` `{date}` `{time}` `{file}` `{path}` `{sheet}`; needs `HeaderFooterPolicy.EMIT` |
| `<br>`, `<sup>`, `<sub>` | Line break inside a table cell; raised and lowered text |
| `$x$` and `$$` blocks | Inline and display TeX math |
| `---` | Break between non-empty slides |
| `<!-- needs-ocr page N -->`, `<!-- ocr-failed page N -->` | PDF page not OCR'd, or whose OCR failed, under `OnLoss.REPORT` |
| link label alone | Unsafe link target dropped; `unsafe_link` diagnostic |

- Only OCR table fills write HTML tables; per-format losses: [Format support](../support.md).
- `Conversion.units` rows are `(kind, ordinal, start, end)`, kind `page`, `slide`, `sheet` or
  `spine_item`; Word, OpenDocument text and RTF have none. Python offsets count characters; Rust
  `Conversion::units()` gives `UnitSpan.bytes`, UTF-8 byte ranges.
- `Parsed.to_json()` returns `{schema_version, identity, document, diagnostics}`. Readers ignore
  unknown fields: `schema_version` changes only when a field is removed, renamed or re-defined.
- A diagnostic's JSON key is `class`; Python spells it `loss_class`. Python 3.11 loads a deeply
  nested document only under a raised recursion limit.

```python
import json
from pathlib import Path

import almiladoc

parsed = almiladoc.Converter().parse(Path("report.docx").read_bytes())
conversion = parsed.render()
assets = {asset_id: data for asset_id, media_type, data in parsed.assets()}
document = json.loads(parsed.to_json())
print(conversion.markdown, conversion.units, conversion.is_complete)
print(document["schema_version"], document["identity"], len(assets))
```

## Content policies

| `Config` field | Default | Other values | Effect |
|---|---|---|---|
| `images` | `ImagePolicy.PLACEHOLDER` | `ImagePolicy.ALT_TEXT`, `ImagePolicy.OMIT` | Asset reference, alt text only, or nothing |
| `headers_footers` | `HeaderFooterPolicy.OMIT` | `HeaderFooterPolicy.EMIT` | Write them as labelled blockquotes |
| `comments` | `CommentPolicy.OMIT` | `CommentPolicy.EMIT` | Write comments as footnotes |
| `hidden` | `HiddenPolicy.OMIT` | `HiddenPolicy.EMIT` | Keep supported hidden text, slides and sheets |
| `revisions` | `RevisionPolicy.ACCEPT` | `RevisionPolicy.REJECT` | Keep inserted text, or keep deleted text |
| `on_loss` | `OnLoss.FAIL` | `OnLoss.REPORT` | [Errors guide](errors.md) |
| `image_ocr` | `ImageOcr.NONE` | `ImageOcr.ALL` | [OCR guide](ocr.md) |
| `pdf_scans` | `PdfScanPolicy.CONSERVATIVE` | `PdfScanPolicy.COVERAGE_FIRST` | [OCR guide](ocr.md) |
| `threads` | `1` | 1 to 64 | [Integration guide](integrate.md) |
| `limits` | `Limits()` | `Limits(max_work=...)` | [Integration guide](integrate.md) |

- A skipped item counts as an `omitted` diagnostic, not a loss.
- Rust: set fields of `Config::default()` (`non_exhaustive`); variants read `ImagePolicy::AltText`.

```rust
use almiladoc::{Config, Converter, Format, ImagePolicy, Input};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = Config::default();
    config.images = ImagePolicy::AltText;
    let converter = Converter::new(config)?;
    let input = Input::auto(b"name,year\nAda,1843\n", Format::from_extension("csv"));
    let parsed = converter.parse(input)?;
    assert!(parsed.to_json()?.starts_with(r#"{"schema_version":"#));
    let conversion = parsed.render()?;
    assert_eq!(conversion.markdown(), "| name | year |\n| --- | --- |\n| Ada | 1843 |\n");
    assert_eq!(conversion.units().len(), 1);
    Ok(())
}
```
