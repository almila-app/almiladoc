"""Smoke test: the bindings load and every entry point round-trips a fixture."""

import ast
import io
import tempfile
import unittest
import zipfile
from importlib.metadata import distribution
from pathlib import Path

import almiladoc

FIXTURES = Path(__file__).resolve().parents[2] / "tests" / "fixtures"
OUTLINE = FIXTURES / "docx" / "handmade-outline.docx"
RICH = FIXTURES / "docx" / "handmade-rich.docx"
TABLES = FIXTURES / "docx" / "handmade-tables.docx"
CSV = FIXTURES / "csv" / "sheet.csv"
ENCRYPTED = FIXTURES / "malformed" / "encrypted--errors.odt"
ZIPBOMB = FIXTURES / "abuse" / "max_entry_bytes--zipbomb--errors.docx"
MIXED = FIXTURES / "pdf" / "handmade-mixed.pdf"
UNCLOSED = FIXTURES / "malformed" / "unclosed--recovers.docx"


class AlmiladocTest(unittest.TestCase):
    def test_to_markdown_detects_the_format_from_the_file_content(self):
        markdown = almiladoc.to_markdown(OUTLINE)
        self.assertRegex(markdown, r"(?m)^# ")

    def test_to_markdown_falls_back_to_the_extension(self):
        self.assertIn("| --- |", almiladoc.to_markdown(CSV))

    def test_to_markdown_bytes_converts_in_memory(self):
        markdown = almiladoc.to_markdown_bytes(TABLES.read_bytes(), "docx")
        self.assertIn("| Head A | Head B | Head C |", markdown)

    def test_to_markdown_bytes_detects_the_format_when_none_is_named(self):
        markdown = almiladoc.to_markdown_bytes(TABLES.read_bytes())
        self.assertIn("| Head A | Head B | Head C |", markdown)
        # CSV carries no signature, so it has to be named.
        with self.assertRaisesRegex(almiladoc.ConvertError, "unrecognized content"):
            almiladoc.to_markdown_bytes(CSV.read_bytes())
        self.assertIn("| --- |", almiladoc.to_markdown_bytes(CSV.read_bytes(), "csv"))

    def test_to_document_exposes_the_document_model(self):
        document = almiladoc.to_document(OUTLINE.read_bytes(), "docx")
        heading = next(block for block in document.blocks if block.kind == "heading")
        self.assertTrue(1 <= heading.level <= 6)
        self.assertIsInstance(heading.content[0].text, str)
        self.assertEqual(heading.content[0].kind, "text")
        self.assertIsInstance(heading.content[0].style.bold, bool)

    def test_to_document_carries_embedded_assets_as_bytes(self):
        document = almiladoc.to_document(RICH.read_bytes(), "docx")
        image = next(asset for asset in document.assets if asset.media_type == "image/png")
        self.assertIsInstance(image.data, bytes)
        self.assertGreater(len(image.data), 0)
        self.assertEqual(image.id, document.assets.index(image))

    def test_format_detection_reads_content_extension_and_path(self):
        self.assertEqual(almiladoc.format_from_bytes(RICH.read_bytes()), "docx")
        # CSV carries no signature: only the extension names it.
        self.assertIsNone(almiladoc.format_from_bytes(CSV.read_bytes()))
        self.assertEqual(almiladoc.format_from_extension(".pptm"), "pptx")
        self.assertEqual(almiladoc.format_from_extension("xls"), "xlsx")
        self.assertEqual(almiladoc.format_from_path("report.odt"), "odt")
        self.assertIsNone(almiladoc.format_from_path("report.unknown"))

    def test_conversion_errors_raise_the_subclass_that_names_the_failure(self):
        with self.assertRaises(almiladoc.MalformedError) as caught:
            almiladoc.to_markdown_bytes(b"not a document", "docx")
        # The base class still catches every one of them.
        self.assertIsInstance(caught.exception, almiladoc.ConvertError)
        # Nothing about these bytes is a package part.
        self.assertIsNone(caught.exception.part)

        with self.assertRaises(almiladoc.UnsupportedError):
            almiladoc.to_markdown_bytes(CSV.read_bytes())

        with self.assertRaises(almiladoc.EncryptedError):
            almiladoc.to_markdown_bytes(ENCRYPTED.read_bytes(), "odt")

        # A scanned page is reported, not dropped from the output.
        with self.assertRaises(almiladoc.NeedsOcrError) as caught:
            almiladoc.to_markdown(MIXED)
        self.assertEqual((caught.exception.pages, caught.exception.page_count), ([2], 2))

        with self.assertRaises(almiladoc.IncompleteError) as caught:
            almiladoc.to_markdown(UNCLOSED)
        self.assertEqual(caught.exception.diagnostics, [("recovered_xml", "lost", "body", None, 1)])
        self.assertIn("recovered_xml", str(caught.exception))

        with self.assertRaises(almiladoc.ResourceLimitError) as caught:
            almiladoc.to_markdown_bytes(ZIPBOMB.read_bytes(), "docx")
        self.assertEqual(caught.exception.limit, "max_entry_bytes")
        self.assertEqual(caught.exception.max, 128 * 1024 * 1024)
        self.assertNotIn("word/", str(caught.exception))

        # A readable package carrying none of the parts a docx is made of.
        package = io.BytesIO()
        with zipfile.ZipFile(package, "w") as archive:
            archive.writestr("[Content_Types].xml", "<Types/>")
        with self.assertRaises(almiladoc.MissingPartError) as caught:
            almiladoc.to_markdown_bytes(package.getvalue(), "docx")
        self.assertEqual(caught.exception.part, "word/document.xml")

    def test_every_failure_class_subclasses_the_base(self):
        for name in [
            "UnsupportedError",
            "NeedsOcrError",
            "MalformedError",
            "EncryptedError",
            "ResourceLimitError",
            "MissingPartError",
            "IncompleteError",
            "CancelledError",
            "InternalError",
        ]:
            self.assertTrue(issubclass(getattr(almiladoc, name), almiladoc.ConvertError), name)

    def test_a_part_name_is_escaped_in_the_error(self):
        package = io.BytesIO()
        with zipfile.ZipFile(package, "w") as archive:
            archive.writestr(
                "_rels/.rels",
                '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                '<Relationship Id="rId1" Target="word/doc\u202eument.xml" Type="http://schemas.'
                'openxmlformats.org/officeDocument/2006/relationships/officeDocument"/>'
                "</Relationships>",
            )
        with self.assertRaises(almiladoc.MissingPartError) as caught:
            almiladoc.to_markdown_bytes(package.getvalue(), "docx")
        self.assertEqual(caught.exception.part, "word/doc\\u{202e}ument.xml")
        self.assertNotIn("\u202e", str(caught.exception))

    def test_content_and_extension_that_name_no_format_are_unsupported(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sheet.unknown"
            path.write_bytes(CSV.read_bytes())
            with self.assertRaisesRegex(almiladoc.UnsupportedError, "unrecognized content"):
                almiladoc.to_markdown(path)
        with self.assertRaisesRegex(almiladoc.UnsupportedError, "unrecognized content"):
            almiladoc.to_document(CSV.read_bytes())
        self.assertEqual(len(almiladoc.to_document(CSV.read_bytes(), "csv").blocks), 1)

    def test_unreadable_files_and_bad_arguments_raise_the_python_exception(self):
        with self.assertRaises(FileNotFoundError):
            almiladoc.to_markdown("no-such-file.docx")
        with self.assertRaisesRegex(ValueError, "unknown format"):
            almiladoc.to_markdown_bytes(b"", "wat")

    def test_the_package_re_exports_the_native_module_and_nothing_else(self):
        self.assertEqual(distribution("almiladoc").metadata["Name"], "almiladoc")
        self.assertEqual(almiladoc._almiladoc.__name__, "almiladoc._almiladoc")
        for name in set(almiladoc.__all__) - {"Format"}:
            self.assertIs(getattr(almiladoc, name), getattr(almiladoc._almiladoc, name))
        for name in set(almiladoc.__all__) - {"Format"}:
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

    def test_ocr_is_not_a_parameter(self):
        with self.assertRaises(TypeError):
            almiladoc.to_markdown(MIXED, ocr="hosted")
        with self.assertRaises(TypeError):
            almiladoc.to_markdown_bytes(MIXED.read_bytes(), ocr="hosted")
        with self.assertRaises(almiladoc.NeedsOcrError):
            almiladoc.to_markdown_bytes(MIXED.read_bytes())

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
        # __init__.py re-exports the whole module, plus what it adds itself.
        self.assertEqual(set(almiladoc.__all__), exported | {"Format"})


if __name__ == "__main__":
    unittest.main()
