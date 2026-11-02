"""The guides' Python examples run, and the defaults they table equal the code's."""

import contextlib
import inspect
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from itertools import takewhile
from pathlib import Path

import almiladoc
from test_almiladoc import vision_server

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures"
GUIDES = ROOT / "docs" / "guide"
PAGES = [ROOT / "README.md", *sorted(GUIDES.glob("*.md"))]

INPUTS = {
    "report.docx": FIXTURES / "docx" / "handmade-outline.docx",
    "table.csv": FIXTURES / "csv" / "sheet.csv",
    "letter.rtf": FIXTURES / "rtf" / "text.rtf",
    "scan.pdf": FIXTURES / "pdf" / "handmade-mixed.pdf",
}
INPUT_NAME = re.compile(
    r'"([\w-]+\.(?:docx?|docm|odt|pdf|pptx?|rtf|epub|xlsx?|xlsb|ods|odp|csv))"'
)
PYTHON_BLOCK = re.compile(r"^```py\w*\n(.*?)^```$", re.DOTALL | re.MULTILINE)
PROMPT = re.compile(r'DEFAULT_PROMPT: &str = "((?:[^"\\]|\\.)*)";', re.DOTALL)
SIZE = re.compile(r"([\d,]+)(?: (KiB|MiB))?")
UNITS = {None: 0, "KiB": 10, "MiB": 20}
OCR_URL = "https://ocr.example/v1"
EXAMPLE_TIMEOUT = 120
REQUIRED = "required"


def example_blocks():
    return [
        (page.name, number, block)
        for page in PAGES
        for number, block in enumerate(PYTHON_BLOCK.findall(page.read_text(encoding="utf-8")), 1)
    ]


def table(page, head):
    """The rows of `page`'s table whose header opens with `head`, each field to its default."""
    lines = (GUIDES / page).read_text(encoding="utf-8").splitlines()
    heads = [number for number, line in enumerate(lines) if line.startswith(f"| {head} |")]
    assert len(heads) == 1, f"{page}: {len(heads)} tables headed {head}"
    rows = takewhile(lambda line: line.startswith("|"), lines[heads[0] + 2 :])
    cells = [[cell.strip() for cell in row.strip("|").split("|")] for row in rows]
    defaults = {field.strip("`"): default for field, default, *_ in cells}
    assert len(defaults) == len(cells), f"{page}: a field is tabled twice"
    return defaults


def value(cell):
    text = cell.strip("` ")
    if size := SIZE.fullmatch(text):
        return int(size[1].replace(",", "")) << UNITS[size[2]]
    return eval(text, {"__builtins__": {}}, vars(almiladoc))


class DocsTest(unittest.TestCase):
    def run_example(self, block):
        named = set(INPUT_NAME.findall(block))
        with tempfile.TemporaryDirectory() as directory, contextlib.ExitStack() as stack:
            for name in named:
                shutil.copy(INPUTS[name], Path(directory) / name)
            if OCR_URL in block:
                block = block.replace(OCR_URL, stack.enter_context(vision_server()).base_url)
            (Path(directory) / "example.py").write_text(block, encoding="utf-8")
            done = subprocess.run(
                [sys.executable, "-I", "-W", "error", "example.py"],
                cwd=directory,
                env={**os.environ, "OCR_API_KEY": "test-key"},
                capture_output=True,
                text=True,
                timeout=EXAMPLE_TIMEOUT,
            )
        self.assertEqual(done.returncode, 0, done.stderr)
        return named

    def test_every_python_example_runs(self):
        blocks = example_blocks()
        self.assertEqual({page for page, _, _ in blocks}, {page.name for page in PAGES})
        read = set()
        for page, number, block in blocks:
            with self.subTest(page=page, block=number):
                read |= self.run_example(block)
        self.assertEqual(read, set(INPUTS), "a supplied file no example reads")

    def assert_defaults(self, page, head, actual):
        documented = table(page, head)
        self.assertEqual(set(documented), set(actual), f"{page}: {head} table")
        for field, cell in documented.items():
            stated = cell if cell == REQUIRED else value(cell)
            self.assertEqual((type(stated), stated), (type(actual[field]), actual[field]), field)

    def test_the_ocr_guide_quotes_the_default_prompt(self):
        source = (ROOT / "src" / "ocr" / "http" / "mod.rs").read_text(encoding="utf-8")
        prompt = re.sub(r"\\\n\s*", "", PROMPT.search(source)[1])
        guide = (GUIDES / "ocr.md").read_text(encoding="utf-8")
        self.assertTrue(prompt in guide, f"the OCR guide does not quote {prompt!r}")

    def test_the_guides_state_the_defaults(self):
        limits, config = almiladoc.Limits(), almiladoc.Config()
        public = {name: getattr(limits, name) for name in dir(limits) if name.startswith("max_")}
        self.assert_defaults("integrate.md", "`Limits` field", public)
        public = {name: getattr(config, name) for name in dir(config) if not name.startswith("_")}
        self.assert_defaults("convert.md", "`Config` field", public)
        arguments = inspect.signature(almiladoc.OcrHttp).parameters
        public = {
            name: REQUIRED if argument.default is argument.empty else argument.default
            for name, argument in arguments.items()
        }
        self.assert_defaults("ocr.md", "`OcrHttp` argument", public)

    def test_the_tabled_ocr_defaults_are_the_ones_an_engine_takes(self):
        stated = {
            field: value(cell)
            for field, cell in table("ocr.md", "`OcrHttp` argument").items()
            if cell != REQUIRED
        }
        parsed = almiladoc.Converter().parse(INPUTS["scan.pdf"].read_bytes())
        requests = [r for r in parsed.ocr_requests() if isinstance(r, almiladoc.OcrRequest)]
        observed = []
        with vision_server() as server:
            for given in ({}, stated):
                engine = almiladoc.OcrHttp(server.base_url, "vision", **given)
                results = engine.recognize(requests[:1])
                pem, proxy = engine.root_certificates_pem, engine.proxy
                observed.append((repr(engine), pem, proxy, results, server.seen[-1]))
        self.assertEqual(observed[1], observed[0])


if __name__ == "__main__":
    unittest.main()
