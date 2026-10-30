---
name: verify
description: Select and run the repository's verification gates for the current diff. Use before
  reporting work done, before a commit, and after editing the agent setup.
---

# Verify

Run `.claude/skills/verify/scripts/verify.py --dry-run` for selected gate names and triggering
paths; run without `--dry-run` to execute them. `gates()` in that script owns path selection.
`--all` selects all gates, `--base REF` changes the merge base, `--self-test` checks selection.
The default diff includes commits, staged/unstaged edits and untracked files against the merge
base with `origin/main`; DCO always judges `origin/main..HEAD`.

Gate names normally run `make <name>`, using CI's targets. Exceptions:

- `rust`: `make fmt clippy` plus policy lints on changed lines;
- `fixtures`: `.claude/skills/add-fixture/scripts/fixtures.py check`;
- `mirrors`: `.claude/skills/change-core-api/scripts/mirror_drift.py`;
- `public`: added-line privacy checks; `commits`: `make dco` plus commit-message checks.

Exit 0 means all selected gates passed, 1 a failure, 3 an unavailable check. `NOT RUN` is never
a pass: install required tools and rerun. `make tools` fetches pinned cargo tools; fuzz uses
`fuzz/rust-toolchain.toml`. Follow `AGENTS.md` and ADR 0004 for final CI and gate changes.
Keep mutant runs isolated, or restore and diff the tree before committing.

Without `GH_TOKEN`, `actions` checks offline; impostor commits remain unchecked until CI.
`make licences` may rewrite `THIRD-PARTY-LICENSES` before failing; a read-only reviewer uses
`--dry-run` and read-only gates by hand.

CI's `rust-arm` job, LibreOffice/pandoc fixture regeneration, fuzz campaigns and the samples
sweep without `samples/` are outside these gates. Report them when the change depends on them,
alongside each executed check's result and every NOT RUN with its reason.
