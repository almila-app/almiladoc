"""Caller-owned corpus validation and mocked OCR; these images prove no model quality."""

import contextlib
import hashlib
import importlib.util
import io
import json
import struct
import subprocess
import tempfile
import unittest
import zlib
from pathlib import Path
from unittest.mock import Mock, patch

from test_almiladoc import vision_server

ROOT = Path(__file__).resolve().parents[2]


def png(width=64, height=64):
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress((b"\0" + b"\xff" * width) * height))
        + chunk(b"IEND", b"")
    )


class CorpusTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("ocr_live", ROOT / "scripts" / "ocr_live.py")
        cls.live = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.live)

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="ocr-quality-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.image = png()
        (self.root / "page Türk.png").write_bytes(self.image)
        (self.root / "page.txt").write_bytes(self.image)
        self.case = {
            "image": "page Türk.png",
            "sha256": hashlib.sha256(self.image).hexdigest(),
            "text": ["Opening paragraph", "İstanbul \u0131 Şş Ğğ Çç Öö Üü -12.5%"],
            "headings": [{"level": 1, "text": "Chapter One"}],
        }
        self.manifest = self.root / "corpus.json"
        self.write([self.case, self.case | {"text": [], "headings": []}])
        self.argv = ["https://example.invalid/v1", "chosen-model", "--corpus", str(self.manifest)]

    def write(self, cases):
        self.manifest.write_text(json.dumps({"version": 1, "cases": cases}), encoding="utf-8")

    def recognize(self, outcomes):
        engine = Mock()
        engine.recognize.side_effect = [[outcome] for outcome in outcomes]
        with patch.object(self.live.almiladoc, "OcrHttp", return_value=engine) as constructor:
            statuses = self.live.recognize_corpus(self.live.arguments(self.argv))
        return statuses, engine, constructor

    def test_literal_order_headings_normalization_and_blank_control(self):
        reply = "# Chapter One\nOpening\tparagraph\nI\u0307stanbul \u0131 Şş Ğğ Çç Öö Üü -12.5%"
        statuses, engine, constructor = self.recognize([reply, " \n"])
        self.assertEqual(statuses, ["match", "match"])
        constructor.assert_called_once_with(
            "https://example.invalid/v1",
            "chosen-model",
            api_key=None,
            concurrency=1,
            retries=0,
            temperature=0.0,
            max_tokens=7000,
        )
        self.assertEqual(engine.recognize.call_count, 2)
        first = engine.recognize.call_args_list[0].args[0][0]
        self.assertEqual(
            (first.image, first.media_type, first.max_text_bytes), (self.image, "image/png", 65536)
        )
        self.assertEqual(first.gap, (None, None, 1, None, None, []))
        self.assertEqual(
            self.live.judge("Chapter One\n===\nbody", ["body"], [(1, "Chapter One")]), "match"
        )
        for reply in ["Chapter\nOne\n===\nbody", "   # Chapter One  ###\nbody"]:
            self.assertEqual(self.live.judge(reply, ["body"], [(1, "Chapter One")]), "match")

    def test_each_quality_failure_has_its_own_reason(self):
        for reply, text, headings, expected in [
            ("second first", ["first", "second"], [], "text-mismatch"),
            ("12.5%", ["-12.5%"], [], "text-mismatch"),
            ("Istanbul", ["İstanbul"], [], "text-mismatch"),
            ("Chapter One", ["Chapter One"], [(1, "Chapter One")], "heading-mismatch"),
            ("## Chapter One", ["Chapter One"], [(1, "Chapter One")], "heading-mismatch"),
            ("```md\n# Chapter One\n```", [], [(1, "Chapter One")], "heading-mismatch"),
            ("    # Chapter One", [], [(1, "Chapter One")], "heading-mismatch"),
            (" \t# Chapter One", [], [(1, "Chapter One")], "heading-mismatch"),
            ("\u00a0# Chapter One", [], [(1, "Chapter One")], "heading-mismatch"),
            ("body\u2028# Chapter One", [], [(1, "Chapter One")], "heading-mismatch"),
            ("Chapter One\u2028===", [], [(1, "Chapter One")], "heading-mismatch"),
            ("```\n    ```\n# Chapter One", [], [(1, "Chapter One")], "heading-mismatch"),
            ("# Chapter One\n===", [], [(1, "# Chapter One")], "heading-mismatch"),
            ("<!--\n# Chapter One\n-->", [], [(1, "Chapter One")], "heading-mismatch"),
            ("No text.", [], [], "blank-mismatch"),
            ("x" * 65537, ["x"], [], "oversized-reply"),
        ]:
            with self.subTest(expected=expected, text=text):
                self.assertEqual(self.live.judge(reply, text, headings), expected)

    def test_invalid_manifests_never_construct_an_engine(self):
        bad_cases = [
            self.case | {"sha256": "0" * 64},
            self.case | {"image": "missing.png"},
            self.case | {"image": "../outside.png"},
            self.case | {"image": str(self.root / "page Türk.png")},
            self.case | {"image": "page.txt"},
            self.case | {"unknown": 1},
            self.case | {"text": [" "]},
            self.case | {"text": "not a list"},
            self.case | {"headings": [{"level": True, "text": "Chapter One"}]},
            self.case | {"headings": [{"level": 7, "text": "Chapter One"}]},
        ]
        for case in bad_cases:
            with self.subTest(keys=list(case)):
                self.write([case])
                with patch.object(self.live.almiladoc, "OcrHttp") as constructor:
                    self.assertEqual(
                        self.live.recognize_corpus(self.live.arguments(self.argv)),
                        ["invalid-corpus"],
                    )
                    constructor.assert_not_called()
        for raw in [
            "{}",
            json.dumps({"version": True, "cases": [self.case]}),
            '{"version":1,"version":1,"cases":' + json.dumps([self.case]) + "}",
            json.dumps({"version": 1, "cases": [self.case] * 9}),
            json.dumps({"version": 1, "cases": [self.case | {"text": [], "headings": []}]}),
            json.dumps({"version": 1, "cases": [self.case]}) + " " * 65537,
        ]:
            self.manifest.write_text(raw, encoding="utf-8")
            with self.assertRaises(ValueError):
                self.live.corpus(self.manifest)

    def test_image_limits_hashes_and_symlink_containment_are_checked(self):
        for image in [b"not a png", png(1, 1), png(1025, 1025), self.image + b"x" * (4 << 20)]:
            (self.root / "page Türk.png").write_bytes(image)
            self.write([self.case | {"sha256": hashlib.sha256(image).hexdigest()}])
            with self.assertRaises(ValueError):
                self.live.corpus(self.manifest)
        image = self.image + b"x" * ((4 << 20) - len(self.image))
        (self.root / "page Türk.png").write_bytes(image)
        self.write([self.case | {"sha256": hashlib.sha256(image).hexdigest()}] * 5)
        with self.assertRaises(ValueError):
            self.live.corpus(self.manifest)
        outside = self.root.parent / (self.root.name + "-outside.png")
        outside.write_bytes(self.image)
        self.addCleanup(outside.unlink)
        link = self.root / "linked.png"
        try:
            link.symlink_to(outside)
        except OSError:
            return
        self.write([self.case | {"image": "linked.png"}])
        with self.assertRaises(ValueError):
            self.live.corpus(self.manifest)

    def test_failure_and_wrong_outcome_count_cannot_pass(self):
        for attempted, status in [(True, 400), (False, 503), (False, None)]:
            failure = self.live.almiladoc.OcrFailure(attempted=attempted, status=status)
            statuses, engine, _ = self.recognize([failure])
            self.assertEqual(
                statuses,
                [f"{'failed' if attempted else 'unattempted'}-{status or 'none'}"],
            )
            self.assertEqual(engine.recognize.call_count, 1)
        with patch.object(self.live.almiladoc, "OcrHttp") as constructor:
            constructor.return_value.recognize.return_value = []
            self.assertEqual(self.live.recognize_corpus(self.live.arguments(self.argv)), ["error"])

    def test_worker_never_prints_exception_text(self):
        for error, status in [
            (ValueError("private reply/key/path"), "error"),
            (self.live.almiladoc.ConfigError("private key"), "invalid-config"),
        ]:
            out = io.StringIO()
            with (
                patch.object(self.live, "recognize_corpus", side_effect=error),
                contextlib.redirect_stdout(out),
            ):
                self.assertEqual(self.live.corpus_worker(self.argv), 0)
            self.assertEqual(out.getvalue(), json.dumps([status]) + "\n")

    def test_protocol_statuses_and_cli_failures_are_redacted(self):
        for output, code, status in [
            (b'["match","match"]', 0, "match"),
            (b"private reply/key/path", 0, "error"),
            (b'["private reply/key/path"]', 0, "error"),
            (b"[]", 0, "error"),
            (b'["match"]', 1, "error"),
            (b'["match"]' + b" " * 1024, 0, "error"),
            (json.dumps(["match"] * 9).encode(), 0, "error"),
        ]:
            out, err = io.StringIO(), io.StringIO()
            with (
                patch.object(
                    self.live.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], code, output),
                ),
                contextlib.redirect_stdout(out),
                contextlib.redirect_stderr(err),
            ):
                self.assertEqual(self.live.main(self.argv), 0 if status == "match" else 1)
            self.assertEqual(out.getvalue(), "")
            self.assertEqual(
                err.getvalue(),
                f"POST chat/completions status={status}\n" * (2 if status == "match" else 1),
            )
        for option in [
            "--timeout=nan",
            "--timeout=901",
            "--concurrency=2",
            "--max-tokens=0",
            "--unknown=private-key",
        ]:
            with contextlib.redirect_stderr(io.StringIO()) as err:
                self.assertEqual(self.live.main([*self.argv, option]), 1)
            self.assertEqual(err.getvalue(), "POST chat/completions status=error\n")

    def test_timeout_kills_and_reaps_its_worker_without_partial_output(self):
        processes, real_popen = [], subprocess.Popen

        def start(*args, **kwargs):
            process = real_popen(*args, **kwargs)
            processes.append(process)
            return process

        worker = "import time; print('private reply/key/path',flush=True); time.sleep(60)"
        with (
            patch.object(self.live, "WORKER", worker),
            patch.object(self.live.subprocess, "Popen", side_effect=start),
            contextlib.redirect_stderr(io.StringIO()) as err,
        ):
            self.assertEqual(self.live.main([*self.argv, "--timeout=0.1"]), 1)
        self.assertEqual(err.getvalue(), "POST chat/completions status=timeout\n")
        self.assertEqual(len(processes), 1)
        self.assertIsNotNone(processes[0].poll())
        self.assertNotEqual(processes[0].returncode, 0)

    def test_owned_worker_uses_the_installed_binding_against_only_a_fake_server(self):
        self.write([self.case])
        reply = "# Chapter One\nOpening paragraph\nİstanbul \u0131 Şş Ğğ Çç Öö Üü -12.5%"
        for code, content, status in [
            (200, reply, "match"),
            (200, "private raw reply", "text-mismatch"),
            (400, "", "failed-400"),
        ]:
            with (
                vision_server(status=code, content=content) as server,
                contextlib.redirect_stdout(io.StringIO()) as out,
                contextlib.redirect_stderr(io.StringIO()) as err,
            ):
                argv = [server.base_url, *self.argv[1:]]
                self.assertEqual(self.live.main(argv), 0 if status == "match" else 1)
            self.assertEqual(out.getvalue(), "")
            self.assertEqual(err.getvalue(), f"POST chat/completions status={status}\n")
            self.assertEqual(len(server.seen), 1)
            path, authorization, body = server.seen[0]
            self.assertEqual(
                (path, authorization, body["model"], body["max_tokens"]),
                ("/v1/chat/completions", None, "chosen-model", 7000),
            )

    def test_existing_smoke_still_settles_pages_two_and_five_and_renders(self):
        with (
            patch.object(self.live.almiladoc, "OcrHttp") as constructor,
            contextlib.redirect_stdout(io.StringIO()) as out,
            contextlib.redirect_stderr(io.StringIO()),
        ):
            constructor.return_value.recognize.return_value = ["first fill", "second fill"]
            self.assertEqual(self.live.main(["http://localhost:1/v1", "chosen-model"]), 0)
        requests = constructor.return_value.recognize.call_args.args[0]
        self.assertEqual([request.gap[3] for request in requests], [2, 5])
        self.assertEqual(out.getvalue().count("Text on the first page"), 3)
        self.assertIn("first fill", out.getvalue())
        self.assertIn("second fill", out.getvalue())
        self.assertNotIn("ocr-needed", out.getvalue())
