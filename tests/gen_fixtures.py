#!/usr/bin/env python3
"""Regenerate the committed fixture corpus in tests/fixtures/.

LibreOffice (headless) and Pandoc convert the sources in tests/fixture-src/; the handmade,
malformed and abuse fixtures come from tests/fixturegen/<format>.py. /add-fixture owns their
placement and naming.

Usage: python tests/gen_fixtures.py [--skip-office]
"""

import os
import re
import shutil
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

from fixturegen import csv, doc, docx, epub, odf, pdf, ppt, pptx, rtf, sheet
from fixturegen.common import DOT_PNG

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "tests" / "fixture-src"
OUT = ROOT / "tests" / "fixtures"
SOFFICE_FALLBACKS = (
    r"C:\Program Files\LibreOffice\program\soffice.exe",
    "/Applications/LibreOffice.app/Contents/MacOS/soffice",
)
FORMATS = (docx, pptx, odf, doc, epub, rtf, ppt, sheet, pdf, csv)


def run(cmd: Sequence[str]) -> None:
    print("+", " ".join(cmd))
    subprocess.run(cmd, check=True)


def find_soffice() -> str:
    found = os.environ.get("SOFFICE") or shutil.which("soffice")
    if found:
        return found
    for path in SOFFICE_FALLBACKS:
        if Path(path).exists():
            return path
    raise FileNotFoundError("soffice not found: set SOFFICE to the LibreOffice executable")


def convert_lo(src: Path, filter_spec: str, out_dir: Path, final_name: str) -> None:
    soffice = find_soffice()
    out_dir.mkdir(parents=True, exist_ok=True)
    profile = ROOT / "target" / "lo-profile"
    ext = filter_spec.split(":")[0]
    produced = out_dir / (src.stem + "." + ext)
    produced.unlink(missing_ok=True)
    # The very first soffice launch may only initialize its profile; retry.
    for _ in range(3):
        run(
            [
                soffice,
                "--headless",
                "--norestore",
                f"-env:UserInstallation={profile.as_uri()}",
                "--convert-to",
                filter_spec,
                "--outdir",
                str(out_dir),
                str(src),
            ]
        )
        if produced.exists():
            break
    else:
        raise RuntimeError(f"soffice produced no output for {src} -> {filter_spec}")
    target = out_dir / final_name
    if produced != target:
        if target.exists():
            target.unlink()
        produced.rename(target)
    if ext == "rtf":
        anchor_local_paths(target)


# LibreOffice resolves a relative hyperlink against the source file's own
# location on RTF export, so the generating machine's absolute path lands in
# the fixture. Anchor it to a fixed root: the fixture tests that a `HYPERLINK`
# field carrying a file URL is read, not which directory generated it.
FIXTURE_URI_ROOT = b"file:///almiladoc"


def anchor_local_paths(path: Path) -> None:
    data = path.read_bytes()
    anchored = re.sub(
        rb'file:///[^"]*?/tests/fixture-src/', FIXTURE_URI_ROOT + b"/tests/fixture-src/", data
    )
    if anchored != data:
        path.write_bytes(anchored)


def main() -> None:
    skip_office = "--skip-office" in sys.argv
    for sub in [
        "odt",
        "docx",
        "doc",
        "rtf",
        "ods",
        "xlsx",
        "xls",
        "csv",
        "odp",
        "pptx",
        "ppt",
        "epub",
        "pdf",
        "malformed",
        "abuse",
    ]:
        (OUT / sub).mkdir(parents=True, exist_ok=True)

    if not skip_office:
        text = SRC / "text.fodt"
        convert_lo(text, "odt", OUT / "odt", "text.odt")
        convert_lo(text, "docx", OUT / "docx", "text.docx")
        convert_lo(text, "doc:MS Word 97", OUT / "doc", "text.doc")
        convert_lo(text, "rtf:Rich Text Format", OUT / "rtf", "text.rtf")
        convert_lo(text, "pdf:writer_pdf_Export", OUT / "pdf", "text.pdf")
        calc = SRC / "sheet.fods"
        convert_lo(calc, "ods", OUT / "ods", "sheet.ods")
        convert_lo(calc, "xlsx:Calc MS Excel 2007 XML", OUT / "xlsx", "sheet.xlsx")
        convert_lo(calc, "xls:MS Excel 97", OUT / "xls", "sheet.xls")
        convert_lo(calc, "csv:Text - txt - csv (StarCalc)", OUT / "csv", "sheet.csv")
        pres = SRC / "pres.fodp"
        convert_lo(pres, "odp", OUT / "odp", "pres.odp")
        convert_lo(pres, "pptx:Impress MS PowerPoint 2007 XML", OUT / "pptx", "pres.pptx")
        convert_lo(pres, "ppt:MS PowerPoint 97", OUT / "ppt", "pres.ppt")
        (SRC / "dot.png").write_bytes(DOT_PNG)
        pandoc = ["pandoc", str(SRC / "book.md"), "-o", str(OUT / "epub" / "book.epub")]
        run([*pandoc, "--resource-path", str(SRC)])

    for module in FORMATS:
        module.generate(OUT)
    print("fixture generation complete")


if __name__ == "__main__":
    main()
