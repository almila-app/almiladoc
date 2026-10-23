#!/usr/bin/env python3
"""Add or change test fixtures without the generator's churn.

`tests/gen_fixtures.py` rewrites every fixture it generates; on Linux every zip fixture also
changes bytes (`create_system`) while its content stays the same.
"""

import argparse
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
FIXTURES = "tests/fixtures"
GENERATOR = "tests/gen_fixtures.py"
SNAPSHOTS = ROOT / "tests/snapshots"
GENERATE = [sys.executable, GENERATOR, "--skip-office"]
UNSNAPSHOTTED = f"{FIXTURES}/abuse/"
OFFICE_OUTPUT = re.compile(r'OUT / "(\w+)",\s*"([^"]+)"\)|"-o", str\(OUT / "(\w+)" / "([^"]+)"\)')
HAND_PLACED = {f"{FIXTURES}/rtf/handmade-cocoa.rtf"}
CORPUS = [
    "cargo",
    "test",
    "--locked",
    "--test",
    "it",
    "--",
    "--exact",
    "snapshots::corpus",
    "snapshots::corpus_pdf",
]
SUITES = ["cargo", "test", "--locked", "--test", "it", "--", "snapshots::", "robustness::"]


def insta_env(**pinned: str) -> dict[str, str]:
    inherited = {k: v for k, v in os.environ.items() if not k.startswith("INSTA_") and k != "CI"}
    return inherited | {"INSTA_UPDATE": "no"} | pinned


def git_bytes(*args: str) -> bytes:
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env |= {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    return subprocess.run(
        ["git", "-C", str(ROOT), *args], capture_output=True, check=True, env=env
    ).stdout


def git(*args: str) -> list[str]:
    return [entry.decode() for entry in git_bytes(*args).split(b"\0") if entry]


def committed(base: str, path: str) -> bytes:
    return git_bytes("show", f"{base}:{path}")


def changed_fixtures(base: str, tracked: set[str]) -> set[str]:
    return {path for path in tracked if (ROOT / path).read_bytes() != committed(base, path)}


def content(data: bytes, compressed: bool = False) -> object:
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            return sorted(
                (i.filename, i.CRC, i.file_size)
                + ((i.compress_type, i.compress_size) if compressed else ())
                for i in archive.infolist()
            )
    except zipfile.BadZipFile:
        return data


def snapshots_of(fixture: str) -> set[str]:
    name = "snapshots__" + fixture.removeprefix(f"{FIXTURES}/").replace("/", "__")
    return {name + ".snap", name + ".diagnostics.snap"}


def accept(pending: Path) -> None:
    lines = pending.read_bytes().splitlines(keepends=True)
    if not lines or lines[0].rstrip(b"\r\n") != b"---":
        raise ValueError(f"missing snapshot header: {pending.name}")
    end = next((i for i in range(1, len(lines)) if lines[i].rstrip(b"\r\n") == b"---"), None)
    if end is None:
        raise ValueError(f"unclosed snapshot header: {pending.name}")
    accepted = pending.with_name(pending.name.removesuffix(".new"))
    header = [line for line in lines[1:end] if not line.startswith(b"assertion_line:")]
    descriptor, name = tempfile.mkstemp(dir=accepted.parent, prefix=accepted.name + ".")
    replacement = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(b"".join([lines[0], *header, *lines[end:]]))
        mode = accepted.stat().st_mode if accepted.exists() else pending.stat().st_mode
        replacement.chmod(mode & 0o777)
        replacement.replace(accepted)
    finally:
        replacement.unlink(missing_ok=True)
    pending.unlink()


def commands_available(commands: list[list[str]]) -> bool:
    for command in commands:
        if shutil.which(command[0]) is None:
            print(f"NOT RUN: required command is unavailable: {command[0]}")
            return False
    return True


def regular_tree(path: Path) -> bool:
    return (
        path.resolve() == path
        and path.is_dir()
        and not any(p.is_symlink() for p in path.rglob("*"))
    )


def roots_regular() -> bool:
    return all(regular_tree(path) for path in (ROOT / FIXTURES, SNAPSHOTS))


def missing(tracked: set[str]) -> list[str]:
    return sorted(path for path in tracked if not (ROOT / path).is_file())


def writable_directories(root: Path) -> None:
    root.chmod(root.stat().st_mode | 0o700)
    for parent, names, _ in os.walk(root, followlinks=False):
        for name in names:
            directory = Path(parent) / name
            if not directory.is_symlink():
                directory.chmod(directory.stat().st_mode | 0o700)


def restore_tests(backup: Path, proposals: dict[Path, bytes]) -> None:
    mutation_root = ROOT / "tests"
    if mutation_root.is_symlink() or not mutation_root.is_dir():
        mutation_root.unlink(missing_ok=True)
    else:
        writable_directories(mutation_root)
        pending = (
            {
                path
                for path in SNAPSHOTS.glob("*.snap.new")
                if path.is_file() and not path.is_symlink()
            }
            if SNAPSHOTS.resolve() == SNAPSHOTS and SNAPSHOTS.is_dir()
            else set()
        )
        for path in sorted(mutation_root.rglob("*"), key=lambda p: len(p.parts), reverse=True):
            if path in pending or (pending and path == SNAPSHOTS):
                continue
            if path.is_dir() and not path.is_symlink():
                path.rmdir()
            else:
                path.unlink()
    shutil.copytree(backup, mutation_root, symlinks=True, dirs_exist_ok=True)
    for path, data in proposals.items():
        if not path.exists():
            path.write_bytes(data)


def regen(keep: set[str]) -> int:
    if not commands_available([["git"], GENERATE, CORPUS, SUITES]):
        return 2
    try:
        if (
            Path(git("rev-parse", "--show-toplevel")[0].removesuffix("\n")).resolve()
            != ROOT.resolve()
        ):
            raise ValueError("fixture helper is outside its repository")
        base = git("rev-parse", "--verify", "HEAD^{commit}")[0].strip()
        entries = git("ls-tree", "-r", "-z", base, "--", FIXTURES)
        tracked = {entry.split("\t", 1)[1] for entry in entries}
        if any(entry.split(" ", 1)[0] not in {"100644", "100755"} for entry in entries):
            print("committed fixtures must be regular files")
            return 2
    except (subprocess.CalledProcessError, IndexError, ValueError):
        print("cannot resolve the fixture repository and its committed HEAD")
        return 2
    if not roots_regular():
        print("fixture and snapshot roots must be directories without symlinks")
        return 2
    if unknown := sorted(keep - tracked):
        print("--keep names no committed fixture:", *unknown, sep="\n  ")
        return 2
    if absent := missing(tracked):
        print(
            "committed fixture deletion, rename or type change must be handled separately:",
            *absent,
            sep="\n  ",
        )
        return 2
    dirty = changed_fixtures(base, tracked)
    if stray := sorted(dirty - keep):
        print("uncommitted fixture edits not named by --keep would be lost:", *stray, sep="\n  ")
        return 2
    if pending := sorted(SNAPSHOTS.glob("*.snap.new")):
        print(
            "review or move existing pending snapshots before regeneration:", *pending, sep="\n  "
        )
        return 2
    scratch = Path(tempfile.mkdtemp())
    backup = scratch / "tests"
    proposals: dict[Path, bytes] = {}
    result = 1
    cleanup = True
    try:
        shutil.copytree(ROOT / "tests", backup, symlinks=True)
        try:
            result = regenerate(keep, base, tracked, proposals)
        finally:
            if result:
                try:
                    restore_tests(backup, proposals)
                except BaseException:
                    print(f"fixture restoration failed; pre-run backup retained at {backup}")
                    cleanup = False
                    raise
    finally:
        if cleanup:
            writable_directories(scratch)
            shutil.rmtree(scratch)
    return result


def regenerate(keep: set[str], base: str, tracked: set[str], proposals: dict[Path, bytes]) -> int:
    generated = subprocess.run(GENERATE, cwd=ROOT, capture_output=True, text=True)
    if generated.returncode:
        print(generated.stderr[-3000:], "generator failed; restoring the pre-run fixture tree")
        return 1
    if not roots_regular():
        print("generation introduced a symlink or invalid mutation root")
        return 2
    if absent := missing(tracked):
        print("generation removed or replaced committed fixtures:", *absent, sep="\n  ")
        return 2
    changed = changed_fixtures(base, tracked)
    churn = {
        path
        for path in changed
        if content(committed(base, path)) == content((ROOT / path).read_bytes())
    }
    for path in churn:
        (ROOT / path).write_bytes(committed(base, path))
    if unkept := sorted(changed - set(churn) - keep):
        print("the generator changed these committed fixtures; --keep each one you meant to:")
        print(*unkept, sep="\n  ")
        return 2
    listed = git(
        "ls-files",
        "--cached",
        "--others",
        "--exclude-per-directory=.gitignore",
        "-z",
        "--",
        FIXTURES,
    )
    new = sorted(set(listed) - tracked)
    targets = {f for f in [*new, *keep] if not f.startswith(UNSNAPSHOTTED)}
    expected = {name for fixture in targets for name in snapshots_of(fixture)}
    recorded = subprocess.run(
        CORPUS,
        cwd=ROOT,
        env=insta_env(INSTA_UPDATE="new", INSTA_FORCE_PASS="1"),
        capture_output=True,
        text=True,
    )
    if not roots_regular():
        print("recording introduced a symlink or invalid mutation root")
        return 2
    pending = sorted(SNAPSHOTS.glob("*.snap.new"))
    proposals.update({path: path.read_bytes() for path in pending})
    unexpected = [path.name for path in pending if path.name.removesuffix(".new") not in expected]
    print("new fixtures:", *new or ["none"], sep="\n  ")
    print("kept fixtures:", *sorted(keep) or ["none"], sep="\n  ")
    if unexpected:
        print("other snapshots changed (left as .snap.new):", *unexpected, sep="\n  ")
    if recorded.returncode:
        print((recorded.stdout + recorded.stderr)[-4000:])
    if unexpected or recorded.returncode:
        return 1
    for path in pending:
        accept(path)
    final = subprocess.run(SUITES, cwd=ROOT, env=insta_env(), capture_output=True, text=True)
    if final.returncode:
        print((final.stdout + final.stderr)[-4000:])
        return 1
    print(
        "snapshots accepted:",
        *[path.name.removesuffix(".new") for path in pending] or ["none"],
        sep="\n  ",
    )
    return 0


def not_generated() -> set[str]:
    source = (ROOT / GENERATOR).read_text(encoding="utf-8")
    office = {
        f"{FIXTURES}/{groups[0] or groups[2]}/{groups[1] or groups[3]}"
        for groups in OFFICE_OUTPUT.findall(source)
    }
    return office | HAND_PLACED


def check() -> int:
    if not commands_available([GENERATE]):
        return 2
    with tempfile.TemporaryDirectory() as scratch_dir:
        scratch = Path(scratch_dir)
        shutil.copytree(ROOT / "tests", scratch / "tests")
        started = time.time()
        subprocess.run(GENERATE, cwd=scratch, check=True, stdout=subprocess.DEVNULL)

        def files(root: Path) -> set[str]:
            tree = (root / FIXTURES).rglob("*")
            return {p.relative_to(root).as_posix() for p in tree if p.is_file()}

        mine, made = files(ROOT), files(scratch)
        unwritten = {f for f in mine if (scratch / f).stat().st_mtime < started}
        differs = sorted(
            f
            for f in mine - unwritten
            if content((ROOT / f).read_bytes(), f.startswith(UNSNAPSHOTTED))
            != content((scratch / f).read_bytes(), f.startswith(UNSNAPSHOTTED))
        )
        orphans = sorted(unwritten - not_generated())
        extra = sorted(made - mine)
    for title, paths in (
        ("content differs from what the generator writes:", differs),
        ("no generator writes these:", orphans),
        ("generated, not in the checkout:", extra),
    ):
        if paths:
            print(title, *paths, sep="\n  ")
    checked = len(mine) - len(unwritten)
    print(f"fixtures: compared {checked} generated; {len(unwritten)} not generated here")
    return 1 if differs or orphans or extra else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    regen_parser = sub.add_parser("regen")
    regen_parser.add_argument("--keep", action="extend", nargs="+", default=[], metavar="PATH")
    sub.add_parser("check")
    args = parser.parse_args()
    if args.self_test:
        return subprocess.run(
            [
                sys.executable,
                "-I",
                "-B",
                "-m",
                "unittest",
                "discover",
                "-s",
                str(Path(__file__).resolve().parent),
                "-p",
                "test_fixtures*.py",
            ]
        ).returncode
    if args.command == "check":
        return check()
    if args.command != "regen":
        parser.error("choose regen, check or --self-test")
    return regen({Path(os.path.relpath((ROOT / p).resolve(), ROOT)).as_posix() for p in args.keep})


if __name__ == "__main__":
    try:
        sys.exit(main())
    except FileNotFoundError as error:
        print(f"NOT RUN: {error}")
        sys.exit(2)
    except (subprocess.CalledProcessError, OSError, ValueError) as error:
        print(f"fixture operation failed: {error}")
        sys.exit(1)
