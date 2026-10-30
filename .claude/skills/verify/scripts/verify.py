#!/usr/bin/env python3
"""Run the smallest gates that can disprove the change on this branch."""

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SKILLS = ROOT / ".claude/skills"
sys.path[:0] = [str(SKILLS / "verify/scripts"), str(SKILLS / "add-fixture/scripts")]
sys.dont_write_bytecode = True

from check_setup import PATTERNS, public_leaks  # noqa: E402
from fixtures import insta_env, snapshots_of  # noqa: E402

POLICY_LINTS = (
    "clippy::allow_attributes",
    "clippy::allow_attributes_without_reason",
    "clippy::undocumented_unsafe_blocks",
    "clippy::too_long_first_doc_paragraph",
)
CORE_API = (
    "src/lib.rs",
    "src/error.rs",
    "src/config.rs",
    "src/converter.rs",
    "src/diagnostics.rs",
    "src/limits.rs",
    "src/model/",
)
RUST = ("src/", "tests/", "python/src/", "fuzz/fuzz_targets/")
ROOT_CARGO = ("Cargo.toml", "Cargo.lock")
CARGO = (*ROOT_CARGO, "python/Cargo.toml")
TOOLCHAIN = "rust-toolchain.toml"
FUZZ_CARGO = ("fuzz/Cargo.toml", "fuzz/Cargo.lock")
UV = ("pyproject.toml", "uv.lock")
MAKEFILE = "Makefile"
DIST = ("scripts/check_dist.py", "LICENSE", "NOTICE")
FIXTURE_INPUTS = (
    "tests/gen_fixtures.py",
    "tests/fixturegen/",
    "tests/fixture-src/",
    "tests/fixtures/",
)
PY_FIXTURE = re.compile(r'\bFIXTURES\s*/\s*"([^"]+)"\s*/\s*"([^"]+)"')
PY_FIXTURE_USE = re.compile(r"\bFIXTURES\b(?!\s*=)")
NOT_RUN = re.compile(r"^NOT RUN (\S+): (.*)$", re.MULTILINE)
HUNK = re.compile(r"^@@ -\S+ \+(\d+)(?:,(\d+))? @@")
COMMIT_VOICE = re.compile(r"(?<![^\W\d_])(claude|llm|anthropic)(?![^\W\d_])", re.IGNORECASE)
TAIL = 40


@dataclass
class Outcome:
    status: str
    detail: str = ""


def git(*args: str) -> str:
    return subprocess.run(
        ["git", "-c", "core.quotePath=false", "-C", str(ROOT), *args],
        capture_output=True,
        text=True,
        check=True,
    ).stdout


def entries(*args: str) -> list[str]:
    return [entry for entry in git(*args, "-z").split("\0") if entry]


def output(*argv: str) -> str:
    return subprocess.run(argv, cwd=ROOT, capture_output=True, text=True).stdout


@dataclass
class Change:
    base: str
    paths: list[str]
    untracked: set[str] = field(default_factory=set)
    commits: list[str] = field(default_factory=list)
    everything: bool = False

    @classmethod
    def of(cls, base: str, everything: bool) -> "Change":
        found = subprocess.run(
            ["git", "-C", str(ROOT), "merge-base", "HEAD", base], capture_output=True, text=True
        )
        if found.returncode:
            sys.exit(f"verify: no merge base with {base}; fetch it or pass --base REF")
        merge_base = found.stdout.strip()
        untracked = set(entries("ls-files", "--others", "--exclude-standard"))
        tracked = entries("diff", "--name-only", "--no-renames", merge_base)
        commits = git("rev-list", f"{merge_base}..HEAD").split()
        return cls(merge_base, sorted(set(tracked) | untracked), untracked, commits, everything)

    def touches(self, *prefixes: str, suffix: str = "") -> list[str]:
        return [
            p
            for p in self.paths
            if p.startswith(prefixes)
            and p.endswith(suffix)
            and (p.startswith("tests/") or not p.endswith(".md"))
        ]

    def added_lines(self, suffix: str = "") -> dict[str, list[tuple[int, str]]]:
        added: dict[str, list[tuple[int, str]]] = {}
        diff = git("diff", "-U0", "--no-renames", "--no-color", self.base)
        for chunk in re.split(r"^diff --git ", diff, flags=re.MULTILINE)[1:]:
            name = re.search(r"^\+\+\+ b/(.+?)\t?$", chunk, re.MULTILINE)
            if not name or not name.group(1).endswith(suffix):
                continue
            lines = added.setdefault(name.group(1), [])
            for hunk in re.split(r"^(?=@@ )", chunk, flags=re.MULTILINE)[1:]:
                start = int(HUNK.match(hunk).group(1))
                body = [line[1:] for line in hunk.splitlines()[1:] if line.startswith("+")]
                lines += [(start + offset, text) for offset, text in enumerate(body)]
        for path in sorted(self.untracked):
            if not path.endswith(suffix) or not (ROOT / path).is_file():
                continue
            try:
                text = (ROOT / path).read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            added[path] = list(enumerate(text.splitlines(), start=1))
        return added


def tail(text: str) -> str:
    return "\n".join(text.splitlines()[-TAIL:])


def run(*argv: str) -> Outcome:
    print(f"$ {' '.join(argv)}", flush=True)
    try:
        done = subprocess.run(argv, cwd=ROOT, capture_output=True, text=True)
    except FileNotFoundError:
        return Outcome("NOT RUN", f"`{argv[0]}` not installed")
    if done.returncode:
        return Outcome("FAIL", tail(done.stdout + done.stderr))
    return Outcome("PASS")


def combine(*outcomes: Outcome) -> Outcome:
    statuses = {outcome.status for outcome in outcomes}
    status = "FAIL" if "FAIL" in statuses else "NOT RUN" if "NOT RUN" in statuses else "PASS"
    return Outcome(status, "\n".join(o.detail for o in outcomes if o.detail))


def chain(*steps: Callable[[], Outcome]) -> Outcome:
    done = []
    for step in steps:
        done.append(step())
        if done[-1].status != "PASS":
            break
    return combine(*done)


def needs(*tools: str) -> Outcome | None:
    absent = [tool for tool in tools if not shutil.which(tool)]
    return Outcome("NOT RUN", f"not installed: {', '.join(absent)}") if absent else None


def make_outcome(targets: tuple[str, ...], returncode: int, text: str) -> Outcome:
    if returncode:
        return Outcome("FAIL", tail(text))
    if skipped := NOT_RUN.findall(text):
        return Outcome("NOT RUN", "\n".join(f"{target}: {why}" for target, why in skipped))
    own = tuple(f"{name}: " for name in targets)
    notes = [line for line in text.splitlines() if line.startswith(own)]
    return Outcome("PASS", "\n".join(notes))


def make(*targets: str, tools: tuple[str, ...] = ()) -> Outcome:
    if absent := needs("make", *tools):
        return absent
    argv = ["make", "-s", "--no-print-directory", *targets]
    print(f"$ {' '.join(argv)}", flush=True)
    done = subprocess.run(argv, cwd=ROOT, env=insta_env(), capture_output=True, text=True)
    return make_outcome(targets, done.returncode, done.stdout + done.stderr)


def target(*targets: str, cargo: bool = False) -> Callable[["Change"], Outcome]:
    return lambda _: make(*targets, tools=("cargo",) if cargo else ())


def policy_lints(change: Change) -> Outcome:
    flags = [flag for lint in POLICY_LINTS for flag in ("-W", lint)]
    argv = ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--all-features"]
    print(f"$ {' '.join(argv)} --message-format=json -- {' '.join(flags)}", flush=True)
    done = subprocess.run(
        [*argv, "--message-format=json", "--", *flags],
        cwd=ROOT,
        env=insta_env(),
        capture_output=True,
        text=True,
    )
    changed = {path: {n for n, _ in lines} for path, lines in change.added_lines(".rs").items()}
    findings: dict[tuple[str, int, str], str] = {}
    for raw in done.stdout.splitlines():
        message = json.loads(raw)
        body = message.get("message") or {}
        if message.get("reason") != "compiler-message" or not body.get("spans"):
            continue
        if body["level"] not in ("warning", "error"):
            continue
        span = next((s for s in body["spans"] if s["is_primary"]), body["spans"][0])
        code = (body.get("code") or {}).get("code") or body["level"]
        if code in POLICY_LINTS and span["line_start"] not in changed.get(span["file_name"], ()):
            continue
        findings[(span["file_name"], span["line_start"], code)] = body["message"]
    report = [f"{file}:{line}: {code}\n  {text}" for (file, line, code), text in findings.items()]
    if done.returncode and not report:
        report.append(tail(done.stderr))
    return Outcome("FAIL", "\n".join(report)) if report else Outcome("PASS")


def rust(change: Change) -> Outcome:
    if absent := needs("cargo"):
        return absent
    return chain(lambda: make("fmt", "clippy"), lambda: policy_lints(change))


def fixtures(_: Change) -> Outcome:
    return run(sys.executable, str(SKILLS / "add-fixture/scripts/fixtures.py"), "check")


def mirrors(_: Change) -> Outcome:
    return run(sys.executable, str(SKILLS / "change-core-api/scripts/mirror_drift.py"))


def published_mail(change: Change) -> set[str]:
    published = output("git", "grep", "-h", "-I", "-e", "@", change.base)
    return set(PATTERNS["e-mail address"].findall(published))


def public(change: Change) -> Outcome:
    lines = [
        (path, number, text)
        for path, added in change.added_lines().items()
        for number, text in added
    ]
    leaks = public_leaks(lines, published_mail(change))
    return Outcome("FAIL", "\n".join(leaks)) if leaks else Outcome("PASS")


def commits(change: Change) -> Outcome:
    log = git("log", "--format=%h %s%n%b%x1e", f"{change.base}..HEAD")
    known = published_mail(change)
    wrong = []
    messages = [entry.strip() for entry in log.split("\x1e") if entry.strip()]
    for message in messages:
        subject = message.splitlines()[0]
        if voice := COMMIT_VOICE.search(message):
            wrong.append(f"{subject}: names `{voice.group(0)}`; commits read as the author's")
        lines = [(subject, n, text) for n, text in enumerate(message.splitlines(), 1)]
        wrong += public_leaks(lines, known)
    own = Outcome("FAIL", "\n".join(wrong)) if wrong else Outcome("PASS")
    counted = Outcome("PASS", f"commits: {len(messages)} after the base")
    return combine(make("dco"), own, counted)


def python_inputs(root: Path = ROOT) -> tuple[str, ...]:
    opened: set[str] = set()
    for test in sorted((root / "python/tests").glob("*.py")):
        text = test.read_text(encoding="utf-8")
        found = PY_FIXTURE.findall(text)
        if len(found) != len(PY_FIXTURE_USE.findall(text)):
            return ("tests/fixtures/", "tests/snapshots/")
        opened |= {f"tests/fixtures/{folder}/{name}" for folder, name in found}
    snapshots = (
        f"tests/snapshots/{snapshot}"
        for fixture in sorted(opened)
        for snapshot in sorted(snapshots_of(fixture))
    )
    return (*sorted(opened), *snapshots)


Trigger = Callable[[Change], list[str]]


def gates(root: Path = ROOT) -> list[tuple[str, Callable[[Change], Outcome], Trigger]]:
    opened = python_inputs(root)
    return [
        ("setup", target("setup"), lambda c: ["(always)"]),
        (
            "rust",
            rust,
            lambda c: (
                c.touches(*RUST, suffix=".rs")
                + c.touches(*CARGO, TOOLCHAIN, "rustfmt.toml", MAKEFILE)
            ),
        ),
        (
            "test",
            target("test", cargo=True),
            lambda c: c.touches("src/", "tests/", *CARGO, TOOLCHAIN, MAKEFILE),
        ),
        (
            "doc",
            target("doc", cargo=True),
            lambda c: (
                c.touches("src/", "python/src/", suffix=".rs")
                + c.touches(*CARGO, TOOLCHAIN, MAKEFILE)
            ),
        ),
        (
            "features",
            target("features", cargo=True),
            lambda c: (
                c.touches("src/", "tests/", "python/src/", suffix=".rs")
                + c.touches(*CARGO, TOOLCHAIN, MAKEFILE)
            ),
        ),
        ("fixtures", fixtures, lambda c: c.touches(*FIXTURE_INPUTS)),
        ("mirrors", mirrors, lambda c: c.touches(*CORE_API, "python/")),
        (
            "python",
            target("python", cargo=True),
            lambda c: c.touches(
                "python/",
                *CORE_API,
                *opened,
                *UV,
                *DIST,
                "LICENSES/",
                *ROOT_CARGO,
                TOOLCHAIN,
                MAKEFILE,
            ),
        ),
        (
            "fuzz",
            target("fuzz", cargo=True),
            lambda c: c.touches(
                "fuzz/", ".github/scripts/fuzz_campaign.py", *CORE_API, *ROOT_CARGO, MAKEFILE
            ),
        ),
        (
            "policy",
            target("policy", cargo=True),
            lambda c: c.touches(*CARGO, *FUZZ_CARGO, "deny.toml", "deny-offline.toml", MAKEFILE),
        ),
        (
            "locks",
            target("locks", cargo=True),
            lambda c: c.touches(*CARGO, *FUZZ_CARGO, ".github/scripts/locks.py", *UV, MAKEFILE),
        ),
        (
            "licences",
            target("licences", cargo=True),
            lambda c: c.touches(
                *CARGO, "about.toml", "about.hbs", "THIRD-PARTY-LICENSES", MAKEFILE
            ),
        ),
        ("reuse", target("reuse"), lambda c: c.paths),
        ("hygiene", target("hygiene"), lambda c: c.paths),
        ("actions", target("actions"), lambda c: c.touches(".github/", *UV, MAKEFILE)),
        ("public", public, lambda c: c.paths),
        (
            "commits",
            commits,
            lambda c: (
                c.commits
                + c.touches(
                    ".github/scripts/dco.py", "REUSE.toml", "LICENSES/MIT.txt", *UV, MAKEFILE
                )
            ),
        ),
    ]


def plan(change: Change, root: Path = ROOT) -> list[tuple[str, Callable, list[str]]]:
    return [
        (name, gate, reasons or (["--all"] if change.everything else []))
        for name, gate, trigger in gates(root)
        for reasons in [trigger(change)]
    ]


def selected_by(paths: list[str], root: Path, commits: list[str] = ()) -> set[str]:
    change = Change("base", paths, commits=list(commits))
    return {name for name, _, why in plan(change, root) if why}


def self_test() -> int:
    make_gates = {"test", "doc", "features", "python", "fuzz", "policy", "locks", "licences"}
    every_path = {"setup", "reuse", "hygiene", "public"}
    cases = {
        "python/src/lib.rs": every_path | {"rust", "doc", "features", "mirrors", "python"},
        "src/model/block.rs": every_path
        | {"rust", "test", "doc", "features", "mirrors", "python", "fuzz"},
        "src/limits.rs": every_path
        | {"rust", "test", "doc", "features", "mirrors", "python", "fuzz"},
        "src/formats/rtf/mod.rs": every_path | {"rust", "test", "doc", "features"},
        "tests/fixture-src/book.md": every_path | {"test", "fixtures"},
        "tests/fixtures/docx/handmade-rich.docx": every_path | {"test", "fixtures", "python"},
        "tests/fixtures/docx/other.docx": every_path | {"test", "fixtures"},
        "tests/snapshots/snapshots__docx__handmade-rich.docx.snap": every_path
        | {"test", "python"},
        "tests/snapshots/snapshots__docx__handmade-rich.docx.diagnostics.snap": every_path
        | {"test", "python"},
        "tests/snapshots/snapshots__docx__other.docx.snap": every_path | {"test"},
        "tests/snapshots/snapshots__docx__other.docx.diagnostics.snap": every_path | {"test"},
        "fuzz/fuzz_targets/xlsx.rs": every_path | {"rust", "fuzz"},
        "fuzz/Cargo.lock": every_path | {"fuzz", "policy", "locks"},
        "Cargo.lock": every_path | make_gates | {"rust"},
        "uv.lock": every_path | {"python", "locks", "actions", "commits"},
        "Makefile": every_path | make_gates | {"rust", "actions", "commits"},
        ".github/workflows/ci.yml": every_path | {"actions"},
        ".github/scripts/dco.py": every_path | {"actions", "commits"},
        "REUSE.toml": every_path | {"commits"},
        "LICENSES/MIT.txt": every_path | {"python", "commits"},
        ".github/scripts/fuzz_campaign.py": every_path | {"actions", "fuzz"},
        "THIRD-PARTY-LICENSES": every_path | {"licences"},
        "docs/adr/0001-architecture.md": every_path,
        "AGENTS.md": every_path,
        "CLAUDE.md": every_path,
        ".claude/skills/verify/scripts/check_setup.py": every_path,
    }
    failures = []
    with tempfile.TemporaryDirectory() as tree_dir:
        tree = Path(tree_dir)
        (tree / "python/tests").mkdir(parents=True)
        suite = tree / "python/tests/test_x.py"
        suite.write_text(
            'FIXTURES = Path("t")\nRICH = FIXTURES / "docx" / "handmade-rich.docx"\n',
            encoding="utf-8",
        )
        for path, expected in cases.items():
            if (found := selected_by([path], tree)) != expected:
                failures.append(f"{path}: selects {sorted(found)}, expected {sorted(expected)}")
        if selected_by([], tree, commits=["abc"]) != {"setup", "commits"}:
            failures.append("a commit alone does not select exactly setup and commits")
        suite.write_text(suite.read_text() + "ALL = sorted(FIXTURES.rglob('*'))\n")
        if "python" not in selected_by(["tests/fixtures/docx/other.docx"], tree):
            failures.append("an unparsed use of FIXTURES does not widen the python gate")
    readings = {
        "a failed target": (("fuzz",), 2, "error: boom\n", "FAIL"),
        "a skipped target": (("policy",), 0, "NOT RUN policy: no pinned cargo-deny\n", "NOT RUN"),
        "a passing target": (("actions",), 0, "actions: offline, GH_TOKEN unset\n", "PASS"),
        "an echoed recipe": (("python",), 0, '  then echo "NOT RUN python: uv"\n', "PASS"),
    }
    for label, (targets, code, text, expected) in readings.items():
        if (found := make_outcome(targets, code, text)).status != expected:
            failures.append(f"{label} reads as {found.status}, expected {expected}")
    if make_outcome(("actions",), 0, readings["a passing target"][2]).detail == "":
        failures.append("a target's own note is dropped from a pass")
    if combine(Outcome("PASS"), Outcome("NOT RUN", "x"), Outcome("FAIL", "y")).status != "FAIL":
        failures.append("combine lets a failure through")
    print("\n".join(failures) or f"self-test: {len(cases) + len(readings) + 4} cases pass")
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    change = Change.of(args.base, args.all)
    planned = plan(change)
    selected = [(name, gate, why) for name, gate, why in planned if why]
    print(f"base {change.base[:12]}, {len(change.paths)} changed paths")
    for name, _, why in selected:
        print(f"  {name:<10} {', '.join(why[:3])}{' ...' if len(why) > 3 else ''}")
    if args.dry_run:
        return 0
    results = [(name, gate(change)) for name, gate, _ in selected]
    print()
    for name, outcome in results:
        print(f"{outcome.status:<8} {name}")
        if outcome.detail:
            print("\n".join(f"         {line}" for line in outcome.detail.splitlines()))
    if skipped := [name for name, _, why in planned if not why]:
        print(f"not selected: {', '.join(skipped)}")
    statuses = {outcome.status for _, outcome in results}
    return 1 if "FAIL" in statuses else 3 if "NOT RUN" in statuses else 0


if __name__ == "__main__":
    sys.exit(main())
