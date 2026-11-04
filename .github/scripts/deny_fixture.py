"""Prove the policy gate refuses an advisory, a banned crate and an unknown source, and never a
licence, which a control `check licenses` shows the fixture carries."""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable
from pathlib import Path

ADVISED = ("smallvec", "0.6.0")
CHECKS = ("advisories", "bans", "licenses", "sources")
ENV = {
    key: value
    for key, value in os.environ.items()
    if key not in {"GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"}
} | {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1", "CARGO_TERM_COLOR": "always"}
BANNED = "openssl-sys"
LICENCES = {
    "fixture-copyleft": "GPL-3.0-only",
    "fixture-unlicensed": None,
    "fixture-unknown": "LicenseRef-fixture",
}


def manifest(name: str, version: str, licence: str | None, dependencies: dict[str, str]) -> str:
    package = [f'name = "{name}"', f'version = "{version}"', 'edition = "2021"', "publish = false"]
    if licence:
        package.append(f'license = "{licence}"')
    needs = [f"{key} = {value}" for key, value in dependencies.items()]
    return "\n".join(["[package]", *package, "", "[dependencies]", *needs, ""])


def crate(
    parent: Path,
    name: str,
    version: str = "0.1.0",
    licence: str | None = None,
    dependencies: dict[str, str] | None = None,
) -> Path:
    root = parent / name
    (root / "src").mkdir(parents=True)
    (root / "Cargo.toml").write_text(
        manifest(name, version, licence, dependencies or {}), encoding="utf-8"
    )
    (root / "src" / "lib.rs").write_text("", encoding="utf-8")
    return root


def paths(parent: Path, names: list[str]) -> dict[str, str]:
    return {name: f'{{ path = "{parent / name}" }}' for name in names}


def licensed(parent: Path) -> Path:
    for name, licence in LICENCES.items():
        crate(parent, name, licence=licence)
    return crate(parent, "app", dependencies=paths(parent, list(LICENCES)))


def banned(parent: Path) -> Path:
    crate(parent, BANNED)
    return crate(parent, "app", dependencies=paths(parent, [BANNED]))


def unknown_source(parent: Path) -> Path:
    source = crate(parent, "fixture-git")
    identity = {
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@example.com",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@example.com",
    }
    for argv in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "fixture"]):
        subprocess.run(
            ["git", "-C", str(source), *argv], check=True, capture_output=True, env=ENV | identity
        )
    return crate(parent, "app", dependencies={"fixture-git": f'{{ git = "{source.as_uri()}" }}'})


def advised(parent: Path) -> Path:
    name, version = ADVISED
    vendored = crate(parent / "app" / "vendor", name, version)
    (vendored / ".cargo-checksum.json").write_text('{"files":{}}', encoding="utf-8")
    app = crate(parent, "app", dependencies={name: f'"={version}"'})
    (app / ".cargo").mkdir()
    (app / ".cargo" / "config.toml").write_text(
        '[source.crates-io]\nreplace-with = "vendored"\n[source.vendored]\ndirectory = "vendor"\n',
        encoding="utf-8",
    )
    return app


REFUSED: dict[str, tuple[Callable[[Path], Path], str, str]] = {
    "a banned crate": (banned, "bans", BANNED),
    "an unknown git source": (unknown_source, "sources", "fixture-git"),
    "an advisory": (advised, "advisories", ADVISED[0]),
}


def judge(
    args: argparse.Namespace, app: Path, checks: list[str], options: tuple[str, ...] = ()
) -> tuple[int, str]:
    done = subprocess.run(
        [
            str(args.cargo_deny),
            *options,
            "--color",
            "never",
            "--locked",
            "--config",
            str(args.config),
            "check",
            *checks,
        ],
        cwd=app,
        capture_output=True,
        text=True,
        env=ENV,
    )
    return done.returncode, done.stdout + done.stderr


def lock(args: argparse.Namespace, app: Path) -> Path:
    subprocess.run([str(args.cargo), "generate-lockfile", "--quiet"], cwd=app, check=True, env=ENV)
    return app


def diagnostics(text: str) -> list[dict]:
    entries = (json.loads(line) for line in text.splitlines() if line.startswith("{"))
    return [entry["fields"] for entry in entries if entry.get("type") == "diagnostic"]


def refused(found: list[dict]) -> set[str]:
    return {
        graph["Krate"]["name"]
        for fields in found
        if fields["severity"] == "error" and fields["code"] in {"rejected", "unlicensed"}
        for graph in fields["graphs"]
    }


def licence_failures(args: argparse.Namespace, app: Path, checks: list[str]) -> list[str]:
    _, text = judge(args, app, ["licenses"], ("--format", "json"))
    found = diagnostics(text)
    unrefused = sorted(set(LICENCES) - refused(found))
    failures = [f"the licence control did not refuse {unrefused}:\n{text}"] if unrefused else []
    warnings = [
        fields
        for fields in found
        if fields["severity"] != "error"
        or not {graph["Krate"]["name"] for graph in fields["graphs"]} & set(LICENCES)
    ]
    if refused(warnings) & set(LICENCES):
        failures.append(f"the licence control counts a warning as a refusal:\n{text}")
    for selection in [checks, *([check] for check in checks)]:
        code, text = judge(args, app, selection)
        if code:
            failures.append(f"licences failed {' '.join(selection)}:\n{text}")
    return failures


def executable(value: str) -> Path:
    if (found := shutil.which(value)) is None:
        raise argparse.ArgumentTypeError(f"{value!r} is not an executable")
    return Path(found).absolute()


def check_names(value: str) -> list[str]:
    names = value.split()
    if not names or any(name not in CHECKS for name in names):
        raise argparse.ArgumentTypeError(f"{value!r} names a check outside {', '.join(CHECKS)}")
    return names


def refuses(parse: Callable[[str], object], value: str) -> bool:
    try:
        parse(value)
    except argparse.ArgumentTypeError:
        return True
    return False


def self_test() -> int:
    failures = []
    with tempfile.TemporaryDirectory() as directory:
        tool, proxy = Path(directory) / "tool", Path(directory) / "proxy"
        tool.write_text("#!/bin/sh\n", encoding="utf-8")
        tool.chmod(0o755)
        proxy.symlink_to(tool)
        if (found := executable(str(proxy))) != proxy:
            failures.append(f"a proxy resolved to {found}, so argv[0] dispatch would break")
        if not refuses(executable, str(Path(directory) / "absent")):
            failures.append("a missing executable was accepted")
    if check_names(" advisories  bans ") != ["advisories", "bans"]:
        failures.append("check names were not split on whitespace")
    failures += [
        f"checks {value!r} were accepted"
        for value in ("", "advisories --config=leak", "bans,sources", "Advisories")
        if not refuses(check_names, value)
    ]
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--cargo", type=executable, default="cargo")
    parser.add_argument("--cargo-deny", type=executable)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--checks", type=check_names)
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if None in (args.cargo_deny, args.config, args.checks):
        parser.error("--cargo-deny, --config and --checks are required without --self-test")
    args.config = args.config.resolve()
    checks = args.checks
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "licensed").mkdir()
        failures = licence_failures(args, lock(args, licensed(root / "licensed")), checks)
        for number, (label, (build, refusal, name)) in enumerate(REFUSED.items()):
            parent = root / str(number)
            parent.mkdir()
            code, text = judge(args, lock(args, build(parent)), checks)
            if not (code and f"{refusal} FAILED" in text and name in text):
                failures.append(f"{label} was not refused by {refusal}:\n{text}")
    for failure in failures:
        print(f"deny_fixture: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
