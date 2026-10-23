import contextlib
import io
import os
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

from test_fixtures import FixtureCase

RELINK = "{0}.rename(outside); {0}.symlink_to(outside, target_is_directory=True)"
OPERATIONS = {
    "delete": "path.unlink()",
    "rename": "path.rename(str(path) + '.renamed')",
    "directory": "path.unlink(); path.mkdir()",
    "file_symlink": "path.unlink(); path.symlink_to(other)",
    "fixture_parent_symlink": RELINK.format("path.parent"),
    "tests_parent_symlink": RELINK.format("tests"),
    "snapshot_parent_symlink": RELINK.format("snapshots"),
}


class SnapshotFailures(FixtureCase):
    def test_git_bootstrap_is_isolated_for_both_harnesses(self):
        from test_fixtures_cli import FixtureCli

        git_dir = self.root / "caller.git"
        work_tree = self.root / "caller-worktree"
        index = self.root / "caller-index"
        index.write_bytes(b"caller index must survive")
        hostile = {
            "GIT_DIR": str(git_dir),
            "GIT_WORK_TREE": str(work_tree),
            "GIT_INDEX_FILE": str(index),
        }
        for harness in (FixtureCase, FixtureCli):
            with self.subTest(harness=harness.__name__), patch.dict(os.environ, hostile):
                case = harness()
                try:
                    case.setUp()
                    result = case.git("rev-parse", "--show-toplevel")
                    self.assertEqual(result.stdout.removesuffix("\n"), str(case.root))
                finally:
                    case.doCleanups()
            self.assertFalse(git_dir.exists())
            self.assertFalse(work_tree.exists())
            self.assertEqual(index.read_bytes(), b"caller index must survive")

    def test_actual_generator_created_fixture_is_discovered(self):
        self.use_fixture("handmade-created ç.docx")
        self.configure()
        self.module.GENERATE = self.command(f"Path({self.fixture!r}).write_bytes(b'new fixture')")
        code, output = self.regen(set())
        self.assertEqual(code, 0, output)
        self.assert_accepted()

    def test_staged_new_fixture_uses_committed_baseline(self):
        self.use_fixture("handmade-created ç.docx")
        self.configure()
        (self.root / self.fixture).write_bytes(b"new fixture")
        self.git("add", self.fixture)
        code, output = self.regen(set())
        self.assertEqual(code, 0, output)
        self.assert_accepted()

    def test_check_missing_tool_reports_not_run(self):
        self.module.GENERATE = [str(self.root / "missing-generator")]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = self.module.check()
        self.assertNotEqual(result, 0)
        self.assertIn("NOT RUN", output.getvalue())

    def run_catching(self, keep=None):
        output = io.StringIO()
        try:
            with contextlib.redirect_stdout(output):
                result = self.module.regen({self.fixture} if keep is None else keep)
            return result, output.getvalue()
        except (OSError, ValueError) as error:
            return 1, output.getvalue() + f"{type(error).__name__}: {error}"

    def only_main(self, *, fail_final=False):
        self.pending = {str(self.main.with_suffix(".snap.new")): "---\nsource: probe\n---\nbody\n"}
        self.module.CORPUS = self.command(self.write_rows())
        self.module.SUITES = self.command("raise SystemExit(3)" if fail_final else "pass")

    def assert_pending(self):
        for name, data in self.pending.items():
            self.assertEqual(Path(name).read_text(), data)

    def restore_after_failure(self, stage):
        absent = self.snapshots / "unrelated.snap"
        failing = self.command(
            f"Path({str(self.main)!r}).write_text('overwritten'); "
            f"Path({str(self.diag)!r}).unlink(); Path({str(absent)!r}).write_text('created'); "
            f"Path({self.fixture!r}).write_bytes(b'changed'); {self.write_rows()}; "
            "raise SystemExit(3)"
        )
        setattr(self.module, stage, failing)
        code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assert_original()
        self.assertFalse(absent.exists())
        self.assertEqual((self.root / self.fixture).read_bytes(), b"fixture")
        self.assert_pending()

    def test_generator_failure_restores_direct_snapshots_and_fixtures(self):
        self.restore_after_failure("GENERATE")

    def test_recording_failure_restores_direct_snapshots_and_proposals(self):
        self.restore_after_failure("CORPUS")

    def test_partial_acceptance_failure_restores_all_and_raw_proposals(self):
        self.pending[str(self.main.with_suffix(".snap.new"))] = "malformed proposal\n"
        self.module.CORPUS = self.command(self.write_rows())
        accepted = []
        actual = self.module.accept

        def accept(path):
            actual(path)
            accepted.append(path.name)

        with patch.object(self.module, "accept", accept):
            code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assertEqual(len(accepted), 1)
        self.assert_original()
        self.assert_pending()

    def test_final_failure_restores_previously_absent_snapshots(self):
        self.use_fixture("created.docx")
        self.configure(fail_final=True)
        self.module.GENERATE = self.command(f"Path({self.fixture!r}).write_bytes(b'new')")
        code, output = self.run_catching(set())
        self.assertNotEqual(code, 0, output)
        self.assertFalse(self.main.exists())
        self.assertFalse(self.diag.exists())
        self.assertFalse((self.root / self.fixture).exists())
        self.assert_pending()

    def test_deliberately_absent_diagnostics_stay_absent_after_failure(self):
        self.diag.unlink()
        self.only_main(fail_final=True)
        code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assertFalse(self.diag.exists())
        self.assertEqual(self.main.read_text(), "old markdown\n")
        self.assert_pending()

    def test_diagnostics_to_none_requires_explicit_owned_deletion(self):
        self.only_main()
        self.module.CORPUS = self.command(
            f"assert not Path({str(self.diag)!r}).exists(), 'old diagnostics remain'; "
            + self.write_rows()
        )
        code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assert_original()
        self.diag.unlink()
        code, output = self.run_catching()
        self.assertEqual(code, 0, output)
        self.assertFalse(self.diag.exists())
        self.assertEqual(self.main.read_text(), "---\nsource: probe\n---\nbody\n")

    def test_unknown_keep_is_refused_before_generation(self):
        code, output = self.run_catching({"tests/fixtures/docx/unknown.docx"})
        self.assertNotEqual(code, 0, output)
        self.assertIn("no committed fixture", output)
        self.assertFalse((self.root / "generated.marker").exists())
        self.assert_original()

    def test_unresolvable_head_is_refused_before_generation(self):
        self.git("update-ref", "-d", "HEAD")
        code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assertIn("committed HEAD", output)
        self.assertFalse((self.root / "generated.marker").exists())
        self.assert_original()

    def test_hostile_git_environment_does_not_redirect_discovery(self):
        self.use_fixture("new ignored ç.docx")
        (self.root / self.fixture).write_bytes(b"new")
        self.git("add", self.fixture)
        (self.root / ".git/info/exclude").write_text("*\n")
        self.configure()
        hostile = {
            "GIT_DIR": str(self.root / "missing-repo"),
            "GIT_WORK_TREE": str(self.root / "missing-tree"),
            "GIT_INDEX_FILE": str(self.root / "missing-index"),
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "core.bare",
            "GIT_CONFIG_VALUE_0": "true",
        }
        with patch.dict(os.environ, hostile):
            code, output = self.run_catching(set())
        self.assertEqual(code, 0, output)
        self.assert_accepted()

    def test_committed_symlink_replaced_with_regular_file_is_not_a_kept_fixture(self):
        fixture = self.root / self.fixture
        fixture.unlink()
        fixture.symlink_to(self.root / self.other)
        self.git("add", self.fixture)
        self.git("commit", "-qm", "Seed a nonregular committed fixture")
        fixture.unlink()
        fixture.write_bytes(b"regular replacement")
        code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assertFalse((self.root / "generated.marker").exists())
        self.assertEqual(fixture.read_bytes(), b"regular replacement")
        self.assert_original()

    def destructive(self, operation, generated):
        path = self.root / self.fixture
        paths = {
            "path": path,
            "other": self.root / self.other,
            "outside": self.root / "outside-fixtures",
            "tests": self.root / "tests",
            "snapshots": self.snapshots,
        }
        assigned = "".join(f"{name} = Path({str(value)!r}); " for name, value in paths.items())
        command = self.command(assigned + OPERATIONS[operation])
        if generated:
            self.module.GENERATE = command
            self.module.CORPUS = self.command("Path('recorded.marker').touch()")
        else:
            subprocess.run(command, check=True)
        result, output = self.regen()
        self.assertEqual(result, 2, output)
        destructive_file = operation in {"delete", "rename", "directory"}
        if generated:
            expected = (
                "generation removed or replaced"
                if destructive_file
                else "generation introduced a symlink"
            )
        else:
            expected = (
                "committed fixture deletion"
                if destructive_file
                else "directories without symlinks"
            )
        self.assertIn(expected, output)
        self.assertFalse((self.root / "recorded.marker").exists())
        if generated:
            self.assert_original()
            self.assertEqual(path.read_bytes(), b"fixture")
            self.assertFalse((self.root / "tests").is_symlink())
        else:
            self.assertFalse((self.root / "generated.marker").exists())

    def test_recording_pending_symlink_is_refused(self):
        outside = self.root / "outside-proposal"
        outside.write_text("---\nsource: outside\n---\noutside\n")
        self.module.CORPUS = self.command(
            f"Path({str(self.main) + '.new'!r}).symlink_to({str(outside)!r})"
        )
        code, output = self.run_catching()
        self.assertNotEqual(code, 0, output)
        self.assert_original()

    def test_recording_accepted_symlink_cannot_write_outside_snapshot_tree(self):
        outside = self.root / "outside-accepted"
        outside.write_text("outside must survive\n")
        self.module.CORPUS = self.command(
            f"Path({str(self.main)!r}).unlink(); "
            f"Path({str(self.main)!r}).symlink_to({str(outside)!r}); " + self.write_rows()
        )
        code, output = self.run_catching()
        self.assertEqual(outside.read_text(), "outside must survive\n", output)
        self.assertNotEqual(code, 0, output)
        self.assert_original()


for phase in ("preexisting", "generated"):
    for operation in OPERATIONS:

        def probe(self, operation=operation, generated=phase == "generated"):
            self.destructive(operation, generated)

        setattr(SnapshotFailures, f"test_{phase}_{operation}_is_refused", probe)


if __name__ == "__main__":
    unittest.main()
