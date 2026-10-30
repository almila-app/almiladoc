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

Write compact English only for a concrete next action. Link to a fact's owner instead of copying it.
Add docs only for essential guidance absent from code or existing docs; update stale docs with code.
Agent instructions carry no dates, tickets, incident history, reports or status; use issues and git.

| Fact | Owner |
|---|---|
| install, usage, API | rustdoc: API reference and examples; README: install and overview, its Rust blocks doctested |
| contribution terms | CONTRIBUTING.md |
| decisions | `docs/adr/` |
| open work, in order; what each reader leaves out, per format | `docs/roadmap.md`; `docs/support.md` |
| licence of each file | NOTICE, REUSE.toml |
| invariants, commands and traps for agents | AGENTS.md |
| a file type's conventions | a rule in `.claude/rules/` scoped to it |
| a recurring procedure; gate selection | its skill; `.claude/skills/verify/` |

## Budgets of the instruction files, which load into context

| File | Ceiling |
|---|---|
| `**/AGENTS.md` | 120 lines |
| `**/CLAUDE.md` | 5 lines |
| `.claude/rules/*.md` | 40 lines |
| `.claude/skills/*/SKILL.md` | 80 lines |
| `.claude/agents/*.md` | 60 lines |
| every line of the above | 100 columns; table rows excepted |

A rule's `paths` stay narrow, or it loads every session. `check_setup.py` enforces a tracked setup,
ignored local files, this table, scoping, no home path, IP, e-mail, host, date, ticket or plan id.
