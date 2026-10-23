# Hand-written stubs for the compiled module. `tests/test_almiladoc.py` checks
# they stay in step with what the module actually exports.
import os
from typing import ClassVar, final

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
    "Identity",
    "ImagePolicy",
    "IncompleteError",
    "InternalError",
    "Limits",
    "MalformedError",
    "MissingPartError",
    "NeedsOcrError",
    "OnLoss",
    "Parsed",
    "ResourceLimitError",
    "UnsupportedError",
    "sniff",
]

class ConfigError(ValueError):
    """A `Limits` or `Config` field is out of range."""

    limit: str
    """The field: a limit's name such as `max_work`, or `threads`."""
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
    `incomplete`, `cancelled` or `internal`."""

class UnsupportedError(ConvertError):
    """The format is unknown, or cannot be converted at all."""

    kind: str
    """Why, as a stable snake_case name: `unknown_format`, `feature_disabled`,
    `format_mismatch` or `pdf_document_model`."""
    detected: Format | None
    """The format the content was recognized as when it is not the one
    named, to retry with; `None` otherwise."""

class NeedsOcrError(ConvertError):
    """Pages of a PDF are scanned or image-only and need OCR, which almiladoc
    does not do."""

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
class Limits:
    """The safety limits of one `Converter`, frozen; each crossing raises
    `ResourceLimitError` naming it. Every field is keyword-only, defaults to
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
        """Slides, sheets and spine items one document holds, each numbered by
        its place in the document's list, at most 4294967295."""

@final
class Config:
    """The configuration of one `Converter`, frozen and keyword-only: its
    `Limits`, the threads each call runs on (1 to 64; only PDF extraction
    uses more than one) and what a loss does. Omitted fields, and `None`,
    take almiladoc's defaults: `Limits()`, 1 thread, `OnLoss.FAIL` and
    `ImagePolicy.PLACEHOLDER`.
    A thread count out of range raises `ConfigError`."""

    def __new__(
        cls,
        *,
        limits: Limits | None = None,
        threads: int = ...,
        on_loss: OnLoss | None = None,
        images: ImagePolicy | None = None,
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

@final
class CancelToken:
    """A flag any thread sets to cancel the calls it was passed to."""

    def cancel(self) -> None:
        """Cancel every call holding this token, from any thread: each raises
        `CancelledError` at its next unit of work, a PDF only before its
        extraction starts."""
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
        """True when no content was lost; only `OnLoss.REPORT` returns a
        conversion for which it is false."""

@final
class Parsed:
    """A parsed document, keeping the `Config` it was parsed under: its model
    and diagnostics as JSON, the bytes of the assets it embeds, and its
    Markdown through `render`."""

    def render(self) -> Conversion:
        """Render the document to Markdown, as `Converter.convert` does, with
        the parse's diagnostics; a loss under `OnLoss.FAIL` raises
        `IncompleteError`."""
    @property
    def diagnostics(self) -> tuple[Diagnostic, ...]:
        """What the parse could not carry into the document, and what
        rendering it drops, as a tuple of `Diagnostic`."""
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
    def assets(self) -> list[tuple[int, str, bytes]]:
        """Every embedded asset as `(id, media_type, data)`, `id` being the
        index an image's `asset` source names."""

@final
class Converter:
    """Converts documents under one `Config`, `Config()` when none is given;
    share it across threads and calls. Each call releases the GIL while it
    works, so a cancellation comes only through its `CancelToken`, never
    from Ctrl-C.

    Without a `format`, a call detects it from the content once the input
    is admitted; a `format` the content contradicts raises
    `UnsupportedError` with `kind` `format_mismatch` and the `detected`
    format to retry with."""

    def __new__(cls, config: Config | None = None) -> Converter: ...
    @property
    def identity(self) -> Identity:
        """The `Identity` that fixes this converter's output."""
    def convert(
        self, data: Buffer, format: Format | None = None, *, cancel: CancelToken | None = None
    ) -> Conversion:
        """Convert `data`, `bytes` or any contiguous byte buffer, to Markdown.
        `bytes` is read in place; any other buffer is copied once, after its
        length is checked against `max_input_bytes`. CSV carries no
        signature, so it has to be named."""
    def parse(
        self, data: Buffer, format: Format | None = None, *, cancel: CancelToken | None = None
    ) -> Parsed:
        """Parse `data`, as `convert` takes it, into the document model. A PDF
        has no model form and raises `UnsupportedError` with `kind`
        `pdf_document_model`."""
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
