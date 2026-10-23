# Roadmap

The open work, in order. Decisions bind from [`docs/adr/`](adr/). The change that lands an item
deletes its section and its place in the Waves table; a part that lands narrows the section to what
remains; a change that finds an item or wave wrong corrects it in the same commit. Item ids and wave
names stay in this file: rustdoc, README, commit messages and test names carry none. "The consumer"
is the primary consumer, a Python service that depends on the wheel.

## Rules every item follows

- **Green.** An item leaves `make ci` green.
- **Test-first.** Each new test fails before its change. A limit or contract test holds every site
  that reads the limit or passes its context onward, one case per site, each proved by a mutant that
  swaps in the default. An abuse input counts only when it trips its limit at default `Limits`; a
  lowered-limit test proves a charge site only. An item that starts enforcing a `Limits` field owns
  that field's rustdoc; one that commits an abuse fixture owns its row in `tests/it/limits.rs`.
- **Size.** At most about 1500 hand-written changed lines; fixtures, snapshots, lockfiles and
  generated text do not count. A larger item splits where its section says, else at its natural seam.
- **One writer per file per wave.** Parallel items of a wave never edit the same file; serial items
  may. Union files: the two path lists of `REUSE.toml` and the `mod` lines of `tests/it/main.rs`. A
  rebase conflict inside those keeps both sides; any other conflict stops the landing.
- **Manifests.** `Cargo.toml`, `Cargo.lock`, `python/Cargo.toml` and `fuzz/Cargo.*` change in one item
  at a time: items of a wave that write them run serially, since a lock regenerates whole.
- **Python.** One item per wave touches `python/`. A change to `Format`, the model, `ConvertError` or a
  public function goes through `/change-core-api`. The model reaches Python only as JSON, and
  `JSON_SCHEMA_VERSION` bumps on a removal, rename or meaning change.
- **Fixtures.** Through `/add-fixture`. Each `tests/fixturegen/<format>.py` has one owner per wave. An
  abuse fixture is named `<limit>--<shape>--errors.<ext>`; an input over 1 MiB is generated inside its
  test; a container of a format the item does not own is built in the test with `zip::ZipWriter` or
  `cfb`. No third party's bytes are committed. An item changes only the snapshots of formats it owns
  or its section names; diagnostics snapshots are separate files.
- **Support page.** A change to what a reader reads, skips or reports corrects `docs/support.md` in
  the same commit; `make hygiene` checks its code set both ways, not each format's attribution.
- **Diagnostics and model.** A new diagnostic code is declared once in `src/diagnostics.rs` with its
  `LossClass`. No public diagnostic code ships before a site emits it: a model type, field, limit or
  code arrives in the item that first emits it, never earlier, and model additions are additive on
  `#[non_exhaustive]` types.
- **Refactors.** Byte-identical: Markdown, snapshots, diagnostics, errors, charges, the public API and
  the binding. A cleanup that would change one is skipped and becomes a defect item; a test keeps every
  case and every mutant it kills, and a verbose one becomes a table, never weaker.
- **Tier.** The change's risk for its `Tier` trailer (ADR 0004 decision 9): T0 mechanical (a
  lockfile), T1 prose that decides nothing or a new test, T2 ordinary code, T3 a public contract, a
  gate or the design.
- **Latest stable.** Every dependency, toolchain, runner and tool moves to its latest stable release
  as soon as it is released (ADR 0003 decision 8). A green Dependabot update is adopted at once,
  outside the waves, by a signed-off T0 commit, and its pull request is closed with a link to that
  commit.

## Waves

`∥` runs in parallel on disjoint files; `then` is serial and binds tighter. An item outside the
table lands on its own, before the first wave whose files it shares.

| Wave | Items | Placement |
|---|---|---|
| W7 | pdf-model (after the unpdf release carrying almiladoc's PDF pull requests) | |
| W8 | ocr-channel | |

Prove the native PDF API and its real-consumer integration together before publication.

## Items

### W7

#### pdf-model: PDF through the model, with page verdicts and page gaps
- **Goal:** PDF gets the same escaping, link policy, limits and diagnostics as every other format; a
  scanned page becomes a fillable gap, not an empty result.
- **Scope:**
  - unpdf's `Document` replaces pdf-inspector and the bypass in `src/converter.rs`,
    `src/formats/pdf.rs` and `src/formats/mod.rs` (ADR 0001 decision 6): default features off,
    `page_count` charged against `max_pages` before any page, then `for_each_page` with a literal
    sequential `PageStreamOptions`, charging each page. deny.toml's RUSTSEC-2026-0192 ignore goes
    with pdf-inspector if ttf-parser leaves the tree.
  - Each page calls `ctx.begin_unit(UnitKind::Page, index)?` (0-based page index) and maps its
    blocks into the model: headings, paragraphs, lists, tables through `GridBuilder` (row and column
    spans), styled runs into `Style` (bold, italic, strike, superscript, subscript); the renderer
    escapes their text as it does every format's. Running headers and folios are stripped across all
    pages, also when `ocr_pages` selects some.
  - The page verdict is almiladoc's rule over unpdf's page facts: Blank with no text, image, fill or
    undecodable stream; `Block::Gap { id: GapId { unit, ordinal }, origin: OcrOrigin::Page(n),
    reasons }` when a content stream is undecodable (`unreadable_content`), when no visible text
    survives over an image or over fill operators, or when the text is invisible; else Text, its
    suppressed runs and unreadable fonts counted as `garbled_font_encoding`.
  - `Converter::inspect(Input)` for PDF only (ADR 0001 decision 4), on the pool: container facts,
    diagnostics and 1-based page units, each `Text`, `Blank` (`no_text`) or `NeedsOcr { reasons }`; any
    other format is `Unsupported`; distinct from `package::directory::inspect`.
  - A placed figure renders nothing, adding `image_omitted`.
  - `Parsed::gaps()`; `parse` accepts PDF; `UnsupportedKind::PdfDocumentModel` goes.
  - Unfilled gaps: `Fail` raises `NeedsOcr { pages, page_count }` before `Incomplete`; `Report` writes
    stable `<!-- needs-ocr page N -->` and counts `ocr_gap`, which joins `src/diagnostics.rs`
    here (`Deferred`).
  - `ocr_pages: PageSelection { None (default), All, Pages(BTreeSet<NonZeroU32>) }`, a parse option
    (`Input::with_ocr_pages`): a selected page becomes a `Page` gap with reason `caller_requested`
    whatever its text layer; its extracted text is discarded, never merged, and reports no loss. A page
    number past `page_count` is refused before extraction as
    the new variant `ConvertError::InvalidConfig(ConfigError::PageOutOfRange { page, page_count })`
    (ADR 0001 decision 9), code `invalid_config`: one variant for every caller-configuration error,
    reusing `Converter::new`'s `ConfigError`, mirrored through change-core-api.
  - Binding via `/change-core-api`. Above 1500 lines, `ocr_pages` splits out.
- **Files:** `src/formats/{pdf,mod}.rs`, `src/model/gap.rs` (new), `src/model/{block,mod}.rs`,
  `src/render/markdown/mod.rs`, `src/inspect.rs` (new), `src/{converter,error,lib,identity}.rs`,
  `Cargo.{toml,lock}`, `deny.toml`, `about.toml`, `THIRD-PARTY-LICENSES`, `NOTICE`, `REUSE.toml`,
  `README.md`, `docs/support.md`, `CLAUDE.md`'s PDF invariants,
  `tests/fixturegen/pdf.py`, PDF fixtures and snapshots, `tests/it/pdf.rs` (new),
  `tests/it/{snapshots,converter}.rs`,
  `python/{src/lib.rs,almiladoc/_almiladoc.pyi,tests/test_almiladoc.py}`.
- **Tests:** five pages TEXT/IMAGE/TEXT/TEXT/IMAGE: `Fail` gives `NeedsOcr { [2, 5], 5 }` despite a
  `Lost`; `Report` keeps pages 1, 3 and 4. Pinned verdicts: blank, header-only, image-only,
  invisible text, at most 10 inkless characters, running-head stamped scan, tiled scan, glyphs drawn
  only by fills, an undecodable-only page, a text page with one undecodable stream. `<script>`,
  `1. x`, `[x](javascript:…)` and a forged marker render as DOCX does; sup, sub and strike; figures
  pass `Fail`; `ocr_pages` both ways, and a page past `page_count` refused before extraction with
  the ruled variant; a lowered `max_pages` trips; the five-page fixture's units are Page 1-5 in
  order, gaps included; a garbled-encoding page still adds `garbled_font_encoding` and stays Text; a
  page of text over a full-page image is Text; the ligature, Arabic and `text.pdf` snapshots keep
  their text; output is byte-identical across two runs as well as at 1 and 4 threads; inspected page
  verdicts and 1-based page indices equal the parse's units and gaps on the five-page fixture, and a
  DOCX gives `Unsupported`.
  - a fixture on a legacy predefined CMap (KSC-EUC-H with EUC-KR bytes and no ToUnicode), which
    pdf-inspector 1.25.2 cannot convert and unpdf's bundled CMaps can, joins the fixtures;
    `handmade-predefined-cmap.pdf`'s snapshot is pdf-model's to change.
- **Credit:** none.
- **Depends on:** the unpdf release carrying almiladoc's PDF pull requests (External tracks).
  **Size:** about 1400 lines. **Tier:** T3.

### W8

#### ocr-channel: fill page and image gaps with the caller's OCR
- **Goal:** the caller's OCR completes flagged pages and pictures in place.
- **Scope:**
  - `Parsed::fill_gap(&mut self, GapId, GapFill) -> Result<(), GapError>` and `fail_gap(GapId)`, on the
    pool; `GapState { Unfilled, Failed, Filled(Vec<Block>) }`; gaps indexed once.
  - `GapFill::Markdown`: `max_ocr_text_bytes` (1 MiB) checked first; a pulldown-cmark reader in the
    new `src/shared/markdown.rs` (all raw HTML escaped, `<sup>` included), tables, strikethrough,
    emphasis, headings, lists and code only; links as label, images as alt; each event charged;
    container plus fill depth within `max_nesting_depth`; NFC.
  - `GapFill::Lines(Vec<OcrLine>)`, `OcrLine { text: String, quad: Option<[Point; 4]>, confidence:
    Option<f32> }`, `Point { x: f32, y: f32 }`: `max_ocr_lines` (3000), NFC, grouped as below. The
    `OcrLine` rustdoc states the requirement on every engine: lines in reading order, each line's text
    in logical order (UAX #9), points in raster pixels of the raster the engine received (origin at its
    top-left corner, x rightward, y downward).
  - `GapError { UnknownGap, Settled, Convert(ConvertError) }`; a refusal changes nothing, fuel included.
  - Filled gaps render their blocks, a failed page `<!-- ocr-failed page N -->`; `render` counts
    `ocr_gap` and `ocr_failed` from state; `NeedsOcr` lists unfilled and failed pages. A gap's
    diagnostics take its container's region (Note inside a note, else Body); a page gap's are Body.
  - `ImageOcr { None, All }` beside `ImagePolicy`: each PNG or JPEG gains an `Image(AssetId)` gap after
    the innermost block holding the image, within that block's container (body, list item, quote, note
    or cell); other images count `image_not_ocred`. The image keeps rendering by its policy whatever its
    gap's state; an unfilled or failed gap renders nothing; a filled one shows both the picture and the
    text after its block. No image decoder.
  - **Line grouping** (`src/ocr/group.rs`), a pure, order-preserving, linear pass that reorders nothing:
    1. *Input.* The accepted lines after NFC and the limit checks, in engine order. In each line every
       `Cc` character (`char::is_control`, U+0085 included), U+2028 and U+2029 becomes U+0020, then the
       line's edge White_Space is trimmed (`str::trim`). A line then empty is blank and dropped, neither
       joining nor breaking. Confidence is ignored. No character is removed beyond the trim, so nothing
       is de-hyphenated.
    2. *Usable quad.* `top` is the least y of the four points, `bottom` the greatest, `left` and `right`
       likewise over x, `height` = bottom - top, `width` = right - left. A quad is usable when its eight
       coordinates are finite and its axis-aligned box has positive width and height (point order is
       irrelevant); otherwise the line is quadless. Arithmetic is f64 widened exactly from f32 and uses
       only min, max, subtraction, ×0.5 and comparisons, no platform float function, so ADR 0001
       decision 15's cross-architecture identity holds.
    3. *Break rule.* The previous kept line `a` and the next `b` join if and only if both quads are
       usable and `min(a.right, b.right) - max(a.left, b.left) >= 0.5 * min(a.width, b.width)`,
       `b.top >= a.top`, and `b.top - a.bottom <= 0.5 * min(a.height, b.height)` (inclusive). Negative
       gaps join, since detector boxes are expanded and overlap in tight leading. A quadless line is its
       own paragraph. Only neighbours are compared. Known cost: a heading closer than half the body
       line's height joins the next line.
    4. *Separator.* For a join, from `a`'s last and `b`'s first characters: none when `a` ends in U+002D,
       U+2010 or U+00AD and the character before it is Alphabetic (the hyphen stays: `well-` + `known`
       gives `well-known`); none when both lie in U+3000-303F, U+3040-30FF, U+3400-4DBF, U+4E00-9FFF,
       U+F900-FAFF, U+FF00-FF9F or U+20000-3FFFF (Hangul excluded, since Korean separates words);
       otherwise U+0020.
    5. *Order.* Multi-column pages whose rows align yield one paragraph per line, interleaved; no text is
       lost either way. Ordering columns is the engine adapter's job.
    6. *Placement.* The paragraphs replace the gap block in place, so fill order never changes output; an
       empty result leaves nothing; text never joins across gaps or with source text. `shape::loose`
       skips gap blocks, so unfilled gaps leave a list tight, and a filled gap's paragraphs
       count as blocks.
    7. *Table cells.* The paragraphs are cell blocks, so `render_cell` `<br>`-joins them; a paragraph's
       lines stay space-joined, no other `<br>` arises, and `|` is escaped. In a list inside a cell an
       item's blocks join with a space; a single-cell table renders as a one-cell table.
  - `image_not_ocred` and `ocr_failed` join `src/diagnostics.rs` here with their `LossClass`
    (`Omitted`, `Deferred`).
  - Python mirror via `/change-core-api`: `fill_gap(gap, *, markdown | lines)`.
  - Revisit the grouping rule if a scan corpus with paragraph ground truth shows single-column pages
    missing over 5% of breaks (or over 5% after headings), more than 2% of joins wrong, or 2° of skew
    missing 5 points more breaks than the upright page (oriented geometry is then owed). A change to
    grouping after release alters OCR Markdown, which ADR 0001 decision 15 allows only outside a patch
    release.
- **Files:** new `src/ocr/{mod,fill,group,images}.rs`, `src/shared/markdown.rs`;
  `src/shared/mod.rs`; `src/{converter,config,identity,lib,limits,diagnostics}.rs`,
  `src/model/gap.rs`, `src/render/markdown/{mod,shape,table}.rs`; `Cargo.toml` (unconditional
  `unicode-normalization` and `pulldown-cmark`), `Cargo.lock`, `fuzz/{Cargo.toml,Cargo.lock}`,
  `fuzz/fuzz_targets/gap_fill.rs` (new), `fuzz/seeds/gap_fill/` (new), `fuzz/regressions/gap_fill/`
  (new); `tests/it/ocr.rs` (new), `tests/fixturegen/docx.py`, snapshots;
  `python/{src/lib.rs,almiladoc/__init__.py,almiladoc/_almiladoc.pyi,tests/test_almiladoc.py}`;
  `README.md`, `REUSE.toml`.
- **Tests:**
  - A heading-and-table fill equals that Markdown born-digital; a `caller_requested` page renders only
    its fill.
  - `<script>`, `[a](javascript:x)` and `![a](http://x)` give escaped text, `a` and `a`; a fill of
    `x<sup>2</sup>` renders the tags escaped.
  - At defaults, 1 MiB + 1 bytes, 3001 lines and 257 nested `>` refuse as no-ops; refilling is `Settled`.
  - Unfilled image gaps equal `ImageOcr::None`; a cell fill gives `x<br>a b<br>c`; reverse fills keep
    order; EMF counts `image_not_ocred`.
  - `group` on fixed quads: a gap of 0.4h joins as `a b`; exactly 0.5h joins and `b.top` at the next f32
    above breaks; an overlap of exactly half the narrower width joins and `b.left` at the next f32 above
    breaks; `(0,0,101,20)` then `(100,0,200,20)` breaks; the narrow fragment `(0,0,100,20)` then
    `(96,0,104,20)` joins; `b` above `a` breaks; a 100 px heading over 20 px body breaks at a 15 px gap
    and joins at 5 px; a line skewed 2° over 2000 px joins its successor; a quadless line splits its
    neighbours; NaN, infinite and zero-height quads are quadless; empty and whitespace-only lines
    neither join nor break; `\n`, `\t` and U+0085 and U+2028 become spaces; `abc ` + `def` gives
    `abc def`; `日本 ` + `語` gives `日本語`, `日本` + `abc` gives `日本 abc`, `ＦＯＯ` + `ＢＡＲ` gives
    `ＦＯＯＢＡＲ`, Hangul keeps its space; `well-` + `known` gives `well-known`, `conver-` + `sion` gives
    `conver-sion`, `conver` U+00AD `sion` keeps the U+00AD, `5 -` + `7` and `1-` + `2` keep the space;
    side-by-side tall columns stay one paragraph each; aligned rows `L1, R1, L2, R2` give four paragraphs
    in that order; `L1, R1, L2, L3, R2` from unequal pitches joins `L2 L3` and keeps every character.
  - Proptests on integer coordinates: invariance under mirror `x → W - x`, translation and power-of-two
    scaling; flattening the paragraphs' lines equals the non-blank input after the mapping and trim, in
    order, and each paragraph is its lines joined by the separators; equal inputs give equal bytes.
  - A fake engine on generated DOCX: images in a paragraph, list item, heading, block quote and note,
    each filled text after its block in its container; a filled image under `Placeholder` renders
    `![alt](asset:N)` and then its text, under `Omit` the text alone; two images in one paragraph
    filled in reverse give document order; a nested cell; a list in a cell; a single-cell table;
    cell text `| <br> # h` renders escaped in one cell; under `Report` with no engine, unfilled image
    gaps give the `ImageOcr::None` Markdown, lists tight. A page gap is replaced in place; an empty fill
    leaves nothing; hand-run phases equal `convert`.
  - Pool threads equal `threads`; Python equals Rust; `gap_fill` replays.
- **Credit:** none; issue #157 moves to Taken.
- **Depends on:** pdf-model. **Size:** about 1100 lines. **Tier:** T3.

## Deferred

Each returns to the roadmap when its trigger fires, as a new item with its own section.

- **side-policies** (headers, footers, comments, rejected revisions and hidden structure as output
  policies). Trigger: the primary consumer asks for any of them. Design in outline: under an `Emit`
  policy headers and footers render in place as labelled blockquotes, each opening with a reserved
  `<a id>` that names its variant, and a model step keeps an item only when it changes what is in force
  for its region and variant; comments render as GFM footnotes in their own label space with replies
  inside their parent; page, page-count, date, time and file fields in headers and footers render fixed
  tokens such as `{page}` (Info); sheet print headers render one paragraph per non-empty section. Side
  content is inert to the body: every anchor inside it emits nothing and the body's anchor and note
  resolution never reads it; asset ids are assigned by one model pass in first-reference order (body,
  notes, side content, comments). `Omit` walks no side content and counts it from references it already
  reads. A `Reject` revision policy keeps deleted text and drops inserted text, joining paragraphs whose
  mark a revision removes. The first step writes the
  grammar down as an ADR 0001 amendment.
- **legacy-tables** (one run cursor in PPT's `paragraphs`, which reads character and paragraph runs
  through two). Trigger: a change in that code that needs it; a readability-only restructure risks a
  silent change. A per-axis merge enum for `CellProp` was judged and dropped: RTF allows all four
  combinations of the two bools.
- **document-meta** (document title, author and dates). Trigger: the consumer needs them; `DocumentMeta`
  lands with that producer (ADR 0001 decision 5).
- **py-ocr** (the parent process OCRs through Python). Trigger: an in-crate engine (rasterizer, ocr-http
  or the local crate) lands; the Python `fill_gap` and `fail_gap` mirror already ships with ocr-channel.
- **rasterizer** (with page-gap wiring). Trigger: a caller without its own rasterizer; the hayro items
  under External tracks gate it.
- **ocr-http** (OCR over the network inside the crate, an OpenAI-compatible vision adapter, ADR 0002
  decision 7). Trigger: a caller wants it. A plain-text reply is split at UAX #14 mandatory breaks into
  one quadless `OcrLine` per non-empty piece, in reply order, so each line is its own paragraph; a JSON
  `{"lines": [...]}` is taken as given, never split; the wire contract in the README says so.
- **ocr-local-pipeline** (offline OCR as a separate crate wrapping oar-ocr or ocrs, never in this one,
  ADR 0002 decision 8). Trigger: demand for offline OCR, the rten pool fixes and a scan corpus. It
  returns lines in reading order across columns, not the engine's row-first order: a two-column fixture
  reads `L1, L2, R1, R2`.

## External tracks

Upstream first, then wait a week for the maintainers, then decide: merged, take the release; silent or
declined, fall back as ADR 0001 decision 6 and ADR 0002 say. unpdf's pull requests are checked
at the start of each wave until pdf-model lands, and hayro's and rten's when the deferred item they gate
is next. Check a pull request with `gh pr view <n> --repo <repo> --json state,mergedAt,reviewDecision`.

- **unpdf release** gates pdf-model (ADR 0001 decision 6): iyulab/unpdf #11-#17 below, one concern
  each with its test, opened 2026-10-23, so the week ends 2026-10-30. The gate waits for all of
  #11-#17 but #15, a list fix it does not need; #13's page facts are what the verdict rule needs to
  tell a page that must go to OCR from one that reads. Issue #18 asks for the notice of the bundled
  Adobe CMap data. A pull request declined, or left a week without review, falls back to a published fork of
  unpdf.
- **hayro** (dormant; gates only the deferred rasterizer and local OCR). `LAB.icc`'s licence is stated
  upstream (#1497), so `NOTICE` cites it from the first hayro release that carries it. Font-loss
  warnings arrive with the first hayro release that carries #1498. Open: undecodable page content has
  no upstream warning, since upstream declined #1499 until it reworks warnings.
- **rten** (dormant; gates only the deferred local OCR). rten's AI policy bars agent-authored pull
  requests, so the maintainer opens and answers them. If they stay unmerged, an rten fork is
  published as ADR 0001 decision 6's unpdf fallback would be.
- **Consumer proof** (a change in the primary consumer's own repository). Trigger: the `almiladoc`
  candidate wheel on TestPyPI (release, W4) or on PyPI. It proves the consumer depends on that wheel
  instead of the converter it replaces, a test asserts `importlib.metadata.version("almiladoc")`
  equals the candidate and that no import of that converter remains, and its suite is green. The
  first run certifies the non-PDF formats and today's PDF path only; each
  0.x minor carrying pdf-model or ocr-channel reruns it, and 1.0 needs it green on the 1.0 candidate.

| Repository | Pull request | Serves | State |
|---|---|---|---|
| LaurenzV/hayro | [#1497](https://github.com/LaurenzV/hayro/pull/1497) | `LAB.icc`'s licence stated | merged |
| LaurenzV/hayro | [#1498](https://github.com/LaurenzV/hayro/pull/1498) | a warning when a font cannot be loaded | merged |
| LaurenzV/hayro | [#1499](https://github.com/LaurenzV/hayro/pull/1499) | a warning when a page content stream fails to decode | declined |
| robertknight/rten | [#1523](https://github.com/robertknight/rten/pull/1523) | the global pool only when no custom pool is set | merged |
| robertknight/rten | [#1524](https://github.com/robertknight/rten/pull/1524) | load-time work on the caller's pool | closed for issue [#1533](https://github.com/robertknight/rten/issues/1533); the maintainer asked for the use context there |
| robertknight/rten | [#1525](https://github.com/robertknight/rten/pull/1525) | pool build failure visible to the caller | closed: its question on the use context went unanswered |
| iyulab/unpdf | [#11](https://github.com/iyulab/unpdf/pull/11) | ligature glyph names in `/Differences` read as their letters (`handmade-ligatures.pdf`) | open |
| iyulab/unpdf | [#12](https://github.com/iyulab/unpdf/pull/12) | superscript, subscript and struck runs (`text.pdf`) | open |
| iyulab/unpdf | [#13](https://github.com/iyulab/unpdf/pull/13) | page facts: invisible, painted, margin and clipped text, blank runs, fills | open |
| iyulab/unpdf | [#14](https://github.com/iyulab/unpdf/pull/14) | brackets mirrored in right-to-left runs (`handmade-arabic.pdf`) | open |
| iyulab/unpdf | [#15](https://github.com/iyulab/unpdf/pull/15) | a number set against its first word opens a list item (`text.pdf`) | open |
| iyulab/unpdf | [#16](https://github.com/iyulab/unpdf/pull/16) | `unmapped_codes` for codes a font draws but gives no character (`text.pdf`'s false `binary_density`) | open |
| iyulab/unpdf | [#17](https://github.com/iyulab/unpdf/pull/17) | a glyph drawn into a gap of an earlier run reads there (`text.pdf`'s clef) | open |
| iyulab/unpdf | issue [#18](https://github.com/iyulab/unpdf/issues/18) | the bundled Adobe CMap data's BSD-3-Clause notice | open |

## Maintainer steps

Asked only when the item that needs them is next.

| Step | Needed by | When |
|---|---|---|
| Publish the unpdf fork's first version, only if unpdf declines a pull request, or leaves one a week without review (External tracks; a scoped crates.io token, ADR 0003 decision 9) | pdf-model | before pdf-model starts |
| After a green dispatched rehearsal of `release.yml`, the first candidate tag `v0.1.0-rc.1`, approved in the `testpypi` environment; before the first final tag, the crates.io `publish-new` token as the `crates-io` environment secret `CRATES_IO_PUBLISH_NEW_TOKEN`; the final 0.x tag after a green consumer proof, its row then added to `docs/release-dates.tsv` before the next final (ADR 0003 decisions 7 and 9) | the first release | now, then as each tag is due |
| Agree with the consumer the PDF-path scope and how it maps `Incomplete` and `Internal` | consumer proof | when the proof starts |
| A scan corpus with paragraph ground truth and headings marked (the consumer's) | grouping revisit, ocr-local-pipeline | when either is next |
