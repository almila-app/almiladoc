---
name: continue
description: Resume implementation of the next ready item in docs/roadmap.md. Use for roadmap
  work, not for continuing an unrelated conversation.
---

# Continue roadmap work

Read `AGENTS.md`, the rules and Waves table in `docs/roadmap.md`, the selected item's section,
and ADR 0004 for commits and landing. They own development policy, item updates and publication.

- Select the earliest wave with an open item whose dependencies have landed; take its first ready
  item. Follow its ordering; place items outside the table as that table's lead directs.
- Validate the item against `origin/main` before implementation. Correct a false or solved item
  in the roadmap rather than implementing it.
- Account for every Scope clause: finish it or move the remaining work in the roadmap with its
  reason. A partial landing narrows the item; only a completed landing removes it.
- When renaming a fact or changing a rule, search `docs/`, README.md, `AGENTS.md` and `.claude/`
  for the old wording and correct affected references.
- Use the applicable repository skills, verification and independent review from `AGENTS.md`.
  Complete reviewed landing under ADR 0004, follow CI to its verdict and update the roadmap;
  report any work that remains unlanded.
