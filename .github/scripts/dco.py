"""Require every commit of a range to carry its author's Signed-off-by trailer."""

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

DEPENDABOT = ("dependabot[bot]", "49699333+dependabot[bot]@users.noreply.github.com")
ISOLATED = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR")
FIELDS = "%H%x00%an%x00%ae%x00%(trailers:key=Signed-off-by,valueonly,separator=%x01)%x00"


def git(root: Path, *args: str, env: dict[str, str] | None = None) -> str:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=True, text=True, encoding="utf-8", env=env).stdout


def check(root: Path, revisions: str, skip_dependabot: bool) -> list[str]:
    fields = git(root, "log", f"--format={FIELDS}", revisions).split("\0")
    problems = []
    for start in range(0, len(fields) - 4, 4):
        sha, name, email, trailers = (field.strip("\n") for field in fields[start : start + 4])
        if skip_dependabot and (name, email) == DEPENDABOT:
            continue
        signed = {value.strip() for value in trailers.split("\x01") if value.strip()}
        if f"{name} <{email}>" not in signed:
            problems.append(f"{sha[:12]} by {name} <{email}> lacks its author's Signed-off-by (git commit -s)")
    return problems


def self_test() -> int:
    for key in ISOLATED:
        os.environ.pop(key, None)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        git(root, "init", "-q")
        (root / ".git" / "no-hooks").mkdir()
        env = {**os.environ, "GIT_COMMITTER_NAME": "C", "GIT_COMMITTER_EMAIL": "c@example.com"}

        def commit(name: str, message: str, email: str = "") -> None:
            author = {"GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email or f"{name}@example.com"}
            git(root, "-c", "commit.gpgsign=false", "-c", "core.hooksPath=.git/no-hooks", "commit", "-q", "--allow-empty", "-m", message, env={**env, **author})

        commit("Base", "base")
        base = git(root, "rev-parse", "HEAD").strip()
        commit("Ann", "signed\n\nSigned-off-by: Ann <Ann@example.com>")
        commit("Bob", "unsigned")
        commit("Cid", "someone else's sign-off\n\nSigned-off-by: Ann <Ann@example.com>")
        commit("Dee", "sign-off in the body only: Signed-off-by: Dee <Dee@example.com>")
        commit(DEPENDABOT[0], "bump", DEPENDABOT[1])
        commit(DEPENDABOT[0], "impostor", "Eve@example.com")
        on_push = {line.split(" by ")[1].split(" <")[0] for line in check(root, f"{base}..HEAD", skip_dependabot=False)}
        pulled = check(root, f"{base}..HEAD", skip_dependabot=True)
        on_pull = {line.split(" by ")[1].split(" <")[0] for line in pulled}
    failures = []
    if on_push != {"Bob", "Cid", "Dee", DEPENDABOT[0]}:
        failures.append(f"a candidate range reported {sorted(on_push)}")
    if on_pull != {"Bob", "Cid", "Dee", DEPENDABOT[0]} or sum(DEPENDABOT[0] in line for line in pulled) != 1:
        failures.append(f"a pull-request range reported {sorted(on_pull)}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--base", help="the range's exclusive start")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--pull-request", action="store_true", help="skip commits authored by dependabot[bot] from its noreply address")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.base:
        parser.error("--base is required")
    problems = check(Path.cwd(), f"{args.base}..{args.head}", args.pull_request)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
