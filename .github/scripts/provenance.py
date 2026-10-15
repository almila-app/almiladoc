"""Check each file's REUSE licence and copyright against its provenance from the upstream base."""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

MIT = "MIT"
FSL = "FSL-1.1-MIT"
MIXED = f"{FSL} AND {MIT}"
YEARS = re.compile(r"^(?:SPDX-FileCopyrightText:\s*)?(?:(?:\(c\)|©|Copyright)\s*)*(?:\d{4}(?:\s*-\s*\d{4})?\s*,?\s*)+", re.IGNORECASE)


@dataclass(frozen=True)
class Config:
    base: str
    upstream: str
    licensor: str
    named: dict[str, str]

    @classmethod
    def load(cls, path: Path) -> "Config":
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        return cls(data["base"], data["upstream"], data["licensor"], dict(data.get("named", {})))


def git(root: Path, *args: str, env: dict[str, str] | None = None) -> str:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=True, text=True, encoding="utf-8", env=env).stdout


def snapshot(root: Path) -> str:
    index = Path(git(root, "rev-parse", "--path-format=absolute", "--git-path", "index").strip())
    with tempfile.TemporaryDirectory() as directory:
        copy = Path(directory) / "index"
        if index.exists():
            copy.write_bytes(index.read_bytes())
        env = {**os.environ, "GIT_INDEX_FILE": str(copy)}
        git(root, "add", "-A", env=env)
        return git(root, "write-tree", env=env).strip()


def expected_licences(root: Path, config: Config, tree: str) -> dict[str, str]:
    fields = git(root, "diff", "-z", "--name-status", "-M", "-C", "--find-copies-harder", "-l100000", config.base, tree).split("\0")
    status: dict[str, str] = {}
    index = 0
    while index < len(fields) and fields[index]:
        kind = fields[index]
        if kind[0] in "RC":
            status[fields[index + 2]] = kind
            index += 3
        else:
            status[fields[index + 1]] = kind
            index += 2
    deleted = set(git(root, "log", "--no-renames", "--diff-filter=D", "--format=", "--name-only", "-z", f"{config.base}..HEAD").split("\0"))
    deleted.update(path for path, kind in status.items() if kind == "D")
    expected = {}
    for path in git(root, "ls-tree", "-r", "-z", "--name-only", tree).split("\0"):
        if not path:
            continue
        kind = status.get(path, "unchanged")
        exact = kind == "unchanged" or kind in ("R100", "C100")
        derived = kind in ("M", "T") or (kind[0] in "RC" and not exact)
        if path in config.named or (derived and path not in deleted):
            expected[path] = MIXED
        elif exact:
            expected[path] = MIT
        else:
            expected[path] = FSL
    return expected


def declared(root: Path, reuse: list[str]) -> dict[str, tuple[list[str], set[str]]]:
    result = subprocess.run([*reuse, "--root", str(root), "lint", "--json"], capture_output=True, text=True, encoding="utf-8")
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise SystemExit(f"reuse lint --json gave no report: {result.stderr.strip()}") from error
    files = {}
    for entry in report["files"]:
        expressions = [item["value"] for item in entry["spdx_expressions"]]
        holders = {YEARS.sub("", item["value"]).strip() for item in entry["copyrights"]}
        files[entry["path"].replace("\\", "/")] = (expressions, holders)
    return files


def terms(expression: str) -> frozenset[str]:
    return frozenset(term.strip() for term in expression.split(" AND "))


def check(root: Path, config: Config, reuse: list[str]) -> list[str]:
    if subprocess.run(["git", "-C", str(root), "cat-file", "-e", f"{config.base}^{{commit}}"], capture_output=True).returncode:
        return [f"upstream base {config.base} is not in this clone: provenance needs full history (fetch-depth: 0)"]
    expected = expected_licences(root, config, snapshot(root))
    problems = [f"{path}: named, but absent from the upstream base" for path, origin in config.named.items() if not git(root, "ls-tree", "--name-only", config.base, "--", origin).strip()]
    problems += [f"{path}: named, but no longer in the tree" for path in config.named if path not in expected]
    holders = {MIT: {config.upstream}, FSL: {config.licensor}, MIXED: {config.upstream, config.licensor}}
    for path, (expressions, copyrights) in sorted(declared(root, reuse).items()):
        if path not in expected:
            continue
        want = expected[path]
        if [terms(expression) for expression in expressions] != [terms(want)]:
            problems.append(f"{path}: declared {' ; '.join(expressions) or 'nothing'}, provenance says {want}")
        missing = holders[want] - copyrights
        extra = copyrights - holders[want]
        if missing or (extra and want != MIXED):
            problems.append(f"{path}: copyright {sorted(copyrights)}, provenance says {sorted(holders[want])}")
    return problems


def self_test(reuse: list[str]) -> int:
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
        os.environ.pop(key, None)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        git(root, "init", "-q")
        (root / ".git" / "no-hooks").mkdir()

        def write(path: str, text: str) -> None:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(text, encoding="utf-8")

        def commit(message: str) -> None:
            git(root, "add", "-A")
            git(root, "-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=.git/no-hooks", "commit", "-q", "-m", message)

        body = "".join(f"line {n}\n" for n in range(40))
        for name in ("same", "changed", "recreated", "moved", "edited-move", "copied", "origin"):
            write(f"{name}.txt", f"{name}\n{body}")
        commit("base")
        base = git(root, "rev-parse", "HEAD").strip()
        (root / "recreated.txt").unlink()
        commit("delete")
        write("recreated.txt", f"recreated\n{body}extra\n")
        write("changed.txt", f"changed\n{body}extra\n")
        (root / "moved.txt").rename(root / "dir-moved.txt")
        (root / "edited-move.txt").rename(root / "edited.txt")
        write("edited.txt", f"edited-move\n{body}extra\n")
        write("copy-of-copied.txt", f"copied\n{body}")
        write("new.txt", "unrelated\n")
        write("named.txt", "split below the threshold\n")
        commit("work")
        write("LICENSES/MIT.txt", "MIT\n")
        write("LICENSES/FSL-1.1-MIT.txt", "FSL\n")
        config = Config(base, "Upstream Inc.", "Licensor", {"named.txt": "origin.txt"})
        right = {
            MIT: ["same.txt", "dir-moved.txt", "copied.txt", "copy-of-copied.txt", "origin.txt"],
            FSL: ["recreated.txt", "new.txt"],
            MIXED: ["changed.txt", "edited.txt", "named.txt"],
        }

        def declare(table: dict[str, list[str]]) -> None:
            text = 'version = 1\n'
            for licence, paths in table.items():
                names = {MIT: ['"2026 Upstream Inc."'], FSL: ['"2026 Licensor"'], MIXED: ['"2026 Upstream Inc."', '"2026 Licensor"']}[licence]
                text += f'[[annotations]]\npath = {json.dumps(paths)}\nprecedence = "override"\nSPDX-FileCopyrightText = [{", ".join(names)}]\nSPDX-License-Identifier = "{licence}"\n'
            write("REUSE.toml", text)

        declare(right)
        failures = [f"a correct declaration was refused: {problem}" for problem in check(root, config, reuse)]
        defects = {
            "same.txt": MIXED,
            "dir-moved.txt": FSL,
            "copy-of-copied.txt": FSL,
            "recreated.txt": MIXED,
            "new.txt": MIT,
            "changed.txt": MIT,
            "edited.txt": FSL,
            "named.txt": FSL,
        }
        for path, wrong in defects.items():
            table = {licence: [p for p in paths if p != path] for licence, paths in right.items()}
            table[wrong] = [*table[wrong], path]
            declare(table)
            if not any(problem.startswith(f"{path}:") for problem in check(root, config, reuse)):
                failures.append(f"{path} declared {wrong} was not reported")
        write("REUSE.toml", 'version = 1\n[[annotations]]\npath = "**"\nSPDX-FileCopyrightText = "2026 Licensor"\nSPDX-License-Identifier = "MIT"\n')
        if not any(problem.startswith("same.txt: copyright") for problem in check(root, config, reuse)):
            failures.append("a wrong copyright holder was not reported")
        declare(right)
        with (root / "REUSE.toml").open("a", encoding="utf-8") as reuse_toml:
            reuse_toml.write(f'[[annotations]]\npath = "new.txt"\nprecedence = "override"\nSPDX-FileCopyrightText = ["2026 Upstream Inc.", "2026 Licensor"]\nSPDX-License-Identifier = "{FSL}"\n')
        if not any(problem.startswith("new.txt: copyright") for problem in check(root, config, reuse)):
            failures.append("an extra copyright holder was not reported")
        gone = Config(base, config.upstream, config.licensor, {"gone.txt": "origin.txt", "named.txt": "nowhere.txt"})
        reported = check(root, gone, reuse)
        for path in ("gone.txt", "named.txt"):
            if not any(problem.startswith(f"{path}: named") for problem in reported):
                failures.append(f"a stale named path {path} was not reported")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--config", type=Path, default=Path(".github/provenance.toml"))
    args = parser.parse_args()
    reuse = [sys.executable, "-m", "reuse"]
    if args.self_test:
        return self_test(reuse)
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").strip())
    problems = check(root, Config.load(args.config), reuse)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
