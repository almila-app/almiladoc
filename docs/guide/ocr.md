# OCR for scans and images

Decide what gets OCR, pick a route, set it up and handle failures. almiladoc ships no OCR model.

## What becomes a gap

- A PDF page whose own text does not stand for what it shows becomes a gap; its `reasons` are
  `unreadable_content`, `invisible_text`, `garbled_text`, `image_backed`, `vector_drawn` or
  `unreadable_text`.
- `ocr_pages="all"` or a set such as `{1, 3}` forces those PDF pages (`caller_requested`) and
  discards their text; other formats ignore it. A page past the last raises `PageOutOfRangeError`.
- `Config.pdf_scans`: `PdfScanPolicy.CONSERVATIVE` takes a page with an image and no readable
  text, or one at least half covered by images whose only text is margin text or running heads
  and folios (at 90% coverage or more, text that recurs on other pages too).
  `PdfScanPolicy.COVERAGE_FIRST` also takes every page at least half covered by images, whatever
  text it holds. The full rule: [Format support](../support.md#pdf).
- `Config.image_ocr=ImageOcr.ALL` adds a gap after each use of an embedded PNG or JPEG in
  non-PDF formats; other images count `image_not_ocred`, and PDF images are omitted, not gapped.
- Preview with `Converter.inspect(data)`: `Inspection.pages` holds `(page, verdict, reasons)` with
  verdict `text`, `no_text` or `needs_ocr`. Only a PDF can be inspected.

## Choose a route

| Route | Use it when | How |
|---|---|---|
| Built-in HTTP | An OpenAI-compatible vision server exists | `OcrHttp` (Rust: `ocr-http`) |
| Own OCR in process | Your engine runs in the parse process | `Parsed.ocr_requests`, `Parsed.fill_gap`; Rust `OcrEngine` with `Parsed::ocr` |
| OCR in another process | Parse and OCR run apart | Pickle `OcrRequest` and `OcrResult`; `OcrHttp.recognize` |
| Offline Rust adapter | No network | [ocr-local](../../ocr-local/README.md) |

## Built-in HTTP engine

```python
import os
from pathlib import Path

import almiladoc

key = os.environ["OCR_API_KEY"]
engine = almiladoc.OcrHttp("https://ocr.example/v1", "vision-model", api_key=key, concurrency=4)
converter = almiladoc.Converter()
print(converter.convert_path("scan.pdf", ocr=engine).markdown)

parsed = converter.parse(Path("scan.pdf").read_bytes())
summary = parsed.ocr(engine)
print(summary.filled, summary.failed, summary.unattempted, summary.refused)
```

```rust,no_run
use almiladoc::{Config, Converter, Format, Input, OcrHttp, OcrHttpConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = OcrHttpConfig::new("https://ocr.example/v1", "vision-model");
    config.api_key = Some(std::env::var("OCR_API_KEY")?);
    let engine = OcrHttp::new(config)?;
    let pdf = std::fs::read("scan.pdf")?;
    let converter = Converter::new(Config::default())?;
    converter.convert_with_ocr(Input::new(&pdf, Format::Pdf), &engine)?;
    Ok(())
}
```

- The server gets `POST {base_url}/chat/completions` with the image as a data URL. It must answer
  `choices[0].message.content` with `finish_reason` `stop`; a leading `<think>` block is dropped.
- The key travels only over `https` or to loopback; only an explicit `proxy` is used, and
  redirects are refused. Retries (408, 429, 500, 502, 503, 504) wait 0.5 s doubling to 30 s, or
  `Retry-After` up to 60 s. Timeouts: connect 10 s, response 300 s.
- Rust: set `OcrHttpConfig` fields; `raster` (`RasterSpec`) holds `dpi`, `max_pixels`, `max_bytes`.
  Its `ureq` logs request heads, key included, at trace level.

| `OcrHttp` argument | Default | Meaning |
|---|---|---|
| `base_url` | required | `http` or `https` URL; no user, query or fragment |
| `model` | required | Model name sent with each request |
| `api_key` | `None` | Bearer token, visible ASCII |
| `prompt` | `None` | Sends "Transcribe all text in this image as Markdown, keeping its headings, lists and tables, and reply with the Markdown alone." |
| `layout` | `"image_then_text"` | Or `"text_then_image"`, `"developer_then_image"`, `"system_then_image"` |
| `extra_body` | `None` | Extra JSON fields; not `messages`, `model`, token limits, `temperature`, `n`, `stream`, `service_tier` |
| `max_tokens` | `8192` | Reply token limit |
| `token_limit_field` | `"max_tokens"` | Or `"max_completion_tokens"` |
| `temperature` | `0.0` | 0 to 2, or `None` to send none |
| `image_detail` | `None` | `"low"`, `"high"` or `"auto"` |
| `answer_model` | `None` | A reply must name this model |
| `service_tiers` | `()` | Up to 4 tier names; the first is sent |
| `tier_fallback` | `None` | `(codes, message_prefix)`: a matching 429 moves to the next tier |
| `preserve_table_spans` | `False` | Replies keep HTML and OTSL table spans; a nested gap shows each merge once |
| `dpi` | `150` | Page painting resolution, 50 to 600 |
| `max_pixels` | `12,845,056` | Pixel cap per page image, 1,048,576 to 41,943,040 |
| `max_bytes` | `None` | Refuse an encoded image over this size |
| `retries` | `3` | Extra tries, at most 10 |
| `concurrency` | `1` | Requests in flight, 1 to 16 |
| `root_certificates_pem` | `None` | Certificate-only PEM replacing the built-in roots |
| `proxy` | `None` | HTTP(S) CONNECT proxy URI |

## Your own OCR

```python
from pathlib import Path

import almiladoc


def my_ocr(image, media_type):
    return "Text read from the image"


parsed = almiladoc.Converter().parse(Path("scan.pdf").read_bytes())
for request in parsed.ocr_requests():
    if isinstance(request, almiladoc.OcrRequest):
        try:
            parsed.fill_gap(request.gap, my_ocr(request.image, request.media_type))
        except ValueError:
            parsed.fail_gap(request.gap)
print(parsed.render().markdown)
```

| Fill | Input | Notes |
|---|---|---|
| `Parsed.fill_gap` | Markdown | TeX math and OTSL tables are read; other raw HTML stays text; links keep their label, images their alt |
| `Parsed.fill_gap_lines` | `OcrLine(text, (left, top, right, bottom))` | Bounds 0 to 1; columns and paragraphs are inferred, headings and tables are not |
| `Parsed.fill_gap_tables` | Markdown with `table`, `tr`, `td` HTML or OTSL | Spans kept at a top-level gap; a nested gap shows each merge once |

- A fill past `max_ocr_text_bytes` raises `GapRefused`; `fail_gap` records the failure
  (`ocr_failed`). `ocr_requests` returns an `OcrRefusal` (`unpaintable`, `over_bytes`) where no
  image can be made, and paints at its own `dpi` and `max_pixels`.
- An image outside a document takes the same fills through `Converter.render_ocr`: see the
  [Convert guide](convert.md#choose-the-call).

```python
from pathlib import Path

import almiladoc

engine = almiladoc.OcrHttp("https://ocr.example/v1", "vision-model")
parsed = almiladoc.Converter().parse(Path("scan.pdf").read_bytes())
requests = [r for r in parsed.ocr_requests() if isinstance(r, almiladoc.OcrRequest)]
for request, result in zip(requests, engine.recognize(requests), strict=True):
    if result.text is not None:
        (parsed.fill_gap_tables if result.tables else parsed.fill_gap)(request.gap, result.text)
    elif result.failure.attempted:
        parsed.fail_gap(request.gap)
print(parsed.render().markdown)
```

```rust
use almiladoc::{CancelToken, GapFill, OcrEngine, OcrOutcome, OcrRequest};

struct Mine;

impl OcrEngine for Mine {
    fn recognize(&self, requests: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
        requests.iter().map(|_| OcrOutcome::Fill(GapFill::Markdown("Text".into()))).collect()
    }
}
```

## Failures

| `kind` | `OcrHttp` settles as | Do this |
|---|---|---|
| `rejected` | Unattempted | The server refused the request (4xx): check key, model, `extra_body` |
| `too_large`, `status` 413 | Failed | The server refused the image. For a PDF page lower `dpi` or `max_pixels`; they never resize an embedded image, so downscale its `OcrRequest.image` and send it through `OcrHttp.recognize`, or refuse it unsent with `max_bytes` |
| `too_large`, 2xx `status` | Failed | The reply passed 6 × `max_ocr_text_bytes` + 64 KiB: raise `Limits.max_ocr_text_bytes` |
| `refused` | Failed | The model or its content filter refused the image |
| `unfinished` | Failed | Raise `max_tokens` |
| `no_text`, `protocol` | Unattempted | The reply held no text, or was not a chat completion |
| `wrong_model` | Unattempted | The reply names a model other than `answer_model` |
| `busy` | Unattempted | Retry later; honour `OcrFailure.retry_after` |
| `unreachable` | Unattempted | No answer: check network, proxy, certificates |
| `cancelled` | Unattempted | The `CancelToken` was cancelled |

- Unattempted gaps stay open and the batch stops: call `Parsed.ocr` again later. Failed gaps are
  settled (`ocr_failed`). Branch on `OcrFailure.attempted`, not on `kind`.
- Under `OnLoss.FAIL` an open or failed page gap raises `NeedsOcrError` at `Parsed.render`; a
  filled page raises `IncompleteError` if painting left text, images or content streams out
  (`raster_incomplete`); `OcrRequest.fidelity` counts every kind, colours and annotations too.
- One request per gap and attempt; `OcrResult` carries `usage`, `realized_tier` and `attempts`.
