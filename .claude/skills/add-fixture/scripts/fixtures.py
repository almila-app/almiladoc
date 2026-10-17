#!/usr/bin/env python3
"""Add or change test fixtures without the generator's churn.

`tests/gen_fixtures.py` rewrites every fixture it generates; on Linux every zip fixture also
changes bytes (`create_system`) while its content stays the same.

`regen [--keep PATH ...]` regenerates without LibreOffice or pandoc and restores every fixture
whose content did not change. A committed fixture whose content changed must be named by
`--keep`, or the run stops. Snapshots are accepted for new and kept fixtures only; any other
snapshot change stays a `.snap.new` and fails the run.

`check` regenerates into a scratch copy, names every fixture whose content the checkout does not
match, and every fixture no generator writes beyond the LibreOffice and pandoc outputs.
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
CORPUS = ["cargo", "test", "--locked", "--test", "it", "corpus"]
SUITES = ["cargo", "test", "--locked", "--test", "it", "--", "snapshots::", "robustness::"]


def insta_env(**pinned: str) -> dict[str, str]:
    inherited = {k: v for k, v in os.environ.items() if not k.startswith("INSTA_") and k != "CI"}
    return inherited | {"INSTA_UPDATE": "no"} | pinned


def git(*args: str) -> list[str]:
    out = subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, check=True)
    return [entry.decode() for entry in out.stdout.split(b"\0") if entry]


def committed(path: str) -> bytes:
    return subprocess.run(
        ["git", "-C", str(ROOT), "show", f"HEAD:{path}"], capture_output=True, check=True
    ).stdout


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


def snapshot_of(fixture: str) -> str:
    return "snapshots__" + fixture.removeprefix(f"{FIXTURES}/").replace("/", "__") + ".snap"


def accept(pending: Path) -> None:
    lines = pending.read_text(encoding="utf-8").splitlines(keepends=True)
    accepted = pending.with_name(pending.name.removesuffix(".new"))
    accepted.write_text(
        "".join(l for l in lines if not l.startswith("assertion_line:")), encoding="utf-8"
    )
    pending.unlink()


def regen(keep: set[str]) -> int:
    if unknown := sorted(keep - set(git("ls-files", "-z", "--", FIXTURES))):
        print("--keep names no committed fixture:", *unknown, sep="\n  ")
        return 2
    dirty = set(git("diff", "--name-only", "--no-renames", "-z", "HEAD", "--", FIXTURES))
    if stray := sorted(dirty - keep):
        print("uncommitted fixture edits not named by --keep would be lost:", *stray, sep="\n  ")
        return 2
    for stale in SNAPSHOTS.glob("*.snap.new"):
        stale.unlink()
    generated = subprocess.run(GENERATE, cwd=ROOT, capture_output=True, text=True)
    changed = set(git("diff", "--name-only", "--no-renames", "-z", "--", FIXTURES))
    if generated.returncode:
        if restored := sorted(changed - keep):
            git("checkout", "--", *restored)
        print(generated.stderr[-3000:], f"generator failed; restored {len(restored)} fixtures")
        return 1
    churn = sorted(f for f in changed if content(committed(f)) == content((ROOT / f).read_bytes()))
    if churn:
        git("checkout", "--", *churn)
    if unkept := sorted(changed - set(churn) - keep):
        print("the generator changed these committed fixtures; --keep each one you meant to:")
        print(*unkept, sep="\n  ")
        return 2
    new = git("ls-files", "--others", "--exclude-standard", "-z", "--", FIXTURES)
    targets = {f for f in [*new, *keep] if not f.startswith(UNSNAPSHOTTED)}
    expected = {snapshot_of(f) for f in targets}
    recorded = subprocess.run(
        CORPUS, cwd=ROOT, env=insta_env(INSTA_UPDATE="new", INSTA_FORCE_PASS="1"),
        capture_output=True, text=True,
    )
    unexpected = []
    for pending in sorted(SNAPSHOTS.glob("*.snap.new")):
        if pending.name.removesuffix(".new") in expected:
            accept(pending)
        else:
            unexpected.append(pending.name)
    final = subprocess.run(SUITES, cwd=ROOT, env=insta_env(), capture_output=True, text=True)
    print("new fixtures:", *new or ["none"], sep="\n  ")
    print("kept fixtures:", *sorted(keep) or ["none"], sep="\n  ")
    print("snapshots accepted:", *sorted(expected) or ["none"], sep="\n  ")
    if unexpected:
        print("other snapshots changed (left as .snap.new):", *unexpected, sep="\n  ")
    if recorded.returncode or final.returncode:
        print((recorded.stdout + recorded.stderr + final.stdout + final.stderr)[-4000:])
    return 1 if unexpected or recorded.returncode or final.returncode else 0


def not_generated() -> set[str]:
    source = (ROOT / GENERATOR).read_text(encoding="utf-8")
    office = {
        f"{FIXTURES}/{groups[0] or groups[2]}/{groups[1] or groups[3]}"
        for groups in OFFICE_OUTPUT.findall(source)
    }
    return office | HAND_PLACED


def check() -> int:
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
    sub = parser.add_subparsers(dest="command", required=True)
    regen_parser = sub.add_parser("regen")
    regen_parser.add_argument("--keep", action="extend", nargs="+", default=[], metavar="PATH")
    sub.add_parser("check")
    args = parser.parse_args()
    if args.command == "check":
        return check()
    keep = {os.path.relpath(Path(p).resolve(), ROOT) for p in args.keep}
    return regen({Path(p).as_posix() for p in keep})


if __name__ == "__main__":
    sys.exit(main())
