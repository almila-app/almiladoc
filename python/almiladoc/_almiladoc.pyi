# Hand-written stubs for the compiled module. `tests/test_almiladoc.py` checks
# they stay in step with what the module actually exports.
import collections.abc
import os
from typing import Any, ClassVar, Literal, final

from typing_extensions import Buffer

from almiladoc import Format

__all__ = [
    "CancelToken",
    "CancelledError",
    "Config",
    "ConfigError",
    "Conversion",
    "ConvertError",
    "Converter",
    "Diagnostic",
    "EncryptedError",
    "GapRefused",
    "Identity",
    "ImageOcr",
    "ImagePolicy",
    "IncompleteError",
    "Inspection",
    "InternalError",
    "InvalidConfigError",
    "Limits",
    "MalformedError",
    "MissingPartError",
    "NeedsOcrError",
    "OcrFailure",
    "OcrHttp",
    "OcrRequest",
    "OcrSummary",
    "OnLoss",
    "Parsed",
    "ResourceLimitError",
    "UnsupportedError",
    "sniff",
]

_OcrPages = Literal["all"] | collections.abc.Set[int] | None
_GapRow = tuple[str | None, int | None, int, int | None, int | None, list[str]]

class GapRefused(ValueError):
    """A gap's fill crossed a limit that bounds one fill, and the document is
    unchanged."""

    limit: str
    """The limit: `max_ocr_text_bytes`, `max_nesting_depth` or
    `max_grid_slots`, which a smaller fill may pass."""

class ConfigError(ValueError):
    """A `Limits`, `Config` or `OcrHttp` field, or a raster spec, is out of
    range."""

    limit: str
    """The field: a limit's name such as `max_work`, `threads`, an
    `OcrHttp` argument such as `base_url`, or `raster` for the `dpi` and
    `max_pixels` of `Parsed.ocr_requests` or `OcrHttp`."""
    ceiling: int | None
    """The highest value the limit accepts, for a limit with a ceiling
    (`max_nesting_depth`); `None` otherwise."""
    max: int | None
    """The most threads one call may run on, for `threads`; `None`
    otherwise."""

class ConvertError(Exception):
    """A complete conversion was impossible. Catch this to handle every kind
    of failure, or one of the subclasses below to single one out. A file
    `Converter.convert_path` cannot read or refuses unopened raises `OSError`
    instead."""

    code: str
    """The failure's stable snake_case name, one per subclass: `unsupported`,
    `needs_ocr`, `malformed`, `encrypted`, `resource_limit`, `missing_part`,
    `incomplete`, `invalid_config`, `cancelled` or `internal`."""

class UnsupportedError(ConvertError):
    """The format is unknown, or cannot be converted at all."""

    kind: str
    """Why, as a stable snake_case name: `unknown_format`, `feature_disabled`,
    `format_mismatch` or `not_inspectable`."""
    detected: Format | None
    """The format the content was recognized as when it is not the one
    named, to retry with; `None` otherwise."""

class NeedsOcrError(ConvertError):
    """Pages of a PDF await OCR, which almiladoc does not do, under
    `OnLoss.FAIL`; it is raised before `IncompleteError`."""

    pages: list[int]
    """1-indexed pages that need OCR."""
    page_count: int
    """Pages in the document."""

class MalformedError(ConvertError):
    """The document is structurally unusable: no meaningful content could be
    extracted."""

    part: str | None
    """The package part or stream at fault, or `None` when no single part
    is."""

class EncryptedError(ConvertError):
    """The document is encrypted or password-protected."""

class ResourceLimitError(ConvertError):
    """A safety limit was crossed - input size, decompression, nesting depth,
    node count, repeat expansion, work or retained asset bytes - or the
    allocator refused a reservation."""

    limit: str
    """The limit that was crossed, e.g. `max_input_bytes`, or `allocation`."""
    max: int
    """The limit's value; for `allocation`, the size of the reservation or the
    bound of the read the allocator refused, or 0 where the PDF engine does
    not say."""

class MissingPartError(ConvertError):
    """A part required for any meaningful output is absent."""

    part: str
    """The part or stream that is missing."""

class IncompleteError(ConvertError):
    """Content the source document shows could not be converted, so no
    output is returned that would read as complete."""

    diagnostics: tuple[Diagnostic, ...]
    """Every diagnostic of the conversion, the losses among them."""

class InvalidConfigError(ConvertError):
    """The call's own options do not fit the input."""

    page: int
    """The page `ocr_pages` names past the document's last."""
    page_count: int
    """Pages in the document."""

class CancelledError(ConvertError):
    """The call's `CancelToken` was cancelled."""

class InternalError(ConvertError):
    """almiladoc failed in a way no input should cause; recycle the process
    that raised it."""

@final
class OnLoss:
    """What a conversion does once some content was lost."""

    FAIL: ClassVar[OnLoss]
    """Raise `IncompleteError` with the diagnostics."""
    REPORT: ClassVar[OnLoss]
    """Return the `Conversion`, whose `is_complete` is false."""

@final
class ImagePolicy:
    """How images appear in Markdown without fetching their content."""

    PLACEHOLDER: ClassVar[ImagePolicy]
    ALT_TEXT: ClassVar[ImagePolicy]
    OMIT: ClassVar[ImagePolicy]

@final
class ImageOcr:
    """Which images a parse leaves a gap after for the caller's OCR: `NONE`,
    or `ALL`, every embedded PNG or JPEG judged by its bytes, each other image
    counting `image_not_ocred`. The image itself still appears as
    `ImagePolicy` says."""

    NONE: ClassVar[ImageOcr]
    ALL: ClassVar[ImageOcr]

@final
class Limits:
    """The safety limits of one `Converter`, frozen; each crossing raises
    `ResourceLimitError` naming it, bar one `Parsed.fill_gap` crosses other
    than `max_work`, which raises `GapRefused`. Every field is keyword-only, defaults to
    almiladoc's own default, and must be at least 1; `max_nesting_depth` at
    most 1024 and `max_pages` at most 4294967295. A field out of range raises
    `ConfigError`."""

    def __new__(
        cls,
        *,
        max_entry_bytes: int = ...,
        max_total_bytes: int = ...,
        max_entry_count: int = ...,
        max_nesting_depth: int = ...,
        max_xml_nodes: int = ...,
        max_grid_slots: int = ...,
        max_expansion: int = ...,
        max_expansion_text_bytes: int = ...,
        max_asset_total_bytes: int = ...,
        max_record_depth: int = ...,
        max_records: int = ...,
        max_input_bytes: int = ...,
        max_compression_ratio: int = ...,
        max_output_bytes: int = ...,
        max_work: int = ...,
        max_pages: int = ...,
        max_ocr_text_bytes: int = ...,
    ) -> Limits: ...
    @property
    def max_entry_bytes(self) -> int:
        """Decompressed bytes of one archive entry or compound-file stream."""
    @property
    def max_total_bytes(self) -> int:
        """Decompressed bytes read from one archive in total."""
    @property
    def max_entry_count(self) -> int:
        """Entries in one archive."""
    @property
    def max_nesting_depth(self) -> int:
        """Nesting depth of input structure and of the parsed document, at
        most 1024."""
    @property
    def max_xml_nodes(self) -> int:
        """XML nodes one part holds at once."""
    @property
    def max_grid_slots(self) -> int:
        """Grid positions the tables of one parse materialize together."""
    @property
    def max_expansion(self) -> int:
        """Content-bearing cells a span or repeat expansion produces per
        table."""
    @property
    def max_expansion_text_bytes(self) -> int:
        """Text bytes repeat expansion duplicates per document."""
    @property
    def max_asset_total_bytes(self) -> int:
        """Bytes of embedded assets one document retains."""
    @property
    def max_record_depth(self) -> int:
        """Nesting depth of binary record containers."""
    @property
    def max_records(self) -> int:
        """Binary records visited in one record stream."""
    @property
    def max_input_bytes(self) -> int:
        """Bytes of one input, checked before it is copied or read."""
    @property
    def max_compression_ratio(self) -> int:
        """Ratio of an archive entry's decompressed to compressed size."""
    @property
    def max_output_bytes(self) -> int:
        """Markdown or JSON bytes one call writes."""
    @property
    def max_work(self) -> int:
        """Work units one call charges, bounding its time deterministically."""
    @property
    def max_pages(self) -> int:
        """Slides, sheets, spine items and PDF pages one document holds, each
        numbered by its place in the document's list, at most 4294967295; a
        PDF declaring more is refused before any page is read."""
    @property
    def max_ocr_text_bytes(self) -> int:
        """Bytes of one gap's fill, past which `Parsed.fill_gap` raises
        `GapRefused`."""

@final
class Config:
    """The configuration of one `Converter`, frozen and keyword-only: its
    `Limits`, the threads each call runs on (1 to 64; no reader uses more
    than one), what a loss does, how images appear and which leave gaps for
    OCR. Omitted fields, and `None`, take almiladoc's defaults: `Limits()`,
    1 thread, `OnLoss.FAIL`, `ImagePolicy.PLACEHOLDER` and `ImageOcr.NONE`.
    A thread count out of range raises `ConfigError`."""

    def __new__(
        cls,
        *,
        limits: Limits | None = None,
        threads: int = ...,
        on_loss: OnLoss | None = None,
        images: ImagePolicy | None = None,
        image_ocr: ImageOcr | None = None,
    ) -> Config: ...
    @property
    def limits(self) -> Limits:
        """The safety limits every call enforces."""
    @property
    def threads(self) -> int:
        """Threads each call runs on."""
    @property
    def on_loss(self) -> OnLoss:
        """What a conversion does once some content was lost."""
    @property
    def images(self) -> ImagePolicy:
        """How images appear in Markdown."""
    @property
    def image_ocr(self) -> ImageOcr:
        """Which images a parse leaves a gap after for OCR."""

@final
class CancelToken:
    """A flag any thread sets to cancel the calls it was passed to."""

    def cancel(self) -> None:
        """Cancel calls and `Parsed` operations holding this token with `CancelledError`
        at their next charged step.
        PDF extraction checks between pages; painting checks each step."""
    @property
    def cancelled(self) -> bool:
        """True once `cancel` was called."""

@final
class Diagnostic:
    """What a conversion could not carry into its output, holding no input
    text: equal diagnostics compare and hash equal."""

    @property
    def code(self) -> str:
        """What was reported, a stable snake_case name such as
        `part_unreadable`."""
    @property
    def loss_class(self) -> str:
        """The code's fixed class: `omitted` by a policy, `deferred` behind a
        placeholder, `lost`, or `info`; only `lost` makes a conversion
        incomplete."""
    @property
    def region(self) -> str:
        """Where the content sat, such as `body` or `note`."""
    @property
    def unit(self) -> int | None:
        """The 1-based page, slide, sheet or chapter it sat in, or `None`
        across the document."""
    @property
    def count(self) -> int:
        """Occurrences."""

@final
class Identity:
    """The almiladoc version and the configuration fingerprint that together
    fix a conversion's output."""

    @property
    def version(self) -> str:
        """The almiladoc version."""
    @property
    def config_fingerprint(self) -> str:
        """The lowercase hex SHA-256 of the `Config` fields that shape output,
        all but `limits` and `threads`."""

@final
class Conversion:
    """The result of a conversion: its Markdown, source units and diagnostics."""

    @property
    def units(self) -> tuple[tuple[str, int, int, int], ...]:
        """Source units as `(kind, ordinal, start, end)`, with Unicode string offsets."""

    @property
    def markdown(self) -> str:
        """The GitHub-Flavored Markdown."""
    @property
    def diagnostics(self) -> tuple[Diagnostic, ...]:
        """Every `Diagnostic` of the conversion."""
    @property
    def is_complete(self) -> bool:
        """True when no content was lost and every gap was filled; under
        `OnLoss.FAIL` only an image's gap not filled makes it false."""

@final
class OcrRequest:
    """One gap's image for an OCR run elsewhere, which pickles: `gap`, the row of
    `Parsed.gaps` that `Parsed.fill_gap` and `Parsed.fail_gap` take; `image`,
    its PNG or JPEG bytes; `media_type`, `image/png` or `image/jpeg`;
    `max_text_bytes`, the most bytes of Markdown a fill of it takes; and
    `dpi`, the resolution a page was painted at, `None` for an embedded image."""

    def __new__(
        cls,
        gap: _GapRow,
        image: bytes,
        media_type: str,
        max_text_bytes: int,
        dpi: int | None = None,
    ) -> OcrRequest: ...
    @property
    def gap(self) -> _GapRow:
        """The row of `Parsed.gaps` this request reads the image of."""
    @property
    def image(self) -> bytes:
        """The image's PNG or JPEG bytes."""
    @property
    def media_type(self) -> str:
        """`image/png` or `image/jpeg`."""
    @property
    def max_text_bytes(self) -> int:
        """The most bytes of Markdown a fill of this gap takes."""
    @property
    def dpi(self) -> int | None:
        """The resolution a page was painted at; `None` for an embedded image."""

@final
class OcrHttp:
    """Reads gaps' images through an OpenAI-compatible vision server, posting
    each to `base_url`'s `chat/completions` with `model`: for `Parsed.ocr`, the
    `ocr` of `Converter.convert`, or `recognize`. `api_key` goes only over
    `https` or to loopback; `prompt`, `None` for the default, asks for each
    image's Markdown, after the image when `image_first`; `extra_body` adds
    JSON fields to each request; `max_tokens` and `temperature`, 0 to 2, set
    the sampling; a page is painted as `Parsed.ocr_requests` paints it at
    `dpi` and `max_pixels`; a request the server cannot take yet is tried up
    to `retries` more times, at most 10; and `concurrency`, 1 to 16, requests
    go out at once. A field out of range raises `ConfigError` naming it. No
    socket opens before a request, no proxy is used and no redirect followed."""

    def __new__(
        cls,
        base_url: str,
        model: str,
        *,
        api_key: str | None = None,
        prompt: str | None = None,
        image_first: bool = True,
        extra_body: dict[str, Any] | None = None,
        max_tokens: int = 8192,
        temperature: float = 0.0,
        dpi: int = 150,
        max_pixels: int = 12_845_056,
        retries: int = 3,
        concurrency: int = 1,
    ) -> OcrHttp: ...
    def recognize(
        self,
        requests: collections.abc.Sequence[OcrRequest],
        *,
        cancel: CancelToken | None = None,
    ) -> list[str | OcrFailure]:
        """The text the server reads for each of `requests`, in order, with the
        GIL released once their images are copied: a `str` that fills its gap,
        or an `OcrFailure`. Once one is not attempted or `cancel` is
        cancelled, no later request is sent. A request whose image is not its
        `media_type` raises `ValueError`."""

@final
class OcrFailure:
    """Why OCR read no text for a request, which pickles: `attempted` false when
    it was not tried, which leaves its gap for a later call, and true when it
    failed, which fails its gap; `status`, the HTTP status of its last
    attempt, if it had one."""

    def __new__(cls, *, attempted: bool = True, status: int | None = None) -> OcrFailure: ...
    def __getnewargs_ex__(self) -> tuple[tuple[()], dict[str, object]]: ...
    @property
    def attempted(self) -> bool:
        """False when the request was not tried, true when it failed."""
    @property
    def status(self) -> int | None:
        """The HTTP status of the last attempt, if it had one."""

@final
class OcrSummary:
    """What one `Parsed.ocr` call did to the gaps it could request."""

    @property
    def filled(self) -> int:
        """Gaps filled."""
    @property
    def failed(self) -> int:
        """Gaps failed: the engine failed them, or a limit refused their fill."""
    @property
    def unattempted(self) -> int:
        """Gaps left for a later call: the engine did not attempt them, or a
        request before theirs was not attempted."""

@final
class Parsed:
    """A parsed document, keeping the `Config` it was parsed under and what
    OCR gave its gaps: its model and diagnostics as JSON, the bytes of the
    assets it embeds, and its Markdown through `render`."""

    def render(self) -> Conversion:
        """Render the document to Markdown, as `Converter.convert` does, with
        the parse's diagnostics and each filled gap's text in its place; a
        page's gap not filled under `OnLoss.FAIL` raises `NeedsOcrError`, and
        then a loss `IncompleteError`."""
    def fill_gap(self, gap: _GapRow, markdown: str) -> None:
        """Fill `gap`, a row of `gaps`, with the Markdown OCR read for it,
        plain text included, with the GIL released: raw HTML in it stays text,
        a link its label and an image its alt. A gap no row of `gaps` names
        raises `KeyError`, one filled or failed already `ValueError`, Markdown
        past `Limits.max_ocr_text_bytes`, nested past `max_nesting_depth` or
        whose tables may pass `max_grid_slots` `GapRefused`, and work past
        `max_work` `ResourceLimitError`; each leaves the document as it was."""
    def fail_gap(self, gap: _GapRow) -> None:
        """Record that OCR of `gap`, a row of `gaps`, failed: `render` writes
        `<!-- ocr-failed page N -->` in a page's place and nothing after an
        image, and the diagnostics count `ocr_failed` for it instead of
        `ocr_gap`. A gap no row names raises
        `KeyError`, and one filled or failed already `ValueError`."""
    @property
    def diagnostics(self) -> tuple[Diagnostic, ...]:
        """What the parse could not carry into the document, and what
        rendering it drops, as a tuple of `Diagnostic`: `ocr_gap` for each gap
        not yet filled or failed, `ocr_failed` for each failed one, and
        `raster_incomplete` for what the image of a filled page's latest
        request left out."""
    def to_json(self) -> str:
        """The document and its diagnostics as one JSON object,
        `{schema_version, identity, document, diagnostics}`, written with the
        GIL released. A loss does not raise: the diagnostics say what the
        document lacks. Each level of the document's nesting (a table in a
        cell, a list in a list), which `Limits.max_nesting_depth` bounds,
        nests the JSON at most seven levels deeper: a document nested past
        about 140 levels writes JSON over 1000 levels deep, which
        `json.loads` on Python 3.11 reads only under a raised recursion
        limit."""
    @property
    def gaps(self) -> list[_GapRow]:
        """Every gap of the document in document order, filled and failed
        ones included, as `(unit_kind, unit_ordinal, ordinal, page, asset,
        reasons)`: the unit it stands in, both `None` in a document without
        units; its place among that unit's gaps, or the document's; the
        1-based PDF page or the id of the image, an index of `assets`, OCR
        reads to fill it; and why a page's own text does not serve."""
    def ocr_requests(
        self,
        gaps: collections.abc.Sequence[_GapRow] | None = None,
        *,
        dpi: int = 150,
        max_pixels: int = 12_845_056,
    ) -> list[OcrRequest]:
        """An `OcrRequest` for each gap of `gaps`, rows of `Parsed.gaps`, or every
        gap when `None`, that is not yet filled or failed and asks for an
        image, in document order, made with the GIL released: an OCR run
        elsewhere, in another process too, reads each and passes its text to
        `fill_gap(request.gap, text)`, or calls `fail_gap(request.gap)`. An
        embedded image is copied and a page painted as a PNG at the largest
        dpi up to `dpi` at which its media box fits `max_pixels`, a page that
        cannot be painted asking for none, and a row naming no unsettled gap is passed
        over. A `dpi` outside 50 to 600 or `max_pixels` outside 1048576 to
        41943040 raises `ConfigError` with `limit` `raster`, an int that fits
        neither `OverflowError`, and work past `max_work` `ResourceLimitError`."""
    def ocr(
        self,
        engine: OcrHttp,
        gaps: collections.abc.Sequence[_GapRow] | None = None,
    ) -> OcrSummary:
        """Read the image of each gap of `gaps`, rows of `Parsed.gaps`, or of every
        gap when `None`, that `ocr_requests` would ask for, through `engine`, with
        the GIL released: in batches that fill or fail each gap as `fill_gap` and
        `fail_gap` do, a fill a limit refuses failing its gap, until a batch
        holds a gap the engine did not attempt, which stays for a later call.
        Work past `max_work` raises `ResourceLimitError`, and a cancel of the
        parse's token `CancelledError`, each keeping what earlier batches
        settled."""
    def assets(self) -> list[tuple[int, str, bytes]]:
        """Every embedded asset as `(id, media_type, data)`, `id` being the
        index an image's `asset` source names."""

@final
class Inspection:
    """What `Converter.inspect` reports of a PDF."""

    @property
    def page_count(self) -> int:
        """The pages the document holds."""
    @property
    def pages(self) -> tuple[tuple[int, str, list[str]], ...]:
        """Every page in order as `(page, verdict, reasons)`: its 1-based number,
        `text`, `no_text` or `needs_ocr`, and for `needs_ocr` why its own
        text does not serve."""
    @property
    def diagnostics(self) -> tuple[Diagnostic, ...]:
        """Every `Diagnostic` a parse of the document reports."""

@final
class Converter:
    """Converts documents under one `Config`, `Config()` when none is given;
    share it across threads and calls. Each call releases the GIL while it
    works, so a cancellation comes only through its `CancelToken`, never
    from Ctrl-C.

    Without a `format`, a call detects it from the content once the input
    is admitted; a `format` the content contradicts raises
    `UnsupportedError` with `kind` `format_mismatch` and the `detected`
    format to retry with.

    `ocr_pages`, `"all"` or a set of 1-based pages, makes each selected PDF
    page a gap with reason `caller_requested` whatever its text, discarded
    without loss; a page past the last raises `InvalidConfigError`, any other
    page outside 1 to 4294967295 `ValueError`, and any other format ignores it.

    `ocr`, an `OcrHttp`, reads a conversion's gaps before it renders, as
    `parse`, `Parsed.ocr` and `Parsed.render` do in turn."""

    def __new__(cls, config: Config | None = None) -> Converter: ...
    @property
    def identity(self) -> Identity:
        """The `Identity` that fixes this converter's output."""
    def convert(
        self,
        data: Buffer,
        format: Format | None = None,
        *,
        cancel: CancelToken | None = None,
        ocr_pages: _OcrPages = None,
        ocr: OcrHttp | None = None,
    ) -> Conversion:
        """Convert `data`, `bytes` or any contiguous byte buffer, to Markdown.
        `bytes` is read in place; any other buffer is copied once, after its
        length is checked against `max_input_bytes`. CSV carries no
        signature, so it has to be named."""
    def parse(
        self,
        data: Buffer,
        format: Format | None = None,
        *,
        cancel: CancelToken | None = None,
        ocr_pages: _OcrPages = None,
    ) -> Parsed:
        """Parse `data`, as `convert` takes it, into the document model; a PDF's
        pages that await OCR and, under `ImageOcr.ALL`, each embedded PNG or
        JPEG are its `gaps`."""
    def inspect(
        self,
        data: Buffer,
        format: Format | None = None,
        *,
        cancel: CancelToken | None = None,
        ocr_pages: _OcrPages = None,
    ) -> Inspection:
        """Inspect a PDF, as `convert` takes it, without rendering it: its pages,
        each with the verdict `parse` rules for it, and that parse's
        diagnostics. Any other format raises `UnsupportedError` with `kind`
        `not_inspectable`."""
    def sniff(self, data: Buffer, *, cancel: CancelToken | None = None) -> Format | None:
        """The format `data`'s content names, detected as a call without a
        `format` detects it: under this converter's limits and `cancel`, once
        `data` is admitted as `convert` admits it. `None` for CSV, which
        carries no signature, and for anything unrecognized."""
    def convert_path(
        self,
        path: str | os.PathLike[str],
        format: Format | None = None,
        *,
        cancel: CancelToken | None = None,
        ocr_pages: _OcrPages = None,
        ocr: OcrHttp | None = None,
    ) -> Conversion:
        """Convert the regular file at `path` to Markdown. Its size is checked
        against `max_input_bytes` before it is read, and a directory raises
        `OSError` unopened, as does a FIFO or device on Unix. Without a
        `format`, the content names it, else the path's extension does."""

def sniff(data: Buffer) -> Format | None:
    """The format `data`'s content names, detected under the default limits;
    `None` for CSV, which carries no signature, and for anything
    unrecognized. `bytes` is read in place and any other contiguous byte
    buffer copied once, after its length is checked against the default
    `max_input_bytes`."""
