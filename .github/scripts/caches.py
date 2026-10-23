"""Hold the gate caches to exact per-job keys, saves and skips only in a warm run on main, no
cached tools, and one warm caller."""

import argparse
import copy
import re
import sys
from collections.abc import Callable
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
GATES = "gates.yml"
WARM_CALLER = "cache.yml"
CACHE_USERS = {GATES, "scheduled.yml"}
COMPILING = {"clippy", "test", "doc", "features", "python", "fuzz"}
CACHEABLE = {
    "~/.cargo/registry/index",
    "~/.cargo/registry/cache",
    "~/.cargo/git/db",
    "target/debug",
    "target/release",
    "fuzz/target",
}
WARM_ON_MAIN = {"inputs.warm", "github.ref == 'refs/heads/main'"}
WARM_TRIGGERS = {"push": {"branches": ["main"]}, "workflow_dispatch": None}
COLD_ONLY = {"!inputs.warm"}
REQUIRED = "required"

Workflows = dict[str, dict]


def load(root: Path) -> Workflows:
    return {
        path.name: yaml.safe_load(path.read_text(encoding="utf-8"))
        for path in sorted((root / ".github" / "workflows").glob("*.yml"))
    }


def conjuncts(condition: object) -> set[str]:
    text = str(condition).strip()
    if wrapped := re.fullmatch(r"\$\{\{(.*)\}\}", text, re.DOTALL):
        text = wrapped.group(1)
    return {" ".join(part.split()) for part in text.split("&&")}


def uses(step: dict, action: str) -> bool:
    return str(step.get("uses", "")).startswith(action + "@")


def is_cache(step: dict) -> bool:
    return str(step.get("uses", "")).startswith("actions/cache")


def paths(step: dict) -> list[str]:
    return [
        line.strip()
        for line in str(step.get("with", {}).get("path", "")).splitlines()
        if line.strip()
    ]


def make_targets(step: dict) -> set[str]:
    words = str(step.get("run", "")).split()
    return set(words[1:]) if words[:1] == ["make"] else set()


def check_job(job_id: str, job: dict) -> list[str]:
    where = f"{GATES} {job_id}"
    steps = job.get("steps", [])
    if "if" in job:
        return [
            f"job gate: {where} runs under its own condition {job['if']!r}, "
            "so a reader can skip its gate"
        ]
    make = next(
        (index for index, step in enumerate(steps) if make_targets(step) & COMPILING), None
    )
    probes = [
        index
        for index, step in enumerate(steps)
        if uses(step, "actions/cache/restore") and step.get("with", {}).get("lookup-only") is True
    ]
    restores = [
        index
        for index, step in enumerate(steps)
        if uses(step, "actions/cache/restore") and index not in probes
    ]
    saves = [index for index, step in enumerate(steps) if uses(step, "actions/cache/save")]
    caches = [index for index, step in enumerate(steps) if is_cache(step)]
    if (
        len(probes) != 1
        or len(restores) != 1
        or len(saves) != 1
        or len(caches) != 3
        or make is None
    ):
        return [
            f"probe: {where} needs one lookup-only probe, "
            "one restore and one save around its make step"
        ]
    probe, restore, save = steps[probes[0]], steps[restores[0]], steps[saves[0]]
    miss = f"steps.{probe.get('id')}.outputs.cache-hit != 'true'"
    problems = []
    if conjuncts(probe.get("if")) != WARM_ON_MAIN:
        problems.append(
            f"probe: {where} probes under {probe.get('if')!r}, not only in a warm run on main"
        )
    if probes[0] > min(
        index for index, step in enumerate(steps) if "run" in step or index in restores
    ):
        problems.append(f"probe: {where} runs a step before its probe")
    if not restores[0] < make < saves[0]:
        problems.append(f"save: {where} must restore before and save after its make step")
    for index, step in enumerate(steps[probes[0] + 1 :], probes[0] + 1):
        if index != saves[0] and conjuncts(step.get("if")) != {miss}:
            problems.append(
                f"skip: {where} step {index} runs under {step.get('if')!r}, "
                "not only on a probe miss"
            )
    key = str(probe.get("with", {}).get("key", ""))
    if not key.startswith(f"cargo-{job_id}-${{{{ runner.os }}}}-"):
        problems.append(f"key: {where} key {key!r} does not name its job")
    if {
        name: value for name, value in probe.get("with", {}).items() if name != "lookup-only"
    } != restore.get("with"):
        problems.append(f"key: {where} probe and restore differ in key or path")
    if any("restore-keys" in step.get("with", {}) for step in (probe, restore)):
        problems.append(f"key: {where} restores by prefix")
    save_if = conjuncts(save.get("if"))
    if save_if != WARM_ON_MAIN | {miss, f"steps.{restore.get('id')}.outputs.cache-hit != 'true'"}:
        problems.append(
            f"save: {where} saves under {save.get('if')!r}, "
            "not only after a miss in a warm run on main"
        )
    if (
        save.get("with", {}).get("key")
        != f"${{{{ steps.{restore.get('id')}.outputs.cache-primary-key }}}}"
    ):
        problems.append(f"save: {where} saves under another key than the one it restored")
    if paths(save) != paths(restore):
        problems.append(f"save: {where} saves other paths than it restores")
    return problems


def check(workflows: Workflows) -> list[str]:
    problems = []
    for name, workflow in workflows.items():
        for job_id, job in workflow.get("jobs", {}).items():
            for step in job.get("steps", []):
                if is_cache(step) and name not in CACHE_USERS:
                    problems.append(
                        f"cache user: {name} {job_id} uses {step['uses']}; "
                        f"only {sorted(CACHE_USERS)} may"
                    )
                if (
                    uses(step, "astral-sh/setup-uv")
                    and step.get("with", {}).get("enable-cache") is not False
                ):
                    problems.append(
                        f"uv cache: {name} {job_id} sets up uv without enable-cache: false"
                    )
                if (
                    is_cache(step)
                    and name == GATES
                    and not (
                        uses(step, "actions/cache/restore") or uses(step, "actions/cache/save")
                    )
                ):
                    problems.append(
                        f"cache user: {name} {job_id} uses {step['uses']}; "
                        "gates restore and save only by actions/cache/restore "
                        "and actions/cache/save"
                    )
                if is_cache(step) and name == GATES and not set(paths(step)) <= CACHEABLE:
                    problems.append(
                        f"path: {name} {job_id} caches {sorted(set(paths(step)) - CACHEABLE)}"
                    )
            calls_warm = (
                str(job.get("uses", "")).split("@")[0].endswith(f"/.github/workflows/{GATES}")
                and job.get("with", {}).get("warm", False) is not False
            )
            if calls_warm and (
                name != WARM_CALLER or job_id == "ci" or job["with"]["warm"] is not True
            ):
                problems.append(
                    f"warm caller: {name} {job_id} calls {GATES} warm; "
                    f"only {WARM_CALLER}, in a job not named ci, may"
                )
    callers = [
        job
        for job in workflows.get(WARM_CALLER, {}).get("jobs", {}).values()
        if job.get("with", {}).get("warm") is True
    ]
    if len(callers) != 1:
        problems.append(f"warm caller: {WARM_CALLER} needs exactly one job calling {GATES} warm")
    triggers = workflows.get(WARM_CALLER, {}).get(True)
    if triggers != WARM_TRIGGERS:
        problems.append(
            f"warm trigger: {WARM_CALLER} runs on {triggers!r}, "
            "not only on pushes to main and dispatch"
        )
    gates = workflows.get(GATES, {})
    warm = (gates.get(True) or {}).get("workflow_call", {}).get("inputs", {}).get("warm", {})
    if warm.get("type") != "boolean" or warm.get("default") is not False:
        problems.append(f"warm input: {GATES} needs the boolean input warm, false by default")
    for job_id, job in gates.get("jobs", {}).items():
        compiling = any(make_targets(step) & COMPILING for step in job.get("steps", []))
        if compiling:
            problems += check_job(job_id, job)
            continue
        if job_id == REQUIRED:
            if conjuncts(job.get("if")) != {"always()"} | COLD_ONLY:
                problems.append(
                    f"required: {GATES} {job_id} runs under {job.get('if')!r}, "
                    "not always outside a warm run"
                )
        elif "if" in job and conjuncts(job["if"]) != COLD_ONLY:
            problems.append(
                f"job gate: {GATES} {job_id} runs under {job['if']!r}, not only outside a warm run"
            )
        if any(is_cache(step) for step in job.get("steps", [])):
            problems.append(f"cache user: {GATES} {job_id} compiles nothing but uses a cache")
    return problems


def step_of(workflows: Workflows, job_id: str, test: Callable[[dict], bool]) -> dict:
    return next(step for step in workflows[GATES]["jobs"][job_id]["steps"] if test(step))


def probe_of(workflows: Workflows, job_id: str = "rust") -> dict:
    return step_of(workflows, job_id, lambda step: step.get("id") == "probe")


def restore_of(workflows: Workflows, job_id: str = "rust") -> dict:
    return step_of(workflows, job_id, lambda step: step.get("id") == "restore")


def save_of(workflows: Workflows, job_id: str = "rust") -> dict:
    return step_of(workflows, job_id, lambda step: uses(step, "actions/cache/save"))


def make_of(workflows: Workflows, job_id: str = "rust") -> dict:
    return step_of(workflows, job_id, lambda step: bool(make_targets(step) & COMPILING))


def add_path(workflows: Workflows, path: str) -> None:
    for step in (probe_of(workflows), restore_of(workflows), save_of(workflows)):
        step["with"]["path"] += path + "\n"


def move_save_first(workflows: Workflows) -> None:
    steps = workflows[GATES]["jobs"]["rust"]["steps"]
    steps.insert(1, steps.pop(steps.index(save_of(workflows))))


def move_toolchain_first(workflows: Workflows) -> None:
    steps = workflows[GATES]["jobs"]["rust"]["steps"]
    steps.insert(
        1,
        steps.pop(
            steps.index(
                step_of(
                    workflows, "rust", lambda step: step.get("run") == "rustup toolchain install"
                )
            )
        ),
    )


def add_combined_cache(workflows: Workflows) -> None:
    steps = workflows[GATES]["jobs"]["rust"]["steps"]
    combined = copy.deepcopy(restore_of(workflows))
    combined.pop("id")
    combined["uses"] = combined["uses"].replace("actions/cache/restore@", "actions/cache@")
    steps.insert(steps.index(make_of(workflows)), combined)


MUTANTS: dict[str, tuple[str, Callable[[Workflows], None]]] = {
    "a probe without warm": (
        "probe",
        lambda w: probe_of(w).update({"if": "github.ref == 'refs/heads/main'"}),
    ),
    "a probe without the main ref": ("probe", lambda w: probe_of(w).update({"if": "inputs.warm"})),
    "a probe on any warm run or main": (
        "probe",
        lambda w: probe_of(w).update({"if": "inputs.warm || github.ref == 'refs/heads/main'"}),
    ),
    "a job without its probe": (
        "probe",
        lambda w: w[GATES]["jobs"]["features"]["steps"].remove(probe_of(w, "features")),
    ),
    "a reader skipping on a restore hit": (
        "skip",
        lambda w: make_of(w).update({"if": "steps.restore.outputs.cache-hit != 'true'"}),
    ),
    "a reader gated as a job": (
        "job gate",
        lambda w: w[GATES]["jobs"]["python"].update({"if": "github.event_name != 'pull_request'"}),
    ),
    "a save without the main ref": (
        "save",
        lambda w: save_of(w).update(
            {
                "if": "inputs.warm && steps.probe.outputs.cache-hit != 'true' "
                "&& steps.restore.outputs.cache-hit != 'true'"
            }
        ),
    ),
    "a save without warm": (
        "save",
        lambda w: save_of(w).update(
            {
                "if": "github.ref == 'refs/heads/main' "
                "&& steps.probe.outputs.cache-hit != 'true' "
                "&& steps.restore.outputs.cache-hit != 'true'"
            }
        ),
    ),
    "a save under a recomputed key": (
        "save",
        lambda w: save_of(w)["with"].update({"key": probe_of(w)["with"]["key"]}),
    ),
    "a save before the build": ("save", move_save_first),
    "a key without its job": (
        "key",
        lambda w: [
            step["with"].update({"key": step["with"]["key"].replace("cargo-rust-", "cargo-")})
            for step in (probe_of(w), restore_of(w))
        ],
    ),
    "a key under another job's prefix": (
        "key",
        lambda w: [
            step["with"].update(
                {"key": step["with"]["key"].replace("cargo-rust-", "cargo-rust-arm-")}
            )
            for step in (probe_of(w), restore_of(w))
        ],
    ),
    "a probe and restore under different keys": (
        "key",
        lambda w: restore_of(w)["with"].update({"key": restore_of(w)["with"]["key"] + "-x"}),
    ),
    "a prefix restore": (
        "key",
        lambda w: restore_of(w)["with"].update({"restore-keys": "cargo-rust-"}),
    ),
    "a prefix restore on the probe and restore alike": (
        "key",
        lambda w: [
            step["with"].update({"restore-keys": "cargo-rust-"})
            for step in (probe_of(w), restore_of(w))
        ],
    ),
    "a step before the probe": ("probe", move_toolchain_first),
    "a save of fewer paths": (
        "save",
        lambda w: save_of(w)["with"].update(
            {"path": save_of(w)["with"]["path"].replace("target/debug\n", "")}
        ),
    ),
    "a combined cache saving on every run": ("cache user", add_combined_cache),
    "a combined cache counted as a fourth": ("probe", add_combined_cache),
    "cached tools": ("path", lambda w: add_path(w, "target/make")),
    "a whole target cached": ("path", lambda w: add_path(w, "target")),
    "a cache outside the gates": (
        "cache user",
        lambda w: w["ci.yml"]["jobs"].update({"x": {"steps": [copy.deepcopy(restore_of(w))]}}),
    ),
    "a cache in a non-compiling gate": (
        "cache user",
        lambda w: w[GATES]["jobs"]["policy"]["steps"].append(copy.deepcopy(restore_of(w))),
    ),
    "a cache in the required gate": (
        "cache user",
        lambda w: w[GATES]["jobs"][REQUIRED]["steps"].append(copy.deepcopy(save_of(w))),
    ),
    "a uv cache": (
        "uv cache",
        lambda w: step_of(w, "python", lambda step: uses(step, "astral-sh/setup-uv"))[
            "with"
        ].update({"enable-cache": True}),
    ),
    "a warm ci": (
        "warm caller",
        lambda w: w["ci.yml"]["jobs"]["ci"].update({"with": {"warm": True}}),
    ),
    "a warm ci by repository and ref": (
        "warm caller",
        lambda w: w["ci.yml"]["jobs"]["ci"].update(
            {
                "uses": f"almila-app/almiladoc/.github/workflows/{GATES}@main",
                "with": {"warm": True},
            }
        ),
    ),
    "no warm caller": (
        "warm caller",
        lambda w: w[WARM_CALLER]["jobs"]["warm"]["with"].update({"warm": False}),
    ),
    "a required check skipped outside warm runs": (
        "required",
        lambda w: w[GATES]["jobs"][REQUIRED].update({"if": "always() && inputs.warm"}),
    ),
    "a required check without always": (
        "required",
        lambda w: w[GATES]["jobs"][REQUIRED].update({"if": "${{ !inputs.warm }}"}),
    ),
    "a policy gated beyond warm": (
        "job gate",
        lambda w: w[GATES]["jobs"]["policy"].update(
            {"if": "${{ !inputs.warm && github.event_name != 'pull_request' }}"}
        ),
    ),
    "a policy skipped outside warm runs": (
        "job gate",
        lambda w: w[GATES]["jobs"]["policy"].update({"if": "${{ inputs.warm }}"}),
    ),
    "a warm caller named ci": (
        "warm caller",
        lambda w: w[WARM_CALLER].update({"jobs": {"ci": w[WARM_CALLER]["jobs"]["warm"]}}),
    ),
    "a warm run on pull requests": (
        "warm trigger",
        lambda w: w[WARM_CALLER][True].update({"pull_request": None}),
    ),
    "a warm default": (
        "warm input",
        lambda w: w[GATES][True]["workflow_call"]["inputs"]["warm"].update({"default": True}),
    ),
}


def self_test() -> int:
    failures = []
    workflows = load(ROOT)
    if REQUIRED not in workflows[GATES]["jobs"]:
        failures.append(f"{GATES} has no {REQUIRED} job")
    if problems := check(workflows):
        failures.append(f"the tracked workflows fail: {problems}")
    for name, (guard, mutate) in MUTANTS.items():
        mutant = copy.deepcopy(workflows)
        mutate(mutant)
        if not any(problem.startswith(f"{guard}:") for problem in check(mutant)):
            failures.append(f"{name} passed the {guard} guard")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    problems = check(load(ROOT))
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
