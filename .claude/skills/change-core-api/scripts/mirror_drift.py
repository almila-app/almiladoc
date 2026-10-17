#!/usr/bin/env python3
"""Report what the Python binding does not mirror of the core's public surface.

The compiler catches no drift: not a new `Block` or `Inline` variant (the binding's wildcard arm
raises at runtime), a new model field, a new `ConvertError` variant, a new `Format` or its Python
name, and the stub file is never compiled against the core at all. This names each core
identifier that a binding file never spells, however it cases it: a struct's fields among the
fields that type's own block in the mirror declares, everything else anywhere in the file.
`--self-test` injects each silent kind of drift into a copy and expects it named.
"""

import re
import shutil
import sys
import tempfile
from pathlib import Path

ENUM = re.compile(r"^pub enum (\w+)[^{;]*\{(.*?)^\}", re.MULTILINE | re.DOTALL)
STRUCT = re.compile(r"^pub struct (\w+)[^{;]*\{(.*?)^\}", re.MULTILINE | re.DOTALL)
VARIANT = re.compile(r"^    ([A-Z]\w*)\b", re.MULTILINE)
FIELD = re.compile(r"^    pub (\w+):", re.MULTILINE)
CODE_ARM = re.compile(r"ConvertError::(\w+)\b[^=\n]*=> \"(\w+)\"")
AS_STR = re.compile(r"fn as_str\(self\)[^{]*\{(.*?)^    \}", re.MULTILINE | re.DOTALL)
AS_STR_ARM = re.compile(r"Format::(\w+) => \"(\w+)\"")
PY_FORMATS = re.compile(r"const FORMATS: \[almiladoc::Format; \d+\] = \[(.*?)\];", re.DOTALL)
PY_FORMAT = re.compile(r"almiladoc::Format::(\w+)")
TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
VARIANT_FIELD = re.compile(r"^        (\w+):", re.MULTILINE)
DECLARED = re.compile(r"^[ \t]+(?:pub\s+)?(\w+)\??\s*:(?!:)", re.MULTILINE)

CORE_ONLY = {"GridBuilder", "AssetId"}
MIRRORED_AS = {("Asset", "bytes"): "data"}
MODEL_MIRRORS = ("python/src/document.rs", "python/almiladoc/_almiladoc.pyi")
FORMAT_MIRRORS = ("python/src/lib.rs",)
FORMAT_NAME_MIRRORS = ("python/almiladoc/__init__.py",)
ERROR_CLASS_MIRRORS = (
    "python/src/lib.rs",
    "python/almiladoc/_almiladoc.pyi",
    "python/almiladoc/__init__.py",
)
PY_BUILTIN_ERRORS = {"Io"}
READ = tuple(
    dict.fromkeys(
        ("src", *MODEL_MIRRORS, *FORMAT_MIRRORS, *FORMAT_NAME_MIRRORS, *ERROR_CLASS_MIRRORS)
    )
)


def norm(name: str) -> str:
    return name.replace("_", "").lower()


def items(pattern: re.Pattern[str], member: re.Pattern[str], text: str) -> dict[str, list[str]]:
    return {name: member.findall(body) for name, body in pattern.findall(text)}


def declared(text: str, name: str) -> set[str] | None:
    head = rf"^(?:pub |export )?(?:struct|interface|class) {name}\b[^\n]*\n(.*?)(?=^\S|\Z)"
    block = re.search(head, text, re.MULTILINE | re.DOTALL)
    return None if block is None else {norm(field) for field in DECLARED.findall(block.group(1))}


def drift(root: Path) -> list[str]:
    def text(path: str) -> str:
        return (root / path).read_text(encoding="utf-8")

    def missing(mirror: str, names: list[str], what: str) -> list[str]:
        tokens = {norm(token) for token in TOKEN.findall(text(mirror))}
        return [f"{mirror}: no {what} `{name}`" for name in names if norm(name) not in tokens]

    enums: dict[str, list[str]] = {}
    structs: dict[str, list[str]] = {}
    for path in sorted((root / "src/model").glob("*.rs")):
        source = path.read_text(encoding="utf-8")
        enums |= items(ENUM, VARIANT, source)
        enums |= {f"{name} variant": VARIANT_FIELD.findall(b) for name, b in ENUM.findall(source)}
        structs |= items(STRUCT, FIELD, source)
    structs = {name: fields for name, fields in structs.items() if name not in CORE_ONLY}
    formats = items(ENUM, VARIANT, text("src/lib.rs"))["Format"]
    errors = items(ENUM, VARIANT, text("src/error.rs"))["ConvertError"]
    codes = dict(CODE_ARM.findall(text("src/error.rs")))
    as_str = AS_STR.search(text("src/lib.rs"))
    named = dict(AS_STR_ARM.findall(as_str.group(1))) if as_str else {}
    listed = PY_FORMATS.search(text("python/src/lib.rs"))
    bound = set(PY_FORMAT.findall(listed.group(1))) if listed else set()

    found = []
    for mirror in MODEL_MIRRORS:
        for kind, members in enums.items():
            found += missing(mirror, members, f"{kind} member")
        for kind, fields in structs.items():
            mirrored = declared(text(mirror), kind)
            if mirrored is None:
                found.append(f"{mirror}: no struct `{kind}`")
                continue
            found += [
                f"{mirror}: `{kind}` declares no `{field}`"
                for field in fields
                if norm(MIRRORED_AS.get((kind, field), field)) not in mirrored
            ]
    for mirror in FORMAT_MIRRORS:
        found += missing(mirror, formats, "Format variant")
    found += [f"python/src/lib.rs: FORMATS has no `{v}`" for v in formats if v not in bound]
    found += [
        f"src/lib.rs: `Format::as_str` has no arm for `{v}`" for v in formats if v not in named
    ]
    for mirror in FORMAT_NAME_MIRRORS:
        found += missing(mirror, sorted(set(named.values())), "format name")
    exceptions = [f"{v}Error" for v in errors if v not in PY_BUILTIN_ERRORS]
    for mirror in ERROR_CLASS_MIRRORS:
        found += missing(mirror, exceptions, "exception")
    found += [f"src/error.rs: `code()` has no arm for `{v}`" for v in errors if v not in codes]
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


def self_test(root: Path) -> int:
    injections = {
        "`Style` declares no `underline`": ("src/model/style.rs", "    pub code: bool,"),
        "`Table` declares no `text`": ("src/model/table.rs", "    pub kind: TableKind,"),
        "Block variant member `zebracaption`": ("src/model/block.rs", "        text: String,"),
        "Block member `Zebra`": ("src/model/block.rs", "    Rule,"),
        "exception `TimeoutError`": ("src/error.rs", "    Encrypted,"),
        "FORMATS has no `Html`": ("src/lib.rs", "    Csv,"),
        "__init__.py: no format name `html`": ("src/lib.rs", "    Csv,"),
        "`Format::as_str` has no arm for `Html`": ("src/lib.rs", "    Csv,"),
    }
    additions = {
        "    pub code: bool,": "    pub code: bool,\n    /// Underlined.\n    pub underline: bool,",
        "    pub kind: TableKind,": (
            "    pub kind: TableKind,\n    /// Text.\n    pub text: String,"
        ),
        "        text: String,": "        text: String,\n        zebracaption: Option<String>,",
        "    Encrypted,": "    Encrypted,\n    /// Timed out.\n    Timeout,",
        "    Csv,": "    Csv,\n    /// HTML.\n    Html,",
        "    Rule,": "    Rule,\n    /// Zebra.\n    Zebra,",
    }
    failures = []
    if found := drift(root):
        failures.append(f"the checkout itself drifts: {found}")
    for expected, (path, anchor) in injections.items():
        with tempfile.TemporaryDirectory() as tree_dir:
            tree = Path(tree_dir)
            copy(root, tree)
            edit(tree / path, anchor, additions[anchor])
            if path == "src/error.rs":
                edit(
                    tree / path,
                    'ConvertError::Encrypted => "encrypted",',
                    'ConvertError::Encrypted => "encrypted",\n'
                    '            ConvertError::Timeout => "timeout",',
                )
            if "as_str" in expected:
                edit(
                    tree / "python/src/lib.rs",
                    "    almiladoc::Format::Csv,\n];",
                    "    almiladoc::Format::Csv,\n    almiladoc::Format::Html,\n];",
                )
            if "format name" in expected:
                edit(
                    tree / path,
                    '            Format::Csv => "csv",',
                    '            Format::Csv => "csv",\n            Format::Html => "html",',
                )
                edit(
                    tree / "python/src/lib.rs",
                    "    almiladoc::Format::Csv,\n];",
                    "    almiladoc::Format::Csv,\n    almiladoc::Format::Html,\n];",
                )
            if not any(expected in line for line in drift(tree)):
                failures.append(f"`{expected}` not reported")
    print("\n".join(failures) or f"self-test: {len(injections) + 1} cases pass")
    return 1 if failures else 0


def main() -> int:
    root = Path(__file__).resolve().parents[4]
    if not (root / "src/model").is_dir():
        print(f"no core model under {root}")
        return 2
    if sys.argv[1:] == ["--self-test"]:
        return self_test(root)
    found = drift(root)
    print("\n".join(found) or "mirrors: each mirror declares or spells every core identifier")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
