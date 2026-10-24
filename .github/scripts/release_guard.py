"""Hold a release to ADR 0003 decisions 6 and 7: `guard` before anything is built, and
`verify-published` after the registries took the files."""

import argparse
import contextlib
import datetime as dt
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Iterator
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GITHUB = "https://api.github.com/"
USER_AGENT = "almiladoc-release"
CRATE = "almiladoc"
WORKFLOW = "release.yml"
REQUIRED_CHECK = "ci / required"
VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)(?:-rc\.(\d+))?")
FAQ = (
    "### Licensing",
    "- **What is a Competing Use?**",
    "- **What is free?**",
    "- **When does a version become MIT?**",
)
DATES = Path("docs/release-dates.tsv")
DATES_HEADER = "tag\tavailable\tmit"
ZERO = "0" * 40
REGISTRIES = {"testpypi": "https://test.pypi.org/", "pypi": "https://pypi.org/"}
CRATES = "https://crates.io/api/v1/crates/"
ATTEMPTS = 10
PUBLISHER = ("kind", "repository", "workflow", "environment")

Fetch = Callable[[str], tuple[object, str | None]]
Ancestry = Callable[[str, str], bool | None]


def workspace(root: Path) -> tuple[str, str, str]:
    package = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"][
        "package"
    ]
    toolchain = tomllib.loads((root / "rust-toolchain.toml").read_text(encoding="utf-8"))
    return package["version"], package["rust-version"], toolchain["toolchain"]["channel"]


def tree_problems(root: Path) -> list[str]:
    version, rust_version, channel = workspace(root)
    problems = []
    if not VERSION.fullmatch(version):
        problems.append(f"version {version} is neither X.Y.Z nor X.Y.Z-rc.N")
    if rust_version != ".".join(channel.split(".")[:2]):
        problems.append(f"rust-version {rust_version} is not the minor of the toolchain {channel}")
    readme = (root / "README.md").read_text(encoding="utf-8").splitlines()
    problems += [
        f"README.md lacks the licensing FAQ's {line!r}"
        for line in FAQ
        if not any(row.startswith(line) for row in readme)
    ]
    if dates(root) is None:
        problems.append(f"{DATES} does not start with the header {DATES_HEADER!r}")
    return problems


def dates(root: Path) -> set[str] | None:
    rows = (root / DATES).read_text(encoding="utf-8").splitlines()
    if rows[:1] != [DATES_HEADER]:
        return None
    return {row.split("\t", 1)[0] for row in rows[1:] if row}


def channel(tag: str, version: str) -> str | None:
    if tag != f"v{version}" or not (match := VERSION.fullmatch(version)):
        return None
    return "testpypi" if match[4] else "pypi"


def key(version: str) -> tuple[int, ...]:
    match = VERSION.fullmatch(version)
    if not match:
        return ()
    major, minor, patch, rc = match.groups()
    return int(major), int(minor), int(patch), int(rc) if rc else sys.maxsize


def pep440(version: str) -> str:
    match = VERSION.fullmatch(version)
    return f"{'.'.join(match.groups()[:3])}{f'rc{match[4]}' if match and match[4] else ''}"


def mit_day(available: dt.date) -> dt.date:
    try:
        return available.replace(year=available.year + 4)
    except ValueError:
        return dt.date(available.year + 4, 3, 1)


def pages(fetch: Fetch, url: str) -> Iterator[dict]:
    next_url: str | None = url
    while next_url:
        body, next_url = fetch(next_url)
        yield from body


def git_ancestry(root: Path) -> Ancestry:
    def ancestry(commit: str, head: str) -> bool | None:
        code = subprocess.run(
            ["git", "-C", str(root), "merge-base", "--is-ancestor", commit, head],
            capture_output=True,
        ).returncode
        return None if code not in {0, 1} else code == 0

    return ancestry


def compare(fetch: Fetch, repository: str, commit: str, head: str) -> bool:
    body, _ = fetch(f"{GITHUB}repos/{repository}/compare/{commit}...{head}")
    return body["status"] in {"identical", "ahead"}


def contains(fetch: Fetch, ancestry: Ancestry, repository: str, commit: str, head: str) -> bool:
    local = ancestry(commit, head)
    return compare(fetch, repository, commit, head) if local is None else local


def available(fetch: Fetch, ancestry: Ancestry, repository: str, commit: str) -> dt.date | None:
    earliest, known = None, dict[str, bool]()
    for entry in pages(fetch, f"{GITHUB}repos/{repository}/activity?per_page=100"):
        after = entry["after"]
        if after == ZERO:
            continue
        if after not in known:
            known[after] = contains(fetch, ancestry, repository, commit, after)
        if known[after]:
            earliest = dt.datetime.fromisoformat(entry["timestamp"])
    return earliest.date() if earliest else None


def tag_problems(
    fetch: Fetch, ancestry: Ancestry, root: Path, repository: str, tag: str
) -> tuple[list[str], str]:
    version = workspace(root)[0]
    if not channel(tag, version):
        return [f"tag {tag} is not v{version}, the workspace version"], ""
    ref = fetch(f"{GITHUB}repos/{repository}/git/ref/tags/{urllib.parse.quote(tag)}")[0]
    if ref["object"]["type"] != "tag":
        return [f"tag {tag} is lightweight, not annotated"], ""
    annotated = fetch(f"{GITHUB}repos/{repository}/git/tags/{ref['object']['sha']}")[0]
    commit, message = annotated["object"]["sha"], annotated["message"]
    problems = []
    if annotated["object"]["type"] != "commit":
        problems.append(f"tag {tag} points at a {annotated['object']['type']}, not a commit")
    if not compare(fetch, repository, commit, "main"):
        problems.append(f"tag {tag}'s commit {commit} is not on main")
    checks = fetch(
        f"{GITHUB}repos/{repository}/commits/{commit}/check-runs?filter=latest&check_name="
        + urllib.parse.quote(REQUIRED_CHECK)
    )[0]
    if not any(run["conclusion"] == "success" for run in checks["check_runs"]):
        problems.append(f"tag {tag}'s commit {commit} has no green {REQUIRED_CHECK}")
    recorded = dates(root) or set()
    for each in pages(fetch, f"{GITHUB}repos/{repository}/git/matching-refs/tags/v?per_page=100"):
        earlier = each["ref"].removeprefix("refs/tags/")
        final = channel(earlier, earlier[1:]) == "pypi"
        if final and key(earlier[1:]) < key(version) and earlier not in recorded:
            problems.append(f"{DATES} lacks the earlier final tag {earlier}")
    day = available(fetch, ancestry, repository, commit)
    if day is None:
        problems.append(f"no repository activity has made {commit} available")
        return problems, ""
    notes = (
        f"{message.rstrip()}\n\nMade available on {day.isoformat()}: its BUSL-1.1 portions "
        f"become available under the MIT License on {mit_day(day).isoformat()}.\n"
    )
    return problems, notes


def verify_problems(
    fetch: Fetch, root: Path, dist: Path, repository: str, run: tuple[str, str, str]
) -> list[str]:
    version, registry = workspace(root)[0], run[0]
    host, release = REGISTRIES[registry], pep440(version)
    local = {
        path.name: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(dist.iterdir())
        if path.name.endswith((".whl", ".tar.gz"))
    }
    files = fetch(f"{host}pypi/{CRATE}/{release}/json")[0]["urls"]
    published = {file["filename"]: file["digests"]["sha256"] for file in files}
    problems = [
        f"{registry} {name}: sha256 {published.get(name, 'absent')}, attested {digest}"
        for name, digest in local.items()
        if published.get(name) != digest
    ]
    problems += [
        f"{registry} {name}: not built by this run" for name in published.keys() - local.keys()
    ]
    for name in sorted(published.keys() & local.keys()):
        bundles = fetch(f"{host}integrity/{CRATE}/{release}/{name}/provenance")[0]
        found = [
            {field: bundle["publisher"].get(field) for field in PUBLISHER}
            for bundle in bundles["attestation_bundles"]
        ]
        if found != [publisher(repository, registry)]:
            problems.append(f"{registry} {name}: provenance publisher {found}")
    if registry == "pypi":
        problems += crate_problems(fetch, dist, repository, version, run[1:])
    return problems


def crate_problems(
    fetch: Fetch, dist: Path, repository: str, version: str, run: tuple[str, str]
) -> list[str]:
    crate = dist / f"{CRATE}-{version}.crate"
    if not crate.is_file():
        return [f"crates.io: {crate.name} is not among the attested files"]
    digest = hashlib.sha256(crate.read_bytes()).hexdigest()
    published = fetch(f"{CRATES}{CRATE}/{version}")[0]["version"]
    problems = []
    if published["checksum"] != digest:
        problems.append(
            f"crates.io {version}: checksum {published['checksum']}, attested {digest}"
        )
    versions = fetch(f"{CRATES}{CRATE}/versions")[0]["versions"]
    expected = {"provider": "github", "repository": repository, "run_id": run[0], "sha": run[1]}
    if len(versions) > 1 and published.get("trustpub_data") != expected:
        problems.append(
            f"crates.io {version}: trusted publishing {published.get('trustpub_data')}"
        )
    return problems


def publisher(repository: str, environment: str) -> dict[str, str]:
    return dict(zip(PUBLISHER, ("GitHub", repository, WORKFLOW, environment), strict=True))


def request(url: str, token: str | None) -> urllib.request.Request:
    built = urllib.request.Request(
        url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"}
    )
    if token and urllib.parse.urlsplit(url).hostname == "api.github.com":
        built.add_unredirected_header("Authorization", f"Bearer {token}")
    return built


def next_link(header: str | None) -> str | None:
    for part in (header or "").split(","):
        if match := re.fullmatch(r'\s*<([^>]+)>;\s*rel="next"\s*', part):
            return match[1]
    return None


def lagging(code: int, url: str) -> bool:
    return code == 404 and urllib.parse.urlsplit(url).hostname != "api.github.com"


def fetch(url: str) -> tuple[object, str | None]:
    attempt = 1
    while True:
        try:
            with urllib.request.urlopen(
                request(url, os.environ.get("GH_TOKEN")), timeout=60
            ) as response:
                return json.load(response), next_link(response.headers.get("Link"))
        except urllib.error.HTTPError as error:
            if not lagging(error.code, url) or attempt == ATTEMPTS:
                raise
        attempt += 1
        time.sleep(30)


def guard(args: argparse.Namespace, fetch: Fetch, root: Path) -> int:
    problems = tree_problems(root)
    target = ""
    if args.tag:
        found, notes = tag_problems(fetch, git_ancestry(root), root, args.repository, args.tag)
        problems += found
        args.notes.write_text(notes, encoding="utf-8")
        target = channel(args.tag, workspace(root)[0]) or ""
    for problem in problems:
        print(f"guard: {problem}", file=sys.stderr)
    if problems:
        return 1
    with args.output.open("a", encoding="utf-8") as output:
        output.write(f"channel={target}\n")
    print(f"guard: tree checks pass{f'; tag {args.tag} publishes to {target}' if target else ''}")
    return 0


def verify(args: argparse.Namespace, fetch: Fetch, root: Path) -> int:
    problems = verify_problems(
        fetch, root, args.dist, args.repository, (args.channel, args.run_id, args.commit)
    )
    for problem in problems:
        print(f"verify-published: {problem}", file=sys.stderr)
    return 1 if problems else 0


Expect = Callable[[bool, str], None]
REPO = "o/r"
COMMIT = "c" * 40
README = "".join(f"{line} text\n" for line in FAQ)
HEADER = DATES_HEADER + "\n"
ROWS = HEADER + "v0.1.0\t2026-10-23\t2030-10-23\n"
PROVENANCE_CLAIMS = {"claims": None}


def remote(commit: str, head: str) -> None:
    return None


def fake_tree(directory: Path, version: str, rust_version: str, readme: str, rows: str) -> Path:
    (directory / "docs").mkdir(parents=True, exist_ok=True)
    (directory / "Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{version}"\nrust-version = "{rust_version}"\n',
        encoding="utf-8",
    )
    (directory / "rust-toolchain.toml").write_text(
        '[toolchain]\nchannel = "1.99.1"\n', encoding="utf-8"
    )
    (directory / "README.md").write_text(readme, encoding="utf-8")
    (directory / DATES).write_text(rows, encoding="utf-8")
    return directory


def faking(responses: dict[str, object]) -> Fetch:
    def answer(url: str) -> tuple[object, str | None]:
        body = responses[url]
        return body if isinstance(body, tuple) else (body, None)

    return answer


def fake_github(tags: list[str], **overrides: object) -> Fetch:
    repo, tag, other, older = f"{GITHUB}repos/{REPO}/", "t" * 40, "f" * 40, "b" * 40
    responses: dict[str, object] = {
        "git/ref/tags/v0.2.0-rc.1": {"object": {"type": "tag", "sha": tag}},
        f"git/tags/{tag}": {
            "object": {"type": "commit", "sha": COMMIT},
            "message": "almiladoc 0.2.0-rc.1\n",
        },
        f"compare/{COMMIT}...main": {"status": "ahead"},
        f"commits/{COMMIT}/check-runs?filter=latest&check_name=ci%20/%20required": {
            "check_runs": [{"conclusion": "failure"}, {"conclusion": "success"}]
        },
        "git/matching-refs/tags/v?per_page=100": [{"ref": f"refs/tags/{name}"} for name in tags],
        "activity?per_page=100": [
            {"timestamp": "2028-03-01T09:00:00Z", "after": other},
            {"timestamp": "2028-03-01T08:00:00Z", "after": ZERO},
            {"timestamp": "2028-02-29T23:30:00Z", "after": COMMIT},
            {"timestamp": "2028-02-29T21:00:00Z", "after": older},
        ],
        f"compare/{COMMIT}...{other}": {"status": "ahead"},
        f"compare/{COMMIT}...{COMMIT}": {"status": "identical"},
        f"compare/{COMMIT}...{older}": {"status": "behind"},
    }
    return faking({f"{repo}{path}": body for path, body in (responses | overrides).items()})


def wheel_file(name: str, data: bytes) -> dict[str, object]:
    return {"filename": name, "digests": {"sha256": hashlib.sha256(data).hexdigest()}}


def fake_registries(dist: Path, **overrides: object) -> Fetch:
    host, name = REGISTRIES["pypi"], "almiladoc-0.2.0-py3-none-any.whl"
    (dist / name).write_bytes(b"w")
    (dist / "almiladoc-0.2.0.crate").write_bytes(b"c")
    (dist / "SHA256SUMS").write_bytes(b"ignored")
    trusted = {"provider": "github", "repository": REPO, "run_id": "7", "sha": COMMIT}
    responses: dict[str, object] = {
        f"{host}pypi/almiladoc/0.2.0/json": {"urls": [wheel_file(name, b"w")]},
        f"{host}integrity/almiladoc/0.2.0/{name}/provenance": {
            "attestation_bundles": [{"publisher": publisher(REPO, "pypi") | PROVENANCE_CLAIMS}]
        },
        f"{CRATES}almiladoc/0.2.0": {
            "version": {"checksum": hashlib.sha256(b"c").hexdigest(), "trustpub_data": trusted}
        },
        f"{CRATES}almiladoc/versions": {"versions": [{"num": "0.2.0"}, {"num": "0.1.0"}]},
    }
    return faking(responses | overrides)


def test_versions(expect: Expect) -> None:
    pairs = [
        ("v0.1.0-rc.1", "0.1.0-rc.1"),
        ("v1.2.3", "1.2.3"),
        ("v0.1.1", "0.1.0-rc.1"),
        ("0.1.0", "0.1.0"),
        ("v0.1.0-beta.1", "0.1.0-beta.1"),
    ]
    expect(
        [channel(tag, version) for tag, version in pairs]
        == ["testpypi", "pypi", None, None, None],
        "a candidate goes to TestPyPI, a final to PyPI, and a mismatched or other tag nowhere",
    )
    expect(
        [pep440(version) for version in ("0.1.0-rc.1", "1.2.3")] == ["0.1.0rc1", "1.2.3"],
        "PEP 440 versions",
    )
    expect(key("0.1.0-rc.2") < key("0.1.0") < key("0.1.1-rc.1"), "version order")
    expect(
        [mit_day(dt.date(2096, 2, 29)), mit_day(dt.date(2026, 10, 23))]
        == [dt.date(2100, 3, 1), dt.date(2030, 10, 23)],
        "the fourth anniversary",
    )


def test_trees(expect: Expect, base: Path) -> None:
    expect(tree_problems(ROOT) == [], f"the tracked tree: {tree_problems(ROOT)}")
    trees = {
        "a current tree": (("0.2.0-rc.1", "1.99", README, HEADER), []),
        "a ledger with a final's row": (("0.2.0-rc.1", "1.99", README, ROWS), []),
        "a mismatched rust-version": (
            ("0.2.0-rc.1", "1.98", README, HEADER),
            ["rust-version 1.98 is not the minor of the toolchain 1.99.1"],
        ),
        "a missing FAQ entry": (
            ("0.2.0-rc.1", "1.99", README.replace(FAQ[2], "When"), HEADER),
            [f"README.md lacks the licensing FAQ's {FAQ[2]!r}"],
        ),
        "an alpha version": (
            ("0.2.0-alpha.1", "1.99", README, HEADER),
            ["version 0.2.0-alpha.1 is neither X.Y.Z nor X.Y.Z-rc.N"],
        ),
        "a dates file without its header": (
            ("0.2.0-rc.1", "1.99", README, "v0.1.0\t2026-10-23\t2030-10-23\n"),
            [f"{DATES} does not start with the header {DATES_HEADER!r}"],
        ),
    }
    for index, (label, (fields, wanted)) in enumerate(trees.items()):
        found = tree_problems(fake_tree(base / str(index), *fields))
        expect(found == wanted, f"{label}: {found}")


def test_tags(expect: Expect, base: Path) -> None:
    tree = fake_tree(base / "tags", "0.2.0-rc.1", "1.99", README, ROWS)
    tags = ["v0.1.0-rc.1", "v0.1.0", "v0.1.1", "v0.2.0-rc.1", "v0.2.0", "vnext"]
    problems, notes = tag_problems(fake_github(tags), remote, tree, REPO, "v0.2.0-rc.1")
    expect(
        problems == [f"{DATES} lacks the earlier final tag v0.1.1"],
        f"an earlier final tag missing its dates row: {problems}",
    )
    expect(
        notes == "almiladoc 0.2.0-rc.1\n\nMade available on 2028-02-29: its BUSL-1.1 portions "
        "become available under the MIT License on 2032-02-29.\n",
        f"the notes carry the earliest containing activity's day: {notes!r}",
    )
    tagged = "tag v0.2.0-rc.1"
    cases = {
        "a green candidate": ("v0.2.0-rc.1", {}, []),
        "a mismatched tag": (
            "v0.2.1",
            {},
            ["tag v0.2.1 is not v0.2.0-rc.1, the workspace version"],
        ),
        "a lightweight tag": (
            "v0.2.0-rc.1",
            {"git/ref/tags/v0.2.0-rc.1": {"object": {"type": "commit", "sha": COMMIT}}},
            [f"{tagged} is lightweight, not annotated"],
        ),
        "a tag of a tag": (
            "v0.2.0-rc.1",
            {f"git/tags/{'t' * 40}": {"object": {"type": "tag", "sha": COMMIT}, "message": ""}},
            [f"{tagged} points at a tag, not a commit"],
        ),
        "a commit off main": (
            "v0.2.0-rc.1",
            {f"compare/{COMMIT}...main": {"status": "diverged"}},
            [f"{tagged}'s commit {COMMIT} is not on main"],
        ),
        "a red required check": (
            "v0.2.0-rc.1",
            {
                f"commits/{COMMIT}/check-runs?filter=latest&check_name=ci%20/%20required": {
                    "check_runs": [{"conclusion": "failure"}]
                }
            },
            [f"{tagged}'s commit {COMMIT} has no green ci / required"],
        ),
        "no containing activity": (
            "v0.2.0-rc.1",
            {"activity?per_page=100": [{"timestamp": "2028-03-01T09:00:00Z", "after": "b" * 40}]},
            [f"no repository activity has made {COMMIT} available"],
        ),
    }
    for label, (tag, overrides, wanted) in cases.items():
        github = fake_github(["v0.1.0", tag], **overrides)
        found, _ = tag_problems(github, remote, tree, REPO, tag)
        expect(found == wanted, f"{label}: {found}")
    off_main = fake_github(["v0.1.0"], **{f"compare/{COMMIT}...main": {"status": "diverged"}})
    found, _ = tag_problems(off_main, lambda commit, head: True, tree, REPO, "v0.2.0-rc.1")
    expect(
        found == [f"{tagged}'s commit {COMMIT} is not on main"],
        f"a local ref named main leaves the on-main verdict to compare: {found}",
    )


def test_releases(expect: Expect, base: Path) -> None:
    final = fake_tree(base / "release", "0.2.0", "1.99", README, ROWS)
    ref = {"git/ref/tags/v0.2.0": {"object": {"type": "tag", "sha": "t" * 40}}}
    tags = ["v0.1.0", "v0.2.0-rc.1", "v0.2.0"]
    problems, _ = tag_problems(fake_github(tags, **ref), remote, final, REPO, "v0.2.0")
    expect(problems == [], f"a final needs no dates row of its own: {problems}")
    activity = {
        "activity?per_page=100": (
            [{"timestamp": "2028-03-01T09:00:00Z", "after": "f" * 40}],
            f"{GITHUB}repos/{REPO}/activity?page=2",
        ),
        "activity?page=2": [
            {"timestamp": "2028-02-27T19:00:00Z", "after": COMMIT},
            {"timestamp": "2028-02-26T19:00:00Z", "after": "b" * 40},
        ],
    }
    tree = fake_tree(base / "early", "0.2.0-rc.1", "1.99", README, ROWS)
    github = fake_github(["v0.1.0"], **activity)
    _, notes = tag_problems(github, remote, tree, REPO, "v0.2.0-rc.1")
    expect(
        "Made available on 2028-02-27:" in notes,
        f"the earliest containing activity counts on a later page, whatever the commit's date: "
        f"{notes!r}",
    )


def counting(fetch: Fetch, compared: list[str]) -> Fetch:
    def answer(url: str) -> tuple[object, str | None]:
        if "/compare/" in url:
            compared.append(url.rsplit("...", 1)[1])
        return fetch(url)

    return answer


def test_containment(expect: Expect, base: Path) -> None:
    other, older = "f" * 40, "b" * 40
    activity = [
        {"timestamp": "2028-03-01T10:00:00Z", "after": older},
        {"timestamp": "2028-03-01T09:00:00Z", "after": other},
        {"timestamp": "2028-03-01T08:00:00Z", "after": older},
        {"timestamp": "2028-02-29T23:30:00Z", "after": COMMIT},
        {"timestamp": "2028-02-29T21:00:00Z", "after": other},
    ]
    compared: list[str] = []
    github = counting(fake_github([], **{"activity?per_page=100": activity}), compared)
    day = available(github, remote, REPO, COMMIT)
    expect(
        (day, compared) == (dt.date(2028, 2, 29), [older, other, COMMIT]),
        f"a repeated after is compared once: {day}, {compared}",
    )

    def local(commit: str, head: str) -> bool | None:
        return {other: False, COMMIT: True}.get(head)

    compared.clear()
    day = available(github, local, REPO, COMMIT)
    expect(
        (day, compared) == (dt.date(2028, 2, 29), [older]),
        f"a locally known after makes no compare call: {day}, {compared}",
    )
    repository = base / "git"
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": str(base),
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        **{
            f"GIT_{role}_{field}": "t"
            for role in ("AUTHOR", "COMMITTER")
            for field in ("NAME", "EMAIL")
        },
    }
    shas = []
    subprocess.run(["git", "init", "-q", str(repository)], env=env, check=True)
    for message in ("first", "second"):
        subprocess.run(
            ["git", "-C", str(repository), "commit", "-q", "--allow-empty", "-m", message],
            env=env,
            check=True,
        )
        shas.append(
            subprocess.run(
                ["git", "-C", str(repository), "rev-parse", "HEAD"],
                env=env,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
        )
    ancestry = git_ancestry(repository)
    answers = [
        ancestry(shas[0], shas[1]),
        ancestry(shas[1], shas[0]),
        ancestry(shas[0], other),
        ancestry(other, shas[1]),
    ]
    expect(
        answers == [True, False, None, None],
        f"git answers an ancestor, a descendant, and leaves an absent sha to compare: {answers}",
    )


def run_guard(base: Path, tree: Path, tag: str | None) -> tuple[int, str]:
    output = base / "output"
    output.write_text("", encoding="utf-8")
    args = argparse.Namespace(repository=REPO, tag=tag, notes=base / "notes", output=output)
    ref = {f"git/ref/tags/{tag}": {"object": {"type": "tag", "sha": "t" * 40}}}
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        code = guard(args, fake_github(["v0.1.0"], **ref), tree)
    return code, output.read_text(encoding="utf-8")


def test_commands(expect: Expect, base: Path) -> None:
    candidate = fake_tree(base / "guard-rc", "0.2.0-rc.1", "1.99", README, ROWS)
    final = fake_tree(base / "guard-final", "0.2.0", "1.99", README, ROWS)
    broken = fake_tree(base / "guard-broken", "0.2.0", "1.98", README, ROWS)
    runs = [
        run_guard(base, candidate, "v0.2.0-rc.1"),
        run_guard(base, final, "v0.2.0"),
        run_guard(base, candidate, None),
        run_guard(base, broken, None),
        run_guard(base, broken, "v0.2.0"),
    ]
    expect(
        runs
        == [
            (0, "channel=testpypi\n"),
            (0, "channel=pypi\n"),
            (0, "channel=\n"),
            (1, ""),
            (1, ""),
        ],
        f"guard's exit codes and channel lines: {runs}",
    )
    dist = base / "verify-dist"
    dist.mkdir()
    args = argparse.Namespace(
        repository=REPO, dist=dist, channel="pypi", run_id="7", commit=COMMIT
    )
    matching = fake_registries(dist)
    files = f"{REGISTRIES['pypi']}pypi/almiladoc/0.2.0/json"
    wheel = "almiladoc-0.2.0-py3-none-any.whl"
    other = fake_registries(dist, **{files: {"urls": [wheel_file(wheel, b"x")]}})
    with contextlib.redirect_stderr(io.StringIO()):
        codes = [verify(args, matching, final), verify(args, other, final)]
    expect(codes == [0, 1], f"verify-published's exit codes: {codes}")


def test_published(expect: Expect, base: Path) -> None:
    dist = base / "dist"
    dist.mkdir()
    tree = fake_tree(base / "final", "0.2.0", "1.99", README, HEADER)
    run = ("pypi", "7", COMMIT)
    host, name = REGISTRIES["pypi"], "almiladoc-0.2.0-py3-none-any.whl"
    attested = hashlib.sha256(b"w").hexdigest()
    crate = {"checksum": hashlib.sha256(b"c").hexdigest()}
    files = f"{host}pypi/almiladoc/0.2.0/json"
    cases = {
        "files that match the attested ones": ({}, []),
        "a file the registry lacks": (
            {files: {"urls": []}},
            [f"pypi {name}: sha256 absent, attested {attested}"],
        ),
        "a file of another digest": (
            {files: {"urls": [wheel_file(name, b"x")]}},
            [f"pypi {name}: sha256 {hashlib.sha256(b'x').hexdigest()}, attested {attested}"],
        ),
        "a file the run did not build": (
            {files: {"urls": [wheel_file(name, b"w"), wheel_file("almiladoc-0.2.0.tar.gz", b"")]}},
            ["pypi almiladoc-0.2.0.tar.gz: not built by this run"],
        ),
        "another environment's provenance": (
            {
                f"{host}integrity/almiladoc/0.2.0/{name}/provenance": {
                    "attestation_bundles": [
                        {"publisher": publisher(REPO, "testpypi") | PROVENANCE_CLAIMS}
                    ]
                }
            },
            [f"pypi {name}: provenance publisher {[publisher(REPO, 'testpypi')]}"],
        ),
        "another crate checksum": (
            {f"{CRATES}almiladoc/0.2.0": {"version": crate | {"checksum": "0"}}},
            [
                f"crates.io 0.2.0: checksum 0, attested {crate['checksum']}",
                "crates.io 0.2.0: trusted publishing None",
            ],
        ),
        "a second version without trusted publishing": (
            {f"{CRATES}almiladoc/0.2.0": {"version": crate}},
            ["crates.io 0.2.0: trusted publishing None"],
        ),
        "a first version, published by token": (
            {
                f"{CRATES}almiladoc/0.2.0": {"version": crate},
                f"{CRATES}almiladoc/versions": {"versions": [{"num": "0.2.0"}]},
            },
            [],
        ),
    }
    for label, (overrides, wanted) in cases.items():
        found = verify_problems(fake_registries(dist, **overrides), tree, dist, REPO, run)
        expect(found == wanted, f"{label}: {found}")
    candidate = fake_tree(base / "rc", "0.2.0-rc.1", "1.99", README, HEADER)
    rc_dist, rc_host, sdist = base / "rc-dist", REGISTRIES["testpypi"], "almiladoc-0.2.0rc1.tar.gz"
    rc_dist.mkdir()
    (rc_dist / sdist).write_bytes(b"s")
    responses = {
        f"{rc_host}pypi/almiladoc/0.2.0rc1/json": {"urls": [wheel_file(sdist, b"s")]},
        f"{rc_host}integrity/almiladoc/0.2.0rc1/{sdist}/provenance": {
            "attestation_bundles": [{"publisher": publisher(REPO, "testpypi") | PROVENANCE_CLAIMS}]
        },
    }
    found = verify_problems(faking(responses), candidate, rc_dist, REPO, ("testpypi", "7", COMMIT))
    expect(found == [], f"a candidate checks TestPyPI and no crate: {found}")


def test_transport(expect: Expect) -> None:
    sent = {
        url: request(url, "t").header_items()
        for url in (
            GITHUB + "repos",
            REGISTRIES["pypi"],
            CRATES,
            "https://api.github.com.example/",
        )
    }
    expect(
        [url for url, headers in sent.items() if ("Authorization", "Bearer t") in headers]
        == [GITHUB + "repos"],
        f"the token reaches {sent}",
    )
    expect("Authorization" not in request(GITHUB, "t").headers, "the token follows a redirect")
    expect(
        next_link('<https://a/?page=2>; rel="next", <https://a/?page=9>; rel="last"')
        == "https://a/?page=2"
        and next_link('<https://a/?page=1>; rel="prev"') is None
        and next_link(None) is None,
        "the Link header's next page",
    )
    expect(
        [lagging(404, REGISTRIES["pypi"]), lagging(404, GITHUB), lagging(500, CRATES)]
        == [True, False, False],
        "only a registry's 404 waits for the upload to show",
    )


def self_test() -> int:
    failures: list[str] = []

    def expect(holds: bool, what: str) -> None:
        if not holds:
            failures.append(what)

    test_versions(expect)
    test_transport(expect)
    with tempfile.TemporaryDirectory() as directory:
        test_trees(expect, Path(directory))
        test_tags(expect, Path(directory))
        test_containment(expect, Path(directory))
        test_published(expect, Path(directory))
        test_releases(expect, Path(directory))
        test_commands(expect, Path(directory))
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    commands = parser.add_subparsers(dest="command")
    guarding = commands.add_parser("guard")
    guarding.add_argument("--repository", required=True)
    guarding.add_argument("--tag")
    guarding.add_argument("--notes", type=Path, required=True)
    guarding.add_argument("--output", type=Path, required=True)
    verifying = commands.add_parser("verify-published")
    verifying.add_argument("--repository", required=True)
    verifying.add_argument("--channel", choices=sorted(REGISTRIES), required=True)
    verifying.add_argument("--dist", type=Path, required=True)
    verifying.add_argument("--run-id", required=True)
    verifying.add_argument("--commit", required=True)
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.command == "guard":
        return guard(args, fetch, ROOT)
    if args.command == "verify-published":
        return verify(args, fetch, ROOT)
    parser.error("give --self-test, guard or verify-published")
    return 2


if __name__ == "__main__":
    sys.exit(main())
