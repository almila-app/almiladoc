"""Refuse home paths, private addresses, personal e-mail addresses and tracked ignored files in
the tree and the branch name."""

import argparse
import contextlib
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path

LICENSOR_CONTACT = b"almilaapp@gmail.com"
LICENCE_TEXTS = ("LICENSE", "LICENSES/", "THIRD-PARTY-LICENSES", "ocr-local/THIRD-PARTY-LICENSES")
RESERVED_MAIL_DOMAIN = re.compile(
    rb"(?:^|\.)(?:example\.(?:com|net|org)|example|test|invalid|localhost)$", re.IGNORECASE
)
NOREPLY = re.compile(
    rb"^(?:\d+\+)?[A-Za-z0-9-]+(?:\[bot\])?@users\.noreply\.github\.com$", re.IGNORECASE
)

LOCAL_PART = re.compile(rb"[A-Za-z0-9._%+-]+")
DOMAIN = re.compile(rb"[A-Za-z0-9.-]*")
LETTERS = re.compile(rb"[A-Za-z]*")
MAIL_ORACLE = re.compile(
    rb"[A-Za-z0-9._%+-]+(?:\[bot\])?@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"
)
LINEAR_INPUTS = {
    "a long run ending in an address": (b"a" * 2**20 + b"@corp.co", 1),
    "a long run without an address": (b"a." * 2**19, 0),
    "a long run before an invalid domain": (b"a" * 2**20 + b"@corp", 0),
    "a long domain without a top-level label": (b"a@" + b"b-" * 2**19, 0),
}
LINEAR_PROBE = (
    "import runpy, sys, time; find = runpy.run_path(sys.argv[1])['findings']; "
    "data = sys.stdin.buffer.read(); start = time.perf_counter(); "
    "found = find(data, 'probe', None); print(time.perf_counter() - start, len(found))"
)
LINEAR_SECONDS = 1.0
LINEAR_TIMEOUT = 10


def domain_end(data: bytes, start: int) -> int | None:
    first, *labels = DOMAIN.match(data, start).group().split(b".")
    if not first:
        return None
    end, offset = None, start + len(first)
    for label in labels:
        if not label:
            break
        letters = LETTERS.match(label).end()
        if letters >= 2:
            end = offset + 1 + letters
        offset += 1 + len(label)
    return end


def mail_addresses(data: bytes) -> list[bytes]:
    found, cursor = [], 0
    for run in LOCAL_PART.finditer(data):
        start, at = max(run.start(), cursor), run.end()
        if data.startswith(b"[bot]@", at):
            at += len(b"[bot]")
        if start >= run.end() or not data.startswith(b"@", at):
            continue
        if (end := domain_end(data, at + 1)) is not None:
            found.append(data[start:end])
            cursor = end
    return found


OCTET = rb"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)"
SCANNERS = {
    "home path": re.compile(
        rb"(?:/home/|/Users/|[A-Za-z]:\\{1,2}Users\\{1,2})(?:[A-Za-z0-9._-]|[\x80-\xff])+"
    ).findall,
    "private address": re.compile(
        rb"(?<!\d)(?<!\d\.)(?:"
        rb"10(?:\." + OCTET + rb"){3}"
        rb"|172\.(?:1[6-9]|2\d|3[01])(?:\." + OCTET + rb"){2}"
        rb"|192\.168(?:\." + OCTET + rb"){2}"
        rb"|100\.(?:6[4-9]|[7-9]\d|1[01]\d|12[0-7])(?:\." + OCTET + rb"){2}"
        rb"|169\.254(?:\." + OCTET + rb"){2}"
        rb")(?!\.?\d)"
        rb"|(?<![0-9A-Za-z:])f[c-d][0-9a-f]{2}:[0-9a-f]{0,4}:[0-9a-f:]*(?![0-9A-Za-z:])",
        re.IGNORECASE,
    ).findall,
    "e-mail address": mail_addresses,
}


def licence_text(path: str | None) -> bool:
    return path is not None and (path in LICENCE_TEXTS or path.startswith(LICENCE_TEXTS[1]))


def allowed_mail(address: bytes, path: str | None) -> bool:
    if licence_text(path):
        return True
    domain = address.rsplit(b"@", 1)[1]
    return (
        address.lower() == LICENSOR_CONTACT
        or bool(NOREPLY.match(address))
        or bool(RESERVED_MAIL_DOMAIN.search(domain))
    )


def findings(data: bytes, where: str, path: str | None) -> list[str]:
    found = []
    for kind, find in SCANNERS.items():
        for match in sorted(set(find(data))):
            if kind == "e-mail address" and allowed_mail(match, path):
                continue
            found.append(f"{where}: {kind} {match.decode('utf-8', 'replace')!r}")
    return found


def git_env() -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    return env | {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}


def git(root: Path, *args: str) -> bytes:
    return subprocess.run(
        ["git", "-C", str(root), *args], capture_output=True, check=True, env=git_env()
    ).stdout


def tree_bytes(*paths: Path) -> dict[Path, bytes]:
    files = [file for path in paths for file in [path, *path.rglob("*")] if file.is_file()]
    return {file: file.read_bytes() for file in files}


@contextlib.contextmanager
def caller_git_decoys() -> Iterator[list[str]]:
    changed: list[str] = []
    saved = os.environ.copy()
    with tempfile.TemporaryDirectory() as directory:
        repo, index = Path(directory) / "decoy", Path(directory) / "index"
        git(Path(directory), "init", "-q", str(repo))
        (repo / "decoy.txt").write_text("decoy\n", encoding="utf-8")
        git(repo, "add", "decoy.txt")
        shutil.copyfile(repo / ".git" / "index", index)
        before = tree_bytes(repo / ".git", index)
        os.environ |= {"GIT_DIR": str(repo / ".git"), "GIT_INDEX_FILE": str(index)}
        try:
            yield changed
        finally:
            os.environ.clear()
            os.environ.update(saved)
        after = tree_bytes(repo / ".git", index)
        changed += [
            f"the caller's {file.relative_to(directory).as_posix()} changed"
            for file in sorted(before.keys() | after.keys())
            if after.get(file) != before.get(file)
        ]


def scan(root: Path, branch: str) -> list[str]:
    problems = findings(branch.encode("utf-8"), "branch name", None)
    listed = git(
        root, "ls-files", "-z", "--cached", "--others", "--exclude-per-directory=.gitignore"
    ).split(b"\0")
    for name in sorted({entry.decode("utf-8") for entry in listed if entry}):
        file = root / name
        if file.is_symlink():
            target = os.readlink(file)  # noqa: PTH115 -- Path.readlink normalises the target
            problems += findings(os.fsencode(target), name, name)
        elif file.is_file():
            problems += findings(file.read_bytes(), name, name)
    ignored = git(
        root, "ls-files", "-z", "--cached", "--ignored", "--exclude-per-directory=.gitignore"
    ).split(b"\0")
    problems += [f"{entry.decode('utf-8')}: tracked but ignored" for entry in ignored if entry]
    return problems


def self_test() -> int:
    at, dot = "@", "."
    planted = {
        "home.txt": "see /" + "home/someone/project\n",
        "users.md": "C:\\" + "Users\\someone\\doc\n",
        "mac.txt": "/" + "Users/someone/x\n",
        "ten.txt": "host " + dot.join(["10", "1", "2", "3"]) + "\n",
        "sentence.txt": "the LAN host is " + dot.join(["192", "168", "0", "7"]) + ".\n",
        "ellipsis.txt": "see..." + dot.join(["10", "1", "2", "3"]) + "\n",
        "rfc1918.txt": dot.join(["172", "20", "1", "2"]) + "\n",
        "link-local.txt": "ip=" + dot.join(["169", "254", "1", "1"]) + ".\n",
        "unicode-home.txt": "/" + "home/\u7528\u6237/project\n",
        "unicode-users.txt": "C:\\" + "Users\\\u7528\u6237\\project\n",
        "lan.txt": dot.join(["192", "168", "0", "7"]) + "\n",
        "cgnat.txt": dot.join(["100", "64", "1", "1"]) + "\n",
        "ula.txt": "fd12" + ":3456::1\n",
        "mail.txt": "write to someone" + at + "corp.co\n",
        "ignored.log": "tracked although ignored\n",
        "excluded-home.txt": "see /" + "home/someone/excluded\n",
        "global-home.txt": "see /" + "home/someone/global\n",
    }
    clean = {
        "versions.txt": "v10.3.0 1.2.3.4 10.1.2 255.255.255.255 "
        + dot.join(["10", "1", "2", "3", "4"])
        + "\n",
        "dotted-version.txt": "v1." + dot.join(["10", "1", "2", "3"]) + "\n",
        "edges.txt": dot.join(["172", "32", "0", "1"])
        + " "
        + dot.join(["172", "15", "0", "1"])
        + " "
        + dot.join(["169", "255", "0", "1"])
        + "\n",
        "noreply.txt": "1+someone"
        + at
        + "users.noreply.github.com dependabot[bot]"
        + at
        + "users.noreply.github.com\n",
        "contact.md": LICENSOR_CONTACT.decode("ascii")
        + " user"
        + at
        + "example.com x"
        + at
        + "host.test\n",
        "LICENSES/X.txt": "Copyright someone <someone" + at + "corp.co>\n",
        "THIRD-PARTY-LICENSES": "someone" + at + "corp.co\n",
        "ocr-local/THIRD-PARTY-LICENSES": "someone" + at + "corp.co\n",
        "hex.txt": "fcde12ab checksum\n",
        "upper.LOG": "a log only a case-folding config would ignore\n",
    }
    foreign_ignores = {
        "tree/.git/info/exclude": "hex.txt\nexcluded-home.txt\n",
        "xdg/git/ignore": "contact.md\nglobal-home.txt\n",
        "xdg/git/config": "[core]\n\tignoreCase = true\n",
    }
    with caller_git_decoys() as changed, tempfile.TemporaryDirectory() as directory:
        root = Path(directory) / "tree"
        root.mkdir()
        git(root, "init", "-q")
        for name, text in {**planted, **clean}.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text, encoding="utf-8")
        try:
            (root / "link.txt").symlink_to("/" + "home/someone/notes")
            planted["link.txt"] = "a dangling symlink"
            (root / "raw.txt").symlink_to("/" + "home/.")
            planted["raw.txt"] = "a symlink whose target is read unnormalised"
        except OSError:
            print(
                "self-test: this platform cannot create a symlink, so link.txt is not planted",
                file=sys.stderr,
            )
        git(root, "add", "-A", "--", ":!excluded-home.txt", ":!global-home.txt")
        (root / ".gitignore").write_text("*.log\n", encoding="utf-8")
        for name, text in foreign_ignores.items():
            (Path(directory) / name).parent.mkdir(parents=True, exist_ok=True)
            (Path(directory) / name).write_text(text, encoding="utf-8")
        os.environ["XDG_CONFIG_HOME"] = str(Path(directory) / "xdg")
        problems = scan(root, "lane/someone" + at + "corp.co")
    failures = changed + [
        f"planted {name} was not reported"
        for name in planted
        if not any(p.startswith(f"{name}:") for p in problems)
    ]
    failures += [
        f"clean {name} was reported: {p}"
        for name in clean
        for p in problems
        if p.startswith(f"{name}:")
    ]
    if not any(p.startswith("branch name:") for p in problems):
        failures.append("planted branch name was not reported")
    expected = {
        f"a{at}b{at}c.com": [f"b{at}c.com"],
        f"a{at}b.com.x{at}y.org": [f"a{at}b.com", f".x{at}y.org"],
        f"x[bot]{at}y.org": [f"x[bot]{at}y.org"],
    }
    failures += [
        f"e-mail scan of {sample!r} gave {mail_addresses(sample.encode())}"
        for sample, found in expected.items()
        if mail_addresses(sample.encode()) != [address.encode() for address in found]
    ]
    rng = random.Random(20261018)
    tokens = [
        b"a",
        b"Z",
        b"7",
        b".",
        b"-",
        b"_",
        b"%",
        b"@",
        b"[bot]",
        b"[",
        b"]",
        b" ",
        b"co",
        b"\xc3\xa9",
    ]
    samples = [b"".join(rng.choices(tokens, k=rng.randint(1, 14))) for _ in range(50_000)]
    failures += [
        f"e-mail scan of {sample!r} gave {mail_addresses(sample)}, "
        f"the pattern {MAIL_ORACLE.findall(sample)}"
        for sample in samples
        if mail_addresses(sample) != MAIL_ORACLE.findall(sample)
    ]
    for name, (data, count) in LINEAR_INPUTS.items():
        try:
            probe = subprocess.run(
                [sys.executable, "-I", "-B", "-c", LINEAR_PROBE, __file__],
                input=data,
                capture_output=True,
                check=True,
                timeout=LINEAR_TIMEOUT,
            )
        except subprocess.TimeoutExpired:
            failures.append(f"{name}: no verdict within {LINEAR_TIMEOUT} s")
            continue
        seconds, found = probe.stdout.split()
        if float(seconds) >= LINEAR_SECONDS or int(found) != count:
            failures.append(
                f"{name}: {found} findings in {float(seconds):.2f} s, "
                f"not {count} under {LINEAR_SECONDS} s"
            )
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--branch", default="")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").decode("utf-8").strip())
    branch = (
        args.branch
        or os.environ.get("GITHUB_HEAD_REF")
        or os.environ.get("GITHUB_REF_NAME")
        or git(root, "branch", "--show-current").decode("utf-8").strip()
    )
    problems = scan(root, branch)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
