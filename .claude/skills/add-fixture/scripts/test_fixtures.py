import contextlib
import importlib.util
import io
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("fixtures.py")


def git_env(environment):
    return {key: value for key, value in environment.items() if not key.startswith("GIT_")} | {
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_CONFIG_NOSYSTEM": "1",
    }


def seed_repository(git, name, message):
    git("init", "-q")
    git("config", "user.name", name)
    git("config", "user.email", "probe@example.com")
    git("config", "core.hooksPath", "/dev/null")
    git("add", "tests")
    git("commit", "-qm", message)


class FixtureCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.other = "tests/fixtures/docx/handmade-other.docx"
        self.snapshots = self.root / "tests/snapshots"
        self.snapshots.mkdir(parents=True)
        self.use_fixture("handmade-çözüm space.docx")
        self.stray = self.snapshots / "snapshots__docx__handmade-other.docx.diagnostics.snap.new"
        self.main.write_text("old markdown\n")
        self.diag.write_text("old diagnostics\n")
        for name in (self.fixture, self.other):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        self.environment = patch.dict(
            os.environ,
            {
                "GIT_CONFIG_GLOBAL": str(self.root / "no-global-config"),
                "GIT_CONFIG_NOSYSTEM": "1",
                "INSTA_UPDATE": "always",
                "INSTA_FORCE_PASS": "hostile",
                "CI": "true",
            },
        )
        self.environment.start()
        self.addCleanup(self.environment.stop)
        seed_repository(self.git, "Snapshot Probe", "Seed snapshot ownership")
        spec = importlib.util.spec_from_file_location("fixture_probe", SCRIPT)
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.module.ROOT = self.root
        self.module.SNAPSHOTS = self.snapshots
        self.module.GENERATE = self.command("Path('generated.marker').touch()")
        self.configure()

    def command(self, code):
        return [sys.executable, "-c", "from pathlib import Path; " + code]

    def use_fixture(self, name):
        self.fixture = f"tests/fixtures/docx/{name}"
        self.main = self.snapshots / f"snapshots__docx__{name}.snap"
        self.diag = self.snapshots / f"snapshots__docx__{name}.diagnostics.snap"

    def write_rows(self):
        return f"[Path(p).write_text(v) for p, v in {self.pending!r}.items()]"

    def drop_from_baseline(self):
        self.git("rm", "--cached", self.fixture, str(self.main), str(self.diag))
        self.git("commit", "-qm", "Remove future fixture from the baseline")
        self.main.unlink()
        self.diag.unlink()

    def git(self, *args):
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
            env=git_env(os.environ),
        )

    def configure(self, *, stray=False, fail_final=False):
        names = [self.main.with_suffix(".snap.new"), self.diag.with_suffix(".snap.new")]
        if stray:
            names.append(self.stray)
        self.pending = {
            str(path): "---\nsource: probe\nassertion_line: 8\n---\nnew snapshot\n"
            for path in names
        }
        self.module.CORPUS = self.command(
            "import os; assert os.environ['INSTA_UPDATE']=='new'; "
            "assert os.environ['INSTA_FORCE_PASS']=='1'; assert 'CI' not in os.environ; "
            + self.write_rows()
        )
        self.module.SUITES = self.command(
            "import os; assert os.environ['INSTA_UPDATE']=='no'; assert 'CI' not in os.environ; "
            "assert 'INSTA_FORCE_PASS' not in os.environ; "
            + ("raise SystemExit(3)" if fail_final else "pass")
        )

    def regen(self, keep=None):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = self.module.regen({self.fixture} if keep is None else keep)
        return code, output.getvalue()

    def assert_accepted(self):
        for path in (self.main, self.diag):
            self.assertEqual(path.read_text(), "---\nsource: probe\n---\nnew snapshot\n")
            self.assertFalse(path.with_suffix(".snap.new").exists())

    def assert_original(self):
        self.assertEqual(self.main.read_text(), "old markdown\n")
        self.assertEqual(self.diag.read_text(), "old diagnostics\n")


class SnapshotOwnership(FixtureCase):
    def test_kept_fixture_accepts_markdown_and_diagnostics(self):
        code, output = self.regen()
        self.assertEqual(code, 0, output)
        self.assert_accepted()

    def test_new_fixture_accepts_both_snapshots(self):
        self.drop_from_baseline()
        code, output = self.regen(set())
        self.assertEqual(code, 0, output)
        self.assert_accepted()

    def test_unowned_diagnostics_are_not_accepted(self):
        self.configure(stray=True)
        code, output = self.regen()
        self.assertNotEqual(code, 0, output)
        self.assertTrue(self.stray.exists())
        self.assertFalse(self.stray.with_suffix("").exists())
        self.assert_original()

    def test_existing_pending_snapshot_is_preserved_before_generation(self):
        self.stray.write_text("unreviewed snapshot\n")
        code, output = self.regen()
        self.assertNotEqual(code, 0, output)
        self.assertTrue(self.stray.exists(), "the pending snapshot must survive")
        self.assertEqual(self.stray.read_text(), "unreviewed snapshot\n")
        self.assertFalse((self.root / "generated.marker").exists())
        self.assert_original()

    def test_failed_final_checks_restore_snapshots_and_pending_files(self):
        self.configure(fail_final=True)
        code, output = self.regen()
        self.assertNotEqual(code, 0, output)
        self.assert_original()
        for name, contents in self.pending.items():
            self.assertEqual(Path(name).read_text(), contents)

    def test_unkept_fixture_edit_is_refused_without_mutation(self):
        (self.root / self.other).write_bytes(b"owner change")
        code, output = self.regen()
        self.assertEqual(code, 2, output)
        self.assertFalse((self.root / "generated.marker").exists())
        self.assertEqual((self.root / self.other).read_bytes(), b"owner change")
        self.assert_original()

    def test_unkept_generated_fixture_edit_is_restored(self):
        self.module.GENERATE = self.command(
            f"Path({self.other!r}).write_bytes(b'unowned generated change')"
        )
        code, output = self.regen()
        self.assertNotEqual(code, 0, output)
        self.assertEqual((self.root / self.other).read_bytes(), b"fixture")
        self.assert_original()

    def test_container_metadata_churn_restores_committed_bytes(self):
        def archive(system):
            output = io.BytesIO()
            with zipfile.ZipFile(output, "w") as container:
                info = zipfile.ZipInfo("document.xml")
                info.create_system = system
                container.writestr(info, b"same document")
            return output.getvalue()

        original, churn = archive(0), archive(3)
        self.assertNotEqual(original, churn)
        (self.root / self.fixture).write_bytes(original)
        self.git("add", self.fixture)
        self.git("commit", "-qm", "Seed container metadata")
        self.module.GENERATE = self.command(f"Path({self.fixture!r}).write_bytes({churn!r})")
        self.module.CORPUS = [sys.executable, "-c", "pass"]
        code, output = self.regen(set())
        self.assertEqual(code, 0, output)
        self.assertEqual((self.root / self.fixture).read_bytes(), original)

    def test_generator_fixture_edits_are_restored_after_final_failure(self):
        self.configure(fail_final=True)
        self.module.GENERATE = self.command(
            f"Path({self.fixture!r}).write_bytes(b'generated fixture')"
        )
        (self.root / self.fixture).write_bytes(b"owned preexisting fixture edit")
        code, output = self.regen()
        self.assertNotEqual(code, 0, output)
        self.assertEqual(
            (self.root / self.fixture).read_bytes(), b"owned preexisting fixture edit"
        )
        self.assert_original()

    def test_snapshot_payload_line_is_not_metadata(self):
        pending = self.main.with_suffix(".snap.new")
        pending.write_text("---\nassertion_line: 8\n---\nassertion_line: source text\n")
        self.module.accept(pending)
        self.assertEqual(self.main.read_text(), "---\n---\nassertion_line: source text\n")

    def test_snapshot_without_a_first_header_line_is_refused_and_kept(self):
        pending = self.main.with_suffix(".snap.new")
        pending.write_text("x\n---\nbody\n")
        with self.assertRaisesRegex(ValueError, "missing"):
            self.module.accept(pending)
        self.assertEqual(pending.read_text(), "x\n---\nbody\n")
        self.assertEqual(self.main.read_text(), "old markdown\n")

    def test_snapshot_without_a_closing_header_line_is_refused_and_kept(self):
        pending = self.main.with_suffix(".snap.new")
        pending.write_text("---\nassertion_line: 8\nbody\n")
        with self.assertRaisesRegex(ValueError, "unclosed"):
            self.module.accept(pending)
        self.assertEqual(pending.read_text(), "---\nassertion_line: 8\nbody\n")
        self.assertEqual(self.main.read_text(), "old markdown\n")

    def test_snapshot_payload_bytes_keep_their_line_endings(self):
        pending = self.main.with_suffix(".snap.new")
        pending.write_bytes(
            b"---\r\nassertion_line: 8\r\n---\r\nbody\r\nassertion_line: payload\n"
        )
        self.module.accept(pending)
        self.assertEqual(
            self.main.read_bytes(), b"---\r\n---\r\nbody\r\nassertion_line: payload\n"
        )

    def test_unchanged_snapshots_are_not_reported_as_accepted(self):
        self.module.CORPUS = [sys.executable, "-c", "pass"]
        code, output = self.regen()
        self.assertEqual(code, 0, output)
        self.assertIn("snapshots accepted:\n  none", output)
        self.assertNotIn(self.main.name, output)
        self.assertNotIn(self.diag.name, output)

    def test_missing_command_is_reported_without_mutation(self):
        self.module.GENERATE = [str(self.root / "missing-generator")]
        code, output = self.regen()
        self.assertNotEqual(code, 0, output)
        self.assertIn("NOT RUN", output)
        self.assert_original()

    def test_global_and_local_ignores_do_not_hide_new_fixtures(self):
        ignore = self.root / "global-ignore"
        ignore.write_text("*\n")
        (self.root / "no-global-config").write_text(f"[core]\nexcludesFile = {ignore}\n")
        (self.root / ".git/info/exclude").write_text("*\n")
        self.drop_from_baseline()
        code, output = self.regen(set())
        self.assertEqual(code, 0, output)
        self.assert_accepted()


if __name__ == "__main__":
    unittest.main()
