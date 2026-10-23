---
paths:
  - "**/*.rs"
---

# Rust

`CLAUDE.md`'s invariants bind until a better method rewrites one; here: code shape, refactor safety.

Default to no comment: names, types and tests carry intent. A `//` comment is allowed only for
a trap the code cannot show, an external constraint cited (`[MS-DOC] 2.9.336`, a producer's
quirk, an upstream defect), or a tool directive with its reason. Never narration, banners,
change history, commented-out code or a restated signature; a change that touches a commented
line fixes or deletes the comment.

- Silence a lint with `#[expect(lint, reason = "...")]`, never `#[allow]`; a lint that fires on
  some targets only takes `#[cfg_attr(<cfg>, expect(...))]`; a change under one re-reads its
  reason.
- Never write `unsafe`: the workspace lints forbid it (`unsafe_code`), and fuzz targets need none.
- A public item gets a one-sentence `///` summary; `missing_docs` is on in the core crate. A
  private item gets `///` only for a contract its types cannot carry (a precondition, a unit, a
  panic), in two lines at most.
- A spec number is a named `const`, never a literal with a comment.
- A function fits in 60 lines; past that, split it at a named step, or name in its
  `too_many_lines` reason the dispatch or lookup table it is. Nest at most 6 blocks: flatten
  with `?`, `let ... else`, early `continue` and let chains.
- Write `Self` in an `impl`, `'_` for a lifetime elided in a path, `Rc::clone(&rc)`, a plain
  string unless it holds `"` or `\`, and parentheses around a shift beside `|` or `&`.
- Visibility is the narrowest that compiles; a module with children is `name/mod.rs`.
- Two or more `bool` parameters become an enum or a struct.
- A refactor never moves, merges, splits, reorders or resizes a `charge`, `emit`, `emit_count`
  or byte count, never turns a guarded `get(..)` into `[]`, and never turns an argument that
  charges, emits or fails lazy or eager (`unwrap_or(f())` and `unwrap_or_else(f)`).
- A refactor pins the behaviour it touches with rows in an existing case table, and closes a gap
  that predates it only where the change could break that path.
- Past `CLAUDE.md`'s fatal-error rule, an error is swallowed without the `is_fatal()` arm only
  where the callee returns no fatal variant (`path::resolve` returns only `Malformed`).

Clippy's workspace lints and `clippy.toml` hold what `-D warnings` can check; `/verify` adds
only its policy lints, on changed lines. A `clippy.toml` threshold binds only a lint that is on.
