"""Reproduce fuzz/Cargo.lock from the root Cargo.lock and fail when the tracked one differs."""

import argparse
import difflib
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

PACKAGE = "\n[[package]]\n"
FUZZ_LOCK = "fuzz/Cargo.lock"


def entries(text: str) -> tuple[str, list[str]]:
    header, *blocks = text.split(PACKAGE)
    return header, blocks


def name(block: str) -> str:
    return tomllib.loads(block)["name"]


def seed(root: str, fuzz: str) -> str:
    shared = {name(block) for block in entries(root)[1]}
    own = [block for block in entries(fuzz)[1] if name(block) not in shared]
    return root.rstrip("\n") + "".join(PACKAGE + block.rstrip("\n") + "\n" for block in own)


class Scratch:
    def __init__(self, cargo: str, tree: Path, directory: Path) -> None:
        self.cargo, self.root = cargo, directory
        listed = subprocess.run(["git", "-C", str(tree), "ls-files", "-z", "--cached", "--others", "--exclude-standard"], capture_output=True, check=True).stdout
        for entry in sorted({item.decode("utf-8") for item in listed.split(b"\0") if item}):
            source = tree / entry
            if source.is_file() or source.is_symlink():
                (directory / entry).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, directory / entry, follow_symlinks=False)

    def reproduce(self, fuzz: str) -> str:
        lock = self.root / FUZZ_LOCK
        lock.write_text(seed((self.root / "Cargo.lock").read_text(encoding="utf-8"), fuzz), encoding="utf-8", newline="\n")
        subprocess.run([self.cargo, "metadata", "--format-version", "1", "--manifest-path", str(self.root / "fuzz" / "Cargo.toml")], cwd=self.root, stdout=subprocess.DEVNULL, check=True)
        return lock.read_text(encoding="utf-8")


def drift(scratch: Scratch, fuzz: str) -> list[str]:
    return list(difflib.unified_diff(fuzz.splitlines(), scratch.reproduce(fuzz).splitlines(), f"{FUZZ_LOCK} (tracked)", f"{FUZZ_LOCK} (reproduced)", lineterm=""))


def self_test(scratch: Scratch, fuzz: str) -> int:
    header, blocks = entries(fuzz)
    shared = {name(block) for block in entries((scratch.root / "Cargo.lock").read_text(encoding="utf-8"))[1]}
    at = next(index for index, block in enumerate(blocks) if name(block) in shared and "checksum" in block)
    target = blocks[at]
    version = tomllib.loads(target)["version"]

    def planted(block: str | None, extra: str = "") -> str:
        changed = [*blocks[:at], *([block] if block is not None else []), *blocks[at + 1 :]]
        return PACKAGE.join([header, *changed]) + extra

    cases = {
        "another version of a shared package": planted(target.replace(f'version = "{version}"', 'version = "0.0.1"', 1)),
        "another checksum of a shared package": planted(target.replace('checksum = "', 'checksum = "0', 1)),
        "a shared package missing": planted(None),
        "a stale package": planted(target, f'{PACKAGE}name = "almiladoc-stale"\nversion = "1.0.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n'),
    }
    failures = [] if not drift(scratch, fuzz) else ["the tracked lockfile does not reproduce, so the planted cases prove nothing"]
    failures += [f"{label} was not reported" for label, text in cases.items() if not drift(scratch, text)]
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    tree = Path(subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, check=True, text=True, encoding="utf-8").stdout.strip())
    fuzz = (tree / FUZZ_LOCK).read_text(encoding="utf-8")
    with tempfile.TemporaryDirectory() as directory:
        scratch = Scratch(args.cargo, tree, Path(directory))
        if args.self_test:
            return self_test(scratch, fuzz)
        problems = drift(scratch, fuzz)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(f"{FUZZ_LOCK} does not reproduce from Cargo.lock: copy Cargo.lock to {FUZZ_LOCK} and run cargo metadata in fuzz/", file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
