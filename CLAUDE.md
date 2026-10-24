# almiladoc

Rust library that converts Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF to
GitHub-Flavored Markdown, with a Python binding (PyO3, maturin). Crate, PyPI distribution and
import package are `almiladoc`; the native module `almiladoc._almiladoc` is the unpublished
binding crate in `python/`, built through the root `pyproject.toml`. Public repository; licensing
is per file (NOTICE, REUSE.toml): the portions REUSE.toml marks MIT stay MIT, and this
repository's own work is BUSL-1.1.

## This setup

- `CLAUDE.md` and `.claude/` are tracked and public: they hold no machine or personal fact, which
  goes in `CLAUDE.local.md`. The local files (`CLAUDE.local.md`, `.claude/settings.local.json`,
  `.worktreeinclude`, Claude Code's `.claude/worktrees` and `.claude/agent-memory-local`) and every
  other assistant's files stay gitignored: never `git add -f` one. `/verify` checks both.
- Only the orchestrating agent edits the setup, and only after its own work verified; subagents
  report lessons. At a task's end, route what would have prevented a mistake to the cheapest
  owner: a repeated mistake becomes one line here; a file-type convention, a scoped rule; a
  recurring multi-step flow, a skill; a must-always-hold, a test, lint or check script; a machine
  or personal fact, `CLAUDE.local.md`. Edit, merge or delete before appending.

## Decisions

- `docs/adr/`, the invariants below and the rules bind until a better method rewrites or deletes
  one in the change that departs from it, with its reason; legal duties and the owner's decisions
  stay. ADR 0001 is the target design, reached in the order of its decision 19; the invariants
  describe the code as it stands.
- `docs/roadmap.md` is the open work, in order, and says how the change landing an item updates it.
- Every change lands as a candidate `land/owner/<slug>` (an accepted pull request as
  `land/owner/pr-<n>`) that `main` fast-forwards to once its `ci / required` passes (ADR 0004);
  `main` takes no merge commit.

## Invariants

- bytes → `Format` (`Input::new`'s checked label, else `Input::auto`: a content sniff in
  `src/formats/detect.rs`, then its fallback, Python's path extension; CSV is never sniffed) →
  `formats::parse` → frontend → `model::Document` → `render::markdown`. A PDF page that needs OCR
  is a `Block::Gap`; others map as usual, bar running heads and pictures (`image_omitted`).
- Frontends resolve style cascades (tri-state `shared::delta::StyleDelta` into `Style`),
  numbering and list labels, relationships and fields. Note references and internal anchors stay
  ids for the renderer, which alone decides Markdown syntax. Images retain their source locations
  and asset bytes; the renderer applies `Config::images` to their output and loss reporting.
- `Err` means no complete conversion was possible; recoverable quirks are repaired or skipped,
  and each skip is reported as `ParseCtx::emit(DiagnosticCode, Region)` in the current unit, by
  the `DiagnosticCode` in `src/diagnostics.rs` whose doc names that loss; the crate has no
  logging facade. A `Lost` diagnostic fails the conversion `Incomplete` under the default
  `OnLoss::Fail`, so a site that emits for text never rendered (hidden, dropped by policy, a
  field instruction, an unused definition) is a bug;
  content the file declares but that went unread counts at least 1 (`emit_count` drops a 0).
  Judge damage where its text shows: take display decisions on marker-free values, report on
  the branch that emits the text, and keep damage as offsets judged on the shared parser's
  spans, never as marker characters inside text a parser tokenizes later.
  Code that swallows an error still propagates `Err(e) if e.is_fatal()`: `ResourceLimit`,
  `Cancelled` and `Internal` are fatal and hard-fail everywhere, optional parts and OLE streams
  included.
- Under `OnLoss::Fail` any gap not filled, unfilled or failed, fails `NeedsOcr { pages (1-indexed),
  page_count }` before any `Incomplete`; `Report`'s markers and counts are in `docs/support.md`.
- Limits come from `Config` through the one `ParseCtx` (`src/ctx.rs`) each frontend is handed
  as `&ParseCtx`; per-parse state in it is a `Cell` behind `&self`. `Limits::DEFAULT`
  (`src/limits.rs`) holds the defaults. Bytes are charged where consumed, and every unit of work
  goes through `ParseCtx::charge` (`max_work` fuel, then the `CancelToken`); work that grows
  with text per reference (a link label, a lookup by key) is charged by length or done once per
  distinct target: a resolver's step is priced by its key's length, and what it calls on each
  uncached step is memoised per target. A cache shared across references charges each reuse by
  what it walks or copies (every owned string of the copied types, not only text), and each
  reuse reports its loss in its own unit and region. Every recursion over
  input-controlled nesting needs a cap: `max_nesting_depth` bounds package XML only, and the
  renderer recurses with none of its own. A fallback for input past a cap never re-parses or
  copies the uncapped input, and judges loss only on the part the fallback actually drops.
- No `static`, `LazyLock` or `OnceLock` outside the pool, binding, OCR adapters and test code
  (ADR 0001 decisions 14 and 18).
- Tables come only from `GridBuilder` (or `Table::from_rows`); assets only through `AssetSink`.
- Package layer: Strict OOXML URIs are rewritten to Transitional; `mc:AlternateContent` stays in
  the tree and readers choose with `shared::mc::alternate_branch`; OOXML `r:` attributes need
  `attr_qualified`; `path::resolve` resolves a target against the part that holds it.
- Reuse `src/shared/` before writing a reader-local helper. xls and xlsb reuse `build_table`,
  `render_number` and `resolve_format` from `src/formats/sheet/grid.rs`; a fix to one
  spreadsheet reader is checked in the other two.
- Style cycles: docx and ODF fail with `Malformed`; DOC and RTF warn and keep the acyclic prefix.
- `ConvertError::code()` and `UnsupportedKind::as_str()` strings are snake_case public contract
  like the diagnostic codes: never rename one after the first release.
- The model reaches Python only as `Parsed::to_json`; the binding reads `Format` through `FromStr`
  and hand-mirrors `ConvertError`, `Config`, `Limits`, `OnLoss` and `Converter`'s calls. Change
  any of them, or the model's JSON, through `/change-core-api`.

## Commands

```sh
make ci                                         # every gate CI runs: the pre-flight
make fmt clippy test                            # any gate alone is a target of its own
cargo test --locked --test it corpus            # every fixture in one test, stops at a diff
cargo test --locked --test it -- --ignored      # ./samples/ sweep: fails when absent
```

- Gate targets: `fmt clippy test doc features python policy locks licences reuse hygiene
  actions dco fuzz setup`; `make ci` is their union, and `.github/workflows/gates.yml`
  runs them as the jobs behind `ci / required`. A target the host cannot run prints
  `NOT RUN <target>: <reason>` and exits 0, or 1 under `CI=true`.
- Gate selection: `.claude/skills/verify/scripts/verify.py` (`/verify`). Fixtures and their
  snapshots: `.claude/skills/add-fixture/scripts/fixtures.py` (`/add-fixture`).
- Python tests run only through `make python`, against the wheel built from the sdist and
  installed per CPython; `python/almiladoc/` alone has no native module.
- Fuzzing, and refreshing the tracked `fuzz/Cargo.lock` that `make locks` reproduces:
  `fuzz/README.md`.

## Traps

- A change that adds a file updates REUSE.toml: the new file joins the `BUSL-1.1` list. A
  file in an MIT-bearing annotation stays there, with every copyright line, while it exists, and
  each such annotation keeps `precedence = "override"`; `reuse lint` accepts a wrong expression,
  so `make dco`'s licence guard (`.github/scripts/dco.py`) holds this. A file edited while its
  annotation is `MIT` joins the `BUSL-1.1 AND MIT` list, keeping its lines and adding the
  licensor's.
- `git commit -s`: subject at most 72 columns, a few-line body on what changed and why, no gates or
  process narrative; `Co-authored-by` goes in the final trailer block with `Tier`, `Signed-off-by`.
- Never put `[tool.uv] required-version`, or any setting that binds every uv call, in
  `pyproject.toml`: it breaks the maintainer's tooling run inside the checkout. Pin uv where the
  gates run (the Makefile's `UV_VERSION`, `setup-uv`'s `version`).
- `uses: $/.github/workflows/gates.yml` is GitHub's valid self-repository form for a reusable
  workflow (GitHub changelog); keep it.
