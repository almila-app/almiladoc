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
| W7 | unpdf-clip (on the unpdf release that carries #21) | |
| W10 | ocr-engine | |
| W11 | rasterizer | |
| W12 | ocr-http | |

Prove the native PDF API and its real-consumer integration together before publication; the first
release follows W12.

## Items

### W7

#### unpdf-clip: take the unpdf release that counts only what the clip shows
- **Goal:** a run the clip hides entirely reports no loss (iyulab/unpdf #21).
- **Scope:** move `unpdf` to the first release carrying upstream 2418af6 and 6dbc96c; a
  `tests/it/pdf.rs` case: readable text beside a run clipped away entirely (`q 0 0 10 10 re W n … Q`,
  drawn far outside) that holds one code its ToUnicode leaves out converts under `OnLoss::Fail` with
  no diagnostic and gives only the readable text; the docs naming the version follow.
- **Files:** `Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.lock`, `tests/it/pdf.rs`, `THIRD-PARTY-LICENSES`,
  ADR 0001 decision 6.
- **Credit:** none.
- **Depends on:** that unpdf release. **Size:** about 40 lines. **Tier:** T2.

### W10

#### ocr-engine: an engine fills the gaps, in-process or across processes
- **Goal:** an engine fills gaps in-process or from exported requests (ADR 0002 decisions 1-3, 12).
- **Scope:** through `/change-core-api`:
  - `src/ocr/engine.rs`: `OcrEngine: Send + Sync`, `batch_size()` (default 1) and
    `recognize(&self, &[OcrRequest], &CancelToken) -> Vec<OcrOutcome>`; `#[non_exhaustive]`
    `OcrOutcome { Fill(GapFill), Failed(OcrFailure), Unattempted(OcrFailure) }` and
    `OcrFailure { status: Option<u16> }`.
  - `src/ocr/request.rs`, owned and `#[non_exhaustive]`: `OcrRequest { gap, origin, reasons, image,
    max_text_bytes }` (the parse's `max_ocr_text_bytes`) and `OcrImage { bytes: Arc<[u8]>,
    media_type }` (PNG or JPEG); an image request copies its asset at one unit per 16 bytes.
  - `Parsed::ocr_requests(&mut self, gaps: Option<&[GapId]>)`: the listed unfilled gaps' requests in
    document order; page gaps yield none until the rasterizer.
  - `Parsed::ocr(&mut self, &dyn OcrEngine, gaps) -> Result<OcrSummary, ConvertError>`: pooled
    batches of `batch_size`, each outcome a `fill_gap` or `fail_gap`; an `Unattempted` stops later
    batches; a `Refused` fill fails its gap, a wrong outcome count its batch.
  - `Converter::convert_with_ocr(Input, &dyn OcrEngine)`; `ocr_requests` and `ocr` check the
    `CancelToken` `Parsed` keeps.
  - Python: `Parsed.ocr_requests` and a pickling `OcrRequest`; a Python OCR fills the gaps through
    `fill_gap` and `fail_gap`, and Python takes no engine of its own.
- **Files:** new `src/ocr/{engine,request,driver}.rs`, `python/src/ocr.rs`; `src/ocr/`, `tests/`,
  `src/{converter,lib}.rs`, `python/`, `README.md`, `REUSE.toml`, ADR 0001, `CLAUDE.md`, `.claude/`.
- **Tests:**
  - A fake engine on the DOCX image fixtures: `ocr`, hand-run `ocr_requests` and `fill_gap`,
    `convert_with_ocr` and a Python loop over unpickled requests agree.
  - `batch_size` 3 over 7 gaps calls `recognize` with 3, 3 and 1 requests in order; a second batch
    answering `Fill`, `Unattempted`, `Fill` fills 4 and 6, never requests 7, and a later call fills
    5 and 7; 2 outcomes for 3 requests fail all 3; a 1 MiB + 1 byte fill fails its gap alone.
  - A cancel in `recognize` keeps earlier fills and refuses `render`; a panic gives `Internal`; a
    lowered `max_work` trips on image bytes; a request carries a raised `max_ocr_text_bytes`.
- **Credit:** none.
- **Depends on:** none open. **Size:** about 1000 lines. **Tier:** T3.

### W11

#### rasterizer: every page gap reaches the engine as an image
- **Goal:** a page gap's request carries its page as the parse read it (ADR 0002 decisions 5 and 6).
- **Scope:**
  - Feature `raster = ["pdf", "unpdf/raster", "unpdf/standard-fonts"]`, on by default. `Parsed`
    keeps unpdf's parser in an `Arc`; a page gap keeps its media box and rotation. Without it
    `RasterSpec` and `ocr_requests`' signature stay, and page gaps yield no request.
  - `RasterSpec { dpi: 150, max_pixels: 12_845_056 }` and `OcrEngine::raster()`; a dpi outside
    50-600 or `max_pixels` outside 1 to 40 Mpx (`MAX_PIXELS_CEILING`) fails `ConfigError::Raster`.
  - `ocr_requests(&mut self, &RasterSpec, gaps)`: per page gap the largest dpi up to the spec's
    whose rotated media box (an upper bound for the crop box painted), rounded up, fits
    `max_pixels`; one unit per 128 pixels; `render_page` (crop box, `/Rotate`, opaque white, RGB
    PNG); one unit per 16 PNG bytes; the effective dpi as `OcrImage::dpi: Option<u32>`, `None` for
    an embedded image; up to `threads` pages at once through a new `pool::map_ordered`, in order,
    the token checked per page.
  - Once a page gap is filled, unpdf's `RasterGaps` text runs, images, inline images and undecodable
    content streams report `raster_incomplete` (`Lost`) in its unit, shadings and soft masks
    nothing. `max_work`'s rustdoc states both rates and that they and the cap alone bound painting.
  - Python via `/change-core-api`: `ocr_requests(*, dpi=150, max_pixels=12_845_056, gaps=None)`,
    `OcrRequest.dpi`; fuzz target `pdf_raster` paints every page under `ocr_pages: All`; release
    smoke checks a page's PNG `IHDR`; NOTICE names the CC0 ICC profiles `raster` builds in.
- **Files:** new `src/ocr/raster.rs`; `src/ocr/`, `src/formats/pdf.rs`, `src/model/gap.rs`,
  `src/{converter,config,diagnostics,limits,lib,pool}.rs`, `Cargo.*`, `fuzz/`, `tests/`, fixtures,
  `python/`, licence files, `README.md`, `docs/support.md`, `.github/`, ADRs 0001 and 0003.
- **Tests:**
  - The five-page fixture exports pages 2 and 5 only, PNGs 1275 by 1650 at a stated 150 dpi;
    `ocr_pages: All` exports five; bytes are equal across runs and at 1 and 4 threads.
  - A 2725 by 2089 pt page renders within 12 845 056 pixels at a lower dpi; a `/CropBox` half its
    `/MediaBox` renders below the cap; `/Rotate 90` swaps the sides; dpi 49 and 601,
    `max_pixels` 1 048 575 and past `MAX_PIXELS_CEILING` are refused.
  - Under `ocr_pages: All`, `handmade-predefined-cmap.pdf` filled reports `raster_incomplete` and
    fails `Incomplete` under `Fail`; a scanned JPEG page and a mesh-shading page, filled, report
    none.
  - A cancel from another thread stops a 20-page export before the next page; a lowered `max_work`
    trips before painting and at the PNG charge; `convert_with_ocr` equals the hand flow; Python's
    `dpi=300` reaches its requests; `pdf_raster` replays.
- **Credit:** none.
- **Depends on:** ocr-engine. **Size:** about 900 lines. **Tier:** T3.

### W12

#### ocr-http: an OpenAI-compatible vision server fills the gaps, with no code
- **Goal:** a base URL, a model and a key let a vision server fill the gaps (ADR 0002 decision 7).
- **Scope:**
  - Feature `ocr-http`, on in the binding: ureq 3, latest stable (`rustls-no-provider`,
    `rustls-webpki-roots`), `rustls` on `ring`, `base64`, `serde_json`; `python/Cargo.toml` takes
    `log` with `max_level_off`.
  - `OcrHttpConfig`: `base_url`, `model`, `api_key`, `prompt` (its own one-sentence default, in the
    rustdoc), `image_first` (true), `extra_body`, `max_tokens` 8192, `temperature` 0.0, `raster`,
    `concurrency` 1 (`batch_size()` twice it), `retries` 3; `raster: RasterSpec` exists without
    the `raster` feature; `new` opens no socket and refuses a bad field, a reserved `extra_body`
    key or a key over `http://` beyond loopback.
  - A reply counts only as string `choices[0].message.content` with `finish_reason` `stop`; a
    leading `<think>…</think>` is stripped, an unclosed one fails the gap.
  - Resets before a response and 408, 429, 500, 502-504 retry after 0.5 to 30 s or `Retry-After` up
    to 60 s, which a cancel ends; 400, 413, 422, `length` and a malformed or refused reply fail the
    gap, other failures leave it and the rest of the batch unattempted, each with its status.
  - ureq: one agent built in `new` on the caller's thread, never on a worker, its proxy-environment
    read discarded by `proxy(None)`; timeouts connect 10 s, send request 60 s, send body 60 s,
    response 300 s and body 60 s, none for resolve or overall; ring as rustls' provider; no
    redirect; a body capped at its request's `max_text_bytes` + 64 KiB; errors mapped without
    `Display`.
  - `pool::fan_out` runs `min(concurrency, n)` scoped workers; `scripts/ocr_live.py` logs method,
    path, status and gap.
  - Python via `/change-core-api`: `OcrHttp(base_url, model, *, api_key=None, …)`, releasing the
    GIL, with `recognize(requests)` giving `str` or a pickling `OcrFailure(*, attempted=True,
    status=None)` each, so a parent fills a child's exported requests; `Parsed.ocr(engine)` with
    `OcrSummary`, and `Converter.convert(…, ocr=None)`.
- **Files:** `src/ocr/` (new `http/`), `scripts/`, `src/{pool,ledger,config,lib}.rs`, `README.md`,
  `Cargo.*`, `fuzz/`, `tests/`, `python/`, `deny*.toml`, `Makefile`, licences, `.github/`, ADRs.
- **Tests:** against a loopback server on port 0:
  - `<think>x</think>\n# A` fills `# A`; an unclosed think block, `null` content, `length` and a
    body past the cap fail the gap, a 1.5 MiB reply under a raised limit fills; the body holds the
    model, one `image_url` data URI and the prompt, image first unless `image_first = false`;
    `Authorization` only with a key.
  - 503 then 200 fills after 2 attempts; 400 fails after 1 with status 400; a 302 elsewhere is not
    followed and leaves its gap with status 302; a closed port leaves a batch of 4 unattempted after
    one attempt; `concurrency` 3 keeps at most 3 in flight; a cancel in backoff returns in 100 ms.
  - No failure, error or `Debug` holds an echoed key or prompt, or a URL's userinfo or query; `new`
    refuses `extra_body` holding `model`, `concurrency` 0 and a key over `http://` to 192.0.2.1, not
    to 127.0.0.1, `[::1]` or `localhost`; dead `HTTPS_PROXY` and `SSL_CERT_FILE` change nothing.
  - A cancel in a stalled response attempts no later request of its batch; no thread outlives
    `recognize`; a self-signed server leaves its gaps; Python equals Rust, a child's pickled
    requests filled by the parent's `OcrHttp` included; `ocr_reply` replays.
- **Credit:** none.
- **Depends on:** rasterizer. **Size:** about 1150 lines. **Tier:** T3.

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
- **ocr-lines** (`GapFill::Lines`: located lines grouped into paragraphs in the crate). Trigger: a
  caller whose engine returns only located lines.
- **ocr-http-roots** (caller trust anchors and an explicit proxy for `OcrHttp`). Trigger: a caller
  behind a private CA or an explicit proxy.
- **raster-hook** (painting charged per operation by the area it paints and cancelled inside a page,
  and counts of glyphs without an outline and of unpainted annotation appearances; needs `unpdf`
  changes). Trigger: a caller painting untrusted PDFs outside a memory-capped worker, or a measured
  paint cost.
- **ocr-local-pipeline** (offline OCR as a separate crate wrapping oar-ocr or ocrs, never in this one,
  ADR 0002 decision 8). Trigger: demand for offline OCR, the rten pool fixes and a scan corpus. It
  returns lines in reading order across columns, not the engine's row-first order: a two-column fixture
  reads `L1, L2, R1, R2`.

## External tracks

For rten: upstream first, then wait a week for the maintainers, then decide: merged, take the
release; silent or declined, fall back as its bullet says. unpdf's pull requests are checked at the
start of each wave, rten's when ocr-local-pipeline is next. Check a pull request with
`gh pr view <n> --repo <repo> --json state,mergedAt,reviewDecision`.

- **unpdf** (ADR 0001 decision 6): unpdf 0.50.0 carries every pull request below but #21, which is
  on its main for the next release; unpdf-clip takes that release.
- **rten** (dormant; gates only the deferred local OCR). rten's contribution policy has this
  repository's maintainer open and answer its pull requests. If they stay unmerged, rten is forked
  and published under a new name.
- **Consumer proof** (a change in the primary consumer's own repository). Trigger: the `almiladoc`
  candidate wheel on TestPyPI (after W12) or on PyPI. It proves the consumer depends on that wheel
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
| iyulab/unpdf | [#21](https://github.com/iyulab/unpdf/pull/21) | unreadable runs counted only where the clip shows them | on main |
| iyulab/unpdf | to open | a page fact counting the bare page numbers dropped from a page's margins, so `footer_omitted` reports them | to open |
| iyulab/unpdf | to open | link annotations read as links on the text they cover (`text.pdf`) | to open |

## Maintainer steps

Asked only when the item that needs them is next.

| Step | Needed by | When |
|---|---|---|
| An OpenAI-compatible endpoint serving the consumer's model: its base URL, model id and a key in a file outside every repository, for `scripts/ocr_live.py` on the five-page fixture; the key never reaches a log, CI or a commit | the first release | after W12, before the first candidate tag |
| After a green dispatched rehearsal of `release.yml`, the first candidate tag `v0.1.0-rc.1`, approved in the `testpypi` environment; before the first final tag, the crates.io `publish-new` token as the `crates-io` environment secret `CRATES_IO_PUBLISH_NEW_TOKEN`; the final 0.x tag after a green consumer proof, its row then added to `docs/release-dates.tsv` before the next final (ADR 0003 decisions 7 and 9) | the first release | after W12, then as each tag is due |
| Agree with the consumer the PDF-path scope, how it maps `Incomplete` and `Internal`, its two-phase transport, prompt and `concurrency` | consumer proof | when the proof starts |
| A scan corpus with paragraph ground truth and headings marked (the consumer's) | ocr-local-pipeline, ocr-lines | when either is next |
