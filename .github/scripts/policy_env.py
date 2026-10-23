"""Prove that a policy gate stops when either policy environment sync fails, and runs its tool
only after both succeed."""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SYNC = "sync --locked --only-group policy --quiet"
FIRST_SYNC = "--no-install-package reuse"
SECOND_SYNC = "--no-build-isolation-package reuse"
FAKE_UV = (
    '#!/bin/sh\necho "$*" >> "$POLICY_ENV_LOG"\ncase "$*" in *"$POLICY_ENV_FAIL"*) exit 3;; esac\n'
)
FAKE_REUSE = '#!/bin/sh\ntouch "$POLICY_ENV_MARKER"\n'
CASES = {
    "the first sync fails": (FIRST_SYNC, False, (FIRST_SYNC,)),
    "the second sync fails": (SECOND_SYNC, False, (FIRST_SYNC, SECOND_SYNC)),
    "both syncs succeed": ("no sync matches this", True, (FIRST_SYNC, SECOND_SYNC)),
}
MAKE_VARIABLES = ("MAKEFLAGS", "MFLAGS", "MAKELEVEL", "CARGO_TARGET_DIR")


def executable(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    path.chmod(0o755)
    return path


def run_case(directory: Path, fail_on: str) -> tuple[int, list[str], bool]:
    uv = executable(directory / "uv", FAKE_UV)
    executable(directory / "make" / "policy" / "bin" / "reuse", FAKE_REUSE)
    log, marker = directory / "syncs.log", directory / "reuse.ran"
    env = {key: value for key, value in os.environ.items() if key not in MAKE_VARIABLES}
    env |= {
        "POLICY_ENV_LOG": str(log),
        "POLICY_ENV_MARKER": str(marker),
        "POLICY_ENV_FAIL": fail_on,
    }
    status = subprocess.run(
        ["make", "-s", "-C", str(ROOT), "reuse", f"UV={uv}", f"CARGO_TARGET_DIR={directory}"],
        env=env,
        capture_output=True,
        check=False,
        timeout=60,
    ).returncode
    syncs = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
    return status, syncs, marker.exists()


def main() -> int:
    failures = []
    for name, (fail_on, passes, expected) in CASES.items():
        with tempfile.TemporaryDirectory() as directory:
            status, syncs, ran = run_case(Path(directory), fail_on)
        if (status == 0) != passes:
            failures.append(f"{name}: make reuse exited {status}")
        wanted = [f"{SYNC} {flags}" for flags in expected]
        if syncs != wanted:
            failures.append(f"{name}: the syncs ran were {syncs}, not {wanted}")
        if ran != passes:
            failures.append(f"{name}: reuse {'ran' if ran else 'did not run'}")
    for failure in failures:
        print(f"policy_env: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
