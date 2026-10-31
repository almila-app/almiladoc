# Roadmap

The open work, in order. Decisions are in [`docs/adr/`](adr/). The change that lands an item
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
  Two mechanisms for one behaviour (an up-front refusal and an in-flight check) hide each other's
  mutants, so each gets a test that alone kills its own.
- **Size.** At most about 800 hand-written changed lines; fixtures, snapshots, lockfiles and
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
- **Refactors.** Pure refactors preserve Markdown, snapshots, diagnostics, errors, charges, the public
  API and the binding. Necessary behavior changes have explicit acceptance and regression tests;
  they are not disguised as cleanup or deferred solely to preserve the old design. Tests keep their
  cases and the defects they catch; a verbose test becomes a table without weakening it.
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
| W7 | unpdf-clip (on the unpdf release that fixes hidden raster gaps) | |

Prove the native PDF API and its real-consumer integration together before publication; the first
release is next.

## Items

### W7

#### unpdf-clip: count only unpaintable runs the page shows
- **Goal:** a filled page reports no loss for an unpaintable run hidden by its clip or page box.
- **Scope:** take the unpdf release whose `RasterGaps` counts only runs a page shows. A filled page
  whose unpaintable run is clipped away or off the page reports no `raster_incomplete`, and
  ADR 0002's consequence on it goes.
- **Files:** `Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.lock`, `tests/it/raster.rs`,
  `THIRD-PARTY-LICENSES`, ADR 0001 decision 6, ADR 0002.
- **Credit:** none.
- **Depends on:** that unpdf release. **Size:** about 40 lines. **Tier:** T2.

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
- **ocr-lines** (`GapFill::Lines`: located lines grouped into paragraphs in the crate). Trigger: a
  caller whose engine returns only located lines.
- **ocr-http-roots** (caller trust anchors and an explicit proxy for `OcrHttp`). Trigger: a caller
  behind a private CA or an explicit proxy.
- **ocr-local-pipeline** (offline OCR as a separate crate wrapping oar-ocr or ocrs, never in this one,
  ADR 0002 decision 8). Trigger: demand for offline OCR, the rten pool fixes and a scan corpus. It
  returns lines in reading order across columns, not the engine's row-first order: a two-column fixture
  reads `L1, L2, R1, R2`.

## External tracks

For rten: upstream first, then wait a week for the maintainers, then decide: merged, take the
release; silent or declined, fall back as its bullet says. unpdf's pull requests are checked at the
start of each wave, rten's when ocr-local-pipeline is next. Check a pull request with
`gh pr view <n> --repo <repo> --json state,mergedAt,reviewDecision`.

- **unpdf** (ADR 0001 decision 6): unpdf 0.51.0 carries every merged pull request below and
  exposes the paint budget; unpdf-clip awaits hidden raster gaps being excluded.
- **rten** (dormant; gates only the deferred local OCR). rten's contribution policy has this
  repository's maintainer open and answer its pull requests. If they stay unmerged, rten is forked
  and published under a new name.
- **Consumer proof** (a change in the primary consumer's own repository). Trigger: the `almiladoc`
  candidate wheel on TestPyPI or on PyPI. It proves the consumer depends on that wheel
  instead of the converter it replaces, a test asserts `importlib.metadata.version("almiladoc")`
  equals the candidate and that no import of that converter remains, and its suite is green. The
  first run certifies every format, PDF through the model and the OCR stack on the consumer's own
  endpoint included, and the `RasterSpec` and `temperature` defaults follow what it measures there
  before the first final tag; each later 0.x minor reruns it, and 1.0 needs it green on the 1.0
  candidate.

| Repository | Pull request | Serves | State |
|---|---|---|---|
| robertknight/rten | [#1523](https://github.com/robertknight/rten/pull/1523) | the global pool only when no custom pool is set | merged |
| robertknight/rten | [#1524](https://github.com/robertknight/rten/pull/1524) | load-time work on the caller's pool | closed for issue [#1533](https://github.com/robertknight/rten/issues/1533); the maintainer asked for the use context there |
| robertknight/rten | [#1525](https://github.com/robertknight/rten/pull/1525) | pool build failure visible to the caller | closed: its question on the use context went unanswered |
| iyulab/unpdf | [#11](https://github.com/iyulab/unpdf/pull/11) | ligature glyph names in `/Differences` read as their letters (`handmade-ligatures.pdf`) | merged |
| iyulab/unpdf | [#12](https://github.com/iyulab/unpdf/pull/12) | superscript, subscript and struck runs (`text.pdf`) | closed: reworked by the maintainer, in 0.49.0 |
| iyulab/unpdf | [#13](https://github.com/iyulab/unpdf/pull/13) | page facts: invisible, painted, margin and clipped text, blank runs, fills | in 0.49.0 |
| iyulab/unpdf | [#14](https://github.com/iyulab/unpdf/pull/14) | brackets mirrored in right-to-left runs (`handmade-arabic.pdf`) | merged |
| iyulab/unpdf | [#15](https://github.com/iyulab/unpdf/pull/15) | a number set against its first word opens a list item (`text.pdf`) | in 0.49.0 |
| iyulab/unpdf | [#16](https://github.com/iyulab/unpdf/pull/16) | `unmapped_codes` for codes a font draws but gives no character (`text.pdf`'s false `binary_density`) | in 0.49.0 |
| iyulab/unpdf | [#17](https://github.com/iyulab/unpdf/pull/17) | a glyph drawn into a gap of an earlier run reads there (`text.pdf`'s clef) | in 0.49.0 |
| iyulab/unpdf | issue [#18](https://github.com/iyulab/unpdf/issues/18) | the bundled Adobe CMap data's BSD-3-Clause notice | fixed in 0.49.0 |
| iyulab/unpdf | [#19](https://github.com/iyulab/unpdf/pull/19) | the lines beside a column gutter counted by baseline (`handmade-legacy-cmap.pdf`) | merged |
| iyulab/unpdf | [#20](https://github.com/iyulab/unpdf/pull/20) | codes a composite font's ToUnicode leaves out, read through its own data or counted (`undecoded_text_codes`) | in 0.50.0 |
| iyulab/unpdf | [#21](https://github.com/iyulab/unpdf/pull/21) | unreadable runs counted only where the clip shows them | in 0.51.0 |
| iyulab/unpdf | to open | a page fact counting the bare page numbers dropped from a page's margins, so `footer_omitted` reports them | to open |
| iyulab/unpdf | to open | link annotations read as links on the text they cover (`text.pdf`) | to open |
| iyulab/unpdf | [#24](https://github.com/iyulab/unpdf/pull/24) | `RasterGaps` counts only the unpaintable runs a page shows (unpdf-clip) | open |
| iyulab/unpdf | [#23](https://github.com/iyulab/unpdf/pull/23) | a paint budget each operation charges by the pixels it covers (raster-hook) | in 0.51.0 |

## Maintainer steps

Asked only when the item that needs them is next.

| Step | Needed by | When |
|---|---|---|
| An OpenAI-compatible endpoint serving the consumer's model: its base URL, model id and a key in a file outside every repository, for `scripts/ocr_live.py` on the five-page fixture; the key never reaches a log, CI or a commit | the first release | before the first candidate tag |
| After a green dispatched rehearsal of `release.yml`, the first candidate tag `v0.1.0-rc.1`, approved in the `testpypi` environment; before the first final tag, the crates.io `publish-new` token as the `crates-io` environment secret `CRATES_IO_PUBLISH_NEW_TOKEN`; the final 0.x tag after a green consumer proof, its row then added to `docs/release-dates.tsv` before the next final (ADR 0003 decisions 7 and 9) | the first release | as each tag is due |
| Agree with the consumer the PDF-path scope, how it maps `Incomplete` and `Internal`, its two-phase transport, prompt and `concurrency` | consumer proof | when the proof starts |
| A scan corpus with paragraph ground truth and headings marked (the consumer's) | ocr-local-pipeline, ocr-lines | when either is next |
