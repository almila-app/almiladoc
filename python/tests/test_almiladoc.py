"""The binding's contract: its classes, input admission, errors and cancellation."""

import array
import ast
import contextlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zipfile
from importlib.metadata import distribution
from itertools import accumulate
from operator import attrgetter
from pathlib import Path

import almiladoc

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures"
SNAPSHOTS = ROOT / "tests" / "snapshots"
OUTLINE = FIXTURES / "docx" / "handmade-outline.docx"
RICH = FIXTURES / "docx" / "handmade-rich.docx"
TABLES = FIXTURES / "docx" / "handmade-tables.docx"
CSV = FIXTURES / "csv" / "sheet.csv"
RTF = FIXTURES / "rtf" / "text.rtf"
ENCRYPTED = FIXTURES / "malformed" / "encrypted--errors.odt"
ZIPBOMB = FIXTURES / "abuse" / "max_entry_bytes--zipbomb--errors.docx"
MIXED = FIXTURES / "pdf" / "handmade-mixed.pdf"
FIVE_PAGE = FIXTURES / "pdf" / "handmade-five-page.pdf"
PREDEFINED_CMAP = FIXTURES / "pdf" / "handmade-predefined-cmap.pdf"
UNCLOSED = FIXTURES / "malformed" / "unclosed-notes--skips.docx"

TIB = 1 << 40
U64_MAX = (1 << 64) - 1
FLOOD_CAP = 128 << 20
PROBE_TIMEOUT = 120
LINUX = sys.platform == "linux"
JSON_TOKENS = re.compile(r'"(?:[^"\\]|\\.)*"|([\[\]{}])', re.DOTALL)

limit_and_max = attrgetter("limit", "max")
code_and_kind = attrgetter("code", "kind")


def converter(**limits):
    return almiladoc.Converter(almiladoc.Config(limits=almiladoc.Limits(**limits)))


def refusal(error):
    return (type(error), error.code, error.limit, error.max, str(error))


def probe(code, *args, address_space):
    """Run `code` in a fresh interpreter whose address space is capped where
    Linux enforces the cap, so a copy or read of a whole oversized input
    fails at once instead of filling memory; each `report` prints a line,
    the repr of what its call raised or returned."""
    prelude = (
        "import sys\n"
        "if sys.platform == 'linux':\n"
        "    import resource\n"
        f"    resource.setrlimit(resource.RLIMIT_AS, ({address_space}, {address_space}))\n"
        "import almiladoc\n"
        "def limited(n):\n"
        "    return almiladoc.Converter("
        "almiladoc.Config(limits=almiladoc.Limits(max_input_bytes=n)))\n"
        "def report(call):\n"
        "    try:\n"
        "        print(repr(('returned', call())))\n"
        "    except almiladoc.ResourceLimitError as e:\n"
        "        print(repr(('ResourceLimitError', e.code, e.limit, e.max)))\n"
        "    except OSError as e:\n"
        "        print(repr(('OSError', type(e).__name__)))\n"
    )
    child = subprocess.run(
        [sys.executable, "-I", "-c", prelude + code, *args],
        capture_output=True,
        text=True,
        timeout=PROBE_TIMEOUT,
        check=True,
    )
    return [ast.literal_eval(line) for line in child.stdout.splitlines()]


def archive(parts):
    package = io.BytesIO()
    with zipfile.ZipFile(package, "w") as zipped:
        for name, content in parts.items():
            zipped.writestr(name, content)
    return package.getvalue()


def epub(body):
    container = (
        '<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">'
        '<rootfiles><rootfile full-path="content.opf" '
        'media-type="application/oebps-package+xml"/></rootfiles></container>'
    )
    opf = (
        '<package xmlns="http://www.idpf.org/2007/opf" version="3.0">'
        '<manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest>'
        '<spine><itemref idref="c"/></spine></package>'
    )
    chapter = f'<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>'
    return archive(
        {
            "mimetype": "application/epub+zip",
            "META-INF/container.xml": container,
            "content.opf": opf,
            "c.xhtml": chapter,
        }
    )


def odp(pages):
    slides = "".join(f"<draw:page>{page}</draw:page>" for page in pages)
    content = (
        "<office:document-content "
        'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" '
        'xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" '
        'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">'
        f"<office:body><office:presentation>{slides}</office:presentation></office:body>"
        "</office:document-content>"
    )
    return archive(
        {"mimetype": "application/vnd.oasis.opendocument.presentation", "content.xml": content}
    )


def nested_lists(levels):
    return epub("<ul><li>x" * levels + "</li></ul>" * levels)


def json_depth(text):
    steps = (1 if bracket in "[{" else -1 for bracket in JSON_TOKENS.findall(text) if bracket)
    return max(accumulate(steps, initial=0))


def unidentified_package():
    padding = "<!-- padding -->" * 128
    return archive({"_rels/.rels": f"<Relationships>{padding}</Relationships>"})


def rtf_flood(size):
    # `\x` is an unknown control word: it takes the reader's longest dispatch and
    # retains nothing, so a byte costs more time than in a flood of empty groups.
    return b"".join((b"{\\rtf1 ", b"\\x" * (size // 2), b"}"))


class ConversionTest(unittest.TestCase):
    def test_convert_path_detects_the_format_from_the_file_content(self):
        self.assertRegex(almiladoc.Converter().convert_path(OUTLINE).markdown, r"(?m)^# ")

    def test_convert_path_falls_back_to_the_extension(self):
        self.assertIn("| --- |", almiladoc.Converter().convert_path(CSV).markdown)

    def test_a_pdf_without_to_unicode_decodes_through_the_bundled_cid_table(self):
        markdown = almiladoc.Converter().convert_path(PREDEFINED_CMAP).markdown
        snapshot = SNAPSHOTS / "snapshots__pdf__handmade-predefined-cmap.pdf.snap"
        self.assertEqual(markdown, snapshot.read_text(encoding="utf-8").split("---\n", 2)[2])
        self.assertIn("한국어 글꼴의 문자 코드를 유니코드로", markdown)

    def test_a_scanned_page_is_a_gap_that_fails_needs_ocr_or_writes_its_marker(self):
        data = MIXED.read_bytes()
        with self.assertRaises(almiladoc.NeedsOcrError) as caught:
            almiladoc.Converter().convert(data)
        self.assertEqual((caught.exception.pages, caught.exception.page_count), ([2], 2))
        reporting = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        conversion = reporting.convert(data)
        self.assertEqual(
            conversion.markdown, "Text on the first page\n\n<!-- needs-ocr page 2 -->\n"
        )
        self.assertFalse(conversion.is_complete)
        self.assertEqual([unit[:2] for unit in conversion.units], [("page", 1), ("page", 2)])
        parsed = reporting.parse(data)
        self.assertEqual(parsed.gaps, [("page", 2, 1, 2, None, ["image_backed"])])
        self.assertEqual(parsed.render().markdown, conversion.markdown)
        self.assertFalse(parsed.render().is_complete)
        inspection = reporting.inspect(data, "pdf")
        self.assertEqual(inspection.page_count, 2)
        self.assertEqual(inspection.pages, ((1, "text", []), (2, "needs_ocr", ["image_backed"])))
        self.assertEqual([(d.code, d.unit) for d in inspection.diagnostics], [("ocr_gap", 2)])

    def test_ocr_pages_makes_each_chosen_pdf_page_a_gap(self):
        data = MIXED.read_bytes()
        reporting = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        cases = [
            ({1}, [["caller_requested"], ["image_backed"]]),
            (frozenset({1}), [["caller_requested"], ["image_backed"]]),
            ("all", [["caller_requested"], ["image_backed", "caller_requested"]]),
        ]
        for ocr_pages, reasons in cases:
            with self.subTest(ocr_pages=ocr_pages):
                parsed = reporting.parse(data, ocr_pages=ocr_pages)
                gaps = [("page", 1, 1, 1, None, reasons[0]), ("page", 2, 1, 2, None, reasons[1])]
                self.assertEqual(parsed.gaps, gaps)
                self.assertEqual(
                    [(d.code, d.unit) for d in parsed.diagnostics],
                    [("ocr_gap", 1), ("ocr_gap", 2)],
                )
                inspection = reporting.inspect(data, ocr_pages=ocr_pages)
                pages = ((1, "needs_ocr", reasons[0]), (2, "needs_ocr", reasons[1]))
                self.assertEqual(inspection.pages, pages)
                markdown = "<!-- needs-ocr page 1 -->\n\n<!-- needs-ocr page 2 -->\n"
                self.assertEqual(reporting.convert(data, ocr_pages=ocr_pages).markdown, markdown)
                converted = reporting.convert_path(MIXED, ocr_pages=ocr_pages)
                self.assertEqual(converted.markdown, markdown)
        csv = CSV.read_bytes()
        self.assertEqual(
            reporting.convert(csv, "csv", ocr_pages={7}).markdown,
            reporting.convert(csv, "csv").markdown,
        )

    def test_ocr_pages_past_the_last_raises_invalid_config_and_page_0_value_error(self):
        converter_ = almiladoc.Converter()
        data = MIXED.read_bytes()
        calls = [
            converter_.convert,
            converter_.parse,
            converter_.inspect,
            lambda _, **kwargs: converter_.convert_path(MIXED, **kwargs),
        ]
        for call in calls:
            with self.assertRaises(almiladoc.InvalidConfigError) as caught:
                call(data, ocr_pages={2, 3})
            self.assertEqual(
                attrgetter("code", "page", "page_count")(caught.exception),
                ("invalid_config", 3, 2),
            )
            for refused in [{0}, {-1}, {2**64}, "some"]:
                with self.subTest(refused=refused), self.assertRaises(ValueError):
                    call(data, ocr_pages=refused)
            with self.assertRaises(TypeError):
                call(data, ocr_pages={"1"})

    def test_fill_gap_writes_the_ocr_in_place_and_fail_gap_its_marker(self):
        data = FIVE_PAGE.read_bytes()
        parsed = almiladoc.Converter().parse(data)
        two, five = parsed.gaps
        for gap in [two, five]:
            parsed.fill_gap(gap, "# A\n\n| x |\n| - |\n| 1 |")
        conversion = parsed.render()
        self.assertEqual(
            conversion.markdown,
            "Text on the first page\n\n# A\n\n| x |\n| --- |\n| 1 |\n\n"
            "Text on the first page\n\nText on the first page\n\n"
            "# A\n\n| x |\n| --- |\n| 1 |\n",
        )
        self.assertTrue(conversion.is_complete)
        self.assertEqual((parsed.diagnostics, parsed.gaps), ((), [two, five]))
        for refill in [lambda: parsed.fill_gap(two, "x"), lambda: parsed.fail_gap(five)]:
            with self.assertRaises(ValueError) as caught:
                refill()
            self.assertNotIsInstance(caught.exception, almiladoc.GapRefused)

        reporting = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        failing, reported = almiladoc.Converter().parse(data), reporting.parse(data)
        for parsed in [failing, reported]:
            parsed.fill_gap(two, "two")
            parsed.fail_gap(five)
            self.assertEqual([(d.code, d.unit) for d in parsed.diagnostics], [("ocr_failed", 5)])
        with self.assertRaises(almiladoc.NeedsOcrError) as caught:
            failing.render()
        self.assertEqual(caught.exception.pages, [5])
        conversion = reported.render()
        self.assertEqual(
            conversion.markdown,
            "Text on the first page\n\ntwo\n\nText on the first page\n\n"
            "Text on the first page\n\n<!-- ocr-failed page 5 -->\n",
        )
        self.assertFalse(conversion.is_complete)

    def test_the_json_holds_each_fill_in_its_gap_s_place(self):
        reporting = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        parsed = reporting.parse(FIVE_PAGE.read_bytes())
        two, five = parsed.gaps
        parsed.fill_gap(two, "# A\n\n| x |\n| - |\n| 1 |")
        parsed.fail_gap(five)
        envelope = json.loads(parsed.to_json())
        blocks = envelope["document"]["blocks"]
        self.assertEqual(
            [block["kind"] for block in blocks],
            ["unit_start", "paragraph", "unit_start", "heading", "table"]
            + ["unit_start", "paragraph"] * 2
            + ["unit_start", "gap"],
        )
        heading, table = blocks[3]["value"], blocks[4]["value"]
        self.assertEqual(
            (heading["level"], [inline["value"]["text"] for inline in heading["content"]]),
            (1, ["A"]),
        )
        cells = [
            slot["value"]["blocks"][0]["value"][0]["value"]["text"]
            for row in table["grid"]
            for slot in row
        ]
        self.assertEqual((cells, table["header_rows"]), (["x", "1"], 1))
        self.assertEqual(blocks[-1]["value"]["origin"], {"kind": "page", "value": 5})
        self.assertEqual(
            [(d["code"], d["unit"], d["count"]) for d in envelope["diagnostics"]],
            [("ocr_failed", 5, 1)],
        )

    def test_a_fill_of_a_gap_no_row_names_or_past_a_limit_is_refused(self):
        data = MIXED.read_bytes()
        parsed = almiladoc.Converter().parse(data)
        (gap,) = parsed.gaps
        unknown = [
            ("page", 1, 1, 1, None, []),
            ("slide", 2, 1, 2, None, []),
            ("page", 2, 2, 2, None, []),
            (None, None, 1, 2, None, []),
            ("page", 2**64, 1, 2, None, []),
            ("page", 2, 2**64, 2, None, []),
            ("page", 2, -1, 2, None, []),
        ]
        for row in unknown:
            with self.subTest(row=row):
                with self.assertRaises(KeyError):
                    parsed.fill_gap(row, "x")
                with self.assertRaises(KeyError):
                    parsed.fail_gap(row)
        with self.assertRaises(TypeError):
            parsed.fill_gap(("page", "2", 1, 2, None, []), "x")
        refusals = [
            ("a" * ((1 << 20) + 1), "max_ocr_text_bytes"),
            ("> " * 257, "max_nesting_depth"),
        ]
        for markdown, limit in refusals:
            with self.assertRaises(almiladoc.GapRefused) as caught:
                parsed.fill_gap(gap, markdown)
            self.assertEqual(caught.exception.limit, limit)
            self.assertNotIsInstance(caught.exception, almiladoc.ConvertError)
        parsed.fill_gap(gap, "scanned")
        self.assertEqual(parsed.render().markdown, "Text on the first page\n\nscanned\n")

        def parses(max_work: int) -> bool:
            config = almiladoc.Config(limits=almiladoc.Limits(max_work=max_work))
            try:
                almiladoc.Converter(config).parse(data)
            except almiladoc.ResourceLimitError:
                return False
            return True

        low, high = 1, 1 << 20
        while low < high:
            middle = (low + high) // 2
            low, high = (low, middle) if parses(middle) else (middle + 1, high)
        config = almiladoc.Config(limits=almiladoc.Limits(max_work=low))
        starved = almiladoc.Converter(config).parse(data)
        with self.assertRaises(almiladoc.ResourceLimitError) as caught:
            starved.fill_gap(gap, "a")
        self.assertEqual((caught.exception.limit, caught.exception.max), ("max_work", low))

    def test_convert_takes_a_named_format_or_detects_it(self):
        conversion = almiladoc.Converter().convert(TABLES.read_bytes(), "docx")
        self.assertIn("| Head A | Head B | Head C |", conversion.markdown)
        self.assertEqual(conversion.diagnostics, ())
        self.assertTrue(conversion.is_complete)
        detected = almiladoc.Converter().convert(TABLES.read_bytes())
        self.assertEqual(detected.markdown, conversion.markdown)
        with self.assertRaisesRegex(almiladoc.UnsupportedError, "unrecognized content"):
            almiladoc.Converter().convert(CSV.read_bytes())
        self.assertIn("| --- |", almiladoc.Converter().convert(CSV.read_bytes(), "csv").markdown)

    def test_unit_offsets_slice_unicode_and_keep_empty_slides(self):
        pages = ["", "İlk 🦀", "", "Son", ""]
        deck = odp(
            f"<draw:frame><draw:text-box><text:p>{text}</text:p></draw:text-box></draw:frame>"
            for text in pages
        )
        result = almiladoc.Converter().convert(deck)
        self.assertEqual(len(result.units), len(pages))
        self.assertEqual(result.markdown.count("---"), 1)
        for number, ((kind, ordinal, start, end), text) in enumerate(
            zip(result.units, pages, strict=True), 1
        ):
            self.assertEqual((kind, ordinal), ("slide", number))
            self.assertEqual(result.markdown[start:end], text)
            if not text:
                self.assertEqual(start, end)
        self.assertEqual(
            almiladoc.Converter().convert(CSV.read_bytes(), "csv").units[0][:2], ("sheet", 1)
        )

    def test_every_contiguous_byte_buffer_converts_alike(self):
        data = CSV.read_bytes()
        expected = almiladoc.Converter().convert(data, "csv").markdown
        kinds = [
            bytearray,
            memoryview,
            lambda d: array.array("B", d),
            lambda d: memoryview(d).cast("c"),
        ]
        for kind in kinds:
            with self.subTest(kind=kind):
                self.assertEqual(
                    almiladoc.Converter().convert(kind(data), "csv").markdown, expected
                )
        self.assertEqual(almiladoc.sniff(bytearray(RICH.read_bytes())), "docx")
        self.assertEqual(almiladoc.sniff(memoryview(RICH.read_bytes())), "docx")

    def test_an_input_that_is_not_a_contiguous_byte_buffer_is_a_type_error(self):
        data = CSV.read_bytes()
        for value in [data.decode(), 7, None, array.array("i", [1, 2]), memoryview(data)[::2]]:
            with self.subTest(value=type(value)):
                with self.assertRaises(TypeError):
                    almiladoc.Converter().convert(value, "csv")
                with self.assertRaises(TypeError):
                    almiladoc.sniff(value)

    def test_parse_serialises_the_document_model_to_json(self):
        parsed = json.loads(almiladoc.Converter().parse(OUTLINE.read_bytes(), "docx").to_json())
        self.assertEqual(list(parsed), ["schema_version", "identity", "document", "diagnostics"])
        self.assertEqual(parsed["schema_version"], 5)
        self.assertEqual(list(parsed["identity"]), ["version", "config_fingerprint"])
        heading = next(b for b in parsed["document"]["blocks"] if b["kind"] == "heading")
        self.assertTrue(1 <= heading["value"]["level"] <= 6)
        text = heading["value"]["content"][0]
        self.assertEqual(text["kind"], "text")
        self.assertIsInstance(text["value"]["text"], str)
        self.assertIsInstance(text["value"]["style"]["bold"], bool)
        self.assertEqual(text["value"]["style"]["position"], "baseline")

    def test_a_lossy_parse_keeps_its_diagnostics_and_render_decides(self):
        parsed = almiladoc.Converter().parse(UNCLOSED.read_bytes(), "docx")
        self.assertEqual(
            json.loads(parsed.to_json())["diagnostics"],
            [
                {
                    "code": "part_unreadable",
                    "class": "lost",
                    "region": "note",
                    "unit": None,
                    "count": 1,
                }
            ],
        )
        [diagnostic] = parsed.diagnostics
        self.assertEqual(
            attrgetter("code", "loss_class", "region", "unit", "count")(diagnostic),
            ("part_unreadable", "lost", "note", None, 1),
        )
        with self.assertRaises(almiladoc.IncompleteError) as caught:
            parsed.render()
        self.assertEqual(caught.exception.diagnostics, parsed.diagnostics)

    def test_render_equals_convert(self):
        outcome = attrgetter("markdown", "diagnostics", "is_complete")
        for config, path in [
            (almiladoc.Config(), TABLES),
            (almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT), UNCLOSED),
        ]:
            with self.subTest(path=path.name):
                converter = almiladoc.Converter(config)
                converted = converter.convert(path.read_bytes())
                rendered = converter.parse(path.read_bytes()).render()
                self.assertEqual(outcome(rendered), outcome(converted))

    def test_report_returns_the_conversion_with_its_diagnostics(self):
        reporting = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        conversion = reporting.convert_path(UNCLOSED)
        self.assertFalse(conversion.is_complete)
        self.assertTrue(conversion.markdown)
        [diagnostic] = conversion.diagnostics
        self.assertIsInstance(diagnostic, almiladoc.Diagnostic)
        self.assertEqual(
            repr(diagnostic),
            "Diagnostic(code='part_unreadable', loss_class='lost', region='note', "
            "unit=None, count=1)",
        )
        with self.assertRaises(almiladoc.IncompleteError) as caught:
            almiladoc.Converter().convert_path(UNCLOSED)
        self.assertEqual(caught.exception.diagnostics, (diagnostic,))
        self.assertEqual(hash(caught.exception.diagnostics[0]), hash(diagnostic))
        self.assertIn("part_unreadable", str(caught.exception))
        with self.assertRaises(AttributeError):
            diagnostic.count = 2
        for name, message in [
            ("markdown", "^readonly attribute$"),
            ("units", "^readonly attribute$"),
            ("diagnostics", "^readonly attribute$"),
            (
                "is_complete",
                "^attribute 'is_complete' of 'almiladoc.Conversion' objects is not writable$",
            ),
        ]:
            with self.subTest(name=name):
                with self.assertRaisesRegex(AttributeError, message):
                    setattr(conversion, name, getattr(conversion, name))
                with self.assertRaisesRegex(AttributeError, message):
                    delattr(conversion, name)
        for name in ["markdown", "units", "diagnostics"]:
            self.assertIs(getattr(conversion, name), getattr(conversion, name), name)
        with self.assertRaises(TypeError):
            almiladoc.Diagnostic()

    def test_assets_carry_the_bytes_the_json_measures(self):
        parsed = almiladoc.Converter().parse(RICH.read_bytes(), "docx")
        measured = json.loads(parsed.to_json())["document"]["assets"]
        assets = parsed.assets()
        self.assertEqual(
            [(asset["id"], asset["media_type"], asset["byte_len"]) for asset in measured],
            [(index, media_type, len(data)) for index, media_type, data in assets],
        )
        png = next(data for _, media_type, data in assets if media_type == "image/png")
        self.assertIsInstance(png, bytes)
        self.assertTrue(png.startswith(b"\x89PNG"))

    def test_sniff_reads_only_the_content(self):
        self.assertEqual(almiladoc.sniff(RICH.read_bytes()), "docx")
        self.assertEqual(almiladoc.sniff(RTF.read_bytes()), "rtf")
        self.assertIsNone(almiladoc.sniff(CSV.read_bytes()))
        self.assertIsNone(almiladoc.sniff(b""))

    def test_json_nesting_stays_within_seven_levels_per_model_level(self):
        deep = almiladoc.Converter(
            almiladoc.Config(limits=almiladoc.Limits(max_nesting_depth=1024))
        )
        depth = {
            levels: json_depth(deep.parse(nested_lists(levels)).to_json()) for levels in [1, 300]
        }
        self.assertGreater(depth[300], 1000)
        self.assertLessEqual(depth[300], depth[1] + 7 * 299)

    def test_images_are_an_explicit_output_policy(self):
        default = almiladoc.Config()
        self.assertEqual(default.images, almiladoc.ImagePolicy.PLACEHOLDER)
        self.assertEqual(almiladoc.Config(images=None), default)
        policies = (
            almiladoc.ImagePolicy.PLACEHOLDER,
            almiladoc.ImagePolicy.ALT_TEXT,
            almiladoc.ImagePolicy.OMIT,
        )
        self.assertEqual(
            [repr(policy) for policy in policies],
            ["ImagePolicy.PLACEHOLDER", "ImagePolicy.ALT_TEXT", "ImagePolicy.OMIT"],
        )
        configs = [almiladoc.Config(images=p) for p in policies]
        self.assertEqual(len(set(configs)), 3)
        self.assertEqual(len({almiladoc.Converter(c).identity for c in configs}), 3)
        outcome = attrgetter("markdown", "diagnostics")
        source = rb"{\rtf1 before {\pict\wbitmap 01} after}"
        for policy, expected in zip(
            policies[1:], ("before Image after\n", "before  after\n"), strict=True
        ):
            converter = almiladoc.Converter(almiladoc.Config(images=policy))
            converted = converter.convert(source)
            self.assertEqual(converted.markdown, expected)
            self.assertTrue(converted.is_complete)
            rendered = converter.parse(source).render()
            self.assertEqual(outcome(rendered), outcome(converted))
        with self.assertRaises(TypeError):
            almiladoc.Config(images="omit")

    def test_image_ocr_leaves_a_gap_after_each_png_or_jpeg_for_the_caller_to_fill(self):
        self.assertEqual(almiladoc.Config().image_ocr, almiladoc.ImageOcr.NONE)
        self.assertEqual(
            [repr(setting) for setting in (almiladoc.ImageOcr.NONE, almiladoc.ImageOcr.ALL)],
            ["ImageOcr.NONE", "ImageOcr.ALL"],
        )
        config = almiladoc.Config(
            on_loss=almiladoc.OnLoss.REPORT, image_ocr=almiladoc.ImageOcr.ALL
        )
        self.assertNotEqual(config, almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        converter = almiladoc.Converter(config)
        self.assertNotEqual(
            converter.identity,
            almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT)).identity,
        )
        parsed = converter.parse(RICH.read_bytes())
        (gap,) = parsed.gaps
        self.assertEqual(gap, (None, None, 1, None, 0, []))
        self.assertEqual(parsed.assets()[0][2][:8], b"\x89PNG\r\n\x1a\n")
        self.assertIn(("ocr_gap", None), [(d.code, d.unit) for d in parsed.diagnostics])
        parsed.fill_gap(gap, "Read *aloud*")
        self.assertEqual(
            parsed.render().markdown,
            "Rich objects follow.\n\n**Quarterly Widgets**\n\n"
            "| Quarter | Widgets |\n| --- | --- |\n| Q1 | 10 |\n| Q2 | 14 |\n\n"
            "- Plan\n- Build\n- Ship\n\n"
            "![tiny dot image](asset:0)\n\nRead _aloud_\n\n"
            "After the objects.\n",
        )
        self.assertNotIn("ocr_gap", [d.code for d in parsed.diagnostics])

    def test_the_identity_is_the_configs(self):
        identity = almiladoc.Converter().identity
        self.assertRegex(identity.version, r"^\d+\.\d+\.\d+(-rc\.\d+)?$")
        self.assertEqual(identity.version.replace("-rc.", "rc"), distribution("almiladoc").version)
        self.assertRegex(identity.config_fingerprint, r"^[0-9a-f]{64}$")
        resourced = almiladoc.Converter(
            almiladoc.Config(threads=4, limits=almiladoc.Limits(max_work=9))
        )
        self.assertEqual(resourced.identity, identity)
        self.assertEqual(hash(resourced.identity), hash(identity))
        reporting = almiladoc.Converter(almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT))
        self.assertNotEqual(reporting.identity, identity)
        self.assertEqual(
            json.loads(almiladoc.Converter().parse(RTF.read_bytes()).to_json())["identity"],
            {"version": identity.version, "config_fingerprint": identity.config_fingerprint},
        )


class ConfigTest(unittest.TestCase):
    def test_defaults_are_almiladocs(self):
        limits = almiladoc.Limits()
        self.assertEqual(limits.max_input_bytes, 256 << 20)
        self.assertEqual(limits.max_nesting_depth, 256)
        self.assertEqual(limits.max_work, 512 << 20)
        self.assertEqual(limits.max_pages, 100_000)
        config = almiladoc.Config()
        self.assertEqual(
            attrgetter("limits", "threads", "on_loss")(config), (limits, 1, almiladoc.OnLoss.FAIL)
        )
        self.assertEqual(repr(almiladoc.OnLoss.REPORT), "OnLoss.REPORT")
        self.assertRegex(
            repr(config),
            r"^Config\(limits=Limits\(max_entry_bytes=134217728, .*, max_work=536870912, "
            r"max_pages=100000, max_ocr_text_bytes=1048576\), "
            r"threads=1, on_loss=OnLoss\.FAIL, images=ImagePolicy\.PLACEHOLDER, "
            r"image_ocr=ImageOcr\.NONE\)$",
        )

    def test_limits_and_config_are_frozen_keyword_only_values(self):
        limits = almiladoc.Limits(max_work=5, max_records=7)
        self.assertEqual(attrgetter("max_work", "max_records")(limits), (5, 7))
        self.assertEqual(limits, almiladoc.Limits(max_records=7, max_work=5))
        self.assertEqual(hash(limits), hash(almiladoc.Limits(max_records=7, max_work=5)))
        self.assertNotEqual(limits, almiladoc.Limits(max_work=5))
        config = almiladoc.Config(limits=limits, threads=2, on_loss=almiladoc.OnLoss.REPORT)
        same = almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT, threads=2, limits=limits)
        self.assertEqual(config, same)
        self.assertEqual(hash(config), hash(same))
        self.assertEqual(len({config, same, almiladoc.Config()}), 2)
        for other in [
            almiladoc.Config(limits=limits, threads=3, on_loss=almiladoc.OnLoss.REPORT),
            almiladoc.Config(limits=limits, threads=2),
            almiladoc.Config(threads=2, on_loss=almiladoc.OnLoss.REPORT),
        ]:
            self.assertNotEqual(config, other)
        with self.assertRaises(AttributeError):
            config.threads = 4
        with self.assertRaises(AttributeError):
            limits.max_work = 4
        with self.assertRaises(TypeError):
            almiladoc.Config(None, 2)
        with self.assertRaises(TypeError):
            almiladoc.Limits(5)

    def test_a_field_out_of_range_raises_config_error_where_it_is_set(self):
        cases = [
            (lambda: almiladoc.Limits(max_work=0), ("max_work", None, None)),
            (lambda: almiladoc.Limits(max_input_bytes=0), ("max_input_bytes", None, None)),
            (lambda: almiladoc.Limits(max_nesting_depth=1025), ("max_nesting_depth", 1024, None)),
            (lambda: almiladoc.Limits(max_pages=1 << 32), ("max_pages", (1 << 32) - 1, None)),
            (lambda: almiladoc.Config(threads=0), ("threads", None, 64)),
            (lambda: almiladoc.Config(threads=65), ("threads", None, 64)),
        ]
        for make, expected in cases:
            with (
                self.subTest(expected=expected),
                self.assertRaises(almiladoc.ConfigError) as caught,
            ):
                make()
            error = caught.exception
            self.assertEqual(attrgetter("limit", "ceiling", "max")(error), expected)
            self.assertIsInstance(error, ValueError)
            self.assertNotIsInstance(error, almiladoc.ConvertError)
        self.assertEqual(almiladoc.Limits(max_nesting_depth=1024).max_nesting_depth, 1024)
        self.assertEqual(almiladoc.Limits(max_pages=(1 << 32) - 1).max_pages, (1 << 32) - 1)
        self.assertEqual(almiladoc.Config(threads=64).threads, 64)
        self.assertEqual(almiladoc.Limits(max_work=U64_MAX).max_work, U64_MAX)
        with self.assertRaises(OverflowError):
            almiladoc.Limits(max_work=-1)

    def test_detection_runs_under_the_converters_limits(self):
        package = unidentified_package()
        self.assertIsNone(almiladoc.sniff(package))
        with self.assertRaisesRegex(almiladoc.UnsupportedError, "unrecognized content"):
            almiladoc.Converter().convert(package)
        starved = converter(max_work=1)
        for call in [starved.convert, starved.parse, starved.sniff]:
            with self.subTest(call=call.__name__):
                with self.assertRaises(almiladoc.ResourceLimitError) as caught:
                    call(package)
                self.assertEqual(limit_and_max(caught.exception), ("max_work", 1))

    def test_converter_sniff_names_the_format_the_content_is(self):
        converter_ = almiladoc.Converter()
        self.assertEqual(converter_.sniff(RICH.read_bytes()), "docx")
        self.assertEqual(converter_.sniff(bytearray(RTF.read_bytes())), "rtf")
        self.assertEqual(converter_.sniff(memoryview(MIXED.read_bytes())), "pdf")
        self.assertIsNone(converter_.sniff(CSV.read_bytes()))
        self.assertIsNone(converter_.sniff(unidentified_package()))
        with self.assertRaises(TypeError):
            converter_.sniff(RTF.read_bytes().decode())

    def test_limits_reach_every_call(self):
        nested = epub("<ul><li>x" * 9 + "</li></ul>" * 9)
        shallow = converter(max_nesting_depth=8)
        for call in [shallow.convert, shallow.parse]:
            with (
                self.subTest(call=call.__name__),
                self.assertRaises(almiladoc.ResourceLimitError) as caught,
            ):
                call(nested)
            self.assertEqual(limit_and_max(caught.exception), ("max_nesting_depth", 8))
        with self.assertRaises(almiladoc.ResourceLimitError) as caught:
            converter(max_work=1).convert(TABLES.read_bytes(), "docx")
        self.assertEqual(caught.exception.limit, "max_work")
        threaded = almiladoc.Converter(almiladoc.Config(threads=3)).convert(TABLES.read_bytes())
        self.assertEqual(
            threaded.markdown, almiladoc.Converter().convert(TABLES.read_bytes()).markdown
        )

    def test_max_pages_refuses_the_first_slide_numbered_past_it(self):
        deck = odp([""] * 3)
        self.assertTrue(converter(max_pages=3).convert(deck, "odp").is_complete)
        with self.assertRaises(almiladoc.ResourceLimitError) as caught:
            converter(max_pages=2).convert(deck, "odp")
        self.assertEqual(limit_and_max(caught.exception), ("max_pages", 2))


class AdmissionTest(unittest.TestCase):
    def test_an_input_over_max_input_bytes_is_refused_before_it_is_copied_or_read(self):
        data = b"a,b\n1,23\n"
        self.assertEqual(len(data), 9)
        small = converter(max_input_bytes=8)
        refused = (
            almiladoc.ResourceLimitError,
            "resource_limit",
            "max_input_bytes",
            8,
            "resource limit exceeded: max_input_bytes is 8",
        )
        for value in [data, bytearray(data), memoryview(data), array.array("B", data)]:
            with self.subTest(value=type(value)):
                with self.assertRaises(almiladoc.ResourceLimitError) as caught:
                    small.convert(value, "csv")
                self.assertEqual(refusal(caught.exception), refused)
                with self.assertRaises(almiladoc.ResourceLimitError):
                    small.parse(value, "csv")
                with self.assertRaises(almiladoc.ResourceLimitError):
                    small.convert(value)
                with self.assertRaises(almiladoc.ResourceLimitError) as caught:
                    small.sniff(value)
                self.assertEqual(refusal(caught.exception), refused)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sheet.csv"
            path.write_bytes(data)
            with self.assertRaises(almiladoc.ResourceLimitError) as caught:
                small.convert_path(path)
            self.assertEqual(refusal(caught.exception), refused)
            self.assertIn("| 1 | 23 |", converter(max_input_bytes=9).convert_path(path).markdown)
        for value in [data[:8], bytearray(data[:8]), memoryview(data[:8])]:
            self.assertTrue(small.convert(value, "csv").markdown)

    def test_a_file_at_any_max_input_bytes_reads_whole(self):
        widest = converter(max_input_bytes=U64_MAX)
        expected = almiladoc.Converter().convert_path(CSV).markdown
        self.assertEqual(widest.convert_path(CSV).markdown, expected)

    def test_sniff_admits_under_the_default_max_input_bytes(self):
        with self.assertRaises(almiladoc.ResourceLimitError) as caught:
            almiladoc.sniff(bytes((256 << 20) + 1))
        self.assertEqual(limit_and_max(caught.exception), ("max_input_bytes", 256 << 20))

    @unittest.skipUnless(LINUX, "the probe caps the address space and maps a sparse file")
    def test_a_terabyte_mapping_is_refused_before_any_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "huge.bin"
            with path.open("wb") as file:
                file.truncate(TIB)
            caught = probe(
                "import mmap\n"
                "with open(sys.argv[1], 'rb') as f:\n"
                "    with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:\n"
                "        report(lambda: limited(8).convert(m, 'csv'))\n"
                "        report(lambda: limited(2**64 - 1).convert(m, 'csv'))\n",
                str(path),
                address_space=TIB + (16 << 30),
            )
        self.assertEqual(
            caught,
            [
                ("ResourceLimitError", "resource_limit", "max_input_bytes", 8),
                ("ResourceLimitError", "resource_limit", "allocation", TIB),
            ],
        )

    @unittest.skipUnless(LINUX, "the probe caps the address space and reads a sparse file")
    def test_a_terabyte_file_is_refused_by_its_size_unread(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "huge.csv"
            with path.open("wb") as file:
                file.truncate(TIB)
            [caught] = probe(
                "report(lambda: limited(int(sys.argv[2])).convert_path(sys.argv[1]))\n",
                str(path),
                str(TIB - 1),
                address_space=4 << 30,
            )
        self.assertEqual(
            caught, ("ResourceLimitError", "resource_limit", "max_input_bytes", TIB - 1)
        )

    @unittest.skipUnless(Path("/proc/self/pagemap").exists(), "needs files that report no size")
    def test_a_file_longer_than_its_size_is_read_one_byte_past_the_limit(self):
        with self.assertRaises(almiladoc.ResourceLimitError) as caught:
            converter(max_input_bytes=8).convert_path("/proc/self/status", "csv")
        self.assertEqual(limit_and_max(caught.exception), ("max_input_bytes", 8))
        # pagemap reads only in multiples of 8 bytes, and holds 8 per page of the address space.
        [caught] = probe(
            "report(lambda: limited(7).convert_path('/proc/self/pagemap', 'csv'))\n",
            address_space=4 << 30,
        )
        self.assertEqual(caught, ("ResourceLimitError", "resource_limit", "max_input_bytes", 7))

    @unittest.skipUnless(hasattr(os, "mkfifo"), "needs a FIFO")
    def test_a_file_that_is_not_regular_raises_os_error_unopened(self):
        with tempfile.TemporaryDirectory() as directory:
            fifo = Path(directory) / "pipe.csv"
            os.mkfifo(fifo)
            [caught] = probe(
                "report(lambda: almiladoc.Converter().convert_path(sys.argv[1]))\n",
                str(fifo),
                address_space=4 << 30,
            )
            self.assertEqual(caught, ("OSError", "OSError"))
            with self.assertRaises(OSError) as raised:
                almiladoc.Converter().convert_path(directory)
            self.assertNotIsInstance(raised.exception, almiladoc.ConvertError)


class CancellationTest(unittest.TestCase):
    def test_a_cancelled_token_refuses_every_call_before_detection_and_size(self):
        token = almiladoc.CancelToken()
        self.assertFalse(token.cancelled)
        self.assertTrue(
            almiladoc.Converter().convert(CSV.read_bytes(), "csv", cancel=token).markdown
        )
        self.assertEqual(almiladoc.Converter().sniff(RTF.read_bytes(), cancel=token), "rtf")
        token.cancel()
        self.assertTrue(token.cancelled)
        for call in [
            lambda: almiladoc.Converter().convert(b"no format at all", cancel=token),
            lambda: almiladoc.Converter().parse(b"no format at all", cancel=token),
            lambda: converter(max_input_bytes=8).convert(
                bytearray(CSV.read_bytes()), cancel=token
            ),
            lambda: converter(max_input_bytes=8).convert(CSV.read_bytes(), "csv", cancel=token),
            lambda: almiladoc.Converter().convert_path(CSV, cancel=token),
            lambda: almiladoc.Converter().convert(TABLES.read_bytes(), "docx", cancel=token),
            lambda: almiladoc.Converter().sniff(RTF.read_bytes(), cancel=token),
            lambda: almiladoc.Converter().sniff(b"no format at all", cancel=token),
            lambda: converter(max_input_bytes=8).sniff(CSV.read_bytes(), cancel=token),
            lambda: converter(max_input_bytes=8).sniff(bytearray(CSV.read_bytes()), cancel=token),
        ]:
            with self.assertRaises(almiladoc.CancelledError) as caught:
                call()
            self.assertEqual(caught.exception.code, "cancelled")

    def test_a_parse_keeps_its_token_for_render_and_to_json(self):
        token = almiladoc.CancelToken()
        parsed = almiladoc.Converter().parse(CSV.read_bytes(), "csv", cancel=token)
        self.assertEqual(parsed.render().markdown[:6], "| Kind")
        token.cancel()
        for call in [parsed.render, parsed.to_json]:
            with self.assertRaises(almiladoc.CancelledError) as caught:
                call()
            self.assertEqual(caught.exception.code, "cancelled")

    def test_a_running_conversion_is_cancelled_from_another_thread(self):
        unbounded = converter(max_work=U64_MAX)
        sample = rtf_flood(4 << 20)
        began = time.perf_counter()
        unbounded.convert(sample, "rtf")
        seconds_per_byte = (time.perf_counter() - began) / len(sample)
        data = rtf_flood(min(int(1.5 / seconds_per_byte), FLOOD_CAP))
        expected = len(data) * seconds_per_byte
        self.assertGreater(expected, 0.5, "the host converts too fast to cancel a running call")
        token = almiladoc.CancelToken()
        started = threading.Event()
        outcome = {}

        def work():
            started.set()
            try:
                outcome["result"] = unbounded.convert(data, "rtf", cancel=token)
            except almiladoc.ConvertError as error:
                outcome["result"] = error
            outcome["ended"] = time.perf_counter()

        worker = threading.Thread(target=work)
        worker.start()
        started.wait()
        time.sleep(0.05)
        cancelled = time.perf_counter()
        token.cancel()
        worker.join(timeout=60)
        self.assertFalse(worker.is_alive())
        self.assertIsInstance(outcome["result"], almiladoc.CancelledError)
        self.assertLess(outcome["ended"] - cancelled, expected / 2)


class ErrorTest(unittest.TestCase):
    def test_conversion_errors_raise_the_subclass_that_names_the_failure(self):
        converter_ = almiladoc.Converter()
        cases = [
            (
                almiladoc.MalformedError,
                lambda: converter_.convert(b"not a document", "docx"),
                {"code": "malformed", "part": None},
            ),
            (
                almiladoc.UnsupportedError,
                lambda: converter_.convert(CSV.read_bytes()),
                {"code": "unsupported"},
            ),
            (
                almiladoc.EncryptedError,
                lambda: converter_.convert(ENCRYPTED.read_bytes(), "odt"),
                {"code": "encrypted"},
            ),
            (
                almiladoc.NeedsOcrError,
                lambda: converter_.convert_path(MIXED),
                {"code": "needs_ocr", "pages": [2], "page_count": 2},
            ),
            (
                almiladoc.IncompleteError,
                lambda: converter_.convert_path(UNCLOSED),
                {"code": "incomplete"},
            ),
            (
                almiladoc.InvalidConfigError,
                lambda: converter_.convert_path(MIXED, ocr_pages={3}),
                {"code": "invalid_config", "page": 3, "page_count": 2},
            ),
            (
                almiladoc.ResourceLimitError,
                lambda: converter_.convert(ZIPBOMB.read_bytes(), "docx"),
                {"code": "resource_limit", "limit": "max_entry_bytes", "max": 128 * 1024 * 1024},
            ),
            (
                almiladoc.MissingPartError,
                lambda: converter_.convert(archive({"[Content_Types].xml": "<Types/>"}), "docx"),
                {"code": "missing_part", "part": "word/document.xml"},
            ),
        ]
        raised = {}
        for error_class, call, attributes in cases:
            with self.subTest(error=error_class.__name__):
                with self.assertRaises(error_class) as caught:
                    call()
                error = raised[error_class] = caught.exception
                self.assertIsInstance(error, almiladoc.ConvertError)
                self.assertEqual({name: getattr(error, name) for name in attributes}, attributes)
        self.assertNotIn("word/", str(raised[almiladoc.ResourceLimitError]))

    def test_content_its_format_contradicts_names_the_format_to_retry_with(self):
        converter_ = almiladoc.Converter()
        data = RTF.read_bytes()
        for call in [converter_.convert, converter_.parse]:
            with self.assertRaisesRegex(
                almiladoc.UnsupportedError, "the content is rtf"
            ) as caught:
                call(data, "doc")
            self.assertEqual(
                attrgetter("code", "kind", "detected")(caught.exception),
                ("unsupported", "format_mismatch", "rtf"),
            )
        self.assertTrue(converter_.convert(data, format=caught.exception.detected).markdown)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "letter.doc"
            path.write_bytes(data)
            self.assertEqual(
                converter_.convert_path(path).markdown, converter_.convert(data).markdown
            )
            with self.assertRaises(almiladoc.UnsupportedError) as caught:
                converter_.convert_path(path, "doc")
            self.assertEqual(caught.exception.detected, "rtf")

        with self.assertRaises(almiladoc.UnsupportedError) as caught:
            converter_.inspect(CSV.read_bytes(), "csv")
        self.assertIsNone(caught.exception.detected)
        self.assertEqual(code_and_kind(caught.exception), ("unsupported", "not_inspectable"))
        with self.assertRaises(almiladoc.UnsupportedError) as caught:
            converter_.parse(CSV.read_bytes())
        self.assertIsNone(caught.exception.detected)
        self.assertEqual(code_and_kind(caught.exception), ("unsupported", "unknown_format"))

    def test_every_failure_class_subclasses_the_base(self):
        for name in [
            "UnsupportedError",
            "NeedsOcrError",
            "MalformedError",
            "EncryptedError",
            "ResourceLimitError",
            "MissingPartError",
            "IncompleteError",
            "InvalidConfigError",
            "CancelledError",
            "InternalError",
        ]:
            self.assertTrue(issubclass(getattr(almiladoc, name), almiladoc.ConvertError), name)
        self.assertTrue(issubclass(almiladoc.ConfigError, ValueError))
        self.assertFalse(issubclass(almiladoc.ConfigError, almiladoc.ConvertError))

    def test_a_part_name_is_escaped_in_the_error(self):
        package = archive(
            {
                "_rels/.rels": (
                    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/'
                    'relationships"><Relationship Id="rId1" Target="word/doc\u202eument.xml" '
                    'Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
                    'officeDocument"/></Relationships>'
                )
            }
        )
        with self.assertRaises(almiladoc.MissingPartError) as caught:
            almiladoc.Converter().convert(package, "docx")
        self.assertEqual(caught.exception.part, "word/doc\\u{202e}ument.xml")
        self.assertNotIn("\u202e", str(caught.exception))

    def test_content_and_extension_that_name_no_format_are_unsupported(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sheet.unknown"
            path.write_bytes(CSV.read_bytes())
            with self.assertRaisesRegex(
                almiladoc.UnsupportedError, "unrecognized content"
            ) as caught:
                almiladoc.Converter().convert_path(path)
            self.assertEqual(code_and_kind(caught.exception), ("unsupported", "unknown_format"))
            self.assertIsNone(caught.exception.detected)
            self.assertIn("| --- |", almiladoc.Converter().convert_path(path, "csv").markdown)
        parsed = json.loads(almiladoc.Converter().parse(CSV.read_bytes(), "csv").to_json())
        unit, table = parsed["document"]["blocks"]
        self.assertEqual(unit, {"kind": "unit_start", "value": {"kind": "sheet", "ordinal": 1}})
        self.assertEqual(table["kind"], "table")

    def test_unreadable_files_and_bad_arguments_raise_the_python_exception(self):
        with self.assertRaises(FileNotFoundError) as caught:
            almiladoc.Converter().convert_path("no-such-file.docx")
        self.assertFalse(hasattr(caught.exception, "code"))
        for name in ["wat", "docm", "DOCX", ".docx", ""]:
            for call in [almiladoc.Converter().convert, almiladoc.Converter().parse]:
                with self.subTest(name=name), self.assertRaises(ValueError) as caught:
                    call(RTF.read_bytes(), name)
                self.assertNotIsInstance(caught.exception, almiladoc.ConvertError)
                self.assertFalse(hasattr(caught.exception, "code"))
                self.assertEqual(
                    str(caught.exception),
                    "unknown format; expected one of "
                    "doc, docx, odt, pdf, ppt, pptx, rtf, epub, xlsx, ods, odp, csv",
                )

    def test_ocr_is_not_a_parameter(self):
        with self.assertRaises(TypeError):
            almiladoc.Converter().convert_path(MIXED, ocr="hosted")
        with self.assertRaises(TypeError):
            almiladoc.Converter().convert(MIXED.read_bytes(), ocr="hosted")
        with self.assertRaises(almiladoc.NeedsOcrError):
            almiladoc.Converter().convert(MIXED.read_bytes())


class PackageTest(unittest.TestCase):
    def test_the_package_re_exports_the_native_module_and_nothing_else(self):
        self.assertEqual(distribution("almiladoc").metadata["Name"], "almiladoc")
        self.assertEqual(almiladoc._almiladoc.__name__, "almiladoc._almiladoc")
        for name in set(almiladoc.__all__) - {"Format"}:
            self.assertIs(getattr(almiladoc, name), getattr(almiladoc._almiladoc, name))
            if isinstance(getattr(almiladoc, name), type):
                self.assertEqual(getattr(almiladoc, name).__module__, "almiladoc", name)
        source = Path(almiladoc.__file__).read_text(encoding="utf-8")
        imported = {
            node.module if isinstance(node, ast.ImportFrom) else alias.name
            for node in ast.walk(ast.parse(source))
            if isinstance(node, (ast.Import, ast.ImportFrom))
            for alias in node.names
        }
        self.assertEqual(imported, {"typing", "almiladoc._almiladoc"})

    def test_every_public_name_has_a_docstring(self):
        for name in sorted(set(almiladoc.__all__) - {"Format"}):
            value = getattr(almiladoc, name)
            self.assertTrue(value.__doc__, name)
            for member, attribute in vars(value).items() if isinstance(value, type) else ():
                if not member.startswith("_"):
                    self.assertTrue(getattr(attribute, "__doc__", None), f"{name}.{member}")

    def test_conversion_documents_itself_as_its_stub_does(self):
        stub = Path(almiladoc.__file__).with_name("_almiladoc.pyi")
        [conversion] = [
            node
            for node in ast.parse(stub.read_text(encoding="utf-8")).body
            if isinstance(node, ast.ClassDef) and node.name == "Conversion"
        ]
        stubbed = {
            node.name: ast.get_docstring(node)
            for node in conversion.body
            if isinstance(node, ast.FunctionDef)
        }
        self.assertEqual(set(stubbed), {"markdown", "units", "diagnostics", "is_complete"})
        documented = {name: getattr(almiladoc.Conversion, name).__doc__ for name in stubbed}
        self.maxDiff = None
        self.assertEqual(
            (almiladoc.Conversion.__doc__, documented), (ast.get_docstring(conversion), stubbed)
        )

    def test_the_stubs_cover_the_module(self):
        self.assertTrue(Path(almiladoc.__file__).with_name("py.typed").is_file())
        stub = Path(almiladoc.__file__).with_name("_almiladoc.pyi")
        stubbed = {
            node.name
            for node in ast.parse(stub.read_text(encoding="utf-8")).body
            if isinstance(node, (ast.FunctionDef, ast.ClassDef))
        }
        exported = {name for name in dir(almiladoc._almiladoc) if not name.startswith("_")}
        self.assertEqual(stubbed, exported)
        self.assertEqual(set(almiladoc.__all__), exported | {"Format"})

    def test_the_readme_example_runs(self):
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        block = re.search(r"```python\n(.*?)```", readme, re.DOTALL).group(1)
        files = {
            "report.docx": OUTLINE,
            "table.csv": CSV,
            "letter.rtf": RTF,
            "scan.pdf": MIXED,
        }
        self.assertEqual(set(re.findall(r'"([\w-]+\.\w+)"', block)), set(files))
        with tempfile.TemporaryDirectory() as directory:
            for name, fixture in files.items():
                shutil.copy(fixture, Path(directory) / name)
            with contextlib.chdir(directory):
                exec(compile(block, "README.md", "exec"), {"__name__": "readme"})


if __name__ == "__main__":
    unittest.main()
