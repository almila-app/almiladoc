#!/usr/bin/env python3
"""Check the agent setup.

Every `CLAUDE.md` in the tree and `.claude/` are tracked and public: none of their files may be
ignored by a `.gitignore` or untracked, they stay within the budgets that
`.claude/rules/docs-style.md` states, rules are narrowly scoped, skills name scripts that exist,
and no file of theirs that reads as UTF-8 holds a machine or personal fact. The local files stay
ignored by a `.gitignore` in the tree, and none is tracked. `public_leaks` holds the patterns for
text that will be published; `/verify` applies them to the committed diff.
`--self-test` proves that each check reports the defect it exists for.
"""

import os
import re
import subprocess
import sys
import tempfile
from collections.abc import Iterable
from pathlib import Path

INSTRUCTIONS = "CLAUDE.md"
SETUP_DIR = ".claude"
LOCAL_FILES = (
    "CLAUDE.local.md",
    ".worktreeinclude",
    "AGENTS.md",
    ".mcp.json",
    ".claude/settings.local.json",
    ".claude/worktrees",
    ".claude/agent-memory-local",
)
STYLE = ".claude/rules/docs-style.md"
SKIPPED_DIRS = {"worktrees", "agent-memory-local", "__pycache__"}
HERMETIC = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
TREE_IGNORES_ONLY = ("-c", f"core.excludesFile={os.devnull}")

BUDGET_ROW = re.compile(r"^\| `([^`]+)` \| (\d+) lines \|", re.MULTILINE)
WIDTH_ROW = re.compile(r"^\| every line of the above \| (\d+) columns", re.MULTILINE)
SCRIPT_REF = re.compile(r"(?<![\w/.-])(?:\.claude/skills/[\w-]+/)?scripts/[\w.-]+\.(?:py|sh)")
CATCH_ALL = {"*", "**", "**/*", "**/*.*", "*.*"}
FEWEST_BUDGET_ROWS = 4
NOT_TICKETS = "UTF|ISO|SHA|ECMA|RFC|CP|MD|X|CVE|GHSA|RUSTSEC|PYSEC|OSV"
PATTERNS = {
    "home path": re.compile(r"/ho" r"me/|/Us" r"ers/|(?<![\w.])~" r"/|[A-Z]:\\Us" r"ers"),
    "IP address": re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
    "private IP address": re.compile(
        r"\b(?:10\.\d{1,3}|127\.\d{1,3}|192\.168|172\.(?:1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}\b"
    ),
    "e-mail address": re.compile(r"[\w.+-]+@[\w-]+(?:\.[\w-]+)*\.[A-Za-z]{2,}"),
    "host": re.compile(r"\b(?:[\w-]+\.)+(?:local|lan|internal|home\.arpa)(?!\.?[\w-])"),
    "date": re.compile(r"\b20\d\d-[01]\d-[0-3]\d\b"),
    "ticket id": re.compile(rf"\b(?!(?:{NOT_TICKETS})-)[A-Z][A-Z0-9]{{1,9}}-\d{{2,}}\b"),
    "plan id": re.compile(r"\b(?:plan|row|item)s? [A-Z]{1,2}\d{1,3}\b"),
}
SETUP_LEAKS = (
    "home path",
    "IP address",
    "e-mail address",
    "host",
    "date",
    "ticket id",
    "plan id",
)
PUBLIC_LEAKS = ("home path", "private IP address", "e-mail address", "host", "ticket id")
PUBLIC_MAIL = re.compile(r"@(?:users\.noreply\.github\.com|example\.(?:com|org|net))$")


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=True,
        env=os.environ | HERMETIC,
    ).stdout


def rel(root: Path, path: Path) -> str:
    return path.relative_to(root).as_posix()


def local(path: str) -> bool:
    return any(path == name or path.startswith(f"{name}/") for name in LOCAL_FILES)


def instruction_files(root: Path) -> list[Path]:
    listed = git(
        root,
        "ls-files",
        "-z",
        "-c",
        "-o",
        "--exclude-per-directory=.gitignore",
        "--",
        f":(glob)**/{INSTRUCTIONS}",
    )
    return [
        root / path
        for path in sorted(set(listed.split("\0")) - {""})
        if not local(path) and (root / path).is_file()
    ]


def setup_files(root: Path) -> list[Path]:
    found = [path for path in instruction_files(root) if rel(root, path).split("/")[0] != SETUP_DIR]
    base = root / SETUP_DIR
    if base.is_dir():
        found += [
            path
            for path in sorted(base.rglob("*"))
            if path.is_file()
            and not SKIPPED_DIRS & set(path.relative_to(base).parts)
            and rel(root, path) not in LOCAL_FILES
        ]
    return found


def tracked_ignored_files(root: Path) -> list[str]:
    tracked_but_ignored = git(
        root, "ls-files", "-ci", "--exclude-per-directory=.gitignore", "-z"
    ).split("\0")
    return [
        f"{path}: tracked although .gitignore ignores it" for path in tracked_but_ignored if path
    ]


def ignored(root: Path, paths: list[str]) -> set[str]:
    with tempfile.TemporaryDirectory() as no_excludes:
        git(root, "init", "-q", "--bare", "--template=", no_excludes)
        only_tree = (*TREE_IGNORES_ONLY, f"--git-dir={no_excludes}", f"--work-tree={root}")
        probe = subprocess.run(
            [
                "git",
                *only_tree,
                "-C",
                str(root),
                "check-ignore",
                "--no-index",
                "-v",
                "-z",
                "--stdin",
            ],
            input="\0".join(paths),
            capture_output=True,
            check=False,
            text=True,
            encoding="utf-8",
            env=os.environ | HERMETIC,
        )
    if probe.returncode > 1:
        raise subprocess.CalledProcessError(
            probe.returncode, probe.args, probe.stdout, probe.stderr
        )
    fields = probe.stdout.split("\0")
    return {path for _, _, pattern, path in zip(*[iter(fields)] * 4) if not pattern.startswith("!")}


def ignored_setup_files(root: Path) -> list[str]:
    paths = [rel(root, path) for path in setup_files(root)]
    return [
        f"{path}: ignored by .gitignore although the setup is tracked"
        for path in sorted(ignored(root, paths))
    ]


def untracked_setup_files(root: Path) -> list[str]:
    tracked = set(git(root, "ls-files", "-z").split("\0"))
    return [
        f"{path}: not tracked"
        for path in (rel(root, path) for path in setup_files(root))
        if path not in tracked
    ]


def unignored_local_files(root: Path) -> list[str]:
    caught = ignored(root, list(LOCAL_FILES))
    return [
        f"{path}: local, yet not ignored by the repository's .gitignore"
        for path in LOCAL_FILES
        if path not in caught
    ]


def budget_problems(root: Path) -> list[str]:
    style = root / STYLE
    if not style.is_file():
        return [f"{STYLE}: missing; it owns the budget table"]
    text = style.read_text(encoding="utf-8")
    table = {glob: int(lines) for glob, lines in BUDGET_ROW.findall(text)}
    width = WIDTH_ROW.search(text)
    if len(table) < FEWEST_BUDGET_ROWS or width is None:
        return [f"{STYLE}: budget table unreadable ({len(table)} rows, width row {bool(width)})"]
    problems, budgeted = [], set()
    setup = setup_files(root)
    for glob, ceiling in table.items():
        files = [path for path in setup if glob_regex(glob).match(rel(root, path))]
        if not files:
            problems.append(f"{STYLE}: budget row `{glob}` matches no file")
        for path in files:
            budgeted.add(rel(root, path))
            lines = path.read_text(encoding="utf-8").splitlines()
            if len(lines) > ceiling:
                problems.append(f"{rel(root, path)}: {len(lines)} lines, budget {ceiling}")
            problems += [
                f"{rel(root, path)}:{number}: {len(line)} columns, budget {width.group(1)}"
                for number, line in enumerate(lines, start=1)
                if len(line) > int(width.group(1)) and not line.lstrip().startswith("|")
            ]
    problems += [
        f"{STYLE}: {rel(root, path)} is not budgeted"
        for path in instruction_files(root)
        if rel(root, path) not in budgeted
    ]
    return problems


def frontmatter(text: str) -> dict[str, list[str]]:
    if not text.startswith("---\n"):
        return {}
    fields: dict[str, list[str]] = {}
    key = None
    for line in text[4:].split("\n---", 1)[0].splitlines():
        if match := re.match(r"^([\w-]+):\s*(.*)$", line):
            key, value = match.groups()
            fields[key] = [v.strip().strip("\"'") for v in value.split(",") if v.strip()]
        elif key and (item := re.match(r"^\s+-\s+(.+)$", line)):
            fields[key].append(item.group(1).strip().strip("\"'"))
    return fields


def glob_regex(glob: str) -> re.Pattern[str]:
    out, i = "", 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out, i = out + "(?:.*/)?", i + 3
        elif glob.startswith("**", i):
            out, i = out + ".*", i + 2
        elif glob[i] in "*?":
            out, i = out + ("[^/]*" if glob[i] == "*" else "[^/]"), i + 1
        elif glob[i] == "{":
            end = glob.index("}", i)
            options = "|".join(map(re.escape, glob[i + 1 : end].split(",")))
            out, i = out + f"(?:{options})", end + 1
        else:
            out, i = out + re.escape(glob[i]), i + 1
    return re.compile(out + r"\Z")


def scope_problems(root: Path) -> list[str]:
    universe = [p for p in git(root, "ls-files", "-z").split("\0") if p]
    universe += [rel(root, path) for path in setup_files(root)]
    problems = []
    for rule in sorted((root / SETUP_DIR / "rules").glob("**/*.md")):
        name = rel(root, rule)
        globs = frontmatter(rule.read_text(encoding="utf-8")).get("paths", [])
        if not globs:
            problems.append(f"{name}: no `paths` frontmatter; an unscoped rule loads every session")
        for glob in globs:
            if glob in CATCH_ALL:
                problems.append(f"{name}: `{glob}` scopes to everything")
            elif not any(glob_regex(glob).match(path) for path in universe):
                problems.append(f"{name}: `{glob}` matches no file")
    return problems


def manifest_problems(root: Path) -> list[str]:
    problems = []
    for skill in sorted((root / SETUP_DIR / "skills").glob("*/SKILL.md")):
        text = skill.read_text(encoding="utf-8")
        fields = frontmatter(text)
        if fields.get("name") != [skill.parent.name] or not fields.get("description"):
            problems.append(f"{rel(root, skill)}: name must be its directory, with a description")
        problems += [
            f"{rel(root, skill)}: names {ref}, which does not exist from the repository root"
            for ref in sorted(set(SCRIPT_REF.findall(text)))
            if not (root / ref).is_file()
        ]
    for agent in sorted((root / SETUP_DIR / "agents").glob("*.md")):
        fields = frontmatter(agent.read_text(encoding="utf-8"))
        if fields.get("name") != [agent.stem] or not fields.get("description"):
            problems.append(f"{rel(root, agent)}: name must be its file stem, with a description")
        if not fields.get("model"):
            problems.append(f"{rel(root, agent)}: no model chosen")
    return problems


def leaks(
    lines: Iterable[tuple[str, int, str]], kinds: Iterable[str], public: set[str]
) -> list[str]:
    found = []
    for path, number, line in lines:
        for kind in kinds:
            text_of = rust_text(line) if kind == "host" and path.endswith(".rs") else line
            for match in PATTERNS[kind].finditer(text_of):
                text = match.group(0)
                if kind == "e-mail address" and (text in public or PUBLIC_MAIL.search(text)):
                    continue
                found.append(f"{path}:{number}: {kind}: {text}")
    return found


RUST_STRING = re.compile(r'"(?:[^"\\]|\\.)*"')


def rust_text(line: str) -> str:
    """A Rust line's string literals and `//` comment; elsewhere a dotted name is a field access."""
    comment = RUST_STRING.sub(" ", line).partition("//")[2]
    return " ".join([*RUST_STRING.findall(line), comment])


def public_leaks(lines: Iterable[tuple[str, int, str]], public: set[str]) -> list[str]:
    return leaks(lines, PUBLIC_LEAKS, public)


def leak_problems(root: Path) -> list[str]:
    scanned = []
    for path in setup_files(root):
        try:
            text = path.read_bytes().decode("utf-8")
        except UnicodeDecodeError:
            continue
        scanned += [(rel(root, path), n, line) for n, line in enumerate(text.splitlines(), 1)]
    return leaks(scanned, SETUP_LEAKS, set())


CHECKS = (
    tracked_ignored_files,
    ignored_setup_files,
    untracked_setup_files,
    unignored_local_files,
    budget_problems,
    scope_problems,
    manifest_problems,
    leak_problems,
)


def check(root: Path) -> list[str]:
    return [problem for run in CHECKS for problem in run(root)]


GOOD_STYLE = """---
paths:
  - "*.md"
---

| File | Ceiling |
|---|---|
| `**/CLAUDE.md` | 5 lines |
| `.claude/rules/*.md` | 20 lines |
| `.claude/skills/*/SKILL.md` | 20 lines |
| `.claude/agents/*.md` | 20 lines |
| every line of the above | 100 columns; table rows excepted |
"""
GOOD_SKILL = "---\nname: verify\ndescription: Pick gates.\n---\n\nRun `{script}`.\n"
SCRIPT = ".claude/skills/verify/scripts/run.py"
GOOD_AGENT = "---\nname: reviewer\ndescription: Review.\nmodel: opus\n---\n\nReview it.\n"


LOCAL_IGNORE = "".join(f"{name}\n" for name in LOCAL_FILES)
AT, DOT = "@", "."
HOME = "/" + "home/someone"
PRIVATE_IP = DOT.join(("10", "1", "2", "3"))
ROW = "row" + " Q7"
XDG = "xdg"
ACCENTED = ".claude/rules/caf\u00e9 notes.md"


def seed(root: Path) -> None:
    git(root, "init", "-q")
    files = {
        ".gitignore": LOCAL_IGNORE,
        "README.md": "# demo\n",
        "CLAUDE.md": "# demo\n",
        STYLE: GOOD_STYLE,
        ".claude/skills/verify/SKILL.md": GOOD_SKILL.format(script=SCRIPT),
        SCRIPT: "",
        ".claude/agents/reviewer.md": GOOD_AGENT,
    }
    for name, text in files.items():
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(text, encoding="utf-8")
    git(root, "add", *files)


def write(name: str, text: str):
    def inject(root: Path) -> None:
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(text, encoding="utf-8")

    return inject


def self_test() -> int:
    defects = {
        "CLAUDE.local.md: tracked although .gitignore ignores it": lambda r: (
            write("CLAUDE.local.md", "notes\n")(r),
            git(r, "add", "-f", "CLAUDE.local.md"),
        ),
        ".claude/agents/reviewer.md: ignored by .gitignore although the setup is tracked": write(
            ".gitignore", LOCAL_IGNORE + ".claude\n"
        ),
        "CLAUDE.md: ignored by .gitignore although the setup is tracked": write(
            ".gitignore", LOCAL_IGNORE + "CLAUDE.md\n"
        ),
        "CLAUDE.md: not tracked": lambda r: git(r, "rm", "-q", "--cached", "CLAUDE.md"),
        ".claude/rules/new.md: not tracked": write(".claude/rules/new.md", GOOD_STYLE),
        ".claude/settings.local.json: local, yet not ignored": write(
            ".gitignore", LOCAL_IGNORE.replace(".claude/settings.local.json\n", "")
        ),
        "CLAUDE.local.md: local, yet not ignored": write(
            ".gitignore", LOCAL_IGNORE.replace("CLAUDE.local.md\n", "")
        ),
        "lines, budget 5": write("CLAUDE.md", "x\n" * 6),
        "columns, budget 100": write("CLAUDE.md", "x" * 101 + "\n"),
        "no `paths` frontmatter": write(STYLE, GOOD_STYLE.split("---\n", 2)[2]),
        "scopes to everything": write(STYLE, GOOD_STYLE.replace('"*.md"', '"**"')),
        "matches no file": write(STYLE, GOOD_STYLE.replace('"*.md"', '"docs/**"')),
        "does not exist from the repository root": write(
            ".claude/skills/verify/SKILL.md", GOOD_SKILL.format(script="scripts/run.py")
        ),
        "no model chosen": write(
            ".claude/agents/reviewer.md", GOOD_AGENT.replace("model: opus\n", "")
        ),
        "CLAUDE.md:1: home path": write("CLAUDE.md", f"see {HOME}/x\n"),
        "CLAUDE.md:1: IP address": write("CLAUDE.md", f"host {PRIVATE_IP}\n"),
        "CLAUDE.md:1: e-mail address": write("CLAUDE.md", f"ask someone{AT}mail.org\n"),
        f"CLAUDE.md:1: host: box{DOT}lan": write("CLAUDE.md", f"ssh box{DOT}lan\n"),
        "CLAUDE.md:1: date": write("CLAUDE.md", "since 2026" + "-01-31\n"),
        "CLAUDE.md:1: ticket id": write("CLAUDE.md", "closes ABC" + "-123\n"),
        f"CLAUDE.md:1: plan id: {ROW}": write("CLAUDE.md", f"see {ROW}\n"),
        f"{ACCENTED}: not tracked": write(ACCENTED, GOOD_STYLE),
        f"{ACCENTED}: ignored by .gitignore": lambda r: (
            write(ACCENTED, GOOD_STYLE)(r),
            write(".gitignore", LOCAL_IGNORE + ".claude/rules/*.md\n")(r),
        ),
        "run.py:1: home path": write(SCRIPT, f"ROOT = '{HOME}'\n"),
        "CLAUDE.local.md: local, yet not ignored by the repository's .gitignore": write(
            ".gitignore", LOCAL_IGNORE + "!CLAUDE.local.md\n"
        ),
        "CLAUDE.local.md: local, yet not ignored by the repository's": lambda r: (
            write(".gitignore", LOCAL_IGNORE.replace("CLAUDE.local.md\n", ""))(r),
            write(".git/info/exclude", "CLAUDE.local.md\n")(r),
        ),
        "AGENTS.md: local, yet not ignored by the repository's": lambda r: (
            write(".gitignore", LOCAL_IGNORE.replace("AGENTS.md\n", ""))(r),
            write(f"{XDG}/git/ignore", "AGENTS.md\n")(r),
        ),
        "data.toml:1: home path": write(".claude/skills/verify/data.toml", f"root = '{HOME}'\n"),
        "docs/CLAUDE.md:1: home path": write("docs/CLAUDE.md", f"see {HOME}/x\n"),
        "docs/CLAUDE.md: not tracked": write("docs/CLAUDE.md", "# docs\n"),
        "docs/CLAUDE.md: 6 lines, budget 5": write("docs/CLAUDE.md", "x\n" * 6),
    }
    failures = []

    def tree_with(inject) -> list[str]:
        saved = os.environ.copy()
        with tempfile.TemporaryDirectory() as tree_dir:
            tree = Path(tree_dir)
            os.environ["XDG_CONFIG_HOME"] = str(tree / XDG)
            try:
                seed(tree)
                inject(tree)
                return check(tree)
            finally:
                os.environ.clear()
                os.environ.update(saved)

    if found := tree_with(lambda r: None):
        failures.append(f"a clean setup reports {found}")
    for expected, inject in defects.items():
        found = tree_with(inject)
        if not any(expected in problem for problem in found):
            failures.append(f"`{expected}` not reported: {found}")
    benign = "UTF-16, SHA-256, ECMA-376, RUSTSEC-2000-0000, FSL-1.1-MIT, v0.15.0, E402, padding\n"
    local_facts = f"build host {HOME} at {PRIVATE_IP}\n"
    unreported = {
        "CLAUDE.md": benign,
        "CLAUDE.local.md": local_facts,
        ".claude/settings.local.json": local_facts,
        ".claude/worktrees/w/CLAUDE.md": local_facts,
    }
    for name, text in unreported.items():
        if found := tree_with(write(name, text)):
            failures.append(f"{name} wrongly reported: {found}")
    foreign = {
        ".git/info/exclude": write(".git/info/exclude", "CLAUDE.md\n.claude\n"),
        "$XDG_CONFIG_HOME/git/ignore": write(f"{XDG}/git/ignore", "CLAUDE.md\n.claude\n"),
        "an ignored target/CLAUDE.md": lambda r: (
            write(".gitignore", LOCAL_IGNORE + "/target\n")(r),
            write("target/dep/CLAUDE.md", f"see {HOME}/x\n" * 6)(r),
        ),
        "a binary setup file": lambda r: (
            (r / ".claude/icon.png").write_bytes(b"\xff\xfe" + HOME.encode()),
            git(r, "add", ".claude/icon.png"),
        ),
    }
    for name, inject in foreign.items():
        if found := tree_with(inject):
            failures.append(f"{name} wrongly reported: {found}")
    clean_line = f"x{AT}users.noreply.github.com {DOT.join('8888')} RUSTSEC-2000-0000"
    public = public_leaks([("a.rs", 1, clean_line)], set())
    field_access = f'if list{DOT}local == "list" {{ node{DOT}lan{DOT}internal() }}'
    public += public_leaks([("a.rs", 1, field_access)], set())
    lan_ip = DOT.join(("192", "168", "1", "9"))
    if public or not public_leaks([("a.rs", 1, lan_ip)], set()):
        failures.append(f"public scan misjudges: {public}")
    if not public_leaks([("a.rs", 1, f'let h = "printer{DOT}local";')], set()):
        failures.append("a host in a Rust string is not reported")
    with tempfile.TemporaryDirectory() as scratch:
        git(Path(scratch), "init", "-q")
        try:
            ignored(Path(scratch), ["../outside"])
            failures.append("a git failure reads as nothing ignored")
        except subprocess.CalledProcessError:
            pass
    cases = len(defects) + len(unreported) + len(foreign) + 4
    print("\n".join(failures) or f"self-test: {cases} cases pass")
    return 1 if failures else 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").strip())
    problems = check(root)
    print("\n".join(problems) or f"setup: {len(setup_files(root))} files clean")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
