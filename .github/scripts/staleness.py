"""Report each pin Dependabot cannot reach that is behind its newest stable release at least seven days old."""

import argparse
import datetime as dt
import hashlib
import io
import json
import os
import re
import sys
import tarfile
import tempfile
import tomllib
import urllib.parse
import urllib.request
from pathlib import Path
from typing import NamedTuple

ROOT = Path(__file__).resolve().parents[2]
COOLDOWN = dt.timedelta(days=7)
USER_AGENT = "almiladoc-research"
GITHUB = "https://api.github.com/"
UV_REPOSITORY = "astral-sh/uv"
PYTHON_CYCLE = "https://peps.python.org/api/release-cycle.json"
OSV = "https://api.osv.dev/v1/vulns/"
CRATES = "https://crates.io/api/v1/crates/"
CRATE_FILES = "https://static.crates.io/crates/"
SUPPORTED = {"bugfix", "security"}
STABLE_TAG = re.compile(r"v?(\d+(?:\.\d+)*)")
LABEL = re.compile(r"(ubuntu|macos|windows)-(\d+(?:\.\d+)?)(-[a-z0-9-]+)?")
TOOL_URL = re.compile(r"https://github\.com/([\w.-]+/[\w.-]+)/releases/download/(v?)\$\((\w+)_VERSION\)/([^,\s]+)")


class Tool(NamedTuple):
    prefix: str
    repository: str
    version: str
    sums: dict[str, str]
    asset: str
    musl: str
    tag_prefix: str

    @property
    def tag(self) -> str:
        return self.tag_prefix + self.version

    def asset_name(self, version: str, arch: str) -> str:
        return self.asset.replace(f"$({self.prefix}_VERSION)", version).replace("$(MUSL)", self.musl.replace("$(HOST_ARCH)", arch))


def version(text: str) -> tuple[int, ...]:
    return tuple(int(part) for part in text.split("."))


def makefile(root: Path) -> tuple[str, dict[str, str]]:
    text = (root / "Makefile").read_text(encoding="utf-8")
    return text, dict(re.findall(r"(?m)^(\w+)\s*[:?]?=\s*(.*?)\s*$", text))


def workflow_lines(root: Path) -> list[tuple[str, str]]:
    return [
        (f"{path.relative_to(root).as_posix()}:{number}", line)
        for path in sorted((root / ".github" / "workflows").glob("*.yml"))
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1)
    ]


def runner_labels(root: Path) -> dict[str, list[str]]:
    labels: dict[str, list[str]] = {}
    for site, line in workflow_lines(root):
        if match := re.match(r"\s*runs-on:\s*['\"]?([^'\"\s#]+)", line):
            labels.setdefault(match[1], []).append(site)
    return labels


def tools(root: Path) -> list[Tool]:
    text, values = makefile(root)
    found = []
    for repository, tag_prefix, prefix, asset in TOOL_URL.findall(text):
        sums = {key.removeprefix(f"{prefix}_SHA256_"): value for key, value in values.items() if key.startswith(f"{prefix}_SHA256_")}
        found.append(Tool(prefix, repository, values[f"{prefix}_VERSION"], sums, asset, values["MUSL"], tag_prefix))
    return found


def uv_pins(root: Path) -> dict[str, str]:
    pins = {"Makefile UV_VERSION": makefile(root)[1]["UV_VERSION"]}
    lines = workflow_lines(root)
    for index, (site, line) in enumerate(lines):
        if not re.match(r"\s*(?:- )?uses:\s*astral-sh/setup-uv@", line):
            continue
        column, pins[site] = line.index("uses:"), ""
        for _, following in lines[index + 1 :]:
            if following.strip() and len(following) - len(following.lstrip(" ")) < column:
                break
            if match := re.match(r"\s+version:\s*['\"]?([^'\"\s#]+)", following):
                pins[site] = match[1]
    return pins


def pythons(root: Path) -> tuple[list[str], str]:
    requires = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))["project"]["requires-python"]
    floor = re.fullmatch(r">=\s*(\d+\.\d+)", requires)
    return makefile(root)[1]["PYTHONS"].split(), floor[1] if floor else requires


def advisory_ignores(root: Path) -> list[str]:
    entries = tomllib.loads((root / "deny.toml").read_text(encoding="utf-8"))["advisories"].get("ignore", [])
    return [entry if isinstance(entry, str) else entry["id"] for entry in entries]


def clarifications(root: Path) -> dict[str, dict[str, str]]:
    config = tomllib.loads((root / "about.toml").read_text(encoding="utf-8"))
    return {crate: {file["path"]: file["checksum"] for file in entry["clarify"].get("files", [])} for crate, entry in config.items() if isinstance(entry, dict) and "clarify" in entry}


def locked_versions(root: Path) -> dict[str, str]:
    return {package["name"]: package["version"] for package in tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))["package"]}


def locked_crates(root: Path) -> set[str]:
    return {package["name"] for lock in ("Cargo.lock", "fuzz/Cargo.lock") for package in tomllib.loads((root / lock).read_text(encoding="utf-8"))["package"]}


def consistency(root: Path) -> list[str]:
    found = [f"{site}: uv {pin or 'unpinned'} differs from UV_VERSION" for site, pin in uv_pins(root).items() if pin != makefile(root)[1]["UV_VERSION"]]
    listed, floor = pythons(root)
    if floor != min(listed, key=version):
        found.append(f"requires-python floor {floor} differs from the oldest PYTHONS entry {min(listed, key=version)}")
    return found


def newest_stable(releases: list[dict], now: dt.datetime) -> dict | None:
    eligible = [
        release
        for release in releases
        if not release["draft"] and not release["prerelease"] and STABLE_TAG.fullmatch(release["tag_name"]) and dt.datetime.fromisoformat(release["published_at"]) <= now - COOLDOWN
    ]
    return max(eligible, key=lambda release: version(STABLE_TAG.fullmatch(release["tag_name"])[1]), default=None)


def newer(pinned: str, releases: list[dict], now: dt.datetime) -> dict | None:
    newest = newest_stable(releases, now)
    return newest if newest and version(STABLE_TAG.fullmatch(newest["tag_name"])[1]) > version(pinned) else None


def crate_releases(rows: list[dict]) -> list[dict]:
    return [{"tag_name": row["num"], "published_at": row["created_at"], "draft": False, "prerelease": False} for row in rows if not row["yanked"]]


def ga_labels(readme: str) -> set[str]:
    section = readme.split("## Available Images", 1)[1].split("\n## ", 1)[0]
    rows = [row.split("|") for row in section.splitlines()]
    labels = {label for cells in rows if len(cells) > 4 and "[![" not in cells[1] for label in re.findall(r"`([^`]+)`", cells[3])}
    if not labels:
        raise ValueError("the runner-images README lists no generally available label")
    return labels


def label_findings(pinned: dict[str, list[str]], ga: set[str]) -> list[str]:
    found = []
    for label, sites in sorted(pinned.items()):
        if not (match := LABEL.fullmatch(label)):
            continue
        family = [(version(other[2]), other[0]) for other in map(LABEL.fullmatch, sorted(ga)) if other and (other[1], other[3]) == (match[1], match[3])]
        if family and max(family)[0] > version(match[2]):
            found.append(f"runner {label} ({', '.join(sites)}): {max(family)[1]} is generally available")
    return found


def tool_findings(tool: Tool, releases: list[dict], pinned: dict, now: dt.datetime) -> list[str]:
    def digests(release: dict) -> dict[str, str]:
        return {asset["name"]: (asset.get("digest") or "").removeprefix("sha256:") for asset in release["assets"]}

    found = [
        f"{tool.prefix}_SHA256_{arch} {tool.version}: the release's sha256 is {digest or 'unpublished'}"
        for arch, digest in ((arch, digests(pinned).get(tool.asset_name(tool.version, arch))) for arch in tool.sums)
        if digest != tool.sums[arch]
    ]
    if newest := newer(tool.version, releases, now):
        release = STABLE_TAG.fullmatch(newest["tag_name"])[1]
        sums = ", ".join(f"{arch} {digests(newest).get(tool.asset_name(release, arch)) or 'no digest'}" for arch in sorted(tool.sums))
        found.append(f"{tool.prefix}_VERSION {tool.version}: {release} ({sums})")
    return found


def python_findings(cycle: dict[str, dict], listed: list[str], floor: str, now: dt.datetime) -> list[str]:
    supported = [series for series, row in cycle.items() if row["status"] in SUPPORTED]
    final = [series for series in supported if dt.date.fromisoformat(cycle[series]["first_release"]) <= (now - COOLDOWN).date()]
    newest, oldest = max(final, key=version), min(supported, key=version)
    found = []
    if version(max(listed, key=version)) < version(newest):
        found.append(f"PYTHONS {' '.join(listed)}: CPython {newest} is final")
    if version(min(listed, key=version)) < version(oldest):
        found.append(f"PYTHONS {' '.join(listed)}: the oldest supported CPython is {oldest}")
    if version(floor) < version(oldest):
        found.append(f"requires-python >={floor}: the oldest supported CPython is {oldest}")
    return found


def advisory_findings(ident: str, osv: dict, locked: set[str]) -> list[str]:
    if osv.get("withdrawn"):
        return [f"deny.toml ignores {ident}: withdrawn"]
    found = []
    for affected in osv["affected"]:
        crate = affected["package"]["name"]
        fixed = [event["fixed"] for span in affected.get("ranges", []) for event in span["events"] if "fixed" in event]
        if crate not in locked:
            found.append(f"deny.toml ignores {ident}: {crate} is no longer locked")
        elif fixed:
            found.append(f"deny.toml ignores {ident}: {crate} is fixed in {', '.join(fixed)}")
    return found


def clarify_findings(crate: str, release: str, pinned: dict[str, str], archive: bytes) -> list[str]:
    found = []
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as crate_file:
        names = set(crate_file.getnames())
        for path, checksum in sorted(pinned.items()):
            member = f"{crate}-{release}/{path}"
            digest = hashlib.sha256(crate_file.extractfile(member).read()).hexdigest() if member in names else "absent"
            if digest != checksum:
                found.append(f"about.toml {crate} {path}: {release} has sha256 {digest}")
    return found


def request(url: str, accept: str, token: str | None) -> urllib.request.Request:
    built = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": accept})
    if token and urllib.parse.urlsplit(url).hostname == "api.github.com":
        built.add_unredirected_header("Authorization", f"Bearer {token}")
    return built


def fetch(url: str, accept: str = "application/json") -> bytes:
    with urllib.request.urlopen(request(url, accept, os.environ.get("GH_TOKEN")), timeout=60) as response:
        return response.read()


def github(path: str) -> list | dict:
    return json.loads(fetch(GITHUB + path))


def report(root: Path, now: dt.datetime) -> list[str]:
    found = consistency(root)
    until = (now - COOLDOWN).strftime("%Y-%m-%dT%H:%M:%SZ")
    commit = github(f"repos/actions/runner-images/commits?path=README.md&until={until}&per_page=1")[0]["sha"]
    readme = fetch(f"{GITHUB}repos/actions/runner-images/contents/README.md?ref={commit}", "application/vnd.github.raw+json").decode("utf-8")
    found += label_findings(runner_labels(root), ga_labels(readme))
    for tool in tools(root):
        found += tool_findings(tool, github(f"repos/{tool.repository}/releases?per_page=100"), github(f"repos/{tool.repository}/releases/tags/{tool.tag}"), now)
    pinned_uv = makefile(root)[1]["UV_VERSION"]
    if uv := newer(pinned_uv, github(f"repos/{UV_REPOSITORY}/releases?per_page=100"), now):
        found.append(f"UV_VERSION {pinned_uv}: {uv['tag_name']}")
    found += python_findings(json.loads(fetch(PYTHON_CYCLE)), *pythons(root), now)
    versions = locked_versions(root)
    for crate, pinned in clarifications(root).items():
        upcoming = newer(versions[crate], crate_releases(json.loads(fetch(f"{CRATES}{crate}/versions"))["versions"]), now)
        for release in dict.fromkeys([versions[crate], *([upcoming["tag_name"]] if upcoming else [])]):
            found += clarify_findings(crate, release, pinned, fetch(f"{CRATE_FILES}{crate}/{crate}-{release}.crate", "application/gzip"))
    locked = locked_crates(root)
    for ident in advisory_ignores(root):
        found += advisory_findings(ident, json.loads(fetch(OSV + ident)), locked)
    return found


def self_test() -> int:
    failures: list[str] = []

    def expect(holds: bool, what: str) -> None:
        if not holds:
            failures.append(what)

    workflows = "".join(path.read_text(encoding="utf-8") for path in (ROOT / ".github" / "workflows").glob("*.yml"))
    text, _ = makefile(ROOT)
    labels = runner_labels(ROOT)
    expect(sum(map(len, labels.values())) == workflows.count("runs-on:") > 0 and all(map(LABEL.fullmatch, labels)), f"runner labels {labels}")
    found = tools(ROOT)
    expect(len(found) == text.count("/releases/download/") > 0, f"tools {found}")
    for tool in found:
        expect(re.fullmatch(r"\d+(?:\.\d+)+", tool.version) is not None and "x86_64" in tool.sums, f"{tool.prefix} pin {tool}")
        expect(all(re.fullmatch(r"[0-9a-f]{64}", digest) for digest in tool.sums.values()), f"{tool.prefix} checksums {tool.sums}")
        expect(tool.asset_name(tool.version, "x86_64").endswith((f"{tool.version}-x86_64-unknown-linux-musl.tar.gz", f"{tool.tag}-linux-amd64.tar.gz")), f"{tool.prefix} asset {tool.asset}")
    pins = uv_pins(ROOT)
    expect(len(pins) == 1 + workflows.count("astral-sh/setup-uv@") and all(map(STABLE_TAG.fullmatch, pins.values())), f"uv pins {pins}")
    listed, floor = pythons(ROOT)
    expect(len(listed) > 1 and all(re.fullmatch(r"3\.\d+", series) for series in [*listed, floor]), f"pythons {listed} {floor}")
    expect(consistency(ROOT) == [], f"consistency {consistency(ROOT)}")
    expect(advisory_ignores(ROOT) == re.findall(r"RUSTSEC-\d{4}-\d{4}", (ROOT / "deny.toml").read_text(encoding="utf-8")), "advisory ignores")
    expect({"zip", "libfuzzer-sys"} <= locked_crates(ROOT), "locked crates of both lock files")
    clarified = clarifications(ROOT)
    expect(
        clarified.keys() <= locked_versions(ROOT).keys() and "almiladoc-python" in locked_versions(ROOT) and len(clarified) > 0 and all(re.fullmatch(r"[0-9a-f]{64}", digest) for files in clarified.values() for digest in files.values()),
        f"about.toml clarifications {clarified}",
    )
    pinned_sums = re.findall(r'checksum = "(\w+)"', (ROOT / "about.toml").read_text(encoding="utf-8"))
    expect(sorted(digest for files in clarified.values() for digest in files.values()) == sorted(pinned_sums), "every about.toml checksum")
    packed = io.BytesIO()
    with tarfile.open(fileobj=packed, mode="w:gz") as crate_file:
        info = tarfile.TarInfo("c-1.0.0/LICENSE")
        info.size = 4
        crate_file.addfile(info, io.BytesIO(b"text"))
    held = hashlib.sha256(b"text").hexdigest()
    expect(clarify_findings("c", "1.0.0", {"LICENSE": held}, packed.getvalue()) == [], "a held clarification")
    expect(
        clarify_findings("c", "1.0.0", {"LICENSE": "0" * 64, "x/LICENSE": held}, packed.getvalue()) == [f"about.toml c LICENSE: 1.0.0 has sha256 {held}", "about.toml c x/LICENSE: 1.0.0 has sha256 absent"],
        "a moved and a missing clarification",
    )
    with tempfile.TemporaryDirectory() as directory:
        fake = Path(directory)
        (fake / ".github" / "workflows").mkdir(parents=True)
        (fake / "Makefile").write_text("UV_VERSION := 0.1.0\nPYTHONS ?= 3.12 3.14\n", encoding="utf-8")
        (fake / "pyproject.toml").write_text('[project]\nrequires-python = ">=3.11"\n', encoding="utf-8")
        steps = "      - uses: astral-sh/setup-uv@a\n        with:\n          enable-cache: false\n      - uses: other/action@b\n        with:\n          version: 9.9.9\n"
        steps += "      - uses: astral-sh/setup-uv@a\n        with:\n          version: '0.2.0'\n      - uses: astral-sh/setup-uv@a\n        with:\n          version: 0.1.0\n"
        (fake / ".github" / "workflows" / "w.yml").write_text("jobs:\n  a:\n    steps:\n" + steps, encoding="utf-8")
        site = ".github/workflows/w.yml:"
        expect(uv_pins(fake) == {"Makefile UV_VERSION": "0.1.0", site + "4": "", site + "10": "0.2.0", site + "13": "0.1.0"}, f"setup-uv steps {uv_pins(fake)}")
        expected = [f"{site}4: uv unpinned differs from UV_VERSION", f"{site}10: uv 0.2.0 differs from UV_VERSION", "requires-python floor 3.11 differs from the oldest PYTHONS entry 3.12"]
        expect(consistency(fake) == expected, f"consistency {consistency(fake)}")

    now = dt.datetime(2026, 10, 15, 12, tzinfo=dt.UTC)

    def release(tag: str, days: float, prerelease: bool = False, draft: bool = False, digest: str = "a" * 64, name: str = "cargo-x-{tag}-x86_64-unknown-linux-musl.tar.gz") -> dict:
        published = (now - dt.timedelta(days=days)).isoformat().replace("+00:00", "Z")
        asset = {"name": name.format(tag=tag), "digest": f"sha256:{digest}"}
        return {"tag_name": tag, "published_at": published, "prerelease": prerelease, "draft": draft, "assets": [asset]}

    releases = [release("1.5.0", 1), release("1.4.1-rc.1", 20), release("1.4.0", 10, prerelease=True), release("1.3.5", 9, draft=True), release("1.3.0", 7), release("1.2.9", 30)]
    expect(newest_stable(releases, now)["tag_name"] == "1.3.0", "the newest stable release at least seven days old")
    expect(newest_stable(releases[:1], now) is None, "a release younger than seven days")
    expect(newer("1.2.9", releases, now) is releases[-2] and newer("1.3.0", releases, now) is None and newer("1.10.0", releases, now) is None, "a newer release")
    rows = [{"num": num, "created_at": (now - dt.timedelta(days=days)).isoformat(), "yanked": yanked} for num, days, yanked in (("1.2.0", 8, True), ("1.1.0", 9, False))]
    expect((newer("1.0.0", crate_releases(rows), now) or {}).get("tag_name") == "1.1.0", "the newest unyanked crate release")
    sent = {url: request(url, "application/json", "t").header_items() for url in (GITHUB + "repos", PYTHON_CYCLE, OSV + "X", "https://api.github.com.example/")}
    expect([url for url, headers in sent.items() if ("Authorization", "Bearer t") in headers] == [GITHUB + "repos"], f"the token reaches {sent}")
    expect(all(("User-agent", USER_AGENT) in headers for headers in sent.values()), "the User-Agent")
    expect("Authorization" not in request(GITHUB, "application/json", "t").headers, "the token follows a redirect")
    tool = Tool("CARGO_X", "o/cargo-x", "1.2.9", {"x86_64": "b" * 64}, "cargo-x-$(CARGO_X_VERSION)-$(MUSL).tar.gz", "$(HOST_ARCH)-unknown-linux-musl", "")
    expect(
        tool_findings(tool, releases, releases[-1], now) == ["CARGO_X_SHA256_x86_64 1.2.9: the release's sha256 is " + "a" * 64, "CARGO_X_VERSION 1.2.9: 1.3.0 (x86_64 " + "a" * 64 + ")"],
        "tool findings",
    )
    expect(tool_findings(tool._replace(version="1.3.0", sums={"x86_64": "a" * 64}), releases, releases[-2], now) == [], "a current tool")
    with tempfile.TemporaryDirectory() as directory:
        fake = Path(directory)
        url = "https://github.com/o/age/releases/download/v$(AGE_VERSION)/age-v$(AGE_VERSION)-linux-amd64.tar.gz"
        (fake / "Makefile").write_text(f"MUSL := $(HOST_ARCH)-unknown-linux-musl\nAGE_VERSION := 1.2.9\nAGE_SHA256_x86_64 := {'b' * 64}\nAGE_URL := {url}\n", encoding="utf-8")
        tagged = tools(fake)
    expect([(each.repository, each.tag, each.asset_name("1.3.0", "x86_64")) for each in tagged] == [("o/age", "v1.2.9", "age-v1.3.0-linux-amd64.tar.gz")], f"a v-tagged tool {tagged}")
    v_releases = [release(tag, days, name="age-{tag}-linux-amd64.tar.gz") for tag, days in (("v1.3.1", 2), ("v1.3.0", 8), ("v1.2.9", 30))]
    expect(
        bool(tagged)
        and tool_findings(tagged[0], v_releases, v_releases[-1], now)
        == ["AGE_SHA256_x86_64 1.2.9: the release's sha256 is " + "a" * 64, "AGE_VERSION 1.2.9: 1.3.0 (x86_64 " + "a" * 64 + ")"],
        "v-tagged tool findings",
    )

    readme = "## Available Images\n| Image | A | Label | S |\n|--|--|--|--|\n"
    readme += "| Ubuntu 28.04 [![beta](https://img.shields.io/badge/beta-1)](x) | x64 | `ubuntu-28.04` | [u] |\n| Ubuntu 26.04 | x64 | `ubuntu-latest` or `ubuntu-26.04` | [u] |\n"
    readme += "| Ubuntu 24.04 Arm64 | arm64 | `ubuntu-24.04-arm` | [u] |\n| macOS 26 | x64 | `macos-26-intel` | [m] |\n## Next\n| Old | x | `ubuntu-30.04` | y |\n"
    ga = ga_labels(readme)
    expect(ga == {"ubuntu-latest", "ubuntu-26.04", "ubuntu-24.04-arm", "macos-26-intel"}, f"generally available labels {ga}")
    try:
        ga_labels("## Available Images\nNo table.\n## Next\n| Old | x | `ubuntu-30.04` | y |\n")
        expect(False, "a README listing no generally available label")
    except ValueError:
        pass
    pinned = {"ubuntu-24.04": ["w:1"], "ubuntu-26.04": ["w:2"], "ubuntu-24.04-arm": ["w:3"], "macos-15-intel": ["w:4"], "ubuntu-latest": ["w:5"]}
    expect(
        label_findings(pinned, ga) == ["runner macos-15-intel (w:4): macos-26-intel is generally available", "runner ubuntu-24.04 (w:1): ubuntu-26.04 is generally available"],
        f"label findings {label_findings(pinned, ga)}",
    )

    cycle = {
        "3.17": {"status": "feature", "first_release": "2027-10-01"},
        "3.16": {"status": "bugfix", "first_release": "2026-10-09"},
        "3.15": {"status": "bugfix", "first_release": "2026-10-08"},
        "3.12": {"status": "security", "first_release": "2023-10-02"},
        "3.11": {"status": "end-of-life", "first_release": "2022-10-24"},
    }
    expect(python_findings(cycle, ["3.12", "3.15"], "3.12", now) == [], "current CPython series, 3.16 younger than seven days")
    expect(
        python_findings(cycle, ["3.11", "3.14"], "3.11", now)
        == ["PYTHONS 3.11 3.14: CPython 3.15 is final", "PYTHONS 3.11 3.14: the oldest supported CPython is 3.12", "requires-python >=3.11: the oldest supported CPython is 3.12"],
        f"python findings {python_findings(cycle, ['3.11', '3.14'], '3.11', now)}",
    )

    def osv(crate: str, *events: dict, withdrawn: bool = False) -> dict:
        return {"withdrawn": "2026-01-01T00:00:00Z"} if withdrawn else {"affected": [{"package": {"name": crate}, "ranges": [{"events": [{"introduced": "0"}, *events]}]}]}

    locked = {"kept", "patched"}
    expect(advisory_findings("A", osv("kept"), locked) == [], "an ignore still needed")
    expect(advisory_findings("B", osv("patched", {"fixed": "1.2.3"}), locked) == ["deny.toml ignores B: patched is fixed in 1.2.3"], "a fixed advisory")
    expect(advisory_findings("C", osv("gone"), locked) == ["deny.toml ignores C: gone is no longer locked"], "an unlocked crate")
    expect(advisory_findings("D", osv("kept", withdrawn=True), locked) == ["deny.toml ignores D: withdrawn"], "a withdrawn advisory")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    found = report(ROOT, dt.datetime.now(dt.UTC))
    for finding in found:
        print(finding, file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
