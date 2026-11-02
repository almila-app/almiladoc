# Run almiladoc in a service

Process model, limits, isolation, cancellation and caching for untrusted input.

## Process model

- Build one `Converter` per `Config` and share it across threads; Python calls release the GIL.
- Get throughput from concurrent calls or processes. `Config.threads` (1 to 64, default 1) only
  widens the pool that paints PDF pages for OCR; parsing, rendering and JSON use one thread.
  No network access happens unless you pass `OcrHttp`.

## Untrusted input

- A crossed limit raises `ResourceLimitError`; `limit` names the field, or `allocation` when memory
  was refused. A `Parsed` fill past a per-fill limit raises `GapRefused` instead, except `max_work`.
- A limit is at least 1; `max_nesting_depth` is at most 1024 and `max_pages` at most 4,294,967,295.
- PDF page decoding has no per-page budget: convert PDFs in a memory-capped worker process and
  recycle it after `InternalError` or a crash; `max_tasks_per_child=1` below does so per file.

| `Limits` field | Default | Caps |
|---|---|---|
| `max_entry_bytes` | 128 MiB | Decompressed bytes of one ZIP entry, compound-file stream, embedded OLE object or OfficeArt picture; not PDF streams |
| `max_total_bytes` | 512 MiB | Decompressed bytes one parse inflates from ZIP entries, OfficeArt pictures and embedded OLE objects; not PDF streams |
| `max_entry_count` | 100,000 | Entries in one ZIP archive |
| `max_nesting_depth` | 256 | Nesting of XML elements, RTF groups, Word fields and equation records in the input, and of the parsed document's lists, cells, quotes and links |
| `max_xml_nodes` | 2,000,000 | XML nodes one part or OCR table fill holds at once |
| `max_grid_slots` | 4,000,000 | Table grid positions one parse holds together, spans and padding included, in every table but a CSV's and in OCR table fills |
| `max_expansion` | 4,000,000 | Cells one span or repeat expansion produces per table |
| `max_expansion_text_bytes` | 64 MiB | Text bytes OpenDocument table repeats duplicate per document |
| `max_asset_total_bytes` | 128 MiB | Embedded asset bytes one document retains |
| `max_record_depth` | 64 | Nesting of PPT and XLSB record containers and of XLS custom worksheet views |
| `max_records` | 16,000,000 | Records visited in one PPT, XLS or XLSB stream or equation object |
| `max_input_bytes` | 256 MiB | Bytes of one input, checked before it is read |
| `max_compression_ratio` | 1,000 | Decompressed to compressed size of a ZIP entry, OfficeArt picture or embedded OLE object declaring 1 MiB or more |
| `max_output_bytes` | 256 MiB | Markdown or JSON bytes one call writes |
| `max_work` | 536,870,912 | Work units one conversion charges over its life, bounding its time: a `Parsed` keeps the count, `ocr_requests`, OCR batches and fills add to it, and each `render` or `to_json` charges on from it |
| `max_pages` | 100,000 | Slides, sheets, spine items or PDF pages in one document |
| `max_ocr_text_bytes` | 1 MiB | Bytes of one OCR fill |

## Cancel and time out

- Pass a `CancelToken` (`cancel=` in Python, `Input::with_cancel` in Rust) and cancel it from any
  thread, such as a timer: the call raises `CancelledError` at its next unit of work, a PDF between
  pages. A `Parsed` keeps its token for `render`, `to_json`, fills and OCR. Ctrl-C does not
  interrupt a call.

## Reproduce and cache

- `Converter.identity` holds `version` and `config_fingerprint`, which ignores `limits`, `threads`
  and the OCR engine. Equal identity, input, `ocr_pages` and gap outcomes give byte-identical
  output across calls, threads and, except for PDF, CPU architectures: key a cache on all four.
- `render_ocr` output is fixed by `identity` and the fill alone.
- A gap's outcome is open, failed (`fail_gap`), or the fill call with its complete arguments: the
  Markdown, or every `OcrLine` in order with its text and bounds. A filled page also carries the
  `RasterFidelity.lost` of the last `OcrRequest` made for it, reported as `raster_incomplete`; it
  is 0 when no request was made or an `OcrRefusal` came after it. To replay a page fill, call
  `ocr_requests` for its gap with the same `dpi`, `max_pixels` and `max_bytes` before filling.

## Recipe: batch conversion

```python
import sys
import threading
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import almiladoc

CONVERTER = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))


def cap_memory():
    if sys.platform == "linux":
        import resource

        resource.setrlimit(resource.RLIMIT_AS, (4 << 30, 4 << 30))


def convert(path):
    token = almiladoc.CancelToken()
    timer = threading.Timer(60, token.cancel)
    timer.start()
    try:
        conversion = CONVERTER.convert_path(path, cancel=token)
    except almiladoc.ConvertError as error:
        return path.name, error.code
    finally:
        timer.cancel()
    path.with_suffix(".md").write_text(conversion.markdown, encoding="utf-8")
    return path.name, sorted({d.code for d in conversion.diagnostics if d.loss_class == "lost"})


if __name__ == "__main__":
    paths = [Path(name) for name in ("report.docx", "table.csv", "scan.pdf")]
    with ProcessPoolExecutor(2, max_tasks_per_child=1, initializer=cap_memory) as pool:
        for name, outcome in pool.map(convert, paths):
            print(name, outcome)
```

## Recipe: upload handler

```python
from pathlib import Path

import almiladoc

CONVERTER = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
STATUS = {"unsupported": 415, "resource_limit": 413, "malformed": 422, "encrypted": 422}


def handle(upload, filename):
    named = "csv" if filename.lower().endswith(".csv") else None
    try:
        conversion = CONVERTER.convert(upload, named)
    except almiladoc.ConvertError as error:
        return STATUS.get(error.code, 500), {"error": error.code}
    body = {"markdown": conversion.markdown, "complete": conversion.is_complete}
    return 200, body | {"codes": [d.code for d in conversion.diagnostics]}


assert handle(Path("report.docx").read_bytes(), "report.docx")[0] == 200
assert handle(b"plain text", "notes.txt") == (415, {"error": "unsupported"})
assert not handle(Path("scan.pdf").read_bytes(), "scan.pdf")[1]["complete"]
```

## Rust

- Share the `Converter` by reference across `std::thread::scope` workers; a build without the
  `pdf` feature refuses PDFs with `UnsupportedKind::FeatureDisabled`.

```rust
use std::thread;

use almiladoc::{Config, Converter, Format, Input};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let converter = Converter::new(Config::default())?;
    thread::scope(|scope| {
        for csv in [b"a,b\n1,2\n".as_slice(), b"c,d\n3,4\n".as_slice()] {
            let converter = &converter;
            scope.spawn(move || assert!(converter.convert(Input::new(csv, Format::Csv)).is_ok()));
        }
    });
    Ok(())
}
```
