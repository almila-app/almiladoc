---
name: verify
description: Select and run the repository's verification gates for the current diff. Use before
  reporting work done, before a commit, and after editing the agent setup.
---

# Verify

- Plan: `.claude/skills/verify/scripts/verify.py --dry-run`; execute without `--dry-run`.
  `gates()` owns selection; `--all` selects every gate, `--base REF` changes base,
  `--self-test` tests selection. Default: merge base with `origin/main`, all local
  changes/untracked;
  DCO judges `origin/main..HEAD`.
- Gates run `make <name>` except `rust` (fmt/clippy and changed-line policy lints), `fixtures`
  (`.claude/skills/add-fixture/scripts/fixtures.py check`), `mirrors`
  (`.claude/skills/change-core-api/scripts/mirror_drift.py`), `public` (added-line privacy),
  `commits` (DCO and message checks).
- Exit: 0 pass, 1 fail, 3 unavailable. NOT RUN never passes; install missing tools and rerun.
  `make tools` fetches pins; fuzz toolchain: `fuzz/rust-toolchain.toml`; final CI: ADR 0004.
- Isolate mutants or restore/diff before commit; `make licences` may rewrite notices before failing,
  so read-only reviewers use dry-run/manual read-only gates. Actions without GH_TOKEN run offline.
- Report executed results and every NOT RUN/reason; excluded checks: CI rust-arm, LibreOffice/pandoc
  regeneration, fuzz campaigns and samples sweep without `samples/` when relevant.
