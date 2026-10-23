"""Check that docs/support.md names every DiagnosticCode that src/diagnostics.rs declares, and no
other snake_case code."""

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SOURCE = "src/diagnostics.rs"
PAGE = "docs/support.md"
LIST = re.compile(r"^diagnostic_codes! \{\r?\n(.*?)^\}\r?$", re.MULTILINE | re.DOTALL)
DECLARATION = re.compile(r'^\s+[A-Z]\w* = "([^"]*)", [A-Z]\w*;', re.MULTILINE)
LINE = re.compile(r'\s+(?:///.*|[A-Z]\w* = "[^"]*", [A-Z]\w*;)\r?')
NAME = re.compile(r"`([a-z][a-z0-9]*(?:_[a-z0-9]+)+)`")


def declared(source: str) -> tuple[list[str], list[str]]:
    block = LIST.search(source)
    if not block:
        return [], []
    body = block.group(1)
    stray = [
        line.strip() for line in body.splitlines() if line.strip() and not LINE.fullmatch(line)
    ]
    return DECLARATION.findall(body), stray


def problems(source: str, page: str) -> list[str]:
    codes, stray = declared(source)
    if not codes:
        return [f"{SOURCE}: no diagnostic_codes! list found"]
    if stray:
        return [
            f"{SOURCE}: a line in diagnostic_codes! is neither a declaration nor a doc comment: "
            f"{line}"
            for line in stray
        ]
    named = set(NAME.findall(page))
    missing = [f"{PAGE}: names no `{code}`" for code in codes if code not in named]
    unknown = [f"{PAGE}: `{name}` is not a DiagnosticCode" for name in sorted(named - set(codes))]
    return missing + unknown


def self_test() -> int:
    source = """//! Decoys.
macro_rules! diagnostic_codes {
    ($($variant:ident = $name:literal, $class:ident;)*) => {};
}

diagnostic_codes! {
    /// A part that will not read.
    ///   Decoy = "decoy_comment", Lost;
    PartUnreadable = "part_unreadable", Lost;
    /// A unit that is absent.
    UnitUnreadable = "unit_unreadable", Lost;
    /// A header dropped.
    HeaderOmitted = "header_omitted", Omitted;
}

const AFTER: &str = r#"
    Late = "late_code", Info;
"#;
"""
    page = "# Limits\n\nThe `part_unreadable`, `unit_unreadable` and `header_omitted` codes.\n"
    cases = {
        "a page missing a code": (
            page.replace("`unit_unreadable`", "unit_unreadable"),
            "names no `unit_unreadable`",
        ),
        "a page missing the last code": (
            page.replace("`header_omitted`", ""),
            "names no `header_omitted`",
        ),
        "a page naming a misspelt code": (
            page + "Also `unit_unreadabel`.\n",
            "`unit_unreadabel` is not a DiagnosticCode",
        ),
        "a page naming a setting in snake_case": (
            page + "Also `max_work`.\n",
            "`max_work` is not a DiagnosticCode",
        ),
        "a page naming a code a doc comment mentions": (
            page + "Also `decoy_comment`.\n",
            "`decoy_comment` is not a DiagnosticCode",
        ),
        "a page naming a declaration after the list": (
            page + "Also `late_code`.\n",
            "`late_code` is not a DiagnosticCode",
        ),
    }
    failures = []
    if problems(source, page):
        failures.append(f"a page naming each code was refused: {problems(source, page)}")
    hostile_page = "\N{ZERO WIDTH NO-BREAK SPACE}" + page.replace("\n", "\r\n").replace(
        "Limits", "Limits \N{EM DASH} \N{LATIN SMALL LETTER U WITH DIAERESIS}"
    )
    if problems(source, hostile_page):
        failures.append(
            "a page with CRLF, a BOM and non-ASCII text was refused: "
            f"{problems(source, hostile_page)}"
        )
    crlf_source = source.replace("\n", "\r\n")
    if problems(crlf_source, page):
        failures.append(
            f"a source with CRLF line endings was refused: {problems(crlf_source, page)}"
        )
    for label, (text, reason) in cases.items():
        if not any(reason in problem for problem in problems(source, text)):
            failures.append(f"{label} was not reported")
    if not any(
        "no diagnostic_codes! list" in problem for problem in problems("fn main() {}\n", page)
    ):
        failures.append("a source without the list was not reported")
    odd = source.replace(
        '    HeaderOmitted = "header_omitted", Omitted;',
        '    HeaderOmitted="header_omitted",Omitted;',
    )
    if not any("neither a declaration" in problem for problem in problems(odd, page)):
        failures.append("a declaration in another layout was not reported")
    real, _ = declared((ROOT / SOURCE).read_text(encoding="utf-8"))
    if (
        not real
        or len(set(real)) != len(real)
        or not all(re.fullmatch(r"[a-z]+(_[a-z]+)+", name) for name in real)
    ):
        failures.append(
            f"the declaration list of {SOURCE} did not read as unique snake_case names: {real}"
        )
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    source = (ROOT / SOURCE).read_text(encoding="utf-8")
    found = problems(source, (ROOT / PAGE).read_text(encoding="utf-8"))
    for problem in found:
        print(problem, file=sys.stderr)
    if not found:
        print(
            f"support: {PAGE} names each of the {len(declared(source)[0])} codes "
            f"{SOURCE} declares, and no other snake_case name"
        )
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
