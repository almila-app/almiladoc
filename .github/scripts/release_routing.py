"""Hold release.yml's publication routing to ADR 0003 decision 6: only a v* tag push writes
anything, a candidate goes to TestPyPI, a final to PyPI and then crates.io."""

import argparse
import copy
import re
import sys
from collections.abc import Callable
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
RELEASE = ROOT / ".github" / "workflows" / "release.yml"
TAG_PUSH = "github.event_name == 'push'"
TRIGGERS = {"push": {"tags": ["v*"]}, "schedule": None, "workflow_dispatch": None}
PYPI_PUBLISH = "pypa/gh-action-pypi-publish@"
PUBLISHERS = {
    "publish-testpypi": ("testpypi", "testpypi", {"guard", "attest"}),
    "publish-pypi": ("pypi", "pypi", {"guard", "attest"}),
    "publish-crates": ("crates-io", "pypi", {"guard", "publish-pypi"}),
}
UPLOADS = {"publish-testpypi": "https://test.pypi.org/legacy/", "publish-pypi": None}

Workflow = dict


def conjuncts(condition: object) -> set[str]:
    text = str(condition).strip()
    if wrapped := re.fullmatch(r"\$\{\{(.*)\}\}", text, re.DOTALL):
        text = wrapped.group(1)
    if "||" in text:
        return set()
    return {" ".join(part.split()) for part in text.split("&&")}


def writes(job: dict) -> bool:
    return "environment" in job or "write" in (job.get("permissions") or {}).values()


def trigger_problems(workflow: Workflow) -> list[str]:
    triggers = workflow.get(True, workflow.get("on")) or {}
    found = {name: triggers[name] if name == "push" else None for name in triggers}
    if found != TRIGGERS:
        return [f"triggers: {sorted(triggers)} with push {triggers.get('push')}, not {TRIGGERS}"]
    return []


def publisher_problems(job_id: str, job: dict) -> list[str]:
    environment, target, needs = PUBLISHERS[job_id]
    problems = []
    if (job.get("environment") or {}).get("name") != environment:
        problems.append(f"environment: {job_id} publishes from {job.get('environment')}")
    if f"needs.guard.outputs.channel == '{target}'" not in conjuncts(job.get("if", "")):
        problems.append(f"channel: {job_id} runs on {job.get('if')!r}, not channel {target}")
    if not needs <= set(job.get("needs", [])):
        problems.append(f"order: {job_id} needs {job.get('needs')}, not all of {sorted(needs)}")
    if job_id in UPLOADS:
        found = [
            (step.get("with") or {}).get("repository-url")
            for step in job.get("steps", [])
            if str(step.get("uses", "")).startswith(PYPI_PUBLISH)
        ]
        if found != [UPLOADS[job_id]]:
            problems.append(f"registry: {job_id} uploads to {found}, not {UPLOADS[job_id]}")
    return problems


def check(workflow: Workflow) -> list[str]:
    problems = trigger_problems(workflow)
    jobs = workflow.get("jobs", {})
    problems += [
        f"tag only: {job_id} writes on {job.get('if')!r}"
        for job_id, job in jobs.items()
        if writes(job) and TAG_PUSH not in conjuncts(job.get("if", ""))
    ]
    for job_id in PUBLISHERS:
        if job_id not in jobs:
            problems.append(f"publisher: {job_id} is missing")
            continue
        problems += publisher_problems(job_id, jobs[job_id])
    return problems


def job_if(job_id: str, condition: str) -> Callable[[Workflow], None]:
    return lambda w: w["jobs"][job_id].update({"if": condition})


def upload(job_id: str, url: str | None) -> Callable[[Workflow], None]:
    def mutate(workflow: Workflow) -> None:
        for step in workflow["jobs"][job_id]["steps"]:
            if str(step.get("uses", "")).startswith(PYPI_PUBLISH):
                step["with"] = {"repository-url": url} if url else {}

    return mutate


CANDIDATE = "github.event_name == 'push' && needs.guard.outputs.channel == 'testpypi'"
FINAL = "github.event_name == 'push' && needs.guard.outputs.channel == 'pypi'"
MUTANTS: dict[str, tuple[str, Callable[[Workflow], None]]] = {
    "a branch push": ("triggers", lambda w: w[True]["push"].update({"branches": ["main"]})),
    "a pull request trigger": ("triggers", lambda w: w[True].update({"pull_request": None})),
    "a rehearsal attest": ("tag only", job_if("attest", "always()")),
    "a rehearsal GitHub release": (
        "tag only",
        job_if(
            "github-release", "${{ !cancelled() && needs.verify-published.result == 'success' }}"
        ),
    ),
    "a rehearsal crates publish": (
        "tag only",
        job_if("publish-crates", "needs.guard.outputs.channel == 'pypi'"),
    ),
    "a disjunction before the tag condition": (
        "tag only",
        job_if("attest", "always() || false && github.event_name == 'push'"),
    ),
    "a disjunction before the channel condition": (
        "channel",
        job_if(
            "publish-pypi",
            "inputs.x || needs.guard.result == 'success' && " + FINAL,
        ),
    ),
    "a writing job without a condition": (
        "tag only",
        lambda w: w["jobs"]["sdist"]["permissions"].update({"contents": "write"}),
    ),
    "a candidate to PyPI": ("channel", job_if("publish-pypi", CANDIDATE)),
    "a final to TestPyPI": ("channel", job_if("publish-testpypi", FINAL)),
    "a candidate crate": ("channel", job_if("publish-crates", CANDIDATE)),
    "a PyPI job in the TestPyPI environment": (
        "environment",
        lambda w: w["jobs"]["publish-pypi"]["environment"].update({"name": "testpypi"}),
    ),
    "a crates job in the PyPI environment": (
        "environment",
        lambda w: w["jobs"]["publish-crates"]["environment"].update({"name": "pypi"}),
    ),
    "a TestPyPI job uploading to PyPI": ("registry", upload("publish-testpypi", None)),
    "a PyPI job uploading to TestPyPI": (
        "registry",
        upload("publish-pypi", UPLOADS["publish-testpypi"]),
    ),
    "a crates publish before PyPI": (
        "order",
        lambda w: w["jobs"]["publish-crates"].update({"needs": ["guard", "attest"]}),
    ),
    "a PyPI publish before the attestation": (
        "order",
        lambda w: w["jobs"]["publish-pypi"].update({"needs": ["guard"]}),
    ),
    "a missing TestPyPI job": ("publisher", lambda w: w["jobs"].pop("publish-testpypi")),
}


def self_test() -> int:
    failures = []
    workflow = yaml.safe_load(RELEASE.read_text(encoding="utf-8"))
    if problems := check(workflow):
        failures.append(f"the tracked release.yml fails: {problems}")
    for name, (guard, mutate) in MUTANTS.items():
        mutant = copy.deepcopy(workflow)
        mutate(mutant)
        if not any(problem.startswith(f"{guard}:") for problem in check(mutant)):
            failures.append(f"{name} passed the {guard} guard")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    if parser.parse_args().self_test:
        return self_test()
    problems = check(yaml.safe_load(RELEASE.read_text(encoding="utf-8")))
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
