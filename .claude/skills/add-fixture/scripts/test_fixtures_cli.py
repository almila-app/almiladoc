import contextlib
import importlib.util
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from test_fixtures import git_env, seed_repository

GENERATOR = Path(__file__).resolve().parents[4] / "tests/gen_fixtures.py"
WRITES_OUTPUT = (
    "import sys\n"
    "from pathlib import Path\n"
    "args = sys.argv[1:]\n"
    "outdir = Path(args[args.index('--outdir') + 1])\n"
    "extension = args[args.index('--convert-to') + 1].split(':')[0]\n"
    "(outdir / (Path(args[-1]).stem + '.' + extension)).write_bytes(b'converted')\n"
)
OFFICE_OUTPUTS = {
    "tests/fixtures/odt/text.odt",
    "tests/fixtures/docx/text.docx",
    "tests/fixtures/doc/text.doc",
    "tests/fixtures/rtf/text.rtf",
    "tests/fixtures/pdf/text.pdf",
    "tests/fixtures/ods/sheet.ods",
    "tests/fixtures/xlsx/sheet.xlsx",
    "tests/fixtures/xls/sheet.xls",
    "tests/fixtures/csv/sheet.csv",
    "tests/fixtures/odp/pres.odp",
    "tests/fixtures/pptx/pres.pptx",
    "tests/fixtures/ppt/pres.ppt",
    "tests/fixtures/epub/book.epub",
}


def load_module(path, *search):
    spec = importlib.util.spec_from_file_location(path.stem + "_probe", path)
    module = importlib.util.module_from_spec(spec)
    with patch.object(sys, "path", [*map(str, search), *sys.path]):
        spec.loader.exec_module(module)
    return module


class FixtureCli(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="fixture cli ç ")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.script = self.root / ".claude/skills/add-fixture/scripts/fixtures.py"
        self.script.parent.mkdir(parents=True)
        shutil.copyfile(Path(__file__).with_name("fixtures.py"), self.script)
        self.fixture = "tests/fixtures/docx/handmade-example.docx"
        path = self.root / self.fixture
        path.parent.mkdir(parents=True)
        path.write_bytes(b"fixture")
        self.snapshots = self.root / "tests/snapshots"
        self.snapshots.mkdir()
        self.main = self.snapshots / "snapshots__docx__handmade-example.docx.snap"
        self.diag = self.snapshots / "snapshots__docx__handmade-example.docx.diagnostics.snap"
        self.main.write_text("old main\n")
        self.diag.write_text("old diagnostics\n")
        (self.root / "tests/gen_fixtures.py").write_text("pass\n")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        cargo = self.bin / "cargo"
        cargo.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\nfrom pathlib import Path\n"
            "root = Path(__file__).resolve().parents[1]\n"
            "snapshots = root / 'tests/snapshots'\n"
            "assert 'CI' not in os.environ\n"
            "if os.environ['INSTA_UPDATE'] == 'new':\n"
            "    assert os.environ['INSTA_FORCE_PASS'] == '1'\n"
            "    assert '--exact' in sys.argv\n"
            "    assert sys.argv[-2:] == ['snapshots::corpus', 'snapshots::corpus_pdf']\n"
            "    for name in ['snapshots__docx__handmade-example.docx.snap',\n"
            "                 'snapshots__docx__handmade-example.docx.diagnostics.snap']:\n"
            "        (snapshots / (name + '.new'))"
            ".write_text('---\\nassertion_line: 4\\n---\\nnew\\n')\n"
            "else:\n"
            "    assert os.environ['INSTA_UPDATE'] == 'no'\n"
            "    assert 'INSTA_FORCE_PASS' not in os.environ\n"
            "    assert not list(snapshots.glob('*.snap.new'))\n"
            "(root / 'cargo-calls').open('a').write(json.dumps(sys.argv[1:]) + '\\n')\n"
        )
        cargo.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "GIT_CONFIG_GLOBAL": str(self.root / "global-git-config"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "INSTA_UPDATE": "always",
            "INSTA_FORCE_PASS": "hostile",
            "CI": "true",
        }
        seed_repository(self.git, "Fixture Probe", "Seed fixtures")
        self.nested = self.root / "nested directory ç"
        self.nested.mkdir()

    def git(self, *args):
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            env=git_env(self.env),
            capture_output=True,
            text=True,
            check=True,
        )

    def run_cli(self, *args, cwd=None, env=None):
        return subprocess.run(
            [sys.executable, "-I", "-B", str(self.script), *args],
            cwd=cwd or self.nested,
            env=env or self.env,
            capture_output=True,
            text=True,
        )

    def test_relative_keep_is_repository_relative_from_nested_directory(self):
        result = self.run_cli("regen", "--keep", self.fixture)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.main.read_text(), "---\n---\nnew\n")
        self.assertEqual(self.diag.read_text(), "---\n---\nnew\n")
        self.assertEqual(len((self.root / "cargo-calls").read_text().splitlines()), 2)

    def test_absolute_keep_works_from_outside_the_checkout(self):
        result = self.run_cli(
            "regen", "--keep", str(self.root / self.fixture), cwd=self.root.parent
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_escape_keep_fails_before_generation(self):
        result = self.run_cli("regen", "--keep", "../outside.docx")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("Traceback", result.stderr)
        self.assertFalse((self.root / "cargo-calls").exists())
        self.assertEqual(self.main.read_text(), "old main\n")

    def test_unresolvable_head_fails_before_generation(self):
        self.git("symbolic-ref", "HEAD", "refs/heads/unborn-probe")
        result = self.run_cli("regen", "--keep", self.fixture)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("committed HEAD", result.stdout)
        self.assertNotIn("Traceback", result.stderr)
        self.assertEqual(self.main.read_text(), "old main\n")

    def test_missing_git_is_not_run(self):
        result = self.run_cli("regen", env=self.env | {"PATH": str(self.bin)})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("NOT RUN", result.stdout)
        self.assertNotIn("Traceback", result.stderr)

    def test_caller_git_directory_cannot_select_another_repository(self):
        result = self.run_cli(
            "regen",
            "--keep",
            self.fixture,
            env=self.env | {"GIT_DIR": str(self.root / "absent.git")},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_child_home_and_xdg_global_configuration_do_not_affect_regeneration(self):
        for origin in ("HOME", "XDG"):
            with self.subTest(origin=origin):
                child_home = self.root / ("child home " + origin)
                child_xdg = self.root / ("child xdg " + origin)
                child_home.mkdir()
                (child_xdg / "git").mkdir(parents=True)
                ignore = child_home / "ignore"
                ignore.write_text("*.docx\n")
                config = (
                    child_home / ".gitconfig" if origin == "HOME" else child_xdg / "git/config"
                )
                config.write_text(f'[core]\n abbrev = 1\n excludesFile = "{ignore}"\n')
                child_env = self.env | {"HOME": str(child_home), "XDG_CONFIG_HOME": str(child_xdg)}
                config_env = {
                    key: value for key, value in child_env.items() if not key.startswith("GIT_")
                }
                config_env["GIT_CONFIG_NOSYSTEM"] = "1"
                visible = subprocess.run(
                    ["git", "config", "--global", "--get", "core.abbrev"],
                    cwd=self.root,
                    env=config_env,
                    capture_output=True,
                    text=True,
                    check=True,
                )
                self.assertEqual(visible.stdout.strip(), "1")
                result = self.run_cli("regen", "--keep", self.fixture, env=child_env)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.main.read_text(), "---\n---\nnew\n")


class OfficeGenerator(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="office generator ç ")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.out = self.root / "out"
        self.source = self.root / "text.fodt"
        self.source.write_text("source")
        self.generator = load_module(GENERATOR, GENERATOR.parent)

    def stub(self, directory, name, body=""):
        path = directory / name
        path.write_text(f"#!{sys.executable}\n{body}")
        path.chmod(0o755)
        return path

    def convert(self, filter_spec, final_name):
        with contextlib.redirect_stdout(io.StringIO()):
            self.generator.convert_lo(self.source, filter_spec, self.out, final_name)

    def test_launch_that_writes_nothing_is_an_error_not_a_stale_success(self):
        self.out.mkdir()
        (self.out / "text.odt").write_bytes(b"stale")
        silent = self.stub(self.bin, "silent")
        with patch.dict(os.environ, {"SOFFICE": str(silent)}), self.assertRaises(RuntimeError):
            self.convert("odt", "text.odt")

    def test_the_executable_named_by_the_environment_is_the_one_run(self):
        self.out.mkdir()
        (self.out / "text.doc").write_bytes(b"stale")
        writer = self.stub(self.bin, "writer", WRITES_OUTPUT)
        for filter_spec, final_name in (("odt", "text.odt"), ("doc:MS Word 97", "text.doc")):
            with (
                self.subTest(filter_spec=filter_spec),
                patch.dict(os.environ, {"SOFFICE": str(writer)}),
            ):
                self.convert(filter_spec, final_name)
                self.assertEqual((self.out / final_name).read_bytes(), b"converted")

    def test_lookup_takes_the_environment_then_the_path_then_a_known_location(self):
        named = self.stub(self.bin, "named")
        on_path = self.stub(self.bin, "soffice")
        known = self.stub(self.root, "known")
        fallbacks = (str(self.root / "absent"), str(known))
        cases = (
            ({"SOFFICE": str(named), "PATH": str(self.bin)}, str(named)),
            ({"SOFFICE": "", "PATH": str(self.bin)}, str(on_path)),
            ({"PATH": str(self.bin)}, str(on_path)),
            ({"PATH": ""}, str(known)),
        )
        for environment, expected in cases:
            with (
                self.subTest(environment=environment),
                patch.dict(os.environ, environment, clear=True),
                patch.object(self.generator, "SOFFICE_FALLBACKS", fallbacks),
            ):
                self.assertEqual(self.generator.find_soffice(), expected)

    def test_no_executable_found_names_the_variable(self):
        with (
            patch.dict(os.environ, {"PATH": ""}, clear=True),
            patch.object(self.generator, "SOFFICE_FALLBACKS", (str(self.root / "absent"),)),
            self.assertRaisesRegex(FileNotFoundError, "SOFFICE"),
        ):
            self.generator.find_soffice()

    def test_skipping_office_never_looks_for_the_executable(self):
        with (
            patch.object(sys, "argv", ["gen_fixtures.py", "--skip-office"]),
            patch.object(self.generator, "OUT", self.out),
            patch.object(self.generator, "FORMATS", ()),
            patch.object(self.generator, "find_soffice", side_effect=AssertionError),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.generator.main()
        self.assertTrue((self.out / "docx").is_dir())

    def test_every_office_output_stays_visible_to_the_fixture_helper(self):
        helper = load_module(Path(__file__).with_name("fixtures.py"))
        self.assertEqual(helper.not_generated() - helper.HAND_PLACED, OFFICE_OUTPUTS)


if __name__ == "__main__":
    unittest.main()
