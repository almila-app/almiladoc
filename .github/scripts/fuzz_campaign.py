"""Run one scheduled fuzz, corpus-minimising or proptest campaign, print only its verdict, and
seal what it found with age."""

import argparse
import base64
import contextlib
import hashlib
import io
import json
import os
import secrets
import shutil
import subprocess
import sys
import tempfile
import tomllib
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import NamedTuple
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
KINDS = ("crash", "leak", "timeout", "oom", "slow-unit", "drill")
MAX_TOTAL_TIME = 1800
TIMEOUT = 10
PROPTEST_CASES = 4096
CMIN_FAILED = b"Failed to minimize corpus"

Runner = Callable[[Sequence[str], Path, Mapping[str, str], Path], int]


class Campaign(NamedTuple):
    root: Path
    host: str
    seed: int
    run: Runner

    @property
    def fuzz(self) -> Path:
        return self.root / "fuzz"

    @property
    def work(self) -> Path:
        return self.fuzz / "artifacts"


def execute(command: Sequence[str], cwd: Path, env: Mapping[str, str], log: Path) -> int:
    with log.open("wb") as out:
        return subprocess.run(
            command,
            cwd=cwd,
            env={**os.environ, **env},
            stdout=out,
            stderr=subprocess.STDOUT,
            check=False,
        ).returncode


def targets(root: Path) -> list[str]:
    return [
        binary["name"]
        for binary in tomllib.loads((root / "fuzz" / "Cargo.toml").read_text(encoding="utf-8"))[
            "bin"
        ]
    ]


def known(root: Path, target: str) -> str:
    if target not in targets(root):
        raise SystemExit(f"{target!r} is not a fuzz target")
    return target


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def seal(source: Path, sealed: Path, recipients: Path) -> None:
    subprocess.run(
        [
            "age",
            "--encrypt",
            "--recipients-file",
            str(recipients),
            "--output",
            str(sealed),
            str(source),
        ],
        check=True,
    )


def conclude(
    campaign: Campaign, name: str, status: int, found: list[tuple[str, Path]], log: Path
) -> int:
    if status == 0 and not found:
        print(f"{name}: ok")
        return 0
    recipients = campaign.fuzz / "recipients.txt"
    sealed = campaign.work / "sealed" / name
    sealed.mkdir(parents=True)
    seed = campaign.work / "logs" / f"{name}.seed"
    seed.write_text(f"{campaign.seed}\n", encoding="utf-8")
    seal(log, sealed / f"{name}.log.age", recipients)
    seal(seed, sealed / f"{name}.seed.age", recipients)
    lines = [] if found else [f"{name}: error sha256:{digest(log)}"]
    for kind, path in found:
        hashed = digest(path)
        seal(path, sealed / f"{name}-{kind}-{hashed}.age", recipients)
        lines.append(f"{name}: {kind} sha256:{hashed}")
    print("\n".join(lines))
    return 0 if status == 0 and found and all(kind == "drill" for kind, _ in found) else 1


def prepare(campaign: Campaign, name: str) -> Path:
    shutil.rmtree(campaign.work / "sealed" / name, ignore_errors=True)
    (campaign.work / "logs").mkdir(parents=True, exist_ok=True)
    return campaign.work / "logs" / f"{name}.log"


def artifacts(directory: Path) -> list[tuple[str, Path]]:
    files = sorted(directory.iterdir()) if directory.is_dir() else []
    return [(kind, path) for path in files for kind in KINDS if path.name.startswith(f"{kind}-")]


def findings(campaign: Campaign, target: str) -> Path:
    found = campaign.work / target
    shutil.rmtree(found, ignore_errors=True)
    found.mkdir(parents=True)
    return found


def fuzz(campaign: Campaign, target: str, drill: bool = False) -> int:
    log = prepare(campaign, target)
    found = findings(campaign, target)
    (campaign.fuzz / "corpus" / target).mkdir(parents=True, exist_ok=True)
    corpora = [f"seeds/{target}", f"regressions/{target}"]
    if drill:
        options = ["-runs=0"]
    else:
        corpora.insert(0, f"corpus/{target}")
        options = [
            f"-max_total_time={MAX_TOTAL_TIME}",
            f"-timeout={TIMEOUT}",
            f"-seed={campaign.seed}",
        ]
        if (campaign.fuzz / "dicts" / f"{target}.dict").is_file():
            options.append(f"-dict=dicts/{target}.dict")
    command = [
        "cargo",
        "fuzz",
        "run",
        "--target",
        campaign.host,
        target,
        *corpora,
        "--",
        f"-artifact_prefix={found}/",
        *options,
    ]
    status = campaign.run(command, campaign.fuzz, {}, log)
    if drill and status == 0:
        seed = min(
            path
            for path in (campaign.fuzz / "seeds" / target).iterdir()
            if path.is_file() and not path.name.startswith(".")
        )
        shutil.copyfile(seed, found / f"drill-{hashlib.sha1(seed.read_bytes()).hexdigest()}")
    return conclude(campaign, target, status, artifacts(found), log)


def cmin(campaign: Campaign, target: str) -> int:
    log = prepare(campaign, target)
    corpus = campaign.fuzz / "corpus" / target
    corpus.mkdir(parents=True, exist_ok=True)
    if not any(corpus.iterdir()):
        print(f"{target}: ok")
        return 0
    found = findings(campaign, target)
    command = [
        "cargo",
        "fuzz",
        "cmin",
        "--target",
        campaign.host,
        target,
        f"corpus/{target}",
        "--",
        f"-artifact_prefix={found}/",
        f"-timeout={TIMEOUT}",
    ]
    status = campaign.run(command, campaign.fuzz, {}, log)
    if status == 0 and CMIN_FAILED in log.read_bytes():
        status = 1
    return conclude(campaign, target, status, artifacts(found), log)


def proptest(campaign: Campaign) -> int:
    log = prepare(campaign, "proptest")
    persisted = campaign.root / "proptest-regressions"

    def regressions() -> dict[Path, bytes]:
        return (
            {path: path.read_bytes() for path in sorted(persisted.rglob("*.txt"))}
            if persisted.is_dir()
            else {}
        )

    before = regressions()
    env = {"PROPTEST_RNG_SEED": str(campaign.seed), "PROPTEST_CASES": str(PROPTEST_CASES)}
    status = campaign.run(
        ["cargo", "test", "--locked", "--all-features", "--lib"], campaign.root, env, log
    )
    found = [("failure", path) for path, data in regressions().items() if before.get(path) != data]
    return conclude(campaign, "proptest", status, found, log)


def host_triple(fuzz_dir: Path) -> str:
    version = subprocess.run(
        ["rustc", "-vV"], cwd=fuzz_dir, capture_output=True, text=True, check=True
    ).stdout
    return next(
        line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: ")
    )


def self_test() -> int:
    failures: list[str] = []

    def expect(holds: bool, what: str) -> None:
        if not holds:
            failures.append(what)

    names = targets(ROOT)
    expect(
        len(names) == len(set(names)) > 0
        and all((ROOT / "fuzz" / "fuzz_targets" / f"{name}.rs").is_file() for name in names),
        f"targets {names}",
    )
    recipients = [
        line
        for line in (ROOT / "fuzz" / "recipients.txt").read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]
    expect(len(recipients) == 1, f"recipients {recipients}")
    sealed_byte = subprocess.run(
        ["age", "--encrypt", "--recipients-file", str(ROOT / "fuzz" / "recipients.txt")],
        input=b"x",
        capture_output=True,
        check=False,
    )
    expect(
        sealed_byte.returncode == 0 and sealed_byte.stdout.startswith(b"age-encryption.org/v1\n"),
        f"the recipient encrypts: {sealed_byte.stderr!r}",
    )

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory) / "repository"
        (root / "fuzz" / "seeds" / "xlsx").mkdir(parents=True)
        (root / "fuzz" / "Cargo.toml").write_text(
            '[[bin]]\nname = "xlsx"\n\n[[bin]]\nname = "csv"\n', encoding="utf-8"
        )
        seed_input = root / "fuzz" / "seeds" / "xlsx" / "a.xlsx"
        seed_input.write_bytes(b"PK\x03\x04seed")
        (root / "fuzz" / "seeds" / "xlsx" / "b.xlsx").write_bytes(b"PK\x03\x04other")
        (root / "fuzz" / "seeds" / "xlsx" / ".DS_Store").write_bytes(b"hidden")
        (root / "fuzz" / "seeds" / "xlsx" / "0").mkdir()
        identity = Path(directory) / "identity.txt"
        subprocess.run(["age-keygen", "-o", str(identity)], capture_output=True, check=True)
        recipient = subprocess.run(
            ["age-keygen", "-y", str(identity)], capture_output=True, text=True, check=True
        ).stdout
        (root / "fuzz" / "recipients.txt").write_text(recipient, encoding="utf-8")
        seed = 3141592653
        calls: list[tuple[list[str], Path, dict[str, str]]] = []

        def stub(status: int, write: Callable[[Path], None] = lambda log: None) -> Runner:
            def run(command: Sequence[str], cwd: Path, env: Mapping[str, str], log: Path) -> int:
                calls.append((list(command), cwd, dict(env)))
                log.write_bytes(b"INFO: Seed: " + str(seed).encode() + b"\n")
                write(log)
                return status

            return run

        def campaign(runner: Runner) -> Campaign:
            calls.clear()
            return Campaign(root, "x86_64-unknown-linux-gnu", seed, runner)

        def outcome(action: Callable[[], int]) -> tuple[int, str]:
            printed = io.StringIO()
            with contextlib.redirect_stdout(printed):
                code = action()
            return code, printed.getvalue()

        def opened(name: str) -> dict[str, bytes]:
            sealed = root / "fuzz" / "artifacts" / "sealed" / name
            files = sorted(sealed.iterdir()) if sealed.is_dir() else []
            out = {}
            for path in files:
                plain = Path(directory) / "plain"
                subprocess.run(
                    [
                        "age",
                        "--decrypt",
                        "--identity",
                        str(identity),
                        "--output",
                        str(plain),
                        str(path),
                    ],
                    check=True,
                )
                out[path.name] = plain.read_bytes()
            return out

        crashing = b"PK\x03\x04\xff\x00crash"
        crash_hash = hashlib.sha256(crashing).hexdigest()

        def crash(prefix: str) -> Callable[[Path], None]:
            def write(log: Path) -> None:
                found = root / "fuzz" / "artifacts" / "xlsx"
                (found / f"{prefix}-{hashlib.sha1(crashing).hexdigest()}").write_bytes(crashing)
                with log.open("ab") as out:
                    out.write(
                        b"==1== ERROR: libFuzzer: deadly signal\n"
                        + crashing
                        + b"\nBase64: "
                        + base64.b64encode(crashing)
                        + b"\n"
                    )

            return write

        code, printed = outcome(lambda: fuzz(campaign(stub(77, crash("crash"))), "xlsx"))
        expect(
            (code, printed) == (1, f"xlsx: crash sha256:{crash_hash}\n"),
            f"a crash prints {code} {printed!r}",
        )
        command, cwd, env = calls[0]
        artifact_dir = root / "fuzz" / "artifacts" / "xlsx"
        wanted = [
            "cargo",
            "fuzz",
            "run",
            "--target",
            "x86_64-unknown-linux-gnu",
            "xlsx",
            "corpus/xlsx",
            "seeds/xlsx",
            "regressions/xlsx",
            "--",
            f"-artifact_prefix={artifact_dir}/",
        ]
        wanted += ["-max_total_time=1800", "-timeout=10", f"-seed={seed}"]
        expect(
            (command, cwd, env) == (wanted, root / "fuzz", {}),
            f"the campaign command {command} in {cwd}",
        )
        expect(
            (root / "fuzz" / "corpus" / "xlsx").is_dir(),
            "the corpus directory exists before the run",
        )
        log = (root / "fuzz" / "artifacts" / "logs" / "xlsx.log").read_bytes()
        expect(
            opened("xlsx")
            == {
                "xlsx.log.age": log,
                "xlsx.seed.age": f"{seed}\n".encode(),
                f"xlsx-crash-{crash_hash}.age": crashing,
            },
            "the input, log and seed are sealed",
        )
        expect(
            str(seed) not in printed and base64.b64encode(crashing).decode() not in printed,
            "the seed and input stay out of the output",
        )

        code, printed = outcome(lambda: fuzz(campaign(stub(70, crash("timeout"))), "xlsx"))
        expect(
            (code, printed) == (1, f"xlsx: timeout sha256:{crash_hash}\n"),
            f"a timeout prints {code} {printed!r}",
        )
        expect(len(opened("xlsx")) == 3, "a second run replaces the first one's sealed files")

        code, printed = outcome(lambda: fuzz(campaign(stub(0, crash("slow-unit"))), "xlsx"))
        expect(
            (code, printed) == (1, f"xlsx: slow-unit sha256:{crash_hash}\n"),
            f"a slow input in a passing run prints {code} {printed!r}",
        )

        code, printed = outcome(lambda: fuzz(campaign(stub(0)), "xlsx"))
        expect(
            (code, printed, opened("xlsx")) == (0, "xlsx: ok\n", {}),
            f"a clean run prints {code} {printed!r}",
        )

        (root / "fuzz" / "dicts").mkdir()
        (root / "fuzz" / "dicts" / "xlsx.dict").write_text('"<row"\n', encoding="utf-8")
        outcome(lambda: fuzz(campaign(stub(0)), "xlsx"))
        expect(
            calls[0][0][-3:] == ["-timeout=10", f"-seed={seed}", "-dict=dicts/xlsx.dict"],
            f"a campaign passes its target's dictionary {calls[0][0]}",
        )
        outcome(lambda: fuzz(campaign(stub(0)), "csv"))
        expect(
            not any(option.startswith("-dict") for option in calls[0][0]),
            f"a target without a dictionary passes none {calls[0][0]}",
        )

        code, printed = outcome(lambda: fuzz(campaign(stub(1)), "xlsx"))
        log_hash = digest(root / "fuzz" / "artifacts" / "logs" / "xlsx.log")
        expect(
            (code, printed) == (1, f"xlsx: error sha256:{log_hash}\n"),
            f"a failure without an input prints {code} {printed!r}",
        )
        expect(
            sorted(opened("xlsx")) == ["xlsx.log.age", "xlsx.seed.age"],
            "a failure without an input seals the log and seed",
        )

        drill_hash = digest(seed_input)
        code, printed = outcome(lambda: fuzz(campaign(stub(0)), "xlsx", drill=True))
        expect(
            (code, printed) == (0, f"xlsx: drill sha256:{drill_hash}\n"),
            f"a drill prints {code} {printed!r}",
        )
        expect(
            calls[0][0][6:]
            == [
                "seeds/xlsx",
                "regressions/xlsx",
                "--",
                f"-artifact_prefix={artifact_dir}/",
                "-runs=0",
            ],
            f"a drill replays {calls[0][0]}",
        )
        expect(
            opened("xlsx").get(f"xlsx-drill-{drill_hash}.age") == seed_input.read_bytes(),
            "a drill seals its seed input",
        )

        code, printed = outcome(lambda: fuzz(campaign(stub(1)), "xlsx", drill=True))
        expect(
            (code, printed.split(" ")[1]) == (1, "error"),
            f"a drill whose replay fails prints {code} {printed!r}",
        )

        code, printed = outcome(lambda: cmin(campaign(stub(0)), "csv"))
        expect((code, printed, calls) == (0, "csv: ok\n", []), "an empty corpus is not minimised")
        (root / "fuzz" / "corpus" / "csv" / "x").write_bytes(b"a,b")
        code, printed = outcome(lambda: cmin(campaign(stub(0)), "csv"))
        csv_artifacts = root / "fuzz" / "artifacts" / "csv"
        wanted = [
            "cargo",
            "fuzz",
            "cmin",
            "--target",
            "x86_64-unknown-linux-gnu",
            "csv",
            "corpus/csv",
            "--",
            f"-artifact_prefix={csv_artifacts}/",
            "-timeout=10",
        ]
        expect((code, printed) == (0, "csv: ok\n") and calls[0][0] == wanted, f"cmin {calls}")
        code, printed = outcome(
            lambda: cmin(
                campaign(
                    stub(0, lambda log: log.write_bytes(CMIN_FAILED + b": exit status: 1\n"))
                ),
                "csv",
            )
        )
        expect(
            (code, printed.split(" ")[1]) == (1, "error"),
            f"a failed minimisation prints {code} {printed!r}",
        )
        code, printed = outcome(
            lambda: cmin(
                campaign(stub(0, lambda log: (csv_artifacts / "crash-1").write_bytes(crashing))),
                "csv",
            )
        )
        expect(
            (code, printed) == (1, f"csv: crash sha256:{crash_hash}\n"),
            f"a crash the merge skipped prints {code} {printed!r}",
        )
        expect(
            opened("csv").get(f"csv-crash-{crash_hash}.age") == crashing,
            "a crash the merge skipped is sealed",
        )

        persisted = root / "proptest-regressions"
        (persisted / "kept").mkdir(parents=True)
        (persisted / "kept" / "old.txt").write_bytes(b"cc 00 # shrinks to old\n")
        shrunk = b"cc 1f # shrinks to bytes = [0, 1]\n"

        def regression(log: Path) -> None:
            (persisted / "formats").mkdir(exist_ok=True)
            (persisted / "formats" / "csv.txt").write_bytes(shrunk)
            with log.open("ab") as out:
                out.write(b"minimal failing input: bytes = [0, 1]\n")

        code, printed = outcome(lambda: proptest(campaign(stub(101, regression))))
        shrunk_hash = hashlib.sha256(shrunk).hexdigest()
        expect(
            (code, printed) == (1, f"proptest: failure sha256:{shrunk_hash}\n"),
            f"a proptest failure prints {code} {printed!r}",
        )
        expect(
            calls[0]
            == (
                ["cargo", "test", "--locked", "--all-features", "--lib"],
                root,
                {"PROPTEST_RNG_SEED": str(seed), "PROPTEST_CASES": "4096"},
            ),
            f"proptest {calls}",
        )
        expect(
            sorted(opened("proptest"))
            == [f"proptest-failure-{shrunk_hash}.age", "proptest.log.age", "proptest.seed.age"],
            "a proptest failure seals its regression, log and seed",
        )
        code, printed = outcome(lambda: proptest(campaign(stub(0))))
        expect(
            (code, printed) == (0, "proptest: ok\n"),
            f"a passing proptest prints {code} {printed!r}",
        )

        echoed = Path(directory) / "echo.log"
        with mock.patch.dict(os.environ, {"PROPTEST_RNG_SEED": "20261015"}):
            code = execute(
                [
                    sys.executable,
                    "-I",
                    "-c",
                    "import os, sys; os.write(2, b'E'); "
                    "sys.stdout.write(os.environ['PROPTEST_RNG_SEED'])",
                ],
                root,
                {"PROPTEST_RNG_SEED": "7"},
                echoed,
            )
        expect(
            (code, echoed.read_bytes()) == (0, b"E7"),
            "the log holds stderr before stdout and the campaign's environment overrides "
            f"the ambient one: {code} {echoed.read_bytes()!r}",
        )

        expect(
            targets(root) == ["xlsx", "csv"] and known(root, "csv") == "csv", "the listed targets"
        )
        try:
            known(root, "../csv")
            expect(False, "an unknown target")
        except SystemExit:
            pass

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument(
        "--targets",
        action="store_true",
        help="print the fuzz targets, or only TARGET, as a JSON array",
    )
    mode.add_argument(
        "--drill",
        action="store_true",
        help="replay TARGET's seeds and seal one as if it had crashed",
    )
    mode.add_argument("--cmin", action="store_true", help="minimise TARGET's corpus")
    mode.add_argument(
        "--proptest",
        action="store_true",
        help="run the library's tests under a random proptest seed",
    )
    parser.add_argument("target", nargs="?")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.targets:
        print(json.dumps([known(ROOT, args.target)] if args.target else targets(ROOT)))
        return 0
    if args.proptest == bool(args.target):
        parser.error("name one target, except with --proptest")
    seed = secrets.randbelow(2**32 - 1) + 1
    if args.proptest:
        return proptest(Campaign(ROOT, "", seed, execute))
    target = known(ROOT, args.target)
    campaign = Campaign(ROOT, host_triple(ROOT / "fuzz"), seed, execute)
    return cmin(campaign, target) if args.cmin else fuzz(campaign, target, drill=args.drill)


if __name__ == "__main__":
    sys.exit(main())
