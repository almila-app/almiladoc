---
name: verify
description: Select and run the smallest gates that can disprove the current change, and report
  what ran. Use before reporting work done, before a commit, and after editing the agent setup.
---

# Verify

Run `.claude/skills/verify/scripts/verify.py` from anywhere in the checkout: `--dry-run` prints
the plan, `--all` selects every gate, `--base REF` changes the base, `--self-test` checks the
selection below. The change is everything that differs from the merge base with `origin/main`:
commits, staged and unstaged edits, untracked files. Exit 0 means every selected gate passed, 1
that one failed, 3 that none failed but one did not run.

A gate runs the Makefile target CI runs wherever one exists (`make <target>` in the table).

| Change | Gate |
|---|---|
| anything | `.claude/skills/verify/scripts/check_setup.py` |
| Rust, Cargo files, toolchain, `rustfmt.toml` | `make fmt clippy`; then the policy lints of `.claude/rules/rust-comments.md` on changed lines |
| `src/`, `tests/`, Cargo files, toolchain | `make test` |
| Rust in `src/`, `python/src/` (features: `tests/` too), Cargo files, toolchain | `make doc`; `make features` |
| fixtures, their generator or sources | `fixtures.py check`: the generator reproduces each fixture |
| core API, `python/` | `mirror_drift.py` |
| `python/`, core API, `pyproject.toml`, `uv.lock`, `scripts/check_dist.py`, licence files, root Cargo files, toolchain, the fixtures `python/tests/` opens and their snapshots | `make python` |
| `fuzz/`, core API, root Cargo files | `make fuzz` |
| Cargo files, `fuzz/Cargo.*`, `deny*.toml` | `make policy` |
| Cargo files, `fuzz/Cargo.*`, `.github/scripts/locks.py`, `uv.lock` | `make locks` |
| Cargo files, `about.*`, `THIRD-PARTY-LICENSES` | `make licences` |
| any path | `make reuse`; `make provenance`; `make hygiene` |
| `.github/`, `uv.lock` | `make actions` |
| any added line | no home path, private IP, new e-mail, private host or ticket id |
| commits on the branch, `dco.py`, `uv.lock` | `make dco`; no assistant named; nothing private |

A `Makefile` change selects every make gate; `pyproject.toml` goes with `uv.lock`. `make dco`
judges `origin/main..HEAD`, as a candidate's run does, whatever `--base` says.

Never weaken a gate to pass. NOT RUN is never a pass: a make target the host cannot run prints
`NOT RUN <target>: <reason>`, and verify reports it so. When the change needs that gate, install
what it names (uv; `make tools` fetches the pinned cargo-deny, cargo-about and cargo-fuzz; fuzz
needs the nightly in `fuzz/rust-toolchain.toml`) and run it again. Without `GH_TOKEN`, `make
actions` runs zizmor offline and verify prints that note beside its PASS: impostor commits go
unchecked until CI. `make licences` rewrites a stale `THIRD-PARTY-LICENSES` before it fails, so
a read-only reviewer runs `--dry-run` and the read-only gates by hand.

Outside every gate: CI's `rust-arm` job, the LibreOffice and pandoc fixtures, a fuzz campaign,
and the samples sweep without `samples/`. Name them when a change depends on one, and report
each gate that ran with its outcome and every NOT RUN with its reason.
