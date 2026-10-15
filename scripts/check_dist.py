"""Check that a built wheel and sdist ship the licence set, its expression and the typing marker."""

import argparse
import sys
import tarfile
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


def check(label: str, names: set[str], metadata: str, prefix: str, wanted: set[str], expression: str) -> list[str]:
    problems = [f"{label}: {prefix}{name} is missing" for name in sorted(wanted) if f"{prefix}{name}" not in names]
    found = Parser().parsestr(metadata).get_all("License-Expression") or []
    if found != [expression]:
        problems.append(f"{label}: License-Expression is {found}, not [{expression!r}]")
    return problems


def check_wheel(wheel: Path, wanted: set[str], expression: str) -> list[str]:
    with zipfile.ZipFile(wheel) as archive:
        names = set(archive.namelist())
        dist_info = {PurePosixPath(name).parts[0] for name in names if PurePosixPath(name).parts[0].endswith(".dist-info")}
        if len(dist_info) != 1:
            return [f"{wheel.name}: expected one .dist-info directory, found {sorted(dist_info)}"]
        (info,) = dist_info
        metadata = archive.read(f"{info}/METADATA").decode("utf-8")
    problems = check(wheel.name, names, metadata, f"{info}/licenses/", wanted, expression)
    problems += [f"{wheel.name}: {name} is missing" for name in ("almiladoc/py.typed", "almiladoc/_almiladoc.pyi") if name not in names]
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
    return check(sdist.name, names, metadata, f"{top}/", wanted, expression)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expression", required=True)
    parser.add_argument("--licence-set", nargs="+", required=True)
    parser.add_argument("--wheel", type=Path, required=True)
    parser.add_argument("--sdist", type=Path, required=True)
    args = parser.parse_args()
    wanted = licence_files(args.licence_set)
    problems = check_wheel(args.wheel, wanted, args.expression) + check_sdist(args.sdist, wanted, args.expression)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
