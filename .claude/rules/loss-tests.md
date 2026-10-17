---
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
---

# Loss tests

- A match arm that passes on several kinds of loss gets one reporting case per kind, each held
  by a probe that drops that kind. A case that reports nothing (empty, hidden, blank) cannot
  catch a dropped kind.
- Give two fields of a test input different values, or the test cannot tell which one the code
  reads.
- A link-target test uses a label only the decoded target satisfies (`www.example.org`), never
  an absolute URL or a label that never matches.
- A test helper writes the records the producer writes around the one under test: Excel puts an
  MSODRAWING holding OfficeArtClientTextbox between an OBJ and its TXO, and a helper without it
  hides a dropped caption.
- A table row meant to prove one check asserts that check's reason, not only the error kind: a
  row can fail for an unrelated reason and still pass.
- Every path that cannot place a cell (past the grid, a reference that does not parse, before
  any row header) treats an unread value like shown text, with one case per path.
- A count or charge carried up from nested content is tested at two levels of nesting at least:
  one level exercises only the leaf, and a carry that drops what came from below still passes.
- A test of a cost the old code never charged cannot fail a work bound on the old code: charge the
  cost, so the bound fails without the fix, or hold the mechanism itself (a lookup count, shared
  storage) and prove a mutant fails it.
