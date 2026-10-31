"""Reproduce excluded-crate lockfiles from Cargo.lock and refuse drift."""

import argparse
import difflib
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

PACKAGE = "\n[[package]]\n"
LOCKS = ("fuzz/Cargo.lock", "ocr-local/Cargo.lock")


def entries(text: str) -> tuple[str, list[str]]:
    header, *blocks = text.split(PACKAGE)
    return header, blocks


def package_id(block: str) -> tuple[str, str, str | None]:
    package = tomllib.loads(block)
    return package["name"], package["version"], package.get("source")


def seed_key(block: str) -> tuple[str, tuple[int, ...], str | None]:
    name, version, source = package_id(block)
    major, minor, patch = map(int, version.split("+", 1)[0].split("-", 1)[0].split("."))
    if major:
        return name, (major,), source
    if minor:
        return name, (0, minor), source
    return name, (0, 0, patch), source


def seed(root: str, isolated: str) -> str:
    shared = {seed_key(block) for block in entries(root)[1]}
    own = [block for block in entries(isolated)[1] if seed_key(block) not in shared]
    return root.rstrip("\n") + "".join(PACKAGE + block.rstrip("\n") + "\n" for block in own)


class Scratch:
    def __init__(self, cargo: str, tree: Path, directory: Path) -> None:
        self.cargo, self.root = cargo, directory
        listed = subprocess.run(
            [
                "git",
                "-C",
                str(tree),
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
            capture_output=True,
            check=True,
        ).stdout
        for entry in sorted({item.decode("utf-8") for item in listed.split(b"\0") if item}):
            source = tree / entry
            if source.is_file() or source.is_symlink():
                (directory / entry).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, directory / entry, follow_symlinks=False)

    def reproduce(self, relative: str, tracked: str) -> str:
        lock = self.root / relative
        lock.write_text(
            seed((self.root / "Cargo.lock").read_text(encoding="utf-8"), tracked),
            encoding="utf-8",
            newline="\n",
        )
        subprocess.run(
            [
                self.cargo,
                "metadata",
                "--format-version",
                "1",
                "--manifest-path",
                str(lock.with_name("Cargo.toml")),
            ],
            cwd=self.root,
            stdout=subprocess.DEVNULL,
            check=True,
        )
        return lock.read_text(encoding="utf-8")


def drift(scratch: Scratch, relative: str, tracked: str) -> list[str]:
    return list(
        difflib.unified_diff(
            tracked.splitlines(),
            scratch.reproduce(relative, tracked).splitlines(),
            f"{relative} (tracked)",
            f"{relative} (reproduced)",
            lineterm="",
        )
    )


def self_test(scratch: Scratch, relative: str, tracked: str) -> int:
    header, blocks = entries(tracked)
    shared = {
        package_id(block)
        for block in entries((scratch.root / "Cargo.lock").read_text(encoding="utf-8"))[1]
    }
    at = next(
        index
        for index, block in enumerate(blocks)
        if package_id(block) in shared and "checksum" in block
    )
    target = blocks[at]
    version = tomllib.loads(target)["version"]

    def planted(block: str | None, extra: str = "") -> str:
        changed = [*blocks[:at], *([block] if block is not None else []), *blocks[at + 1 :]]
        return PACKAGE.join([header, *changed]) + extra

    cases = {
        "another version of a shared package": planted(
            target.replace(f'version = "{version}"', 'version = "0.0.1"', 1)
        ),
        "another checksum of a shared package": planted(
            target.replace('checksum = "', 'checksum = "0', 1)
        ),
        "a shared package missing": planted(None),
        "a stale package": planted(
            target,
            f'{PACKAGE}name = "almiladoc-stale"\nversion = "1.0.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n',
        ),
    }
    failures = (
        []
        if not drift(scratch, relative, tracked)
        else [f"{relative} does not reproduce, so the planted cases prove nothing"]
    )
    for pinned, isolated, retained in [
        ("1.0.0", "2.0.0", True),
        ("1.0.0", "1.9.9", False),
        ("0.2.0", "0.2.9", False),
        ("0.2.0", "0.3.0", True),
        ("0.0.0", "0.0.1", True),
        ("0.0.1", "0.0.2", True),
        ("1.0.0+root", "1.0.0+drift", False),
        ("2.0.0-rc.13", "2.0.0-rc.15", False),
    ]:
        sample = f'version = 4\n{PACKAGE}name = "shared"\nversion = "{pinned}"\n'
        excluded = sample.replace(f'version = "{pinned}"', f'version = "{isolated}"')
        versions = {
            tomllib.loads(block)["version"] for block in entries(seed(sample, excluded))[1]
        }
        expected = {pinned, isolated} if retained else {pinned}
        if versions != expected:
            failures.append(f"seeding {pinned} with {isolated}: {versions}, expected {expected}")
    other_source = sample + 'source = "registry+https://example.com/index"\n'
    if len(entries(seed(sample, other_source))[1]) != 2:
        failures.append("seeding dropped another package source")
    failures += [
        f"{relative}: {label} was not reported"
        for label, text in cases.items()
        if not drift(scratch, relative, text)
    ]
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    tree = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            check=True,
            text=True,
            encoding="utf-8",
        ).stdout.strip()
    )
    tracked = {relative: (tree / relative).read_text(encoding="utf-8") for relative in LOCKS}
    with tempfile.TemporaryDirectory() as directory:
        scratch = Scratch(args.cargo, tree, Path(directory))
        if args.self_test:
            results = [self_test(scratch, relative, text) for relative, text in tracked.items()]
            return int(any(results))
        problems = [
            problem
            for relative, text in tracked.items()
            for problem in drift(scratch, relative, text)
        ]
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(
            "excluded-crate locks do not reproduce from Cargo.lock; "
            "seed each from Cargo.lock and run cargo metadata with its manifest",
            file=sys.stderr,
        )
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
