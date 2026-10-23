"""Check that a built wheel and sdist ship the licence set, its expression, the typing marker, an
SBOM and the lockfile."""

import argparse
import io
import sys
import tarfile
import tempfile
import zipfile
from email.parser import Parser
from pathlib import Path, PurePosixPath


def licence_files(entries: list[str]) -> set[str]:
    files: set[str] = set()
    for entry in entries:
        path = Path(entry)
        if entry.endswith("/"):
            found = {p.as_posix() for p in path.rglob("*") if p.is_file()}
            if not found:
                raise SystemExit(f"{entry} holds no file")
            files |= found
        elif path.is_file():
            files.add(path.as_posix())
        else:
            raise SystemExit(f"{entry} is not a file")
    return files


def check(
    label: str, names: set[str], metadata: str, prefix: str, wanted: set[str], expression: str
) -> list[str]:
    problems = [
        f"{label}: {prefix}{name} is missing"
        for name in sorted(wanted)
        if f"{prefix}{name}" not in names
    ]
    found = Parser().parsestr(metadata).get_all("License-Expression") or []
    if found != [expression]:
        problems.append(f"{label}: License-Expression is {found}, not [{expression!r}]")
    return problems


def has_sbom(names: set[str], info: str) -> bool:
    sboms = PurePosixPath(info, "sboms")
    return any(
        PurePosixPath(name).parent == sboms and name.endswith(".cyclonedx.json") for name in names
    )


def check_wheel(wheel: Path, wanted: set[str], expression: str) -> list[str]:
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
        metadata = archive.read(f"{info}/METADATA").decode("utf-8")
    problems = check(wheel.name, names, metadata, f"{info}/licenses/", wanted, expression)
    problems += [
        f"{wheel.name}: {name} is missing"
        for name in ("almiladoc/py.typed", "almiladoc/_almiladoc.pyi")
        if name not in names
    ]
    if not has_sbom(names, info):
        problems.append(f"{wheel.name}: {info}/sboms/*.cyclonedx.json is missing")
    return problems


def check_sdist(sdist: Path, wanted: set[str], expression: str) -> list[str]:
    with tarfile.open(sdist) as archive:
        names = set(archive.getnames())
        tops = {PurePosixPath(name).parts[0] for name in names}
        if len(tops) != 1:
            return [f"{sdist.name}: expected one top directory, found {sorted(tops)}"]
        (top,) = tops
        member = archive.extractfile(f"{top}/PKG-INFO")
        if member is None:
            return [f"{sdist.name}: PKG-INFO is missing"]
        metadata = member.read().decode("utf-8")
    problems = check(sdist.name, names, metadata, f"{top}/", wanted, expression)
    if f"{top}/Cargo.lock" not in names:
        problems.append(f"{sdist.name}: {top}/Cargo.lock is missing")
    return problems


EXPRESSION = "MIT"
INFO = "almiladoc-0.0.0.dist-info"
TOP = "almiladoc-0.0.0"
SBOM = f"{INFO}/sboms/almiladoc-python.cyclonedx.json"
WHEEL_FILES = {
    f"{INFO}/METADATA": f"Metadata-Version: 2.4\nLicense-Expression: {EXPRESSION}\n",
    f"{INFO}/licenses/LICENSE": "licence",
    SBOM: "{}",
    "almiladoc/py.typed": "",
    "almiladoc/_almiladoc.pyi": "",
}
SDIST_FILES = {
    f"{TOP}/PKG-INFO": f"Metadata-Version: 2.4\nLicense-Expression: {EXPRESSION}\n",
    f"{TOP}/LICENSE": "licence",
    f"{TOP}/Cargo.lock": "version = 4\n",
}


def write_wheel(path: Path, files: dict[str, str]) -> Path:
    with zipfile.ZipFile(path, "w") as archive:
        for name, text in files.items():
            archive.writestr(name, text)
    return path


def write_sdist(path: Path, files: dict[str, str]) -> Path:
    with tarfile.open(path, "w:gz") as archive:
        for name, text in files.items():
            data = text.encode("utf-8")
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return path


def without(files: dict[str, str], name: str) -> dict[str, str]:
    return {key: text for key, text in files.items() if key != name}


def self_test() -> int:
    wanted = {"LICENSE"}
    lock = f"{TOP}/Cargo.lock"
    sbom = "sboms/*.cyclonedx.json is missing"
    wheels = {
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
        "an sdist without Cargo.lock": (without(SDIST_FILES, lock), f"{lock} is missing"),
        "an sdist whose only Cargo.lock is nested": (
            without(SDIST_FILES, lock) | {f"{TOP}/python/Cargo.lock": SDIST_FILES[lock]},
            f"{lock} is missing",
        ),
    }
    failures = []
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        whole = check_wheel(
            write_wheel(root / "whole.whl", WHEEL_FILES), wanted, EXPRESSION
        ) + check_sdist(write_sdist(root / "whole.tar.gz", SDIST_FILES), wanted, EXPRESSION)
        if whole:
            failures.append(f"a complete pair was refused: {whole}")
        for number, (label, (files, reason)) in enumerate(wheels.items()):
            if not any(
                reason in problem
                for problem in check_wheel(
                    write_wheel(root / f"wheel-{number}.whl", files), wanted, EXPRESSION
                )
            ):
                failures.append(f"{label} was not refused for its missing file")
        for number, (label, (files, reason)) in enumerate(sdists.items()):
            if not any(
                reason in problem
                for problem in check_sdist(
                    write_sdist(root / f"sdist-{number}.tar.gz", files), wanted, EXPRESSION
                )
            ):
                failures.append(f"{label} was not refused for its missing file")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--expression")
    parser.add_argument("--licence-set", nargs="+")
    parser.add_argument("--wheel", type=Path)
    parser.add_argument("--sdist", type=Path)
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if None in (args.expression, args.licence_set, args.wheel, args.sdist):
        parser.error(
            "--expression, --licence-set, --wheel and --sdist are required without --self-test"
        )
    wanted = licence_files(args.licence_set)
    problems = check_wheel(args.wheel, wanted, args.expression) + check_sdist(
        args.sdist, wanted, args.expression
    )
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
