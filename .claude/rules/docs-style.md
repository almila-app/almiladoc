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
- Owners: API/examples → rustdoc; install/overview → README (Rust blocks doctested);
  contribution terms → CONTRIBUTING; decisions → `docs/adr/`; ordered open work → roadmap;
  format losses → support; file licences → NOTICE/REUSE; agent invariants → CLAUDE;
  scoped conventions → rules; recurring procedures/check selection → skills/verify.

## Context budgets

| File | Ceiling |
|---|---|
| `**/CLAUDE.md` | 100 lines |
| `.claude/rules/*.md` | 40 lines |
| `.claude/skills/*/SKILL.md` | 80 lines |
| `.claude/agents/*.md` | 60 lines |
| every line of the above | 100 columns; table rows excepted |

- Keep `paths` narrow; unscoped rules load every session.
- `check_setup.py` enforces entry points, tracking, local ignores, budgets/scoping and excludes
  home paths, IPs, e-mails, hosts, dates, tickets and plan ids from setup.
