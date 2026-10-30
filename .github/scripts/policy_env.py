import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SYNC = "sync --locked --only-group policy --quiet"
FAKE_UV = '#!/bin/sh\necho "$*" >> "$POLICY_ENV_LOG"\nexit "$POLICY_ENV_STATUS"\n'
FAKE_REUSE = '#!/bin/sh\ntouch "$POLICY_ENV_MARKER"\n'
CASES = {"sync fails": (3, False), "sync succeeds": (0, True)}

MAKE_VARIABLES = ("MAKEFLAGS", "MFLAGS", "MAKELEVEL", "CARGO_TARGET_DIR")


def executable(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    path.chmod(0o755)
    return path


def run_case(directory: Path, sync_status: int) -> tuple[int, list[str], bool]:
    uv = executable(directory / "uv", FAKE_UV)
    executable(directory / "make" / "policy" / "bin" / "reuse", FAKE_REUSE)
    log, marker = directory / "syncs.log", directory / "reuse.ran"
    env = {key: value for key, value in os.environ.items() if key not in MAKE_VARIABLES}
    env |= {
        "POLICY_ENV_LOG": str(log),
        "POLICY_ENV_MARKER": str(marker),
        "POLICY_ENV_STATUS": str(sync_status),
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
    for name, (sync_status, passes) in CASES.items():
        with tempfile.TemporaryDirectory() as directory:
            status, syncs, ran = run_case(Path(directory), sync_status)
        if (status == 0) != passes:
            failures.append(f"{name}: make reuse exited {status}")
        wanted = [SYNC]
        if syncs != wanted:
            failures.append(f"{name}: the syncs ran were {syncs}, not {wanted}")
        if ran != passes:
            failures.append(f"{name}: reuse {'ran' if ran else 'did not run'}")
    for failure in failures:
        print(f"policy_env: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
