"""Refuse home paths, private addresses, personal e-mail addresses and tracked ignored files in the tree and the branch name."""

import argparse
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

LICENSOR_CONTACT = b"almilaapp@gmail.com"
LICENCE_TEXTS = ("LICENSE", "LICENSES/", "THIRD-PARTY-LICENSES")
RESERVED_MAIL_DOMAIN = re.compile(rb"(?:^|\.)(?:example\.(?:com|net|org)|example|test|invalid|localhost)$", re.IGNORECASE)
NOREPLY = re.compile(rb"^(?:\d+\+)?[A-Za-z0-9-]+(?:\[bot\])?@users\.noreply\.github\.com$", re.IGNORECASE)

OCTET = rb"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)"
PATTERNS = {
    "home path": re.compile(rb"(?:/home/|/Users/|[A-Za-z]:\\{1,2}Users\\{1,2})(?:[A-Za-z0-9._-]|[\x80-\xff])+"),
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
    ),
    "e-mail address": re.compile(rb"[A-Za-z0-9._%+-]+(?:\[bot\])?@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"),
}


def allowed_mail(address: bytes, path: str | None) -> bool:
    if path is not None and (path in LICENCE_TEXTS or path.startswith(LICENCE_TEXTS[1])):
        return True
    domain = address.rsplit(b"@", 1)[1]
    return address.lower() == LICENSOR_CONTACT or bool(NOREPLY.match(address)) or bool(RESERVED_MAIL_DOMAIN.search(domain))


def findings(data: bytes, where: str, path: str | None) -> list[str]:
    found = []
    for kind, pattern in PATTERNS.items():
        for match in sorted(set(pattern.findall(data))):
            if kind == "e-mail address" and allowed_mail(match, path):
                continue
            found.append(f"{where}: {kind} {match.decode('utf-8', 'replace')!r}")
    return found


def git(root: Path, *args: str) -> bytes:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=True).stdout


def scan(root: Path, branch: str) -> list[str]:
    problems = findings(branch.encode("utf-8"), "branch name", None)
    listed = git(root, "ls-files", "-z", "--cached", "--others", "--exclude-standard").split(b"\0")
    for name in sorted({entry.decode("utf-8") for entry in listed if entry}):
        file = root / name
        if file.is_symlink():
            problems += findings(os.fsencode(os.readlink(file)), name, name)
        elif file.is_file():
            problems += findings(file.read_bytes(), name, name)
    ignored = git(root, "ls-files", "-z", "--cached", "--ignored", "--exclude-standard").split(b"\0")
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
    }
    clean = {
        "versions.txt": "v10.3.0 1.2.3.4 10.1.2 255.255.255.255 " + dot.join(["10", "1", "2", "3", "4"]) + "\n",
        "dotted-version.txt": "v1." + dot.join(["10", "1", "2", "3"]) + "\n",
        "edges.txt": dot.join(["172", "32", "0", "1"]) + " " + dot.join(["172", "15", "0", "1"]) + " " + dot.join(["169", "255", "0", "1"]) + "\n",
        "noreply.txt": "1+someone" + at + "users.noreply.github.com dependabot[bot]" + at + "users.noreply.github.com\n",
        "contact.md": LICENSOR_CONTACT.decode("ascii") + " user" + at + "example.com x" + at + "host.test\n",
        "LICENSES/X.txt": "Copyright someone <someone" + at + "corp.co>\n",
        "THIRD-PARTY-LICENSES": "someone" + at + "corp.co\n",
        "hex.txt": "fcde12ab checksum\n",
    }
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        for name, text in {**planted, **clean}.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text, encoding="utf-8")
        try:
            os.symlink("/" + "home/someone/notes", root / "link.txt")
            planted["link.txt"] = "a dangling symlink"
        except OSError:
            print("self-test: this platform cannot create a symlink, so link.txt is not planted", file=sys.stderr)
        git(root, "add", "-A")
        (root / ".gitignore").write_text("*.log\n", encoding="utf-8")
        problems = scan(root, "lane/someone" + at + "corp.co")
    failures = [f"planted {name} was not reported" for name in planted if not any(p.startswith(f"{name}:") for p in problems)]
    failures += [f"clean {name} was reported: {p}" for name in clean for p in problems if p.startswith(f"{name}:")]
    if not any(p.startswith("branch name:") for p in problems):
        failures.append("planted branch name was not reported")
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
    branch = args.branch or os.environ.get("GITHUB_HEAD_REF") or os.environ.get("GITHUB_REF_NAME") or git(root, "branch", "--show-current").decode("utf-8").strip()
    problems = scan(root, branch)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
