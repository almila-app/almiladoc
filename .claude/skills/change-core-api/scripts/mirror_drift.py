#!/usr/bin/env python3
"""Report what the Python binding does not mirror of the core's public surface, and where the
README and guides drift from it.

`--self-test` injects each silent kind of drift into a copy and expects it named.
"""

import ast
import builtins
import re
import shutil
import sys
import tempfile
import tomllib
from collections import defaultdict
from collections.abc import Callable
from itertools import takewhile
from pathlib import Path

Namespace = dict[str, dict[str, set[str]]]

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
FENCE = re.compile(r"^ {0,3}```.*?^ {0,3}```[ \t]*$", re.MULTILINE | re.DOTALL)
INLINE = re.compile(r"`([^`\n]+)`")
LINK = re.compile(r"\]\(([^)\s]+)\)")
NAME_PATH = re.compile(r"[A-Za-z_]\w*(?:(?:::|\.)[A-Za-z_]\w*)*")
CALL = re.compile(r"(?:::<[^<>]*>)?(?:\(.*\))?$")
QUALIFIED = re.compile(r"([A-Za-z_]\w*)(?:::|\.)([A-Za-z_]\w*)")
CAMEL_CASE = re.compile(r"[a-z][A-Z]")
FROM_EXTENSION = re.compile(r"fn from_extension\(.*?\n    \}\n", re.DOTALL)
EXTENSION_ARM = re.compile(r"((?:\"\w+\"\s*\|?\s*)+)=>\s*Self::(\w+)")
EXTENSION_CELL = re.compile(r"`\.(\w+)`")
FILE_NAME = re.compile(r"[\w-]+\.(?:md|pyi|py|rs|toml|json)")
SEPARATOR = re.compile(r"(::|\.)")
PUBLIC_TYPE = re.compile(r"^pub (?:struct|enum|trait) (\w+)", re.MULTILINE)
TRAIT = re.compile(r"^pub trait (\w+)[^{;]*\{(.*?)^\}", re.MULTILINE | re.DOTALL)
PRIVATE_TRAIT = re.compile(r"^ *(?:pub\([^)]*\) )?trait (\w+)", re.MULTILINE)
IMPL_BLOCK = re.compile(r"^impl\b([^{]*)\{(.*?)^\}", re.MULTILINE | re.DOTALL)
IMPL_TARGET = re.compile(r"\s*(?:([\w:]+) for )?(?:\w+::)*(\w+)")
PUB_MEMBER = re.compile(r"^    pub (?:(?:const |unsafe )*fn (\w+)|const (\w+):)", re.MULTILINE)
TRAIT_MEMBER = re.compile(r"^    (?:fn|const) (\w+)", re.MULTILINE)
DERIVED_DEFAULT = re.compile(
    r"#\[derive\([^)]*\bDefault\b[^)]*\)\]\s*(?:#\[[^\]]*\]\s*)*pub (?:struct|enum) (\w+)"
)
CRATE_ITEM = re.compile(r"^pub (?:mod|fn|const|struct|enum|trait|type) (\w+)", re.MULTILINE)
PUB_USE = re.compile(r"^pub use ([^;]*);", re.MULTILINE)
PYTHON_STUB = "python/almiladoc/_almiladoc.pyi"
MODULE = "almiladoc"
LOCAL_CRATE = "almiladoc_ocr_local"
LOCAL_README = "ocr-local/README.md"

FORMAT_LITERAL = "python/almiladoc/__init__.py"
ERROR_CLASS_MIRRORS = (
    "python/src/errors.rs",
    "python/src/lib.rs",
    "python/almiladoc/_almiladoc.pyi",
    "python/almiladoc/__init__.py",
)
LIMITS_MIRRORS = ("python/src/config.rs", "python/almiladoc/_almiladoc.pyi")
PY_MAPPED_ERRORS = {"InvalidConfig"}
PY_ONLY_EXCEPTIONS = ("ConfigError", "GapRefused")
README = "README.md"
GUIDE_DIR = "docs/guide"
LINKED_PAGES = ("docs/support.md", LOCAL_README)
CRATE_DOC = "src/lib.rs"
NATIVE_CRATE_DOC = "ocr-local/src/lib.rs"
GITHUB_BLOB = "https://github.com/almila-app/almiladoc/blob/main/"
FORMAT_TABLE = "| `format=` name |"
FEATURE_TABLE = "| Cargo feature |"
POLICIES = {
    "src/config.rs": (
        "HeaderFooterPolicy",
        "CommentPolicy",
        "HiddenPolicy",
        "RevisionPolicy",
        "ImagePolicy",
        "ImageOcr",
        "PdfScanPolicy",
    ),
    "src/diagnostics.rs": ("OnLoss",),
}
SOURCE_TREES = ("src", "ocr-local/src", "ocr-local/tests", "python/almiladoc")
READ = tuple(
    dict.fromkeys(
        (
            *SOURCE_TREES,
            "Cargo.toml",
            *ERROR_CLASS_MIRRORS,
            *LIMITS_MIRRORS,
            *LINKED_PAGES,
        )
    )
)


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


def guides(root: Path) -> list[str]:
    pages = sorted(path.relative_to(root).as_posix() for path in (root / GUIDE_DIR).glob("*.md"))
    return [README, *pages]


def code_of(page: str) -> str:
    return "\n".join(INLINE.findall(FENCE.sub("", page)))


def spellings(text: str, owner: str) -> list[str]:
    return sorted(set(dict(ARM.findall(methods(text).get(f"{owner}::as_str", ""))).values()))


def extension_names(text: str, format_names: dict[str, str]) -> dict[str, str]:
    """Each extension `Format::from_extension` maps, to the name of its format."""
    body = FROM_EXTENSION.search(text)
    arms = EXTENSION_ARM.findall(body.group() if body else "")
    return {
        extension: format_names.get(variant, variant)
        for keys, variant in arms
        for extension in QUOTED.findall(keys)
    }


def table_rows(page: str, head: str) -> list[list[str]] | None:
    """The cells of each row of `page`'s table whose header opens with `head`."""
    lines = page.splitlines()
    start = next((n for n, line in enumerate(lines) if line.startswith(head)), None)
    if start is None:
        return None
    rows = takewhile(lambda line: line.startswith("|"), lines[start + 2 :])
    return [[cell.strip() for cell in row.strip("|").split("|")] for row in rows]


def format_table(page: str, extensions: dict[str, str]) -> list[str]:
    rows = table_rows(page, FORMAT_TABLE)
    if rows is None:
        return [f"{README}: no format table headed `format=` name"]
    listed = {
        extension: name.strip("`")
        for name, _, cell, *_ in rows
        for extension in EXTENSION_CELL.findall(cell)
    }
    found = [
        f"{README}: format table lacks `.{extension}`, which maps to `{name}`"
        for extension, name in extensions.items()
        if extension not in listed
    ]
    found += [
        f"{README}: format table maps `.{extension}` to `{name}`, not `{extensions[extension]}`"
        for extension, name in listed.items()
        if extensions.get(extension, name) != name
    ]
    found += [
        f"{README}: format table lists `.{extension}`, which `Format::from_extension` rejects"
        for extension in listed
        if extension not in extensions
    ]
    return found


def default_features(features: dict[str, list[str]]) -> set[str]:
    """The features a build without `--no-default-features` enables, implied ones included."""
    enabled, pending = set(), list(features.get("default", ()))
    while pending:
        name = pending.pop()
        if name in features and name not in enabled:
            enabled.add(name)
            pending += features[name]
    return enabled


def feature_table(page: str, features: dict[str, list[str]]) -> list[str]:
    rows = table_rows(page, FEATURE_TABLE)
    if rows is None:
        return [f"{README}: no feature table headed `Cargo feature`"]
    listed = {name.strip("`"): default for name, default, *_ in rows}
    declared, defaults = set(features) - {"default"}, default_features(features)
    found = [f"{README}: feature table lacks `{name}`" for name in sorted(declared - set(listed))]
    found += [
        f"{README}: feature table lists `{name}`, which Cargo.toml lacks"
        for name in sorted(set(listed) - declared)
    ]
    for name in sorted(declared & set(listed)):
        expected = "yes" if name in defaults else "no"
        if listed[name] != expected:
            found.append(
                f"{README}: feature table marks `{name}` default `{listed[name]}`, "
                f"Cargo.toml `{expected}`"
            )
    return found


def links_of(page: str) -> list[str]:
    return LINK.findall(INLINE.sub("", FENCE.sub("", page)))


def link_problems(root: Path, path: str, page: str) -> list[str]:
    found = []
    for target in links_of(page):
        if target.startswith(GITHUB_BLOB):
            resolved = root / target.removeprefix(GITHUB_BLOB).split("#")[0]
        elif target.startswith(("#", "mailto:")) or "://" in target or path == CRATE_DOC:
            continue
        elif path == README:
            found.append(f"{path}: relative link `{target}` breaks on PyPI and crates.io")
            continue
        else:
            resolved = (root / path).parent / target.split("#")[0]
        if not resolved.exists():
            found.append(f"{path}: link `{target}` resolves to no file")
    return found


def source_tokens(root: Path) -> set[str]:
    sources = (
        file for tree in SOURCE_TREES for file in (root / tree).rglob("*") if file.is_file()
    )
    suffixes = {".rs", ".py", ".pyi"}
    tokens = {
        token
        for file in sources
        if file.suffix in suffixes
        for token in TOKEN.findall(file.read_text(encoding="utf-8"))
    }
    features = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8")).get("features", {})
    return tokens | set(features) | set(dir(builtins))


def without_generics(text: str) -> str:
    kept, depth = [], 0
    for char in text.replace("->", ""):
        depth += char == "<"
        if not depth:
            kept.append(char)
        depth -= char == ">"
    return "".join(kept)


def rust_members(
    root: Path, tree: str
) -> tuple[set[str], dict[str, set[str]], dict[str, set[str]]]:
    """The public types of the crate under `tree`, the public fields, functions and constants of
    each type, and the variants of each enum."""
    files = sorted((root / tree).rglob("*.rs"))
    texts = [file.read_text(encoding="utf-8") for file in files]
    private = {name for text in texts for name in PRIVATE_TRAIT.findall(text)}
    types, members, variants = set(), defaultdict(set), defaultdict(set)
    for text in texts:
        types |= set(PUBLIC_TYPE.findall(text))
        for pattern, member, into in (
            (STRUCT, FIELD, members),
            (TRAIT, TRAIT_MEMBER, members),
            (ENUM, VARIANT, variants),
        ):
            for owner, names in items(pattern, member, text).items():
                into[owner] |= set(names)
        for head, body in IMPL_BLOCK.findall(text):
            trait, owner = IMPL_TARGET.match(without_generics(head)).groups()
            if trait:
                if trait.rsplit("::", 1)[-1] not in private:
                    members[owner] |= set(TRAIT_MEMBER.findall(body))
            else:
                members[owner] |= {fn or const for fn, const in PUB_MEMBER.findall(body)}
        for owner in DERIVED_DEFAULT.findall(text):
            members[owner].add("default")
    return types, members, variants


def crate_names(text: str) -> set[str]:
    names = set(CRATE_ITEM.findall(text))
    for path in PUB_USE.findall(text):
        leaves = path.split("{", 1)[-1].replace("}", "").split(",")
        names |= {TOKEN.findall(leaf)[-1] for leaf in leaves if TOKEN.findall(leaf)}
    return names


def defined(node: ast.stmt) -> set[str]:
    match node:
        case ast.FunctionDef(name=name) | ast.ClassDef(name=name):
            return {name}
        case ast.AnnAssign(target=ast.Name(id=name)):
            return {name}
        case ast.Assign(targets=targets):
            return {target.id for target in targets if isinstance(target, ast.Name)}
        case ast.ImportFrom(names=names):
            return {alias.asname or alias.name for alias in names}
    return set()


def python_members(root: Path) -> tuple[dict[str, set[str]], set[str]]:
    """Each class of the stub to its members, inherited ones included, and the names the
    package defines."""
    stub = ast.parse((root / PYTHON_STUB).read_text(encoding="utf-8")).body
    classes = {node.name: node for node in stub if isinstance(node, ast.ClassDef)}

    def members(name: str) -> set[str]:
        node = classes[name]
        bases = [base.id for base in node.bases if isinstance(base, ast.Name)]
        inherited = [members(base) for base in bases if base in classes]
        return set().union(*map(defined, node.body), *inherited)

    package = ast.parse((root / FORMAT_LITERAL).read_text(encoding="utf-8")).body
    return {name: members(name) for name in classes}, set().union(*map(defined, package))


def rust_namespace(root: Path, tree: str, crate: str, crate_doc: str) -> Namespace:
    """Each public type of the crate under `tree`, and the crate, to the members `::` and `.`
    may name after it: variants only after `::`."""
    types, members, variants = rust_members(root, tree)
    space = {
        owner: {"::": members[owner] | variants[owner], ".": members[owner]} for owner in types
    }
    return space | {crate: {"::": crate_names((root / crate_doc).read_text(encoding="utf-8"))}}


def python_namespace(root: Path) -> Namespace:
    classes, package = python_members(root)
    return {owner: {".": names} for owner, names in classes.items()} | {MODULE: {".": package}}


def namespaces(root: Path) -> dict[str, list[Namespace]]:
    """Each page to the namespaces its references resolve in, the first defining a name winning:
    the core's Python and Rust, then the adapter's Rust, which its own page puts first."""
    core = [python_namespace(root), rust_namespace(root, "src", MODULE, CRATE_DOC)]
    local = rust_namespace(root, "ocr-local/src", LOCAL_CRATE, NATIVE_CRATE_DOC)
    return defaultdict(lambda: [*core, local], {LOCAL_README: [local, *core]})


def member_problems(span: str, parts: list[str], scope: list[Namespace]) -> list[str]:
    """Each member a public type or crate in `parts` lacks, a crate or module confining the
    rest of the path to its own namespace."""
    found = []
    for owner, separator, member in zip(parts[:-1:2], parts[1::2], parts[2::2], strict=True):
        space = next((each for each in scope if separator in each.get(owner, {})), None)
        if space is None or member not in space[owner][separator]:
            if space is not None or any(owner in each for each in scope):
                found.append(f"`{span}` names no `{member}` of `{owner}`")
            continue
        if owner in (MODULE, LOCAL_CRATE):
            scope = [space]
    return found


def unknown_names(page: str, known: set[str], scope: list[Namespace]) -> list[str]:
    """Inline code spelling an identifier path with a part the source never uses, or with a
    member the public type or crate before it lacks."""
    found = []
    for span in INLINE.findall(FENCE.sub("", page)):
        name = CALL.sub("", span.strip(), count=1)
        qualified = "_" in name or "." in name or "::" in name or CAMEL_CASE.search(name)
        parts = SEPARATOR.split(name)
        if FILE_NAME.fullmatch(name) or not NAME_PATH.fullmatch(name) or not qualified:
            continue
        if any(part not in known for part in parts[::2]):
            found.append(f"`{span}` is not in the source")
        found += member_problems(span, parts, scope)
    return found


def doctest_problems(includes: dict[str, list[str]], read: Callable[[str], str]) -> list[str]:
    return [
        f"{crate}: no `include_str!` of {page}, so its Rust blocks are never compiled"
        for crate, pages in includes.items()
        for page in pages
        if f'include_str!("../{page.removeprefix("ocr-local/")}")' not in read(crate)
    ]


def guide_problems(
    root: Path,
    names: dict[str, list[str]],
    extensions: dict[str, str],
    features: dict[str, list[str]],
) -> list[str]:
    def read(path: str) -> str:
        return (root / path).read_text(encoding="utf-8")

    pages = {path: read(path) for path in guides(root)}
    code = "\n".join(code_of(page) for page in pages.values())
    spelled = {norm(token) for token in TOKEN.findall(code)}
    pairs = {(norm(left), norm(right)) for left, right in QUALIFIED.findall(code)}
    found = []
    for what, listed in names.items():
        for name in listed:
            if what == "Cargo feature":
                named = re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", code)
            elif what == "policy variant":
                named = tuple(map(norm, name.split("::"))) in pairs
            else:
                named = norm(name) in spelled
            if not named:
                found.append(f"docs: no guide names {what} `{name}`")
    found += format_table(pages[README], extensions)
    found += feature_table(pages[README], features)
    known, scopes = source_tokens(root), namespaces(root)
    for path, page in {**pages, **{path: read(path) for path in LINKED_PAGES}}.items():
        found += [f"{path}: {problem}" for problem in unknown_names(page, known, scopes[path])]
        found += link_problems(root, path, page)
    found += link_problems(root, CRATE_DOC, read(CRATE_DOC))
    return found + doctest_problems(
        {
            CRATE_DOC: list(pages),
            NATIVE_CRATE_DOC: [LOCAL_README],
        },
        read,
    )


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
    exceptions = [f"{v}Error" for v in errors if v not in PY_MAPPED_ERRORS]
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
    config = items(STRUCT, FIELD, text("src/config.rs"))["Config"]
    http = items(STRUCT, FIELD, text("src/ocr/http/mod.rs"))["OcrHttpConfig"]
    features = tomllib.loads(text("Cargo.toml")).get("features", {})
    optional = sorted(set(features) - set(features.get("default", ())) - {"default"})
    policies = [
        f"{owner}::{variant}"
        for path, owners in POLICIES.items()
        for owner, variants in items(ENUM, VARIANT, text(path)).items()
        if owner in owners
        for variant in variants
    ]
    names = {
        "format name": sorted(set(named.values())),
        "code": sorted(codes.values()),
        "Python exception": [*exceptions, *PY_ONLY_EXCEPTIONS],
        "`UnsupportedKind` name": sorted(kind_names.values()),
        "`Config` field": config,
        "`Limits` field": limits,
        "`OcrHttpConfig` field": http,
        "OCR failure kind": spellings(text("src/ocr/engine.rs"), "OcrFailureKind"),
        "gap reason": spellings(text("src/model/gap.rs"), "GapReason"),
        "page verdict": spellings(text("src/inspect.rs"), "PageVerdict"),
        "Cargo feature": optional,
        "policy variant": policies,
    }
    extensions = extension_names(text("src/lib.rs"), named)
    return found + guide_problems(root, names, extensions, features)


def copy(root: Path, into: Path) -> None:
    pages = [*guides(root), *LINKED_PAGES, CRATE_DOC]
    for path in dict.fromkeys((*READ, *pages)):
        source, target = root / path, into / path
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, target, dirs_exist_ok=True)
        else:
            shutil.copy(source, target)
    for path in pages:
        for link in links_of((root / path).read_text(encoding="utf-8")):
            relative = link.removeprefix(GITHUB_BLOB).split("#")[0]
            local = (
                Path(relative) if link.startswith(GITHUB_BLOB) else Path(path).parent / relative
            )
            if (root / local).is_file() and not (into / local).exists():
                (into / local).parent.mkdir(parents=True, exist_ok=True)
                (into / local).touch()


def edit(path: Path, old: str | None, new: str) -> None:
    if old is None:
        path.write_text(new, encoding="utf-8")
        return
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
    "    NotInspectable,",
    "    NotInspectable,\n    /// Too large.\n    TooLarge,",
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
RETENTION = (
    "src/config.rs",
    "    pub revisions: RevisionPolicy,\n}",
    "    pub revisions: RevisionPolicy,\n    /// Retention.\n    pub retention: u8,\n}",
)
RTFD = ("src/lib.rs", '"rtf" => Self::Rtf,', '"rtf" | "rtfd" => Self::Rtf,')
ALT_LABEL = ("src/config.rs", "    AltText,", "    AltLabel,")
UNKNOWN_NAME = (README, "# almiladoc\n", "# almiladoc\n\nUse `Converter.frobnicate(data)`.\n")
NO_METHOD = ("docs/guide/ocr.md", "`Converter.inspect(data)`", "`Converter.assets(data)`")
NO_VARIANT = ("docs/guide/convert.md", "read `ImagePolicy::AltText`", "read `ImagePolicy::Reject`")
NO_FUNCTION = ("docs/guide/convert.md", "or `sniff(data)`", "or `almiladoc.render(data)`")
SWAPPED_PY = (
    "docs/guide/errors.md",
    "`Converter.parse` and `Parsed.to_json`",
    "`Parsed.parse` and `Converter.to_json`",
)
SWAPPED_RS = (
    "docs/guide/convert.md",
    "`Converter::parse`, `Parsed::render`",
    "`Parsed::parse`, `Converter::render`",
)
CROSSED_SPELLING = (
    "docs/guide/convert.md",
    "`ImagePolicy.ALT_TEXT`, `ImagePolicy.OMIT`",
    "`ImagePolicy.AltText`, `ImagePolicy::OMIT`",
)
CRATE_COLLISION = (
    README,
    "# almiladoc\n",
    "# almiladoc\n\nSet `almiladoc.Limits.max_image_bytes`.\n",
)
CORE_COLLISION = (README, "# almiladoc\n", "# almiladoc\n\nSet `Limits.max_image_bytes`.\n")
LOCAL_COLLISION = (
    LOCAL_README,
    "adapter's `Limits.max_pixels`",
    "adapter's `Limits.max_work`",
)
DEFAULT_CELL = (README, "| `serde` | no |", "| `serde` | yes |")
DEFAULT_FEATURE = (
    "Cargo.toml",
    'default = ["pdf", "raster"]',
    'default = ["pdf", "raster", "serde"]',
)
STRAY_FEATURE = (README, "| `ocr-http` | no |", "| `ocr-https` | no |")
BROKEN_LINK = (README, "blob/main/docs/guide/convert.md", "blob/main/docs/guide/convert-gone.md")
RELATIVE_LINK = (README, GITHUB_BLOB + "docs/guide/convert.md", "docs/guide/convert.md")
WRONG_EXTENSION = (README, "`.docm`", "`.docs`")
NEW_GUIDE = ("docs/guide/extra.md", None, "# Extra\n")
NEW_FEATURE = ("Cargo.toml", "ocr-http = [", "extra = []\nocr-http = [")
THROTTLED = [
    (
        "src/ocr/engine.rs",
        "    Cancelled,\n}",
        "    Cancelled,\n    /// Throttled.\n    Throttled,\n}",
    ),
    (
        "src/ocr/engine.rs",
        'Self::Cancelled => "cancelled",',
        'Self::Cancelled => "cancelled",\n            Self::Throttled => "throttled",',
    ),
]
CASES = [
    ("exception `TimeoutError`", TIMEOUT),
    ("src/lib.rs: no `include_str!` of docs/guide/extra.md", [NEW_GUIDE]),
    ("docs: no guide names Cargo feature `extra`", [NEW_FEATURE]),
    ("docs: no guide names OCR failure kind `throttled`", THROTTLED),
    ("docs: no guide names code `timeout`", TIMEOUT),
    ("docs: no guide names `Limits` field `max_slides`", [MAX_SLIDES]),
    ("docs: no guide names `Config` field `retention`", [RETENTION]),
    ("docs: no guide names policy variant `ImagePolicy::AltLabel`", [ALT_LABEL]),
    ("README.md: format table lacks `.rtfd`, which maps to `rtf`", [RTFD]),
    ("format table lists `.docs`, which `Format::from_extension` rejects", [WRONG_EXTENSION]),
    ("README.md: `Converter.frobnicate(data)` is not in the source", [UNKNOWN_NAME]),
    ("ocr.md: `Converter.assets(data)` names no `assets` of `Converter`", [NO_METHOD]),
    ("convert.md: `ImagePolicy::Reject` names no `Reject` of `ImagePolicy`", [NO_VARIANT]),
    ("`almiladoc.render(data)` names no `render` of `almiladoc`", [NO_FUNCTION]),
    ("errors.md: `Parsed.parse` names no `parse` of `Parsed`", [SWAPPED_PY]),
    ("errors.md: `Converter.to_json` names no `to_json` of `Converter`", [SWAPPED_PY]),
    ("convert.md: `Parsed::parse` names no `parse` of `Parsed`", [SWAPPED_RS]),
    ("convert.md: `Converter::render` names no `render` of `Converter`", [SWAPPED_RS]),
    ("`ImagePolicy.AltText` names no `AltText` of `ImagePolicy`", [CROSSED_SPELLING]),
    ("`ImagePolicy::OMIT` names no `OMIT` of `ImagePolicy`", [CROSSED_SPELLING]),
    (
        "`almiladoc.Limits.max_image_bytes` names no `max_image_bytes` of `Limits`",
        [CRATE_COLLISION],
    ),
    (
        "README.md: `Limits.max_image_bytes` names no `max_image_bytes` of `Limits`",
        [CORE_COLLISION],
    ),
    ("ocr-local/README.md: `Limits.max_work` names no `max_work` of `Limits`", [LOCAL_COLLISION]),
    ("README.md: feature table marks `serde` default `yes`, Cargo.toml `no`", [DEFAULT_CELL]),
    ("README.md: feature table marks `serde` default `no`, Cargo.toml `yes`", [DEFAULT_FEATURE]),
    ("README.md: feature table lacks `extra`", [NEW_FEATURE]),
    ("README.md: feature table lists `ocr-https`, which Cargo.toml lacks", [STRAY_FEATURE]),
    ("convert-gone.md` resolves to no file", [BROKEN_LINK]),
    ("README.md: relative link `docs/guide/convert.md` breaks on PyPI", [RELATIVE_LINK]),
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
        "exception the core defines and names every `Limits` field, every code and kind name "
        "is snake_case, and the README and guides name what the core defines, spell only names "
        "the source has, qualify each by a type or module that has it, link to files that exist "
        "and reach the crates' doctests, and the README tables every Cargo feature and default"
    )
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
