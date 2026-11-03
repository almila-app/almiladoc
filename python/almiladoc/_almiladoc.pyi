import collections.abc
import datetime
import os
from typing import Any, ClassVar, Literal, final

from typing_extensions import Buffer

from almiladoc import Format

__all__ = [
    "CancelToken",
    "CancelledError",
    "CommentPolicy",
    "Config",
    "ConfigError",
    "Conversion",
    "ConvertError",
    "Converter",
    "Diagnostic",
    "EncryptedError",
    "GapRefused",
    "HeaderFooterPolicy",
    "HiddenPolicy",
    "Identity",
    "ImageOcr",
    "ImagePolicy",
    "IncompleteError",
    "Inspection",
    "InternalError",
    "Limits",
    "MalformedError",
    "MissingPartError",
    "NeedsOcrError",
    "OcrAttempt",
    "OcrFailure",
    "OcrHttp",
    "OcrLine",
    "OcrRefusal",
    "OcrRequest",
    "OcrResult",
    "OcrSummary",
    "OnLoss",
    "PageOutOfRangeError",
    "Parsed",
    "PdfScanPolicy",
    "RasterFidelity",
    "ResourceLimitError",
    "RevisionPolicy",
    "TokenUsage",
    "UnsupportedError",
    "sniff",
]

_OcrPages = Literal["all"] | collections.abc.Set[int] | None
_GapRow = tuple[str | None, int | None, int, int | None, int | None, list[str]]

class GapRefused(ValueError):
    """A gap's fill crossed a limit that bounds one fill, and the document is
    unchanged."""

    limit: str
    """The per-fill size, nesting, node, grid or expansion limit that a smaller fill may pass."""

class ConfigError(ValueError):
    """A `Limits`, `Config` or `OcrHttp` field, or a raster spec, is out of
    range."""

    limit: str
    """The field: a limit's name such as `max_work`, `threads`, an
    `OcrHttp` argument such as `base_url`, or `raster` for the `dpi`,
    `long_edge` and `max_pixels` of `Parsed.ocr_requests` or `OcrHttp`, a `dpi`
    given with a `long_edge`, or a zero `max_bytes`."""
    ceiling: int | None
    """The highest value the limit accepts, set only when the value passed
    exceeds it (`max_nesting_depth` over 1024, `max_pages` over 4294967295);
    `None` otherwise, a zero included."""
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
    `incomplete`, `page_out_of_range`, `cancelled` or `internal`."""

class UnsupportedError(ConvertError):
    """The format is unknown, or cannot be converted at all."""

    kind: str
    """Why, as a stable snake_case name: `unknown_format`, `feature_disabled`,
    `format_mismatch` or `not_inspectable`."""
    detected: Format | None
    """The format the content was recognized as when it is not the one
    named, to retry with; `None` otherwise."""

class NeedsOcrError(ConvertError):
    """Pages of a PDF have a gap that awaits OCR or whose OCR failed, under
    `OnLoss.FAIL`; it is raised on conversion or `Parsed.render`, before
    `IncompleteError`."""

    pages: list[int]
    """1-indexed pages whose gap is not filled."""
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

class PageOutOfRangeError(ConvertError):
    """`ocr_pages` names a page past the PDF's last."""

    page: int
    """The highest page `ocr_pages` names."""
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
    """Raise `NeedsOcrError` for a PDF page gap not filled, else `IncompleteError`
    with the diagnostics once content was lost."""
    REPORT: ClassVar[OnLoss]
    """Return the `Conversion`, whose `is_complete` is false."""

@final
class ImagePolicy:
    """How images appear in Markdown without fetching their content."""

    PLACEHOLDER: ClassVar[ImagePolicy]
    """Retain its position as an asset reference, source link or visible marker."""
    ALT_TEXT: ClassVar[ImagePolicy]
    """Render its alternative text, or `Image` when no text is supplied."""
    OMIT: ClassVar[ImagePolicy]
    """Render nothing for the image."""

@final
class CommentPolicy:
    """Whether supported comments appear as isolated GFM footnotes."""

    OMIT: ClassVar[CommentPolicy]
    """Count source references without reading comment parts."""
    EMIT: ClassVar[CommentPolicy]
    """Preserve comments as GFM footnotes, with replies inside their parent."""
    def __repr__(self) -> str: ...

@final
class HiddenPolicy:
    """Whether supported hidden source content appears in the output."""

    OMIT: ClassVar[HiddenPolicy]
    """Count hidden content without reading its side parts."""
    EMIT: ClassVar[HiddenPolicy]
    """Preserve supported hidden text, structures and units."""
    def __repr__(self) -> str: ...

@final
class RevisionPolicy:
    """Which tracked text revision view appears in the output."""

    ACCEPT: ClassVar[RevisionPolicy]
    """Keep inserted text and remove deleted text."""
    REJECT: ClassVar[RevisionPolicy]
    """Keep deleted text and remove inserted text."""
    def __repr__(self) -> str: ...

@final
class HeaderFooterPolicy:
    """Whether supported headers and footers appear in the output."""

    OMIT: ClassVar[HeaderFooterPolicy]
    """Count omitted source values without reading them."""
    EMIT: ClassVar[HeaderFooterPolicy]
    """Preserve their content, page-dependent fields as fixed tokens, in labelled
    blockquotes."""

@final
class ImageOcr:
    """Which images a parse leaves a gap after for the caller's OCR: `NONE`,
    or `ALL`, every embedded PNG or JPEG judged by its bytes, each other image
    counting `image_not_ocred`. The image itself still appears as
    `ImagePolicy` says. A PDF's images get no gap: its pages do, as
    `PdfScanPolicy` and `ocr_pages` say."""

    NONE: ClassVar[ImageOcr]
    """No image."""
    ALL: ClassVar[ImageOcr]
    """Every embedded PNG or JPEG outside a PDF."""

@final
class PdfScanPolicy:
    """Which PDF pages a parse treats as scans, for the caller's OCR:
    `CONSERVATIVE`, a page with an image and no readable text, or one at
    least half covered by images whose only text is margin text or running
    heads and folios (at 90% coverage or more, text that recurs on other
    pages too); `COVERAGE_FIRST`, those pages and every page at least half
    covered by images whatever native text it holds, which OCR then replaces.
    Other formats ignore it."""

    CONSERVATIVE: ClassVar[PdfScanPolicy]
    """A page with an image and no readable text, or one at least half covered
    by images whose only text is margin text or running heads and folios; at
    90% coverage or more, text that recurs on other pages also qualifies."""
    COVERAGE_FIRST: ClassVar[PdfScanPolicy]
    """The conservative pages, and every page at least half covered by images
    whatever native text it holds, which OCR then replaces."""

@final
class Limits:
    """The safety limits of one `Converter`, frozen; each crossing raises
    `ResourceLimitError` naming it, bar one `Parsed.fill_gap` crosses other
    than `max_work`, which raises `GapRefused`. Every field is keyword-only,
    defaults to almiladoc's own default (stated on the field and listed by
    `repr(Limits())`) and must be at least 1; `max_nesting_depth` at most 1024
    and `max_pages` at most 4294967295. A field below 0 or too big for 64 bits
    raises `OverflowError`, a non-int one, `None` included, `TypeError`, and
    any other one out of range `ConfigError`."""

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
        """Decompressed bytes of one archive entry, OfficeArt picture, embedded
        OLE object or compound-file stream. Default 128 MiB."""
    @property
    def max_total_bytes(self) -> int:
        """Decompressed bytes one parse inflates (archive entries, OfficeArt
        pictures and embedded OLE objects). Default 512 MiB."""
    @property
    def max_entry_count(self) -> int:
        """Entries in one archive. Default 100,000."""
    @property
    def max_nesting_depth(self) -> int:
        """Nesting depth of XML elements, RTF groups, Word fields and equation
        records in the input, and of the parsed document's lists, cells, quotes
        and links, at most 1024. Default 256."""
    @property
    def max_xml_nodes(self) -> int:
        """XML nodes one part or opted-in OCR table fragment holds at once. Default 2,000,000."""
    @property
    def max_grid_slots(self) -> int:
        """Grid positions the tables of one parse materialize together, every
        table but a CSV's. Default 4,000,000."""
    @property
    def max_expansion(self) -> int:
        """Content-bearing cells a span or repeat expansion produces per
        table. Default 4,000,000."""
    @property
    def max_expansion_text_bytes(self) -> int:
        """Text bytes OpenDocument table repeats duplicate per document. Default 64 MiB."""
    @property
    def max_asset_total_bytes(self) -> int:
        """Bytes of embedded assets one document retains. Default 128 MiB."""
    @property
    def max_record_depth(self) -> int:
        """Nesting depth of binary record containers. Default 64."""
    @property
    def max_records(self) -> int:
        """Binary records visited in one record stream. Default 16,000,000."""
    @property
    def max_input_bytes(self) -> int:
        """Bytes of one input, checked before it is copied or read. Default 256 MiB."""
    @property
    def max_compression_ratio(self) -> int:
        """Ratio of an archive entry's, OfficeArt picture's or embedded OLE
        object's decompressed to compressed size. Default 1,000."""
    @property
    def max_output_bytes(self) -> int:
        """Markdown or JSON bytes one call writes. Default 256 MiB."""
    @property
    def max_work(self) -> int:
        """Work units one conversion charges over its whole life, bounding its
        time deterministically: a `Parsed` keeps the count, each `ocr_requests`,
        `ocr` batch and fill adding to it, and each `render` or `to_json`
        charging on from it without adding to it. Default 536,870,912."""
    @property
    def max_pages(self) -> int:
        """Slides, sheets, spine items and PDF pages one document holds, each
        numbered by its place in the document's list, at most 4294967295; a
        PDF declaring more is refused before any page is read. Default 100,000."""
    @property
    def max_ocr_text_bytes(self) -> int:
        """Bytes of one gap's fill, past which `Parsed.fill_gap` raises
        `GapRefused`; located lines also count 32 bytes per box. Default 1 MiB."""

@final
class Config:
    """The configuration of one `Converter`, frozen and keyword-only: its
    `Limits`, the threads each call runs on (1 to 64; only `Parsed.ocr_requests`
    and `Parsed.ocr`, which the `ocr` of `Converter.convert` and
    `Converter.convert_path` runs, use more than one, painting PDF pages at
    once), what a loss does, how images appear, which leave gaps for OCR and
    which PDF pages count as scans. Omitted fields take almiladoc's defaults:
    `Limits()`, 1 thread, `OnLoss.FAIL`, `ImagePolicy.PLACEHOLDER`,
    `ImageOcr.NONE`, `PdfScanPolicy.CONSERVATIVE`, `HeaderFooterPolicy.OMIT`,
    `CommentPolicy.OMIT`, `HiddenPolicy.OMIT` and `RevisionPolicy.ACCEPT`; so
    does `None`, for every field but `threads`. `threads` below 0 or too big
    for 64 bits raises `OverflowError`, a non-int, `None` included,
    `TypeError`, and any other value outside 1 to 64 `ConfigError`."""

    def __new__(
        cls,
        *,
        limits: Limits | None = None,
        threads: int = ...,
        on_loss: OnLoss | None = None,
        images: ImagePolicy | None = None,
        image_ocr: ImageOcr | None = None,
        pdf_scans: PdfScanPolicy | None = None,
        headers_footers: HeaderFooterPolicy | None = None,
        comments: CommentPolicy | None = None,
        hidden: HiddenPolicy | None = None,
        revisions: RevisionPolicy | None = None,
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
    @property
    def pdf_scans(self) -> PdfScanPolicy:
        """Which PDF pages a parse treats as scans."""

    @property
    def hidden(self) -> HiddenPolicy:
        """Whether supported hidden source content appears."""
        ...
    @property
    def revisions(self) -> RevisionPolicy:
        """Which tracked text revision view appears."""
        ...
    @property
    def comments(self) -> CommentPolicy:
        """Whether supported source comments appear as isolated footnotes."""
        ...
    @property
    def headers_footers(self) -> HeaderFooterPolicy:
        """Whether the page headers and footers a document declares appear."""

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
        placeholder or gap, `lost`, or `info`; only `lost` raises
        `IncompleteError` under `OnLoss.FAIL`."""
    @property
    def region(self) -> str:
        """Where the content sat, such as `body` or `note`."""
    @property
    def unit(self) -> int | None:
        """The 1-based page, slide, sheet or spine item it sat in, or `None`
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
class OcrLine:
    """A literal horizontal line with finite normalized left, top, right, bottom bounds."""

    BOX_BYTES: ClassVar[int]
    """32: the bytes each line counts against `max_ocr_text_bytes` besides its text."""
    def __new__(cls, text: str, bounds: tuple[float, float, float, float]) -> OcrLine: ...
    @property
    def text(self) -> str:
        """The literal UTF-8 text."""
    @property
    def bounds(self) -> tuple[float, float, float, float]:
        """The normalized image bounds."""
    def __getnewargs__(self) -> tuple[str, tuple[float, float, float, float]]: ...

@final
class RasterFidelity:
    """What painting a page left out or replaced, by kind, which pickles:
    `text_runs`, `images`, `inline_images`, `shadings`, `soft_masks`,
    `content_streams` and `annotations` not painted or not applied as set,
    `substituted_text_runs` painted in a standard face standing in for the
    page's font, and `colors` in a colour the painter does not convert, painted
    in the shade of their ink. `lost` is what `raster_incomplete` counts: every
    kind but shadings, soft masks, substituted runs, colours and annotations.
    An annotation without an appearance stream, or whose `/AS` selects none, is
    neither painted nor counted, and NoZoom/NoRotate are not applied, so zero
    counts are not a completeness claim."""

    def __new__(
        cls,
        text_runs: int = 0,
        images: int = 0,
        inline_images: int = 0,
        shadings: int = 0,
        soft_masks: int = 0,
        content_streams: int = 0,
        substituted_text_runs: int = 0,
        colors: int = 0,
        annotations: int = 0,
    ) -> RasterFidelity: ...
    @property
    def text_runs(self) -> int:
        """Text runs not painted."""
    @property
    def images(self) -> int:
        """Images not painted."""
    @property
    def inline_images(self) -> int:
        """Inline images not painted."""
    @property
    def shadings(self) -> int:
        """Shadings and pattern colours not painted."""
    @property
    def soft_masks(self) -> int:
        """Soft masks not applied as set."""
    @property
    def content_streams(self) -> int:
        """Content streams that could not be decoded."""
    @property
    def substituted_text_runs(self) -> int:
        """Text runs painted in a standard face standing in for the page's font."""
    @property
    def colors(self) -> int:
        """Fills, strokes, text and stencil masks in a colour the painter does not
        convert, painted in the shade of their ink."""
    @property
    def annotations(self) -> int:
        """Annotations whose appearance was not painted, or painted only in part."""
    @property
    def lost(self) -> int:
        """What the painting dropped from the page, as `raster_incomplete` counts it."""
    def __getnewargs__(self) -> tuple[int, int, int, int, int, int, int, int, int]: ...

@final
class OcrRequest:
    """One gap's image for an OCR run elsewhere, which pickles: `gap`, the row of
    `Parsed.gaps` that `Parsed.fill_gap` and `Parsed.fail_gap` take; `image`,
    its PNG or JPEG bytes; `media_type`, `image/png` or `image/jpeg`;
    `max_text_bytes`, the most bytes of Markdown a fill of it takes; `dpi`, the
    resolution a page was painted at, whole, 1 to 2**32 - 1, rounded and clamped
    for an exact `long_edge`, and `fidelity`, what the painting left out, each
    `None` for an embedded image; `width` and `height`, the pixel size of a
    painted page or an embedded image as its header declares it, `None` when
    that does not read."""

    def __new__(
        cls,
        gap: _GapRow,
        image: bytes,
        media_type: str,
        max_text_bytes: int,
        dpi: int | None = None,
        width: int | None = None,
        height: int | None = None,
        fidelity: RasterFidelity | None = None,
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
        """The resolution a page was painted at, whole and at least 1; `None` for an
        embedded image."""
    @property
    def width(self) -> int | None:
        """The pixel width of a painted page, or of an embedded image as its header
        declares it; `None` when that does not read."""
    @property
    def height(self) -> int | None:
        """The pixel height of a painted page, or of an embedded image as its header
        declares it; `None` when that does not read."""
    @property
    def fidelity(self) -> RasterFidelity | None:
        """What painting a page left out; `None` for an embedded image."""

_OcrRefusalKind = Literal["unpaintable", "over_pixels", "over_bytes"]

@final
class OcrRefusal:
    """A gap that has no image to read, which pickles and stays unfilled: `gap`,
    the row of `Parsed.gaps` it stands for, and `kind`, `unpaintable` for a
    page that cannot be painted, `over_pixels` for an exact `long_edge` that
    would hold more than `max_pixels`, whose `pixels` are the pixels it would
    have held, or `over_bytes` for an image over `max_bytes`, whose `bytes` are
    the size it would have held; each is `None` for the other kinds."""

    def __new__(
        cls,
        gap: _GapRow,
        kind: _OcrRefusalKind,
        bytes: int | None = None,
        pixels: int | None = None,
    ) -> OcrRefusal: ...
    @property
    def gap(self) -> _GapRow:
        """The row of `Parsed.gaps` that has no image."""
    @property
    def kind(self) -> _OcrRefusalKind:
        """`unpaintable`, `over_pixels` or `over_bytes`."""
    @property
    def bytes(self) -> int | None:
        """The bytes the encoded image would have held, for `over_bytes`."""
    @property
    def pixels(self) -> int | None:
        """The pixels the page would have held, for `over_pixels`."""
    def __getnewargs__(self) -> tuple[_GapRow, _OcrRefusalKind, int | None, int | None]: ...

_OcrFailureKind = Literal[
    "rejected",
    "too_large",
    "refused",
    "unfinished",
    "no_text",
    "protocol",
    "wrong_model",
    "busy",
    "unreachable",
    "cancelled",
]

@final
class OcrHttp:
    """Reads gaps' images through an OpenAI-compatible vision server, posting each
    to `base_url`'s `chat/completions` with `model`: for `Parsed.ocr`, the `ocr` of
    `Converter.convert` and `Converter.convert_path`, or `recognize`. No socket
    opens before a request and no redirect is followed. An int below 0, or too
    big for its argument's unsigned 32- or 64-bit type, raises `OverflowError`,
    a value of the wrong type `TypeError`, and any other field out of range
    `ConfigError` naming it.

    - `base_url`: `http` or `https` with a host and no userinfo, query or fragment.
    - `api_key`: visible ASCII, sent as a bearer token only over `https` or to
      loopback.
    - `prompt`: what the model is asked of each image. `None` is "Transcribe all
      text in this image as Markdown, keeping its headings, lists and tables, and
      reply with the Markdown alone."
    - `layout`: `image_then_text` or `text_then_image` in one user message, or
      `developer_then_image` or `system_then_image` with the prompt in a message
      of its own before the image's.
    - `extra_body`: JSON fields added to each request, none of `messages`, `model`,
      `max_tokens`, `max_completion_tokens`, `temperature`, `n`, `stream` and
      `service_tier`.
    - `max_tokens`: sent as `token_limit_field`, `max_tokens` or
      `max_completion_tokens`. `temperature` is 0 to 2, or `None` to send none.
    - `image_detail`: `low`, `high` or `auto`, sent with each image.
    - `answer_model`: a reply must name it when one is set.
    - `service_tiers`: up to 4 tokens of up to 32 letters, digits, `-` or `_`; the
      first is sent.
    - `tier_fallback`: `(codes, message_prefix)`, which needs at least 2
      `service_tiers`, one code or a prefix, and at most 16 codes. A 429 whose
      error code is among the codes or whose message starts with the prefix moves
      the request to its next tier, each such 429 one tier on until the last.
    - `preserve_table_spans`: a reply is a `fill_gap_tables` fill, keeping HTML and
      OTSL table spans at a top-level gap and showing each merge once below it.
    - `dpi`, `long_edge`, `max_pixels`, `max_bytes`: a page is painted as
      `Parsed.ocr_requests` paints it, at 150 dpi unless one of `dpi` and
      `long_edge` is set, and `Parsed.ocr` refuses an image over `max_bytes`, `None`
      for no bound.
    - `retries`: at most 10 more tries of a request the server cannot take yet,
      each after 0.5 s doubling to 30 s, or its `Retry-After` seconds up to 60 s.
    - `concurrency`: 1 to 16 requests at once.
    - `root_certificates_pem`: certificates only, at most 1 MiB and 256 of them;
      they replace Mozilla roots, including for an HTTPS proxy.
    - `proxy`: an HTTP(S) CONNECT proxy URI of at most 8 KiB, requiring HTTPS when
      `api_key` is set; without it connections are direct.

    A reply body must be fewer than six times the request's `max_text_bytes` plus
    64 KiB. Timeouts are fixed: 10 s to connect, 60 s to send the request,
    60 s to send its body, 300 s to receive the response, 60 s to receive its
    body."""

    def __new__(
        cls,
        base_url: str,
        model: str,
        *,
        api_key: str | None = None,
        prompt: str | None = None,
        layout: Literal[
            "image_then_text", "text_then_image", "developer_then_image", "system_then_image"
        ] = "image_then_text",
        extra_body: dict[str, Any] | None = None,
        max_tokens: int = 8192,
        token_limit_field: Literal["max_tokens", "max_completion_tokens"] = "max_tokens",
        temperature: float | None = 0.0,
        image_detail: Literal["low", "high", "auto"] | None = None,
        answer_model: str | None = None,
        service_tiers: collections.abc.Sequence[str] = (),
        tier_fallback: tuple[collections.abc.Sequence[str], str] | None = None,
        preserve_table_spans: bool = False,
        dpi: int | None = None,
        long_edge: int | None = None,
        max_pixels: int = 12_845_056,
        max_bytes: int | None = None,
        retries: int = 3,
        concurrency: int = 1,
        root_certificates_pem: bytes | None = None,
        proxy: str | None = None,
    ) -> OcrHttp: ...
    @property
    def root_certificates_pem(self) -> bytes | None:
        """The caller's replacement PEM bundle, absent for Mozilla roots."""
    @property
    def proxy(self) -> str | None:
        """The configured proxy URI, including caller-supplied credentials."""
    def recognize(
        self,
        requests: collections.abc.Sequence[OcrRequest],
        *,
        cancel: CancelToken | None = None,
    ) -> list[OcrResult]:
        """An `OcrResult` for each of `requests`, in order, with the GIL released
        once their images are copied. Once one is not attempted or `cancel` is
        cancelled, no later request is sent, and a cancel ends those in flight.
        A request whose image is not its `media_type` raises `ValueError`."""

@final
class OcrResult:
    """What OCR made of one request, which pickles and holds no reply text but
    its fill: `text`, the Markdown that fills its gap, through
    `Parsed.fill_gap_tables` when `tables`, else `Parsed.fill_gap`; or
    `failure`; with `usage`, the tokens a reply billed, `realized_tier`, the
    index in `service_tiers` of the tier a reply was served on, and
    `attempts`, each request sent."""

    def __new__(
        cls,
        *,
        text: str | None = None,
        tables: bool = False,
        failure: OcrFailure | None = None,
        usage: TokenUsage | None = None,
        realized_tier: int | None = None,
        attempts: collections.abc.Sequence[OcrAttempt] = (),
    ) -> OcrResult: ...
    def __getnewargs_ex__(self) -> tuple[tuple[()], dict[str, object]]: ...
    @property
    def text(self) -> str | None:
        """The Markdown that fills the gap; `None` with a failure."""
    @property
    def tables(self) -> bool:
        """Whether `text` fills through `Parsed.fill_gap_tables`."""
    @property
    def failure(self) -> OcrFailure | None:
        """Why no text was read; `None` with a text."""
    @property
    def usage(self) -> TokenUsage | None:
        """The tokens a reply billed, a refused one included, when it named them."""
    @property
    def realized_tier(self) -> int | None:
        """The index in `service_tiers` of the tier a reply was served on."""
    @property
    def attempts(self) -> list[OcrAttempt]:
        """Each request sent, in order."""

@final
class OcrFailure:
    """Why OCR read no text for a request, which pickles: `kind`, what kept it
    from text, one of `rejected`, `too_large`, `refused`, `unfinished`,
    `no_text`, `protocol`, `wrong_model`, `busy`, `unreachable` or
    `cancelled`; `attempted` false when it was not tried, which leaves its
    gap for a later call, and true when it failed, which fails its gap;
    `status`, the HTTP status of its last attempt, if it had one; and
    `retry_after`, the longest wait a server asked for, as seconds or an
    aware UTC `datetime`, a later wait of the other form replacing it."""

    def __new__(
        cls,
        *,
        kind: _OcrFailureKind,
        attempted: bool = True,
        status: int | None = None,
        retry_after: int | datetime.datetime | None = None,
    ) -> OcrFailure: ...
    def __getnewargs_ex__(self) -> tuple[tuple[()], dict[str, object]]: ...
    @property
    def kind(self) -> _OcrFailureKind:
        """What kept the request from text."""
    @property
    def attempted(self) -> bool:
        """False when the request was not tried, true when it failed."""
    @property
    def status(self) -> int | None:
        """The HTTP status of the last attempt, if it had one."""
    @property
    def retry_after(self) -> int | datetime.datetime | None:
        """The longest wait a server asked for, a later wait of the other form
        replacing it: seconds, or an aware UTC `datetime`."""

@final
class TokenUsage:
    """The tokens one reply billed, which pickles."""

    def __new__(cls, input_uncached: int, input_cached: int, output: int) -> TokenUsage: ...
    def __getnewargs__(self) -> tuple[int, int, int]: ...
    @property
    def input_uncached(self) -> int:
        """Prompt tokens not read from a cache."""
    @property
    def input_cached(self) -> int:
        """Prompt tokens read from a cache."""
    @property
    def output(self) -> int:
        """Reply tokens."""

@final
class OcrAttempt:
    """One request sent for an image, which pickles: `tier`, the index in
    `service_tiers` of the tier asked for, and `status`, the reply's HTTP
    status, when one arrived."""

    def __new__(cls, tier: int | None = None, status: int | None = None) -> OcrAttempt: ...
    def __getnewargs__(self) -> tuple[int | None, int | None]: ...
    @property
    def tier(self) -> int | None:
        """The index in `service_tiers` of the tier asked for."""
    @property
    def status(self) -> int | None:
        """The reply's HTTP status, when one arrived."""

@final
class OcrSummary:
    """What one `Parsed.ocr` call did to the gaps it could request."""

    @property
    def filled(self) -> int:
        """Gaps filled."""
    @property
    def failed(self) -> int:
        """Gaps failed: the engine failed them, or their fill was refused by a
        limit or for an invalid table."""
    @property
    def unattempted(self) -> int:
        """Gaps left for a later call: the engine did not attempt them, or a
        request before theirs was not attempted."""
    @property
    def refused(self) -> int:
        """Gaps left unfilled because no image could be made of them: a page
        that cannot be painted, or an image over `max_bytes`."""

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
        a link its label and an image its alt; TeX math (`\\(..\\)`, `\\[..\\]`,
        `$..$`, `$$..$$`, a `math` fence) becomes math and OTSL table markup
        (`<fcel>A<lcel><nl>`) a table whose merged cell shows once. A gap no row of
        `gaps` names raises `KeyError`, one filled or failed already
        `ValueError`, Markdown past `Limits.max_ocr_text_bytes`, nested past
        `max_nesting_depth` or whose tables may pass `max_grid_slots`
        `GapRefused`, work past `max_work` `ResourceLimitError`, and a cancel of
        the parse's token `CancelledError`; each leaves the document as it was."""
    def fill_gap_lines(self, gap: _GapRow, lines: collections.abc.Sequence[OcrLine]) -> None:
        """Fill `gap` with literal `OcrLine` values, grouping columns and paragraphs in the core.
        Text and 32 bytes per box must fit `max_ocr_text_bytes`; refusal leaves the gap unchanged.

        Raises `KeyError` for an unknown gap, `ValueError` for a settled gap, `GapRefused`
        past `max_ocr_text_bytes` or another per-fill limit, `ResourceLimitError` past
        `max_work`, and `CancelledError` once the parse's token is cancelled."""
    def fill_gap_tables(self, gap: _GapRow, markdown: str) -> None:
        """Fill `gap` with Markdown, standalone top-level HTML tables and OTSL tables.
        Only table/tr/td, text and positive spans are accepted; a top-level gap renders merged
        cells as safe HTML and a nested gap shows each merge once. Unsupported tables or
        context raise `ValueError`, per-fill limits `GapRefused`, work past `max_work`
        `ResourceLimitError`, and a cancel of the parse's token `CancelledError`, each leaving
        the gap unfilled. A gap no row of `gaps` names raises `KeyError`, and one filled or
        failed already `ValueError`."""
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
        dpi: int | None = None,
        long_edge: int | None = None,
        max_pixels: int = 12_845_056,
        max_bytes: int | None = None,
    ) -> list[OcrRequest | OcrRefusal]:
        """An `OcrRequest`, or an `OcrRefusal` where no image can be made, for each
        gap of `gaps`, rows of `Parsed.gaps`, or every gap when `None`, that is
        not yet filled or failed and asks for an image, in document order, made
        with the GIL released: an OCR run elsewhere, in another process too,
        reads each request and passes its text to `fill_gap(request.gap, text)`,
        or calls `fail_gap(request.gap)`. An embedded image is copied, its
        request holding the pixel `width` and `height` its header declares. A
        page is painted as a PNG at the largest dpi up to `dpi`, 150 unless
        `long_edge` is set, at which its media box fits `max_pixels`,
        or with exactly `long_edge` pixels on its long edge, its request holding
        the pixel `width` and `height` and a `RasterFidelity` of what the
        painting left out. A page that cannot be painted is an `unpaintable`
        refusal, an exact `long_edge` that would hold more than `max_pixels`,
        never painted smaller, an `over_pixels` one, and an image over
        `max_bytes` encoded bytes, `None` for no bound, an `over_bytes` one;
        each gap stays unfilled, for a later call or `fill_gap`. A row naming
        no unsettled gap is passed over. A `dpi` outside 50 to 600, a
        `long_edge` outside 64 to 16384, both of them given, `max_pixels`
        outside 1048576 to 41943040 or a `max_bytes` of 0 raises `ConfigError`
        with `limit` `raster`, an int that fits none of them `OverflowError`,
        and work past `max_work` `ResourceLimitError`."""
    def ocr(
        self,
        engine: OcrHttp,
        gaps: collections.abc.Sequence[_GapRow] | None = None,
    ) -> OcrSummary:
        """Read the image of each gap of `gaps`, rows of `Parsed.gaps`, or of every
        gap when `None`, that `ocr_requests` would ask for, through `engine`, with
        the GIL released: in batches that fill or fail each gap as `fill_gap` and
        `fail_gap` do, a fill refused by a limit or for an invalid table failing
        its gap, until a batch holds a gap the engine did not attempt, which
        stays for a later call. A gap `ocr_requests` would refuse is no request:
        it stays unfilled and the summary counts it as `refused`. Work past
        `max_work` raises `ResourceLimitError`, and a cancel of the parse's token
        `CancelledError`, each keeping what earlier batches settled."""
    def assets(self) -> list[tuple[int, str, bytes]]:
        """Every embedded asset as `(id, media_type, data)`, `id` being the
        index an image's `asset` source names: body, stored notes, then headers
        and footers in first-reference order, followed by unreferenced payloads."""

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
    without loss; a page past the last raises `PageOutOfRangeError`, any other
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
        JPEG of any other format are its `gaps`."""
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
    def render_ocr(self, markdown: str, *, cancel: CancelToken | None = None) -> Conversion:
        """Render `markdown`, the text OCR read from one image outside any document, as
        `Parsed.fill_gap` reads it for a gap and a filled page writes it, with the GIL
        released: raw HTML in it stays text, a link its label and an image its alt; TeX
        math (`\\(..\\)`, `\\[..\\]`, `$..$`, `$$..$$`, a `math` fence) becomes math and OTSL
        table markup (`<fcel>A<lcel><nl>`) a table whose merged cell shows once. Empty or
        blank text renders `""`. Text past `Limits.max_ocr_text_bytes` raises
        `ResourceLimitError` before it is copied, as does nesting past `max_nesting_depth`,
        tables past `max_grid_slots` or `max_expansion`, work past `max_work` and output
        past `max_output_bytes`; a cancelled `cancel` raises `CancelledError`."""
    def render_ocr_tables(self, markdown: str, *, cancel: CancelToken | None = None) -> Conversion:
        """Render `markdown` as `render_ocr` does, reading standalone top-level HTML tables and
        OTSL tables too and writing a merged cell as safe HTML, as `Parsed.fill_gap_tables`
        does for a top-level gap. Only table/tr/td, text and positive spans are accepted;
        unsupported markup, or a table inside a quote or list, raises `MalformedError`.
        Limits and cancellation are those of `render_ocr`, and HTML tables past
        `max_xml_nodes` raise `ResourceLimitError`."""
    def render_ocr_lines(
        self, lines: collections.abc.Sequence[OcrLine], *, cancel: CancelToken | None = None
    ) -> Conversion:
        """Render literal `OcrLine` values, grouped into columns and paragraphs in the core as
        `Parsed.fill_gap_lines` groups them. Text and 32 bytes per box past
        `Limits.max_ocr_text_bytes` raise `ResourceLimitError`, the boxes before any line is
        read; the other limits and cancellation are those of `render_ocr`."""

def sniff(data: Buffer) -> Format | None:
    """The format `data`'s content names, detected under the default limits;
    `None` for CSV, which carries no signature, and for anything
    unrecognized. `bytes` is read in place and any other contiguous byte
    buffer copied once, after its length is checked against the default
    `max_input_bytes`."""
