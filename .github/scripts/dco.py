"""Require every commit of a range to carry its author's Signed-off-by and a subject line alone;
every file REUSE.toml marked MIT at the merge base to keep an MIT-bearing annotation and every
copyright line of it; every MIT-bearing annotation of the tree to override, and no file to come
under an MIT-only one it did not have at the merge base."""

import argparse
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

DEPENDABOT = ("dependabot[bot]", "49699333+dependabot[bot]@users.noreply.github.com")
ISOLATED = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR")
SIGN_OFFS = "%(trailers:key=Signed-off-by,valueonly,separator=%x01)"
FIELDS = f"%H%x00%an%x00%ae%x00{SIGN_OFFS}%x00%s%x00%B%x00"
SUBJECT_COLUMNS = 72


def git(root: Path, *args: str, env: dict[str, str] | None = None) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args],
        capture_output=True,
        check=True,
        text=True,
        encoding="utf-8",
        env=env,
    ).stdout


def check(root: Path, revisions: str, skip_dependabot: bool) -> list[str]:
    fields = git(root, "log", f"--format={FIELDS}", revisions).split("\0")
    problems = []
    for start in range(0, len(fields) - 6, 6):
        sha, name, email, trailers, subject = (
            field.strip("\n") for field in fields[start : start + 5]
        )
        first_line = fields[start + 5].split("\n", 1)[0].rstrip()
        if skip_dependabot and (name, email) == DEPENDABOT:
            continue
        signed = {value.strip() for value in trailers.split("\x01") if value.strip()}
        if f"{name} <{email}>" not in signed:
            problems.append(
                f"{sha[:12]} by {name} <{email}> lacks its author's Signed-off-by (git commit -s)"
            )
        if len(subject) > SUBJECT_COLUMNS:
            problems.append(
                f"{sha[:12]} by {name} <{email}> has a subject over {SUBJECT_COLUMNS} columns"
            )
        if first_line != subject:
            problems.append(
                f"{sha[:12]} by {name} <{email}> has no subject alone on its first line"
            )
    return problems


@dataclass(frozen=True)
class Annotation:
    paths: re.Pattern[str]
    terms: frozenset[str]
    copyrights: frozenset[str]
    override: bool

    @property
    def mit(self) -> bool:
        return "MIT" in self.terms


def listed(value: str | list[str]) -> list[str]:
    return [value] if isinstance(value, str) else value


def glob(pattern: str) -> str:
    return "".join(
        ".*" if part == "**" else "[^/]*" if part == "*" else re.escape(part)
        for part in re.split(r"(\*\*|\*)", pattern)
    )


def annotations(text: str) -> list[Annotation]:
    return [
        Annotation(
            re.compile("|".join(glob(path) for path in listed(entry["path"]))),
            frozenset(term.strip() for term in entry["SPDX-License-Identifier"].split(" AND ")),
            frozenset(listed(entry.get("SPDX-FileCopyrightText", []))),
            entry.get("precedence") == "override",
        )
        for entry in tomllib.loads(text).get("annotations", [])
    ]


def resolve(table: list[Annotation], path: str) -> Annotation | None:
    return next((entry for entry in reversed(table) if entry.paths.fullmatch(path)), None)


def snapshot(root: Path) -> str:
    index = Path(git(root, "rev-parse", "--path-format=absolute", "--git-path", "index").strip())
    with tempfile.TemporaryDirectory() as directory:
        copy = Path(directory) / "index"
        if index.exists():
            copy.write_bytes(index.read_bytes())
        env = {**os.environ, "GIT_INDEX_FILE": str(copy)}
        git(root, "add", "-A", env=env)
        return git(root, "write-tree", env=env).strip()


def merge_base(root: Path, base: str, head: str) -> str | None:
    found = subprocess.run(
        ["git", "-C", str(root), "merge-base", base, head], capture_output=True, text=True
    )
    if found.returncode == 1:
        return None
    found.check_returncode()
    return found.stdout.strip()


def renamed(root: Path, base: str, tree: str) -> dict[str, str]:
    fields = git(root, "diff", "-z", "--name-status", "-M", base, tree).split("\0")
    origins, index = {}, 0
    while index < len(fields) and fields[index]:
        if fields[index].startswith("R"):
            origins[fields[index + 2]] = fields[index + 1]
            index += 3
        else:
            index += 2
    return origins


def files(root: Path, tree: str) -> list[str]:
    return [
        path for path in git(root, "ls-tree", "-r", "-z", "--name-only", tree).split("\0") if path
    ]


def resolves(root: Path, revision: str) -> bool:
    return not subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--verify", "--quiet", f"{revision}^{{commit}}"],
        capture_output=True,
    ).returncode


def unguarded(kept: Annotation | None) -> str | None:
    if kept is not None and kept.mit and not kept.override:
        return 'its MIT-bearing annotation lacks precedence = "override"'
    return None


def refusal(was: Annotation | None, kept: Annotation | None) -> str | None:
    if found := unguarded(kept):
        return found
    if kept is not None and kept.terms == {"MIT"} and (was is None or was.terms != {"MIT"}):
        return "falls under an MIT-only annotation it did not have at the merge base"
    if was is None or not was.mit:
        return None
    if kept is None or not kept.mit:
        return "leaves the MIT-bearing annotation it had at the merge base"
    lost = sorted(was.copyrights - kept.copyrights)
    return f"its MIT-bearing annotation lost {', '.join(map(repr, lost))}" if lost else None


def licences(root: Path, base: str, head: str) -> tuple[list[str], str]:
    if not resolves(root, base):
        return [f"{base} does not resolve: the licence guard needs it and full history"], ""
    tree = snapshot(root)
    now = annotations(git(root, "show", f"{tree}:REUSE.toml"))
    merge = merge_base(root, base, head)
    paths = files(root, tree)
    if merge is None:
        problems = [
            f"{path}: {found}" for path in paths if (found := unguarded(resolve(now, path)))
        ]
        return problems, f"licences: {base} shares no history with {head}; the tree judged alone"
    before = annotations(git(root, "show", f"{merge}:REUSE.toml"))
    existed, origins, problems, judged = set(files(root, merge)), renamed(root, merge, tree), [], 0
    for path in paths:
        origin = origins.get(path, path)
        was = resolve(before, origin) if origin in existed else None
        judged += was is not None and was.mit
        found = refusal(was, resolve(now, path))
        if found:
            problems.append(f"{path}: {found}")
    return problems, (
        f"licences: {judged} files MIT-bearing at {merge[:12]} keep it and every copyright line;"
        " the tree's MIT-bearing annotations override and take no file in"
    )


def findings(problems: list[str]) -> Counter[str]:
    return Counter(problem.split(" by ", 1)[1] for problem in problems)


def self_test() -> int:
    for key in ISOLATED:
        os.environ.pop(key, None)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        git(root, "init", "-q")
        (root / ".git" / "no-hooks").mkdir()
        env = {**os.environ, "GIT_COMMITTER_NAME": "C", "GIT_COMMITTER_EMAIL": "c@example.com"}

        def commit(name: str, message: str, email: str = "", cleanup: str = "strip") -> None:
            author = {"GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email or f"{name}@example.com"}
            git(
                root,
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=.git/no-hooks",
                "commit",
                "-q",
                "--allow-empty",
                f"--cleanup={cleanup}",
                "-m",
                message,
                env={**env, **author},
            )

        def signed(name: str, message: str, cleanup: str = "strip") -> None:
            sign_off = f"Signed-off-by: {name} <{name}@example.com>"
            commit(name, f"{message}\n\n{sign_off}", cleanup=cleanup)

        commit("Base", "base")
        base = git(root, "rev-parse", "HEAD").strip()
        signed("Ann", "signed")
        commit("Bob", "unsigned")
        commit("Cid", "someone else's sign-off\n\nSigned-off-by: Ann <Ann@example.com>")
        commit("Dee", "sign-off in the body only: Signed-off-by: Dee <Dee@example.com>")
        signed("Gül", "ş" * SUBJECT_COLUMNS)
        signed("Hal", "x" * (SUBJECT_COLUMNS + 1))
        signed("Ivy", "subject\nbody on the second line")
        signed("Jo", "x" * (SUBJECT_COLUMNS + 1) + "\nbody on the second line")
        signed("Kim", "\t\n\nreal subject\nbody runs on", cleanup="verbatim")
        signed("Lee", "\nsubject after an empty line", cleanup="verbatim")
        signed("Max", "subject\n \nbody after a whitespace-only line", cleanup="verbatim")
        signed("Ned", "subject with trailing blanks \t\n\nbody", cleanup="verbatim")
        signed("Oz", "x" * SUBJECT_COLUMNS + "\nruns on", cleanup="verbatim")
        commit(DEPENDABOT[0], "Bump " + "x" * SUBJECT_COLUMNS, DEPENDABOT[1])
        commit(DEPENDABOT[0], "impostor", "Eve@example.com")
        on_push = findings(check(root, f"{base}..HEAD", skip_dependabot=False))
        on_pull = findings(check(root, f"{base}..HEAD", skip_dependabot=True))
    unsigned = "lacks its author's Signed-off-by (git commit -s)"
    long = f"has a subject over {SUBJECT_COLUMNS} columns"
    joined = "has no subject alone on its first line"
    expected_pull = Counter(
        [
            f"Bob <Bob@example.com> {unsigned}",
            f"Cid <Cid@example.com> {unsigned}",
            f"Dee <Dee@example.com> {unsigned}",
            f"Hal <Hal@example.com> {long}",
            f"Ivy <Ivy@example.com> {joined}",
            f"Jo <Jo@example.com> {long}",
            f"Jo <Jo@example.com> {joined}",
            f"Kim <Kim@example.com> {joined}",
            f"Lee <Lee@example.com> {joined}",
            f"Oz <Oz@example.com> {long}",
            f"Oz <Oz@example.com> {joined}",
            f"{DEPENDABOT[0]} <Eve@example.com> {unsigned}",
        ]
    )
    bot = f"{DEPENDABOT[0]} <{DEPENDABOT[1]}>"
    expected_push = expected_pull + Counter([f"{bot} {unsigned}", f"{bot} {long}"])
    failures = []
    if on_push != expected_push:
        failures.append(f"a candidate range reported {sorted(on_push.elements())}")
    if on_pull != expected_pull:
        failures.append(f"a pull-request range reported {sorted(on_pull.elements())}")
    failures += licence_self_test()
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


PRIOR = "FS" + "L-1.1-MIT"
LICENCE_BASE = f"""version = 1

[[annotations]]
path = "**"
precedence = "override"
SPDX-FileCopyrightText = "2026 Holder Inc."
SPDX-License-Identifier = "MIT"

[[annotations]]
path = ["own/*", "LICENSES/**", "REUSE.toml", "listed.txt"]
precedence = "override"
SPDX-FileCopyrightText = "2026 Licensor"
SPDX-License-Identifier = "BUSL-1.1"

[[annotations]]
path = "third.txt"
precedence = "override"
SPDX-FileCopyrightText = "2026 Third Party"
SPDX-License-Identifier = "MIT"

[[annotations]]
path = "mixed ş.txt"
precedence = "override"
SPDX-FileCopyrightText = ["2026 Holder Inc.", "2026 Licensor"]
SPDX-License-Identifier = "{PRIOR} AND MIT"
"""


def licence_self_test() -> list[str]:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory) / "repo ü"
        os.environ.update(HOME=directory, XDG_CONFIG_HOME=directory, GIT_CONFIG_NOSYSTEM="1")
        git(root.parent, "init", "-q", root.name)
        identity = ("-c", "user.name=T", "-c", "user.email=t@example.com")

        def write(path: str, text: str) -> None:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(text, encoding="utf-8")

        def reuse(old: str, new: str) -> None:
            write("REUSE.toml", LICENCE_BASE.replace(old, new))

        write("LICENSES/MIT.txt", "MIT License\n\nCopyright (c) 2026 Holder Inc.\n")
        write("REUSE.toml", LICENCE_BASE)
        names = ("kept.txt", "moved ü.txt", "mixed ş.txt", "third.txt", "gone.txt", "own/a.txt")
        for name in names:
            write(name, "".join(f"{name} {n}\n" for n in range(20)))
        write("own/deep/sub.txt", "nested\n")
        write("listed.txt", "listed\n")
        git(root, "add", "-A")
        git(root, *identity, "commit", "-q", "--no-verify", "-m", "base")
        alone = git(root, *identity, "commit-tree", "HEAD^{tree}", "-m", "alone").strip()
        git(root, "branch", "main")
        git(root, "branch", "unrelated", alone)
        own = '"own/*", "LICENSES/**",'

        def added() -> None:
            write("fresh.txt", "new\n")
            write("split.txt", "split\n")
            text = LICENCE_BASE.replace(own, f'{own} "fresh.txt",')
            write("REUSE.toml", text.replace('"mixed ş.txt"', '["mixed ş.txt", "split.txt"]'))
            (root / "gone.txt").unlink()

        def undeclared() -> None:
            write("loose ü.txt", "new\n")
            write("third.txt", "third.txt edited\n")

        def unguarded() -> None:
            reuse('path = "**"\nprecedence = "override"\n', 'path = "**"\n')

        def left() -> None:
            text = LICENCE_BASE.replace(own, f'{own} "kept.txt", "own/deep/**",')
            write("REUSE.toml", text.replace(" AND MIT", ""))

        def lost() -> None:
            text = LICENCE_BASE.replace(
                '["2026 Holder Inc.", "2026 Licensor"]', '["2026 Licensor"]'
            )
            write("REUSE.toml", text.replace('"2026 Third Party"', '"2026 Other"'))

        dropped = ["gone.txt", "kept.txt", "moved ü.txt", "own/deep/sub.txt"]
        changes = {
            "a new file and a deletion": (added, "main", []),
            "paths that leave": (left, "main", ["kept.txt", "mixed ş.txt", "own/deep/sub.txt"]),
            "a rename that leaves": (
                lambda: (root / "moved ü.txt").rename(root / "own" / "moved ü.txt"),
                "main",
                ["own/moved ü.txt"],
            ),
            "an own-work file renamed out of its glob": (
                lambda: (root / "own" / "a.txt").rename(root / "a ü.txt"),
                "main",
                ["a ü.txt"],
            ),
            "an own-work file dropped from its list": (
                lambda: reuse(', "listed.txt"]', "]"),
                "main",
                ["listed.txt"],
            ),
            "an own-work id relabelled beside MIT": (
                lambda: reuse(f'"{PRIOR} AND MIT"', '"BUSL-1.1 AND MIT"'),
                "main",
                [],
            ),
            "lost copyright lines": (lost, "main", ["mixed ş.txt", "third.txt"]),
            "an undeclared new file": (undeclared, "main", ["loose ü.txt"]),
            "a dropped override": (unguarded, "main", dropped),
            "no merge base": (left, "unrelated", []),
            "no merge base, a dropped override": (unguarded, "unrelated", dropped),
            "an unresolvable base": (lambda: None, "absent", ["absent"]),
        }
        failures = []
        for label, (change, base, refused) in changes.items():
            change()
            problems, _ = licences(root, base, "HEAD")
            found = sorted(
                problem.split(": ", 1)[0].split(" does not", 1)[0] for problem in problems
            )
            if found != refused:
                failures.append(f"{label} refused {found}, expected {refused}")
            git(root, "reset", "-q", "--hard")
            git(root, "clean", "-q", "-fd")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--base", help="the range's exclusive start")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument(
        "--pull-request",
        action="store_true",
        help="skip commits authored by dependabot[bot] from its noreply address",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.base:
        parser.error("--base is required")
    root = Path.cwd()
    licence_problems, note = licences(root, args.base, args.head)
    problems = licence_problems
    if resolves(root, args.base):
        problems = check(root, f"{args.base}..{args.head}", args.pull_request) + problems
    for problem in problems:
        print(problem, file=sys.stderr)
    if not licence_problems:
        print(note)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
