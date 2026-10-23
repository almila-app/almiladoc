#!/usr/bin/env python3
"""Report what the Python binding does not mirror of the core's public surface.

`--self-test` injects each silent kind of drift into a copy and expects it named.
"""

import re
import shutil
import sys
import tempfile
from pathlib import Path

ENUM = re.compile(r"^pub enum (\w+)[^{;]*\{(.*?)^\}", re.MULTILINE | re.DOTALL)
VARIANT = re.compile(r"^    ([A-Z]\w*)\b", re.MULTILINE)
STRUCT = re.compile(r"^pub struct (\w+)[^{;]*\{(.*?)^\}", re.MULTILINE | re.DOTALL)
FIELD = re.compile(r"^    pub (\w+):", re.MULTILINE)
IMPL = re.compile(r"^impl (\w+) \{(.*?)^\}", re.MULTILINE | re.DOTALL)
METHOD = re.compile(r"fn (\w+)\(&?self\)[^{]*\{(.*?)^    \}", re.MULTILINE | re.DOTALL)
ARM = re.compile(r"Self::(\w+)\b[^=\n]*=> \"(\w+)\"")
LITERAL = re.compile(r"^Format = Literal\[(.*?)\]", re.MULTILINE | re.DOTALL)
QUOTED = re.compile(r"\"(\w+)\"")
SNAKE_CASE = re.compile(r"[a-z]+(_[a-z]+)*")
TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

FORMAT_LITERAL = "python/almiladoc/__init__.py"
ERROR_CLASS_MIRRORS = (
    "python/src/errors.rs",
    "python/src/lib.rs",
    "python/almiladoc/_almiladoc.pyi",
    "python/almiladoc/__init__.py",
)
LIMITS_MIRRORS = ("python/src/config.rs", "python/almiladoc/_almiladoc.pyi")
PY_BUILTIN_ERRORS = {"Io"}
READ = tuple(dict.fromkeys(("src", FORMAT_LITERAL, *ERROR_CLASS_MIRRORS, *LIMITS_MIRRORS)))


def norm(name: str) -> str:
    return name.replace("_", "").lower()


def items(pattern: re.Pattern[str], member: re.Pattern[str], text: str) -> dict[str, list[str]]:
    return {name: member.findall(body) for name, body in pattern.findall(text)}


def methods(text: str) -> dict[str, str]:
    """`Type::method` to its body, for each method of an inherent `impl` that takes `self`."""
    return {
        f"{owner}::{name}": body
        for owner, block in IMPL.findall(text)
        for name, body in METHOD.findall(block)
    }


def drift(root: Path) -> list[str]:
    def text(path: str) -> str:
        return (root / path).read_text(encoding="utf-8")

    def missing(mirror: str, names: list[str], what: str) -> list[str]:
        tokens = {norm(token) for token in TOKEN.findall(text(mirror))}
        return [f"{mirror}: no {what} `{name}`" for name in names if norm(name) not in tokens]

    formats = items(ENUM, VARIANT, text("src/lib.rs"))["Format"]
    enums = items(ENUM, VARIANT, text("src/error.rs"))
    errors, kinds = enums["ConvertError"], enums["UnsupportedKind"]
    limits = items(STRUCT, FIELD, text("src/limits.rs"))["Limits"]
    bodies = methods(text("src/lib.rs")) | methods(text("src/error.rs"))
    named = dict(ARM.findall(bodies.get("Format::as_str", "")))
    codes = dict(ARM.findall(bodies.get("ConvertError::code", "")))
    kind_names = dict(ARM.findall(bodies.get("UnsupportedKind::as_str", "")))
    literal = LITERAL.search(text(FORMAT_LITERAL))
    quoted = set(QUOTED.findall(literal.group(1))) if literal else set()

    found = [
        f"src/lib.rs: `Format::as_str` has no arm for `{v}`" for v in formats if v not in named
    ]
    found += [
        f"{FORMAT_LITERAL}: no format name `{name}` in `Format = Literal[...]`"
        for name in sorted(set(named.values()))
        if name not in quoted
    ]
    exceptions = [f"{v}Error" for v in errors if v not in PY_BUILTIN_ERRORS]
    for mirror in ERROR_CLASS_MIRRORS:
        found += missing(mirror, exceptions, "exception")
    for mirror in LIMITS_MIRRORS:
        found += missing(mirror, limits, "`Limits` field")
    found += [f"src/error.rs: `code()` has no arm for `{v}`" for v in errors if v not in codes]
    found += [
        f"src/error.rs: `UnsupportedKind::as_str` has no arm for `{v}`"
        for v in kinds
        if v not in kind_names
    ]
    for what, names in (("code", codes), ("`UnsupportedKind::as_str` name", kind_names)):
        found += [
            f"src/error.rs: {what} `{name}` is not snake_case"
            for name in names.values()
            if not SNAKE_CASE.fullmatch(name)
        ]
    return found


def copy(root: Path, into: Path) -> None:
    for path in READ:
        source, target = root / path, into / path
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, target)
        else:
            shutil.copy(source, target)


def edit(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    assert old in text, f"self-test anchor `{old}` gone from {path.name}; update the test"
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


TIMEOUT = [
    ("src/error.rs", "    Encrypted,", "    Encrypted,\n    /// Timed out.\n    Timeout,"),
    (
        "src/error.rs",
        'Self::Encrypted => "encrypted",',
        'Self::Encrypted => "encrypted",\n            Self::Timeout => "timeout",',
    ),
]
HTML = ("src/lib.rs", "    Csv,", "    Csv,\n    /// HTML.\n    Html,")
HTML_ARM = (
    "src/lib.rs",
    '            Self::Csv => "csv",',
    '            Self::Csv => "csv",\n            Self::Html => "html",',
)
HTML_IN_DOCSTRING = (FORMAT_LITERAL, "`.ppsx`, ...)", '`.ppsx`, "html", ...)')
TOO_LARGE = (
    "src/error.rs",
    "    PdfDocumentModel,",
    "    PdfDocumentModel,\n    /// Too large.\n    TooLarge,",
)
MAX_SLIDES = (
    "src/limits.rs",
    "    pub max_work: u64,",
    "    pub max_work: u64,\n    /// Slides.\n    pub max_slides: u64,",
)
CAMEL_CODE = ("src/error.rs", '=> "missing_part",', '=> "missingPart",')
CAMEL_KIND = ("src/error.rs", '=> "unknown_format",', '=> "unknownFormat",')
DECOY_TESTS = (
    "src/error.rs",
    "#[cfg(test)]\nmod tests {",
    """#[cfg(test)]
mod decoy {
    enum ConvertError {
        MissingPart,
    }

    impl ConvertError {
        fn code(&self) -> &'static str {
            match self {
                Self::MissingPart => "missing_part",
            }
        }
    }

    enum UnsupportedKind {
        UnknownFormat,
    }

    impl UnsupportedKind {
        fn as_str(&self) -> &'static str {
            match self {
                Self::UnknownFormat => "unknown_format",
            }
        }
    }
}

#[cfg(test)]
mod tests {""",
)
CASES = [
    ("exception `TimeoutError`", TIMEOUT),
    ("__init__.py: no format name `html`", [HTML, HTML_ARM]),
    ("__init__.py: no format name `html`", [HTML, HTML_ARM, HTML_IN_DOCSTRING]),
    ("`Format::as_str` has no arm for `Html`", [HTML]),
    ("`UnsupportedKind::as_str` has no arm for `TooLarge`", [TOO_LARGE]),
    ("python/src/config.rs: no `Limits` field `max_slides`", [MAX_SLIDES]),
    ("_almiladoc.pyi: no `Limits` field `max_slides`", [MAX_SLIDES]),
    ("code `missingPart` is not snake_case", [CAMEL_CODE]),
    ("code `missingPart` is not snake_case", [CAMEL_CODE, DECOY_TESTS]),
    ("`UnsupportedKind::as_str` name `unknownFormat` is not snake_case", [CAMEL_KIND]),
    (
        "`UnsupportedKind::as_str` name `unknownFormat` is not snake_case",
        [CAMEL_KIND, DECOY_TESTS],
    ),
]


def self_test(root: Path) -> int:
    failures = []
    if found := drift(root):
        failures.append(f"the checkout itself drifts: {found}")
    for number, (expected, edits) in enumerate(CASES, 1):
        with tempfile.TemporaryDirectory() as tree_dir:
            tree = Path(tree_dir)
            copy(root, tree)
            for path, old, new in edits:
                edit(tree / path, old, new)
            if not any(expected in line for line in drift(tree)):
                failures.append(f"case {number}: `{expected}` not reported")
    print("\n".join(failures) or f"self-test: {len(CASES) + 1} cases pass")
    return 1 if failures else 0


def main() -> int:
    root = Path(__file__).resolve().parents[4]
    if not (root / "src/lib.rs").is_file():
        print(f"no core crate under {root}")
        return 2
    if sys.argv[1:] == ["--self-test"]:
        return self_test(root)
    found = drift(root)
    print(
        "\n".join(found)
        or "mirrors: the `Format` Literal quotes every format name, each mirror spells every "
        "exception the core defines and names every `Limits` field, and every code and kind "
        "name is snake_case"
    )
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
