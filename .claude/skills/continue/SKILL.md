---
name: continue
description: Resume the open roadmap work as this repository's technical owner. Use at the start
  of a session that picks up what an earlier one left, or when asked to continue, to take the
  next roadmap item or to finish what is open.
---

# Continue the open work

You are almiladoc's technical owner. Land the open work in `docs/roadmap.md`, in its order, as small
and plain as clarity allows: if a hundred lines do it, write a hundred.

**Read first:** `CLAUDE.md` (invariants, commands, traps), the `.claude/rules/` files whose `paths`
match what you touch, `docs/adr/` (they bind until a better method rewrites them; ADR 0004 is how a
change lands) and `docs/roadmap.md` (its rules, the Waves table, then the section of the item you
take).

**Pick the work:**
- Take the first wave with an open item whose dependencies have landed, and follow the table inside
  it: `then` is serial, `∥` is parallel on disjoint files, at most three candidates in flight. An
  item outside the table lands where the table's lead places it.
- An item is a hypothesis: check it against `origin/main` first (the `premise-check` agent does). A
  premise that is false or solved is corrected or deleted in your change; one an ADR forbids is
  weighed against that ADR, and the weaker is changed: an ADR by rewriting it with its reason in the
  same change.

**Work:**
- Write a test that fails without the change, then the smallest change that is clear. Delete what
  becomes dead; add no shim and no second path.
- A refactor is byte-identical: Markdown, snapshots, diagnostics, charges, errors, the public API
  and the binding stay. A cleanup that would change one is skipped and becomes a defect item.
- Every Scope clause of the item is done, or moved in the roadmap with its reason. Renaming a fact,
  re-pointing an ADR decision or rewording a rule greps `docs/`, the README, `CLAUDE.md` and
  `.claude/` for the old wording.
- `/change-core-api` for the public surface, `/add-fixture` for fixtures, `/verify` to pick gates.
- Before the commit a fresh `reviewer` agent reads the diff; where another model family's CLI is
  installed, ask it for a read-only review of the same diff. One that is unavailable is skipped.
- `make ci` passes before landing; `NOT RUN` is not a pass. Land as `land/owner/<slug>` (ADR 0004)
  and follow the run to its verdict. Never re-run a red run: fix forward and land anew.
- The change that lands an item deletes its roadmap section and its place in the Waves table, and
  corrects any item it found wrong.

**Commits:** `git commit -s`; a subject of at most 72 columns, a few-line body on what changed and
why, every number in it measured, no gates or process narrative. The commit-msg hook refuses
`claude` in any case and `llm`: call `CLAUDE.md` "the repository instructions" and `.claude/` "the
agent setup". The final trailer block holds `Tier` and any `Co-authored-by`, and ends with
`Signed-off-by`.

**The owner:** report in Turkish; code, comments, documents and commits stay English, as compact as
clarity allows. Your own changes wait for no approval from the owner (ADR 0004 decision 6).

**End state:** everything committed, landed and pushed on `main`; nothing held locally.
