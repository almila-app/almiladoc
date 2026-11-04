"""Check that a built wheel ships its metadata, the typing marker and an SBOM, and the sdist its
metadata and the lockfile."""

import argparse
import io
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from collections.abc import Callable
from pathlib import Path, PurePosixPath


def has_sbom(names: set[str], info: str) -> bool:
    sboms = PurePosixPath(info, "sboms")
    return any(
        PurePosixPath(name).parent == sboms and name.endswith(".cyclonedx.json") for name in names
    )


def undecodable(data: bytes) -> bool:
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        return True
    return False


def check_wheel(wheel: Path) -> list[str]:
    with zipfile.ZipFile(wheel) as archive:
        names = set(archive.namelist())
        dist_info = {
            PurePosixPath(name).parts[0]
            for name in names
            if PurePosixPath(name).parts[0].endswith(".dist-info")
        }
        if len(dist_info) != 1:
            return [f"{wheel.name}: expected one .dist-info directory, found {sorted(dist_info)}"]
        (info,) = dist_info
        metadata = f"{info}/METADATA"
        malformed = metadata in names and undecodable(archive.read(metadata))
    problems = [
        f"{wheel.name}: {name} is missing"
        for name in (metadata, "almiladoc/py.typed", "almiladoc/_almiladoc.pyi")
        if name not in names
    ]
    if malformed:
        problems.append(f"{wheel.name}: {metadata} is not UTF-8")
    if not has_sbom(names, info):
        problems.append(f"{wheel.name}: {info}/sboms/*.cyclonedx.json is missing")
    return problems


def check_sdist(sdist: Path) -> list[str]:
    with tarfile.open(sdist) as archive:
        members = {member.name: member for member in archive.getmembers()}
        tops = {PurePosixPath(name).parts[0] for name in members}
        if len(tops) != 1:
            return [f"{sdist.name}: expected one top directory, found {sorted(tops)}"]
        (top,) = tops
        problems = [
            f"{sdist.name}: {top}/{name} is missing"
            for name in ("PKG-INFO", "Cargo.lock")
            if not ((member := members.get(f"{top}/{name}")) and member.isfile())
        ]
        info = members.get(f"{top}/PKG-INFO")
        if info and info.isfile() and undecodable(archive.extractfile(info).read()):
            problems.append(f"{sdist.name}: {top}/PKG-INFO is not UTF-8")
    return problems


METADATA = "Metadata-Version: 2.4\nLicense-Expression: GPL-3.0-only\n"
INFO = "almiladoc-0.0.0.dist-info"
TOP = "almiladoc-0.0.0"
SBOM = f"{INFO}/sboms/almiladoc-python.cyclonedx.json"
WHEEL_FILES = {
    f"{INFO}/METADATA": METADATA,
    SBOM: "{}",
    "almiladoc/py.typed": "",
    "almiladoc/_almiladoc.pyi": "",
}
SDIST_FILES = {
    f"{TOP}/PKG-INFO": METADATA,
    f"{TOP}/Cargo.lock": "version = 4\n",
}


def write_wheel(path: Path, files: dict[str, str | bytes]) -> Path:
    with zipfile.ZipFile(path, "w") as archive:
        for name, text in files.items():
            archive.writestr(name, text)
    return path


def write_sdist(path: Path, files: dict[str, str | bytes | None]) -> Path:
    with tarfile.open(path, "w:gz") as archive:
        for name, text in files.items():
            member = tarfile.TarInfo(name)
            if text is None:
                member.type = tarfile.DIRTYPE
                archive.addfile(member)
                continue
            data = text if isinstance(text, bytes) else text.encode("utf-8")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return path


def without(files: dict[str, str], name: str) -> dict[str, str]:
    return {key: text for key, text in files.items() if key != name}


def argument_failures(root: Path, wheel: Path, sdist: Path) -> list[str]:
    zipped, folder = root / "listing.zip", root / "folder.whl"
    zipped.touch()
    folder.mkdir()
    refused = {
        "a wheel named .zip": (zipped, sdist),
        "a missing wheel": (root / "absent.whl", sdist),
        "a wheel that is a directory": (folder, sdist),
        "an sdist named .whl": (wheel, wheel),
    }
    failures = []
    for label, (given_wheel, given_sdist) in refused.items():
        arguments = ["--wheel", str(given_wheel), "--sdist", str(given_sdist)]
        done = subprocess.run(
            [sys.executable, "-I", "-B", __file__, *arguments],
            capture_output=True,
            text=True,
        )
        if done.returncode != 2 or " is not a ." not in done.stderr:
            failures.append(f"{label} was not refused: {done.returncode} {done.stderr}")
    return failures


def self_test() -> int:
    lock = f"{TOP}/Cargo.lock"
    metadata, pkg_info = f"{INFO}/METADATA", f"{TOP}/PKG-INFO"
    sbom = "sboms/*.cyclonedx.json is missing"
    wheels = {
        "a wheel without METADATA": (without(WHEEL_FILES, metadata), f"{metadata} is missing"),
        "a wheel whose METADATA is not UTF-8": (
            WHEEL_FILES | {metadata: b"Metadata-Version: 2.4\nName: \xff\n"},
            f"{metadata} is not UTF-8",
        ),
        "a wheel without an SBOM": (without(WHEEL_FILES, SBOM), sbom),
        "a wheel whose SBOM sits outside its dist-info": (
            without(WHEEL_FILES, SBOM) | {"almiladoc/sboms/almiladoc-python.cyclonedx.json": "{}"},
            sbom,
        ),
        "a wheel whose only SBOM is not CycloneDX": (
            without(WHEEL_FILES, SBOM) | {f"{INFO}/sboms/almiladoc-python.spdx.json": "{}"},
            sbom,
        ),
    }
    sdists = {
        "an sdist without PKG-INFO": (without(SDIST_FILES, pkg_info), f"{pkg_info} is missing"),
        "an sdist whose PKG-INFO is a directory": (
            without(SDIST_FILES, pkg_info) | {pkg_info: None},
            f"{pkg_info} is missing",
        ),
        "an sdist whose PKG-INFO is not UTF-8": (
            SDIST_FILES | {pkg_info: b"Metadata-Version: 2.4\nName: \xff\n"},
            f"{pkg_info} is not UTF-8",
        ),
        "an sdist without Cargo.lock": (without(SDIST_FILES, lock), f"{lock} is missing"),
        "an sdist whose only Cargo.lock is nested": (
            without(SDIST_FILES, lock) | {f"{TOP}/python/Cargo.lock": SDIST_FILES[lock]},
            f"{lock} is missing",
        ),
    }
    failures = []
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        whole = check_wheel(write_wheel(root / "whole.whl", WHEEL_FILES)) + check_sdist(
            write_sdist(root / "whole.tar.gz", SDIST_FILES)
        )
        if whole:
            failures.append(f"a GPL-3.0-only pair without a licence file was refused: {whole}")
        for number, (label, (files, reason)) in enumerate(wheels.items()):
            if not any(
                reason in problem
                for problem in check_wheel(write_wheel(root / f"wheel-{number}.whl", files))
            ):
                failures.append(f"{label} was not refused: {reason}")
        for number, (label, (files, reason)) in enumerate(sdists.items()):
            if not any(
                reason in problem
                for problem in check_sdist(write_sdist(root / f"sdist-{number}.tar.gz", files))
            ):
                failures.append(f"{label} was not refused: {reason}")
        failures += argument_failures(root, root / "whole.whl", root / "whole.tar.gz")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def archive_path(suffix: str) -> Callable[[str], Path]:
    def parse(value: str) -> Path:
        path = Path(value).resolve()
        if not path.name.endswith(suffix) or not path.is_file():
            raise argparse.ArgumentTypeError(f"{value} is not a {suffix} file")
        return path

    return parse


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--wheel", type=archive_path(".whl"))
    parser.add_argument("--sdist", type=archive_path(".tar.gz"))
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if None in (args.wheel, args.sdist):
        parser.error("--wheel and --sdist are required without --self-test")
    problems = check_wheel(args.wheel) + check_sdist(args.sdist)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
