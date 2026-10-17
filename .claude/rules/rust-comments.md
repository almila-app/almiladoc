---
paths:
  - "**/*.rs"
---

# Rust comments

Default to no comment: names, types and tests carry intent. A `//` comment is allowed only for:

- a trap: a hazard the next change would trip that the code cannot show;
- an external constraint: a spec section, a producer's quirk or an upstream defect, cited;
- a tool directive, with its reason.

Never narration, banners, change history, commented-out code or a restated signature. A change
that touches a commented line fixes or deletes the comment.

- Silence a lint with `#[expect(lint, reason = "...")]`, never `#[allow]`; a lint that fires on
  some targets only takes `#[cfg_attr(<cfg>, expect(...))]`.
- Never write `unsafe`: the workspace lints forbid it (`unsafe_code`), and fuzz targets need none.
- A public item gets a short one-sentence `///` summary; `missing_docs` is on in the core crate.
  A private item gets `///` only for a contract its types cannot carry (a precondition, a unit,
  a panic); a doc line that restates its name or body is narration.

`/verify` lints these on changed lines.
