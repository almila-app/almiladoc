# Errors and losses

Know what each outcome means and what to do next.

## Loss policy

- `OnLoss.FAIL` (default): an unfilled or failed PDF page gap raises `NeedsOcrError`, then lost
  content raises `IncompleteError`.
- `OnLoss.REPORT`: the call returns the `Conversion`; `is_complete` is false and `diagnostics`
  list what is missing. Under `FAIL` an unfilled image gap also clears `is_complete`, silently.
- `Converter.parse` and `Parsed.to_json` never raise on loss; `Parsed.render` decides.

## Diagnostics

- A `Diagnostic` has `code`, `loss_class`, `region` (`body`, `header`, `footer`, `note`,
  `comment`, `metadata` or `container`), `unit` (1-based or `None`) and `count`; it holds no input
  text. Codes per format: [Format support](../support.md).
- `loss_class`: `omitted` (a content policy dropped it), `deferred` (a placeholder or gap stands
  in), `lost` (unread; the only class that raises `IncompleteError`), `info` (formatting, repair).

## Errors

| Code | Python exception | Fields | Do this |
|---|---|---|---|
| `unsupported` | `UnsupportedError` | `kind`, `detected` | By `kind`, below |
| `needs_ocr` | `NeedsOcrError` | `pages`, `page_count` | Run OCR ([OCR guide](ocr.md)) or use `OnLoss.REPORT` |
| `malformed` | `MalformedError` | `part` | Refuse the file: it is damaged; from `render_ocr_tables`, the OCR table markup was refused: retry with `render_ocr` |
| `encrypted` | `EncryptedError` | none | Ask for an unprotected copy |
| `resource_limit` | `ResourceLimitError` | `limit`, `max` | Refuse the file, or raise the `limit` field for trusted input; `allocation` means memory was refused |
| `missing_part` | `MissingPartError` | `part` | Refuse the file: it is damaged |
| `incomplete` | `IncompleteError` | `diagnostics` | Use `OnLoss.REPORT`, or reject the file |
| `page_out_of_range` | `PageOutOfRangeError` | `page`, `page_count` | Fix `ocr_pages` |
| `invalid_config` | Rust only; Python raises `ConfigError` (below) | `ConfigError` | Fix the field it names |
| `cancelled` | `CancelledError` | none | Discard the call |
| `internal` | `InternalError` | none | Recycle the worker process |

- `UnsupportedError.kind`: `unknown_format` (name the format), `format_mismatch` (retry with
  `detected`), `feature_disabled` (a Rust build without `pdf`), `not_inspectable` (not a PDF).
- Branch on `code`, `kind`, `loss_class` and `ConfigError.limit`, never on messages. Released
  codes never change.
- Rust: `ConvertError` and its payloads are `non_exhaustive`: match with `..` and a wildcard arm;
  `ConvertError::code` returns the code.

| Other Python exception | Raised by |
|---|---|
| `ConfigError` (a `ValueError`, with `limit`, `ceiling`, `max`; no `code`) | `Limits`, `Config`, `OcrHttp`, `Parsed.ocr_requests`: a value out of range. `limit` names the field, `raster` for dpi, long edge, pixels, a dpi given with a long edge or a zero `max_bytes`; `ceiling`, set only for a value over it, is the most `max_nesting_depth` or `max_pages` takes (a zero leaves it `None`); `max` is the most `threads`; each is `None` otherwise |
| `OverflowError` | The same calls: a negative int, or one too big for the argument's unsigned 32- or 64-bit type |
| `GapRefused` (a `ValueError`, with `limit`) | `Parsed.fill_gap` and its variants: a fill past a per-fill limit |
| `KeyError` | `Parsed.fill_gap`, `Parsed.fail_gap`: no such gap |
| `ValueError` | An unknown format name, a gap already settled, an invalid table fill |
| `TypeError` | A buffer that is not contiguous bytes; `ocr_pages` holding non-integers; an argument of the wrong type, such as `None` for `threads`, a limit or `max_pixels` |
| `OSError` | `Converter.convert_path`: missing file, directory, FIFO or device |

```python
import logging
from pathlib import Path

import almiladoc

strict = almiladoc.Converter()
lenient = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))


def to_markdown(data, format=None):
    try:
        return strict.convert(data, format).markdown
    except almiladoc.UnsupportedError as error:
        if error.kind == "format_mismatch":
            return to_markdown(data, error.detected)
    except almiladoc.IncompleteError:
        conversion = lenient.convert(data, format)
        lost = [d.code for d in conversion.diagnostics if d.loss_class == "lost"]
        logging.warning("lost content: %s", lost)
        return conversion.markdown
    except almiladoc.ConvertError as error:
        logging.error("failed: %s", error.code)


assert to_markdown(Path("letter.rtf").read_bytes(), "doc")
assert to_markdown(Path("scan.pdf").read_bytes()) is None
```

```rust
use almiladoc::{Config, ConvertError, Converter, Format, Input};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let converter = Converter::new(Config::default())?;
    let error = converter.convert(Input::new(br"{\rtf1 Hello}", Format::Docx)).unwrap_err();
    let ConvertError::Unsupported { kind, .. } = &error else { return Err(error.into()) };
    assert_eq!((error.code(), kind.as_str()), ("unsupported", "format_mismatch"));
    Ok(())
}
```
