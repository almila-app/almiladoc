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

Write only what a reader needs for the next correct action. One fact has one owner; link to it,
never copy it. A change that invalidates a document corrects it in the same change, and touching
a document deletes what has gone stale in it. Instruction files (CLAUDE.md, `.claude/`) carry no
dates, ticket ids, incident history, session reports or status, which issues and git keep.

| Fact | Owner |
|---|---|
| install, usage, API | rustdoc: API reference and examples; README: install and overview, its Rust blocks doctested |
| contribution terms | CONTRIBUTING.md |
| decisions | `docs/adr/` |
| open work, in order; what each reader leaves out, per format | `docs/roadmap.md`; `docs/support.md` |
| licence of each file | NOTICE, REUSE.toml |
| invariants, commands and traps for agents | CLAUDE.md |
| a file type's conventions | a rule in `.claude/rules/` scoped to it |
| a recurring procedure; gate selection | its skill; `.claude/skills/verify/` |

## Budgets of the instruction files, which load into context

| File | Ceiling |
|---|---|
| `**/CLAUDE.md` | 120 lines |
| `.claude/rules/*.md` | 40 lines |
| `.claude/skills/*/SKILL.md` | 80 lines |
| `.claude/agents/*.md` | 60 lines |
| every line of the above | 100 columns; table rows excepted |

A rule's `paths` stay narrow, or it loads every session. `check_setup.py` enforces a tracked setup,
ignored local files, this table, scoping, no home path, IP, e-mail, host, date, ticket or plan id.
