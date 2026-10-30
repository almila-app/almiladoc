---
name: add-fixture
description: Add or change a test fixture and its snapshot. Use for every behaviour change that
  a document must prove, and whenever tests/gen_fixtures.py, tests/fixturegen/ or
  tests/fixture-src/ changes.
---

# Add a fixture

Fixtures are committed bytes under `tests/fixtures/<format>/`, built by the driver
`tests/gen_fixtures.py` from one module per format, `tests/fixturegen/<format>.py`, and
published with the repository: never a confidential document.

1. Write a generator function in the format's `tests/fixturegen/<format>.py` and call it from
   that module's `generate(out)`, or extend the one that already builds that kind of document;
   its malformed and abuse shapes live in the same module. A hand-built file in a format directory
   is named `handmade-*`. The LibreOffice and pandoc outputs (`text.*`, `sheet.*`, `pres.*`,
   `book.epub`) need those tools and are not regenerated here; `rtf/handmade-cocoa.rtf` has no
   generator.
2. Place it by what it proves; its extension must name its format, since a fixture its bytes
   cannot identify is parsed by its extension.
   - A format directory: the file is detectable from its bytes alone (`csv/` excepted). A new
     directory needs a row in `fixtures_detect_from_content` in `tests/it/snapshots.rs`.
   - `malformed/`: name the outcome, `name--errors|recovers|skips|ignores.ext`; `errors` must
     fail and the others must convert.
   - `abuse/`: must fail with `ResourceLimit`; it has no snapshot.
3. Run `.claude/skills/add-fixture/scripts/fixtures.py regen`, with `--keep PATH...` naming each
   committed fixture you meant to change, using repository-relative paths. Review or move existing
   pending snapshots first. If diagnostics intentionally disappear, review and delete only that
   fixture's diagnostics snapshot before regeneration. The helper accepts exact Markdown and
   diagnostics companions for new and kept fixtures; unrelated proposals fail and remain pending.
   A failed run restores the pre-run fixtures and accepted snapshots while keeping raw proposals.
4. Read every new snapshot: it is the expected output, so a wrong one bakes in a bug.
5. Declare each new fixture and snapshot, and each fixture and snapshot you changed, in REUSE.toml
   (AGENTS.md, Traps), then stage only those files, the generator and REUSE.toml. Never rename
   a fixture `python/tests/` opens by name; `verify` runs `make python` when one of them or its
   snapshot changes.

`fixtures.py check` regenerates into a scratch copy and fails on any fixture whose content
differs, or that no generator writes beyond the LibreOffice and pandoc outputs.
