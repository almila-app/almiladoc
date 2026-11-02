---
paths:
  - "*.md"
  - "*/README.md"
  - ".github/*.md"
  - ".github/ISSUE_TEMPLATE/**"
  - ".claude/**"
  - "docs/**/*.md"
---

# Documents

- Link canonical facts; add docs only for an essential action absent from source/existing docs.
- Instructions exclude dates, tickets, incidents, reports/status; use issues/git.
- Owners: usage/integration → `docs/guide/`; install/features/formats → README; reference →
  rustdoc and `.pyi`; contribution terms → CONTRIBUTING; decisions → `docs/adr/`; ordered open
  work → roadmap; format losses → support; file licences → NOTICE/REUSE; agent invariants →
  CLAUDE; scoped conventions → rules; recurring procedures/check selection → skills/verify.
- Guide page: H1, one purpose line, then task sections of bullets or tables, each with its
  defaults, errors and limits stated, and a copy-pasteable example; link support and rustdoc
  for the rest.
- Guide examples are executed (`python/tests/test_docs.py`, Rust doctests); a defaults table
  equals the code; inline code names only almiladoc API (`mirror_drift.py` checks names, links
  and the README format table). The README uses absolute links and stays within 120 lines.

## Context budgets

| File | Ceiling |
|---|---|
| `**/CLAUDE.md` | 100 lines |
| `.claude/rules/*.md` | 40 lines |
| `.claude/skills/*/SKILL.md` | 80 lines |
| `.claude/agents/*.md` | 60 lines |
| `docs/guide/*.md` | 180 lines |
| every line of the above | 100 columns; table rows excepted |

- Keep `paths` narrow; unscoped rules load every session.
- `check_setup.py` enforces entry points, tracking, local ignores, budgets/scoping and excludes
  home paths, IPs, e-mails, hosts, dates, tickets and plan ids from setup.
