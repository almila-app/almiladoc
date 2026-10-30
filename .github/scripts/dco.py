"""Require the tree to track .github/signoff, the declaration that every commit is signed off;
every commit of a range to carry its author's Signed-off-by and a subject line alone,
and to name no assistant as its author or committer."""

import argparse
import os
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

DEPENDABOT = ("dependabot[bot]", "49699333+dependabot[bot]@users.noreply.github.com")
COPILOT = ("copilot-swe-agent[bot]", "198982749+Copilot@users.noreply.github.com")
PULL_REQUEST_SKIPS = {DEPENDABOT, COPILOT}
ISOLATED = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR")
SIGN_OFFS = "%(trailers:key=Signed-off-by,valueonly,separator=%x01)"
FIELDS = f"%H%x00%an%x00%ae%x00%cn%x00%ce%x00{SIGN_OFFS}%x00%s%x00%B%x00"
SUBJECT_COLUMNS = 72
SIGN_OFF_POLICY = ".github/signoff"
UNDECLARED = f"{SIGN_OFF_POLICY} is absent: the tree must declare that every commit is signed off"
UNRESOLVED = "{} does not resolve: the range check needs it and full history"
# Bounded by letters only: `_` and digits separate words in logins and e-mail addresses.
ASSISTANT = re.compile(r"(?<![^\W\d_])(claude|anthropic|llm)(?![^\W\d_])", re.IGNORECASE)


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
    for start in range(0, len(fields) - 8, 8):
        sha, name, email, committer, committer_email, trailers, subject = (
            field.strip("\n") for field in fields[start : start + 7]
        )
        first_line = fields[start + 7].split("\n", 1)[0].rstrip()
        if skip_dependabot and (name, email) in PULL_REQUEST_SKIPS:
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
        for role, who in (
            ("author", f"{name} <{email}>"),
            ("committer", f"{committer} <{committer_email}>"),
        ):
            if ASSISTANT.search(who):
                problems.append(
                    f"{sha[:12]} by {name} <{email}> names an assistant as its {role}: {who}"
                )
    return problems


def resolves(root: Path, revision: str) -> bool:
    return not subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--verify", "--quiet", f"{revision}^{{commit}}"],
        capture_output=True,
    ).returncode


def snapshot(root: Path) -> str:
    index = Path(git(root, "rev-parse", "--path-format=absolute", "--git-path", "index").strip())
    with tempfile.TemporaryDirectory() as directory:
        copy = Path(directory) / "index"
        if index.exists():
            copy.write_bytes(index.read_bytes())
        env = {**os.environ, "GIT_INDEX_FILE": str(copy)}
        git(root, "add", "-A", env=env)
        return git(root, "write-tree", env=env).strip()


def declares_sign_off(root: Path, tree: str) -> bool:
    kind = subprocess.run(
        ["git", "-C", str(root), "cat-file", "-t", f"{tree}:{SIGN_OFF_POLICY}"],
        capture_output=True,
        text=True,
    )
    return kind.stdout.strip() == "blob"


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

        def commit(
            name: str,
            message: str,
            email: str = "",
            cleanup: str = "strip",
            by: tuple[str, str] | None = None,
        ) -> None:
            author = {"GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email or f"{name}@example.com"}
            if by:
                author |= {"GIT_COMMITTER_NAME": by[0], "GIT_COMMITTER_EMAIL": by[1]}
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
        pat = "Signed-off-by: Pat <Pat@example.com>"
        commit("Pat", f"committed by an assistant\n\n{pat}", by=("Bot", "bot@anthropic.example"))
        commit("Claude", "authored by one\n\nSigned-off-by: Claude <Claude@example.com>")
        sam = "Signed-off-by: Sam <llm_bot@example.com>"
        commit("Sam", f"an address naming one\n\n{sam}", "llm_bot@example.com")
        quinn = "Signed-off-by: Quinn Llmore <quinn@example.com>"
        commit("Quinn Llmore", f"a name that only begins with one\n\n{quinn}", "quinn@example.com")
        signed("Claudia", "a name that only begins with one")
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
            (
                "Pat <Pat@example.com> names an assistant as its committer: "
                "Bot <bot@anthropic.example>"
            ),
            (
                "Claude <Claude@example.com> names an assistant as its author: "
                "Claude <Claude@example.com>"
            ),
            (
                "Sam <llm_bot@example.com> names an assistant as its author: "
                "Sam <llm_bot@example.com>"
            ),
        ]
    )
    bot = f"{DEPENDABOT[0]} <{DEPENDABOT[1]}>"
    expected_push = expected_pull + Counter([f"{bot} {unsigned}", f"{bot} {long}"])
    failures = []
    if on_push != expected_push:
        failures.append(f"a candidate range reported {sorted(on_push.elements())}")
    if on_pull != expected_pull:
        failures.append(f"a pull-request range reported {sorted(on_pull.elements())}")
    failures += declaration_self_test()
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def declaration_self_test() -> list[str]:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        os.environ.update(HOME=directory, XDG_CONFIG_HOME=directory, GIT_CONFIG_NOSYSTEM="1")
        git(root, "init", "-q")
        identity = ("-c", "user.name=T", "-c", "user.email=t@example.com")
        git(
            root,
            *identity,
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "--no-verify",
            "-m",
            "x",
        )
        marker = root / SIGN_OFF_POLICY

        def run(base: str) -> tuple[int, str]:
            done = subprocess.run(
                [sys.executable, "-I", "-B", __file__, "--base", base],
                cwd=root,
                capture_output=True,
                text=True,
            )
            return done.returncode, done.stderr.strip()

        failures = []

        def expect(label: str, wanted: tuple[int, str], base: str = "HEAD") -> None:
            if (found := run(base)) != wanted:
                failures.append(f"{label} gave {found}, expected {wanted}")

        expect(f"a tree without {SIGN_OFF_POLICY}", (1, UNDECLARED))
        expect("a base that does not resolve", (1, UNRESOLVED.format("absent")), "absent")
        marker.parent.mkdir()
        marker.touch()
        expect(f"an empty {SIGN_OFF_POLICY}", (0, ""))
        marker.write_text("any content\n", encoding="utf-8")
        expect(f"a non-empty {SIGN_OFF_POLICY}", (0, ""))
        marker.unlink()
        marker.symlink_to("missing")
        expect(f"a dangling symlink at {SIGN_OFF_POLICY}", (0, ""))
        marker.unlink()
        marker.mkdir()
        (marker / "file").touch()
        expect(f"a directory at {SIGN_OFF_POLICY}", (1, UNDECLARED))
        (root / "blob.txt").write_text("blob\n", encoding="utf-8")
        blob = git(root, "hash-object", "-w", "blob.txt").strip()
        index = {**os.environ, "GIT_INDEX_FILE": str(root / ".git" / "gitlink-index")}
        for oid, declared in ((blob, True), ("1" * len(blob), False)):
            git(
                root,
                "update-index",
                "--add",
                "--cacheinfo",
                f"160000,{oid},{SIGN_OFF_POLICY}",
                env=index,
            )
            if declares_sign_off(root, git(root, "write-tree", env=index).strip()) != declared:
                failures.append(
                    f"a gitlink to {oid} at {SIGN_OFF_POLICY} was not judged as the hub"
                )
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--base", help="the range's exclusive start")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument(
        "--pull-request",
        action="store_true",
        help="skip commits authored by known automation bots from their noreply addresses",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.base:
        parser.error("--base is required")
    root = Path.cwd()
    if not resolves(root, args.base):
        print(UNRESOLVED.format(args.base), file=sys.stderr)
        return 1
    problems = check(root, f"{args.base}..{args.head}", args.pull_request)
    if not declares_sign_off(root, snapshot(root)):
        problems.append(UNDECLARED)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
