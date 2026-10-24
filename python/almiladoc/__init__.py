"""Convert documents to GitHub-Flavored Markdown.

Build a `Converter` once from a `Config` and share it; its calls return a
`Conversion` or a `Parsed` document. A failed conversion raises a
`ConvertError` subclass whose `code`, and for `UnsupportedError` whose `kind`
too, is a stable snake_case name to branch on; a field out of range raises
`ConfigError`, and a `format` that is not exactly one `Format` name
`ValueError`.
"""

from typing import Literal

from almiladoc._almiladoc import (
    CancelledError,
    CancelToken,
    Config,
    ConfigError,
    Conversion,
    Converter,
    ConvertError,
    Diagnostic,
    EncryptedError,
    Identity,
    ImagePolicy,
    IncompleteError,
    Inspection,
    InternalError,
    Limits,
    MalformedError,
    MissingPartError,
    NeedsOcrError,
    OnLoss,
    Parsed,
    ResourceLimitError,
    UnsupportedError,
    sniff,
)

Format = Literal[
    "doc", "docx", "odt", "pdf", "ppt", "pptx", "rtf", "epub", "xlsx", "ods", "odp", "csv"
]
"""Input format, named after the extension that identifies it. Container
variants that share a parser (`.docm`, `.xlsm`, `.ppsx`, ...) map onto these
through `sniff`, or through the extension in `Converter.convert_path`."""

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
    "Format",
    "Identity",
    "ImagePolicy",
    "IncompleteError",
    "Inspection",
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
