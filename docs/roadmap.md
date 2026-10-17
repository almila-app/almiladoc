# Roadmap

The open work, in order. Decisions bind from [`docs/adr/`](adr/); upstream verdicts live in
[`upstream.md`](upstream.md). An item's section is deleted in the change that lands it, and a change
that finds an item or wave wrong corrects it in the same commit. Item ids and wave names stay in this
file: rustdoc, README, commit messages and test names carry none. "The consumer" is the primary consumer,
a Python service that depends on the wheel.

## Rules every item follows

- **Green.** An item leaves `make ci` green; sign-off follows `CLAUDE.md` and ADR 0004 decision 7.
- **Test-first.** Each new test fails before its change. A limit or contract test holds every site
  that reads the limit or passes its context onward, one case per site, each proved by a mutant that
  swaps in the default. An abuse input counts only when it trips its limit at default `Limits`; a
  lowered-limit test proves a charge site only. An item that starts enforcing a `Limits` field owns
  that field's rustdoc; one that commits an abuse fixture owns its row in `tests/it/limits.rs`.
- **Size.** At most about 1500 hand-written changed lines; fixtures, snapshots, lockfiles and
  generated text do not count. A larger item splits where its section says, else at its natural seam.
- **One writer per file per wave.** Parallel items of a wave never edit the same file; serial items
  may. Union files: the two path lists of `REUSE.toml`, the `[named]` table of
  `.github/provenance.toml` (single-line inserts in sorted position), the `mod` lines of
  `tests/it/main.rs`, and the `docs/upstream.md` rows of the items an item takes. A rebase conflict
  inside those keeps both sides; any other conflict stops the landing.
- **Manifests.** `Cargo.toml`, `Cargo.lock`, `python/Cargo.toml` and `fuzz/Cargo.*` change in one item
  at a time: items of a wave that write them run serially, since a lock regenerates whole.
- **Python.** One item per wave touches `python/`. A change to `Format`, the model, `ConvertError` or a
  public function goes through `/change-core-api`. After serde the model reaches Python only as JSON,
  and `JSON_SCHEMA_VERSION` bumps on a removal, rename or meaning change.
- **Fixtures.** Through `/add-fixture`. Each `tests/fixturegen/<format>.py` has one owner per wave. An
  abuse fixture is named `<limit>--<shape>--errors.<ext>`; an input over 1 MiB is generated inside its
  test; a container of a format the item does not own is built in the test with `zip::ZipWriter` or
  `cfb`. No third party's bytes are committed. An item changes only the snapshots of formats it owns
  or its section names; diagnostics snapshots are separate files.
- **Diagnostics and model.** A new diagnostic code is declared once in `src/diagnostics.rs` with its
  `LossClass`. No public diagnostic code ships before a site emits it: a model type, field, limit or
  code arrives in the item that first emits it, never earlier, and model additions are additive on
  `#[non_exhaustive]` types.
- **Licence.** `REUSE.toml` and `.github/provenance.toml` follow `CLAUDE.md`'s REUSE trap and ADR 0004
  decision 11; a moved or split upstream file also gets its `[named]` row.
- **Credit.** A taken upstream pull request is credited as ADR 0004 decision 11 and `CLAUDE.md`'s
  sign-off trap say; an issue carries no trailer.
- **Tier.** The change's risk for its `Tier` trailer (ADR 0004 decision 9): T0 mechanical (a
  lockfile), T1 prose that decides nothing or a new test, T2 ordinary code, T3 a public contract, a
  gate or the design.
- **Latest stable.** Every dependency, toolchain, runner and tool moves to its latest stable release
  once that release is 7 days old (ADR 0003 decision 8); an advisory fix is taken at once. A green,
  cooled-down Dependabot update is adopted by a signed-off T0 commit in the next wave whose manifest
  owner is free, and its pull request is closed with a link to that commit.
- **Panic-free.** Until lints lands, new code is written free of the deny set of ADR 0001 decision 16:
  run `cargo clippy` with those lints at `-W` on the added files and leave none firing. From lints on,
  the set is fixed in place.
- **Setup.** An item edits `CLAUDE.md` or `.claude/` only when its section says so, and those edits
  are applied by the agent that orchestrates the work (`CLAUDE.md`, *This setup*), never by a
  subagent; otherwise the item reports the lesson in its pull request.

## Waves

`∥` runs in parallel on disjoint files; `then` is serial.

| Wave | Items | Placement |
|---|---|---|
| W1 | shared-helpers ∥ (fuzz-parts then deps-refresh, from 2026-10-21) | fuzz-parts and deps-refresh both write `fuzz/Cargo.*`, so they are serial; deps-refresh lands before lints |
| W2 | lints, then setup-gate | alone: `src/**`, `python/src/**`, the Makefile; both items edit the Makefile |
| W3 | serde ∥ binary-cursor ∥ symbols | serde before py-api |
| W4 | py-api ∥ level-counters ∥ links | py-api leaves `doc/mod.rs` to level-counters; py-api keeps `unsafe_link`, which links emits before the release |
| W5 | shown-text ∥ release ∥ sheet-shape | the release candidate wheel publishes here and the consumer proof starts on it |
| W6 | style ∥ codepages | legacy-shape cannot join: style writes `ppt/mod.rs` |
| W7 | units-meta | alone: legacy-shape (`ppt/mod.rs`) and odf-fidelity (the `pres.odp` snapshot) collide with it |
| W8 | images ∥ legacy-shape ∥ odf-fidelity | odf-fidelity follows units-meta; legacy-shape is the first item free of `ppt/mod.rs` holders |
| W9 | blocks ∥ legacy-style ∥ presentation-style | presentation-style follows images (`pptx/mod.rs`) and legacy-shape (`ppt/**`) |
| W10 | pdf-model (after the pdf-inspector release) | |
| W11 | ocr-channel | |

Critical path: lints → serde → py-api → release (candidate, consumer proof) ∥ shown-text → style →
units-meta → images → blocks → pdf-inspector release → pdf-model → ocr-channel.

## Items

### W1

#### shared-helpers: one main-part resolver for the sniff and the OOXML readers
- **Goal:** the label check and every OOXML reader read the same main part, and a workbook found by
  convention reports `conventional_part_used`, as documents and decks already do.
- **Scope:**
  - `src/package/relationships.rs` gains `pub(crate) fn main_part(pkg: &Package, root: &Relationships,
    conventional: &[&str]) -> Option<MainPart>`, `MainPart { path: String, by_convention: bool }`: the
    internal officeDocument target that `first_of_type` picks if it resolves against the root, else
    the first conventional part present.
  - It also gains `pub(crate) fn read_main_part(parse_ctx: &ParseCtx, pkg: &Package, conventional:
    &[&str]) -> Result<Option<String>, ConvertError>`: it reads `_rels/.rels` through `read_rels(pkg,
    "_rels/.rels", Role::Reported(Region::Body))?`, calls `main_part`, emits `ConventionalPartUsed` in
    `Region::Container` when `by_convention` and the package holds `_rels/.rels`, and returns the path.
  - `detect.rs` keeps its capped probe of `_rels/.rels`, passes all four conventional parts and types
    only the part `main_part` returns; once a target resolves it never falls back to a leftover part.
    It emits nothing.
  - The docx, pptx and sheet entry points call `read_main_part`; `None` stays `MissingPart` (docx,
    pptx) and `not_a_workbook` (sheet).
  - `archive.rs`'s `skip_unreadable` becomes `pub(crate)` and xlsb's `read_optional` delegates to it;
    `unless_fatal` (`shared/binary.rs`) stays, since doc and ppt branch on `Absent` versus `Unreadable`.
  - Not here: `LevelCounters` (level-counters).
- **Files:** `src/package/{relationships,archive}.rs`, `src/formats/detect.rs`,
  `src/formats/docx/mod.rs`, `src/formats/pptx/mod.rs`, `src/formats/sheet/{mod,xlsb}.rs`,
  `tests/it/diagnostics.rs`.
- **Tests:**
  - The `xlsx/sheet.xlsx` case that corrupts `_rels/.rels` (expects no diagnostic today) expects one
    `ConventionalPartUsed` in `Container`; a new `xlsb/handmade-sheet.xlsb` case expects the same.
  - In `detect.rs`: move the handmade xlsb's workbook and that part's relationships to `book/`, type the
    workbook `application/octet-stream` and add a leftover `word/document.xml`. It passes
    `check_label` as Excel and converts; before, it fails `FormatMismatch { detected: Docx }`.
  - `main_part` unit tests: the target wins over the conventional part, an unresolvable target falls
    back, a package with neither returns `None`.
  - Snapshots stay byte-identical, since every spreadsheet fixture carries its relationship.
- **Credit:** none. **Depends on:** none open. **Size:** about 120 lines. **Tier:** T2.

#### fuzz-parts: part-injection fuzz targets and an XML round-trip proptest
- **Goal:** fuzzer mutations reach the DOCX, PPTX, XLSX and ODF part parsers that untrusted documents
  feed instead of stopping at the ZIP container, and the shared XML parser is checked against an oracle.
- **Scope:**
  - Executes ADR 0001 decision 18. Targets `docx_body`, `pptx_slide`, `xlsx_sheet` and `odf_content`
    each write the input as the one fuzzed part of a stored package built in the target, the way
    `fuzz/fuzz_targets/xlsb.rs` builds its package. Each template's `[Content_Types].xml` names its
    main part, so detection never sniffs fuzzed bytes. `odf_content` is an ODT package: `odf::parse`
    picks the text, spreadsheet or presentation body.
  - Each target converts twice with one `Converter` and asserts equal Markdown and diagnostics, or equal
    `ConvertError::code()`.
  - One libFuzzer dictionary per vocabulary, `fuzz/dicts/<target>.dict`; the campaign step passes
    `-dict=dicts/<target>.dict` when present.
  - Seeds are parts cut from fixtures: docx `handmade-tables`, `handmade-rich`; pptx `handmade-inherit`,
    `pres`; xlsx `sheet`, `handmade-merged`; `content.xml` of `text.odt`, `pres.odp`, `sheet.ods`.
  - `fuzz/src/lib.rs` holds the package writer and the twice-convert check. `Node`, `Element` and
    `Attr` derive `PartialEq` (crate-private).
  - No per-format parser proptest is written: this item's XML proptest is the only one.
- **Files:** `fuzz/Cargo.toml`, `fuzz/src/lib.rs` (new), `fuzz/fuzz_targets/{docx_body,pptx_slide,
  xlsx_sheet,odf_content}.rs` (new), `fuzz/dicts/` (new), those four under `fuzz/seeds/` and
  `fuzz/regressions/`, `fuzz/README.md`, `Makefile` (`fmt`, `fuzz`), `.github/scripts/fuzz_campaign.py`
  (the `-dict` flag), `src/package/xml.rs`, `REUSE.toml`, `.github/provenance.toml`.
- **Tests:**
  - `make fuzz` replays the new seeds and runs a fuzz-crate test: every seed, injected, converts
    complete with non-empty Markdown.
  - A proptest over generated trees (prefixed and unqualified names, attributes, text with entity and
    character references, CDATA, depth up to `max_nesting_depth + 1`): `parse_xml` returns the same
    tree with adjacent text merged, or `ResourceLimit { NestingDepth }` past the cap.
- **Credit:** none. New code joins `FSL-1.1-MIT`; a seed cut from an upstream fixture joins
  `FSL-1.1-MIT AND MIT` with a `[named]` row naming it.
- **Depends on:** none open. **Size:** about 330 lines. **Tier:** T2.

#### deps-refresh: take sha2 0.11.1 and pyo3 0.30.1 once their cooldowns end
- **Goal:** the library and the wheel build on the latest stable sha2 and pyo3, with their soundness and
  GC-traversal fixes and no change to the binding.
- **Scope:**
  - Starts 2026-10-21, when both releases are 7 days old (sha2 0.11.1 released 2026-10-12, pyo3 0.30.1
    2026-10-14; ADR 0003 decision 8).
  - sha2: lock only (`cargo update -p sha2 --precise 0.11.1`); the `Cargo.toml` floor stays, since no
    0.11.1 API is used.
  - pyo3: `python/Cargo.toml` moves to `0.30.1`, keeping `abi3-py311`; `python/src` stays unchanged,
    which this item's own gate run proves.
  - The PyBackedBytes URL in ADR 0001 decision 17
    (`pyo3/0.29.3/source/src/pybacked.rs#273-288`) becomes
    `https://docs.rs/crate/pyo3/0.30.1/source/src/pybacked.rs#274-289`, the same
    `From<Bound<PyByteArray>>` impl.
  - `make locks` reseeds `fuzz/Cargo.lock`; `make licences` regenerates `THIRD-PARTY-LICENSES`.
  - Two signed-off commits: sha2, then pyo3. The pyo3 commit says that `#[pyfunction]`s lose
    `METH_STATIC` (their `__self__` becomes the module). If a green Dependabot cargo-group pull request
    carries either bump after its cooldown, these commits adopt it and the pull request is closed with
    a link to them.
  - No other version moves here; each comes through its own Dependabot update.
- **Files:** `python/Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.lock`, `THIRD-PARTY-LICENSES`,
  `docs/adr/0001-architecture.md`.
- **Tests:** none are new, since no behaviour a test can pin changes. `make ci` is green with every
  snapshot byte-identical; `make locks` reproduces `fuzz/Cargo.lock`; `make python` (sdist, wheel,
  `pytest -W error`, stubtest) passes on every CPython in `PYTHONS`; `make policy licences` passes.
- **Credit:** none. **Depends on:** fuzz-parts (both write `fuzz/Cargo.*`). **Size:** about 3 lines;
  lock and licence text are generated. **Tier:** T2.

### W2

#### lints: deny panicking, lossy and ambient operations, every site fixed
- **Goal:** users get release builds that check overflow and non-test code free of unwraps,
  char-boundary slices, lossy casts and hash-order output, held by `make clippy` for every later change.
- **Scope:**
  - `[workspace.lints]`: `missing_docs` and `unreachable_pub` warn; `pedantic` warn at priority -1,
    allowing `similar_names`, `many_single_char_names`, `items_after_statements`, `struct_field_names`;
    deny `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, `unimplemented`, `get_unwrap`,
    `string_slice`, `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`,
    `cast_precision_loss`, `iter_over_hash_type`, `print_stdout`, `print_stderr`, `dbg_macro`, `exit`,
    `allow_attributes(_without_reason)`; rustdoc intra-doc links deny. `indexing_slicing` and
    `arithmetic_side_effects` stay off. `[profile.release]`: `overflow-checks = true`,
    `panic = "unwind"`.
  - `fuzz/Cargo.toml`: `[lints.rust] unsafe_code = "forbid"`; `make clippy` adds the fuzz crate on its
    pinned nightly.
  - New `clippy.toml`: allow-{unwrap,expect,panic,print,dbg}-in-tests; `doc-valid-idents = ["..", <each
    CamelCase term rustdoc flags, e.g. OfficeArt, SpreadsheetML, PowerPoint, OpenDocument>]`, taken
    from one `cargo clippy -W clippy::doc_markdown` run and recorded in the commit message; disallowed
    `std::{env,fs,net,process}`, thread spawn, `ThreadPoolBuilder`, rayon's global pool, `OnceLock`,
    `LazyLock`, `thread_local!`, PyO3 once-cells, and, on `f32` and `f64`, the methods whose 1.99 std
    docs say "the precision of this function is non-deterministic": `powi`, `powf`, `exp`, `exp2`,
    `exp_m1`, `ln`, `ln_1p`, `log`, `log2`, `log10`, `cbrt`, `hypot`, `sin`, `cos`, `tan`, `sin_cos`,
    `asin`, `acos`, `atan`, `atan2`, `sinh`, `cosh`, `tanh`, `asinh`, `acosh`, `atanh`, `gamma`,
    `ln_gamma` (check the list against the 1.99 docs and record any difference). Reasoned item-level
    `#[expect]`s only in `src/pool.rs`, the binding's lib file, PDF code and fixture-test modules.
  - Reviewed `clippy --fix` or `cargo fix`, then every remaining site by hand (about 124 outside
    indexing): let-else or `last_mut` (`numfmt.rs`, `rtf/mod.rs`, `shared/list.rs`); `first_chunk`
    (`xlsb.rs`); `strip_prefix` and `split_at_checked`; `TryFrom`, `From` and `cast_signed`; BTreeMap or
    a sorted Vec where hash order reaches output (`model/table.rs`, `sheet/grid.rs`). `ListMark`
    replaces both `type_complexity` silencers (`docx/content.rs`, `rtf/mod.rs`). Each
    `Option<Option<_>>` becomes a three-state enum: `Outline { Inherit, Body, Level(u8) }` in docx and
    doc; `hlink` in `xls.rs` returns `Option<HlinkRecord>` with `HlinkRecord { InWorkbook,
    External(SheetLink) }`, with an in-module test that it yields `InWorkbook` without a moniker and
    `External` with one; a marker override in rtf. Among the hand-fixed sites only `too_many_lines` keeps
    item expects; `tests/it/main.rs` gets one reasoned crate expect for its helpers. `make doc` adds
    `--document-private-items`.
- **Files:** `Cargo.toml`, `fuzz/Cargo.toml`, `clippy.toml` (new), `Makefile`, `REUSE.toml`, `src/**`,
  `python/src/**`, `tests/it/main.rs`, `tests/it/architecture.rs` (new).
- **Tests:** `make clippy features python` green; corpus and diagnostics byte-identical.
  `tests/it/architecture.rs` scans non-test code: `src/model` names only `crate::{model,limits}`;
  `src/{formats,package,shared}` name no `crate::{render,converter,pool,config,ledger,ocr,raster}`; no
  `static` outside the pool and the OCR adapters; no inner `#![expect]` or `#![allow]` under `src` and
  `python/src`; release `overflow-checks` is set (fails before). A self-test feeds each rule a
  violation.
- **Credit:** none. **Depends on:** deps-refresh, shared-helpers, fuzz-parts; runs alone.
  **Size:** about 1000 lines plus machine fixes. **Tier:** T3.

#### setup-gate: CI checks the tracked agent setup
- **Goal:** a pull request that edits `CLAUDE.md` or `.claude/` is held by the same setup check that
  runs locally, now that the setup is public.
- **Scope:** a `setup` Makefile target runs `.claude/skills/verify/scripts/check_setup.py --self-test`
  and then `check_setup.py`, reporting NOT RUN with its reason when Python is missing, as the other
  targets do; `make ci` and `.github/workflows/gates.yml` run it inside `ci / required` (pinned
  actions, `contents: read`, zizmor clean). `verify.py` already selects a `setup` row for every change,
  running `check_setup.py`, and the verify skill's table names it: that existing row and the table's
  `anything` row now run `make setup`. No new selection row. `CLAUDE.md`'s gate-targets line gains
  `setup`.
- **Files:** `Makefile` (`setup`, `ci`), `.github/workflows/gates.yml`, `.claude/skills/verify/SKILL.md`,
  `.claude/skills/verify/scripts/verify.py` (the existing `setup` row), `CLAUDE.md` (gate-targets
  line).
- **Tests:** `make setup` is red on a planted leak in a scratch copy and green on `main`; `make actions`
  is clean; `verify.py --self-test` still selects `setup` for every change.
- **Credit:** none. **Depends on:** lints (both edit the Makefile). **Size:** about 40 lines.
  **Tier:** T3 (a new gate).

### W3

#### serde: serialise the model to versioned JSON and drop the Python model mirror
- **Goal:** Rust users get models they can serialise and compare (API guidelines C-SERDE,
  C-COMMON-TRAITS); Python reads the whole model as versioned JSON instead of a hand mirror every model
  change has to edit.
- **Scope:**
  - Optional `serde` feature (`dep:serde` with `derive`, `dep:serde_json`), `Serialize` only, so tables
    still come only from `GridBuilder`.
  - `cfg_attr` derives on every `src/model` type, `Diagnostic`, `EngineIdentity` and `Conversion`; data
    enums use `tag = "kind", content = "value"`, unit enums snake_case; `Asset.bytes` serialises as its
    length, `byte_len`. In `json.rs`: `Diagnostics` as a sequence; `DiagnosticCode`, `LossClass` and
    `Region` as `as_str()`.
  - `Parsed::to_json()` writes `{schema_version, identity, document, diagnostics}` on the pool like
    `render`, through a writer charging `Meter` (`render/markdown/sink.rs`): `Meter::write` and
    `mod sink` become `pub(crate)`. It charges against `max_output_bytes` and the ledger from the
    parse's fuel. `pub const JSON_SCHEMA_VERSION: u32 = 1` bumps only on a removal, rename or meaning
    change.
  - `PartialEq, Eq` on Document, Note, Block, Inline, List, ListItem, Table, CellSlot, Cell, Asset and
    ConvertError; `Hash` on NoteKind, TableKind, MarkerKind, Style, LinkTarget, ImageSource,
    UnsupportedKind and the variant types they hold.
  - The binding enables `serde`, deletes `python/src/document.rs` and the stub's 13 model classes;
    `parse(data, format=None) -> Parsed` replaces `to_document`, with GIL-free `to_json()` and
    `assets() -> list[tuple[int, str, bytes]]`, one copy.
  - `pool.rs`'s stack-cost table gains a JSON row; `STACK` grows only if it tops it.
  - The setup follows: `mirror_drift.py` drops its model-mirror check, since the model reaches Python
    only as JSON; the change-core-api skill's step 2 drops `document.rs`; `CLAUDE.md`'s binding line
    says the binding hand-mirrors `Format`, `ConvertError` and the public functions, and that a model
    change still goes through `/change-core-api` for `JSON_SCHEMA_VERSION`.
- **Files:** `Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.{toml,lock}` (`serde = ["almiladoc/serde"]`),
  `THIRD-PARTY-LICENSES`, `REUSE.toml`, `src/model/**`, `src/{diagnostics,identity,error,converter,lib,
  pool}.rs`, `src/render/markdown/sink.rs`, `src/render/markdown/mod.rs` (the `mod sink` line only),
  new `src/json.rs`, `python/Cargo.toml`, `python/src/**`, `python/almiladoc/{__init__.py,
  _almiladoc.pyi}`, `python/tests/`, `README.md`, new `tests/it/json.rs` with its snapshot,
  `tests/it/bounds.rs`, `.claude/skills/change-core-api/{SKILL.md,scripts/mirror_drift.py}`,
  `CLAUDE.md`.
- **Tests:** an insta snapshot of `handmade-rich.docx`'s JSON (the asset carries `byte_len`, no bytes;
  `.identity.version` is redacted, so a version bump leaves it unchanged); every `CASES` input in
  `tests/it/bounds.rs` at the ceiling serialises from a 64 KiB caller thread; a lowered
  `max_output_bytes` fails `ResourceLimit { OutputBytes }`; two parses compare equal. Python:
  `test_to_document_*` become a `json.loads` shape test and an asset-bytes test, and PDF `parse` raises
  `UnsupportedError`. `make features` builds `serde` alone; `make fuzz` builds with it;
  `mirror_drift.py` and its `--self-test` pass.
- **Credit:** none. **Depends on:** lints. **Size:** about 400 lines. **Tier:** T3.

#### binary-cursor: read legacy binary records through one checked cursor
- **Goal:** DOC, OfficeArt, XLS and XLSB read every fixed-width field through one checked
  little-endian cursor, so no fixed-width read can index past a record and three duplicate readers go.
- **Scope:** behaviour-preserving: output, diagnostics and error messages are unchanged.
  - `shared::binary::Cursor<'a>` over `&[u8]`: `array::<N>()` (`split_first_chunk`), little-endian
    `u8`, `u16`, `u32` and `f64`, `take(n)` (`split_at_checked`), `skip(n)`, `offset()`, `rest()`. Each
    returns `Option` and advances only on success. `get_u16` and `get_u32` stay for random access and
    are rebuilt on `first_chunk`.
  - xlsb `Records` and `Fields` read through it; `Fields` maps `None` to today's `Malformed` messages.
  - xls `SegReader`'s fixed-width reads use a Cursor over the current segment, with CONTINUE handling
    unchanged; FORMAT's body, `get_f64` and the FORMULA value take array views.
  - officeart `record_at` and `first_blip`'s `&data[..end]`. `sprm_operand_len` and `walk_sprms`
    (`doc/sprm.rs`). The LSTF, LVLF and LFO-override records (`doc/lists.rs`). FKP pages pass as
    `&[u8; 512]` (`doc/mod.rs`). Also the PAPX `istd`, the compressed-piece walk and the PICF.
  - `#![deny(clippy::indexing_slicing)]` goes in `binary.rs`, `officeart.rs`, `sprm.rs` and `xlsb.rs`,
    and `clippy.toml` sets `allow-indexing-slicing-in-tests = true`. Other guarded indexing stays, per
    ADR 0001 decision 16.
  - ppt is untouched, because its headers come through `record_at`.
- **Files:** `src/shared/{binary,officeart}.rs`, `src/formats/doc/{mod,sprm,lists}.rs`,
  `src/formats/sheet/{xls,xlsb}.rs`, `clippy.toml`.
- **Tests:** `binary.rs` unit tests: a read past the end returns `None` and leaves `offset()` where it
  was; `take(usize::MAX)` returns `None`; `array`, `u16`, `u32` and `f64` decode little-endian; Cursor
  reads equal `get_u16` and `get_u32` at every offset of a buffer. `make clippy` passes the four
  denies, which fail before the change. Snapshots, diagnostics and fuzz replays are byte-identical.
- **Credit:** none. **Depends on:** lints; lands before level-counters, sheet-shape and legacy-shape.
  **Size:** about 400 lines. **Tier:** T2.

#### symbols: DOCX Symbol-font characters read as their Unicode letters
- **Goal:** a Word file that holds μ, ± or α through Insert > Symbol or a Symbol-font run converts to
  those characters, instead of failing `Incomplete` or rendering `m` or `a`.
- **Scope:**
  - New `src/shared/symbol.rs`, this repository's own work (`FSL-1.1-MIT`), defined by the Symbol
    encoding of ISO 32000-1 Annex D.5 (189 codes: 0x20-0x7E, 0xA0-0xEF, 0xF1-0xFE) and Unicode
    character identities; a code-indexed table of an encoding's facts, restated, no third-party file
    copied or vendored. `symbol(code: u8) -> Option<char>`; `SymbolFont::from_name` (ASCII
    case-insensitive `Symbol`; `Wingdings`, `Wingdings 2`, `Wingdings 3`, `Webdings` and `ZapfDingbats`
    are recognised and have no table); `decode_run` applies the character sort below before
    `clean_text` and returns the text and the unmapped count.
  - **Table rule.** Each D.5 code gives the character whose Unicode identity is the glyph's:
    - (i) a Greek letter gives the Greek-block letter, never a letterlike or operator twin (U+00B5,
      U+2126, U+2206);
    - (ii) a delimiter, integral or arrow piece gives its Unicode piece from U+2320-2321, U+239B-23AF
      or U+23D0;
    - (iii) the serif and sans forms of the registered, copyright and trademark signs fold to one sign;
    - (iv) no value is Private Use, a control, Deprecated (U+2329, U+232A) or changed by NFC;
    - (v) a D.5 code no character satisfies, and every code D.5 does not define, gives `None`.

    Adobe's `symbol.txt`, Apple's `SYMBOL.TXT`, the Adobe Glyph List, lopdf and pdf-inspector disagree
    with one another and with the rule. The departures are exactly these rows, and every other code
    takes the one value they all share (188 codes are `Some`: D.5's 189 minus 0x60):

    | Code | Value | Reason |
    |---|---|---|
    | 0x20 | U+0020 | not U+00A0, which Markdown does not collapse |
    | 0x27 | U+220B | U+220D is the small variant |
    | 0x44 Delta | U+0394 | rule (i); U+2206 has no decomposition, so "ΔG" would never match it |
    | 0x57 Omega | U+03A9 | rules (i), (iv); U+2126 decomposes to U+03A9 |
    | 0x60 radicalex | `None` | Unicode encodes no radical extension; any stand-in hides a real loss |
    | 0x6D mu | U+03BC | rule (i); U+00B5 is a compatibility twin |
    | 0xA0 Euro | U+20AC | lopdf's gap is a fact error |
    | 0xA4 fraction | U+2044 | not the division slash U+2215 |
    | 0xA8, 0xA9 | U+2666, U+2665 | D.5 names the black suits; lopdf's white suits are a fact error |
    | 0xBD, 0xBE | U+23D0, U+23AF | rule (ii) |
    | 0xD2-0xD4, 0xE2-0xE4 | U+00AE, U+00A9, U+2122 | rules (iii), (iv) |
    | 0xE1, 0xF1 | U+27E8, U+27E9 | rule (iv); U+2329 and U+232A are Deprecated and NFC turns them into CJK punctuation |
    | 0xE6-0xEF | U+239B, U+239C, U+239D, U+23A1, U+23A2, U+23A3, U+23A7, U+23A8, U+23A9, U+23AA | rule (ii) |
    | 0xF0 | `None` | not in D.5; the installed font draws its own glyph there |
    | 0xF3-0xF5 | U+2320, U+23AE, U+2321 | rule (ii) |
    | 0xF6-0xFE | U+239E, U+239F, U+23A0, U+23A4, U+23A5, U+23A6, U+23AB, U+23AC, U+23AD | rule (ii) |

  - **Sources.** The writer studies those tables, LibreOffice's Symbol recoding included, openly while
    writing, to do better than each: facts may agree, and no comment, name column, row-order quirk or
    Corporate Use value is carried over. The rustdoc names D.5 as the definition, states the rules, and
    names the tables compared against with a one-line summary of where and why the table departs.
  - **`None` consequence.** Under the default `OnLoss::Fail` a `None` code emits `unmapped_symbol`
    (Lost) and fails the document `Incomplete`; `Report` returns the Markdown with the diagnostic. This
    holds for 0x60 and for 0xF0 and 0xFF in any form (`w:sym`, U+F0F0 and U+F0FF, U+00F0 and U+00FF in
    a run), and for 0x00-0x1F and 0x7F-0x9F as a font code (a `w:sym` code, a U+F000-F01F or
    U+F07F-F09F character, or an RTF `\'hh` byte), which name positions D.5 leaves empty. The same
    values as plain run characters are controls and follow the character sort.
  - **Caller rule**, stated in the rustdoc for later symbol-font routes:
    - A font code goes through `symbol` whatever its byte: a `w:sym` `w:char`, folded from U+F000-F0FF
      to a byte (`private - 0xF000`), and an RTF `\'hh` byte in a `\fcharset2` group, taken as the
      byte and not decoded through the code page. `None` emits `unmapped_symbol`.
    - A Symbol `w:sym` whose code is no byte after the fold (above 0xFF outside U+F000-F0FF, such as
      `03BC` or `2022`), or whose `w:char` is absent or malformed (not one to four hex digits), also
      emits `unmapped_symbol` and renders nothing.
    - Narrow with `u8::try_from`, never `as u8`, which would draw the glyph at the low byte with no
      diagnostic; never read the number as Unicode, since `w:char` names a position in the font.
    - A character of a Symbol-font run (DOCX `w:t` text, an RTF `\fcharset2` group's literal text and
      control-word output) is sorted by value:
      - U+F000-F0FF folds and looks up, `None` emitting.
      - U+0020-007E and U+00A0-00FF look up the byte value (`u8::try_from`), `None` at 0x60, 0xF0
        and 0xFF emitting.
      - A control (U+0000-001F, U+007F-009F) is never looked up and never reported; `clean_text` handles
        it as in any run (it keeps `\t`, turns CR and LF into a space and drops the rest).
      - Anything else (U+0100 and above outside U+F000-F0FF, such as a real U+03BC) passes through
        unchanged with no diagnostic.
    - The lookup runs before `clean_text`, which would turn U+00A0 into a space and strip U+00AD,
      losing `€` and `↑`.
    - A `w:sym` or run character in another symbol font emits `unmapped_symbol`; a ballot-box `w:sym`
      stays a checkbox.
  - **DOCX.**
    - `w:sym`: ballot boxes stay `Checkbox`; font Symbol folds, narrows and looks up as above; anything
      else emits `unmapped_symbol` and renders nothing.
    - `w:t`: `Styles::run_fonts`, memoized beside `run_toggles`, resolves each font slot in this order:
      direct `w:rFonts`, the `rStyle` chain, the `pStyle` chain, `docDefaults`.
    - The slot follows ECMA-376 §17.3.2.26: `ascii` for U+0000-007F, `eastAsia` under
      `w:hint="eastAsia"`, otherwise `hAnsi`. A `*Theme` attribute overrides its slot and is never
      routed. One `emit_count` per run.
    - `lvlText` is never routed; other formats are unchanged.
  - Not here: PDF text is pdf-inspector's own and keeps its values (U+00B5 for mu, U+2206 for Delta,
    where the table gives U+03BC and U+0394); aligning them is an upstream change or a later PDF item.
  - New files join the `FSL-1.1-MIT` list; `docs/upstream.md` item #177 drops "other symbols wait for a
    symbol-font map".
  - Reopen the licensing choice (the Adobe Glyph List under its BSD-3-Clause as a fourth provenance
    class) if the committed table shows copied expression, a rights holder claims it, or counsel for
    the licensor's jurisdiction finds a database right it infringes. A later symbol-font table whose
    only per-code source forbids restatement is decided anew.
- **Files:** `src/shared/{symbol,mod}.rs`, `src/formats/docx/{content,styles,mod}.rs`,
  `tests/fixturegen/docx.py`, `tests/fixtures/docx/handmade-symbols.docx`,
  `tests/snapshots/snapshots__docx__handmade-symbols.docx*.snap`, `REUSE.toml`, `docs/upstream.md`.
- **Tests:**
  - One test over all 256 codes: exactly 188 give `Some`; 0x00-1F, 0x60, 0x7F-9F, 0xF0 and 0xFF give
    `None`; no value is in U+E000-F8FF, a control, one `clean_text` rewrites (U+00A0, U+00AD, U+200B,
    U+FEFF) or one of U+00B5, U+2126, U+2206, U+2329, U+232A; a row test pins every departure above.
  - DOCX: `w:sym w:font="Symbol"` with `w:char` `F06D` and `006D` give the same μ, as do a Symbol-font
    run's `m` and U+F06D; in a Symbol-font run a literal tab and a U+03BC render as in a plain run with
    no diagnostic, U+00A0 gives `€` and U+00AD gives `↑`; a run's U+F009 and `w:char="0009"` each add
    `unmapped_symbol`; `w:char` `03BC`, `2022`, `F10D`, `zz` and `1F06D` each add `unmapped_symbol` and
    the output holds none of `…`, `∀` or `μ`.
  - Fonts: ascii Times with hAnsi Symbol keeps `m` and maps U+F06D; a theme slot is not routed; Symbol
    reached through a character style, a paragraph style and `docDefaults` maps; direct Times over any
    of them gives `m`; Wingdings `J` is unmapped.
  - The existing `docx/mod.rs` Symbol expectation now reads `F060`; `handmade-symbols.docx`
    (`10 μm ± 2`) converts under the default `Fail`.
  - `make reuse provenance` is green with no change to `LICENSES/`, `NOTICE` or `provenance.py`.
  - The review gets the writer's out-of-tree comparison of all 256 codes against the compared tables,
    listing each disagreement and the row or fact error that explains it; nothing of it is committed.
- **Credit:** none; this is our own work.
- **Depends on:** lints. **Size:** about 550 lines. **Tier:** T3.

### W4

#### links: gate link destinations by scheme and write them so they parse back unchanged
- **Goal:** rendered links keep their exact target, and no `javascript:`, `data:`, `file:` or
  local-path destination reaches the Markdown.
- **Scope:**
  - `src/shared/uri.rs`: `is_safe_link(dest)`, judged after WHATWG preprocessing (trim leading and
    trailing C0 and space, drop tab, CR and LF), never on the External or Relative classification
    (` javascript:x` is Relative in `fields.rs`). A scheme (`has_scheme`, ASCII-lowercased) must be
    `http`, `https` or `mailto`; a scheme-less destination passes unless it is a drive path
    (`is_drive_path`) or a UNC (`\\`) path.
  - `inline.rs`: a refused link renders as a dangling anchor does, label only, nothing if empty
    (`render_link`); the lookahead arm and `target_has_backtick` agree. Anchors skip the gate.
  - `mod.rs`: `render_losses` counts each refused link as `unsafe_link` (Omitted) per region, notes as
    for dangling links.
  - `format_url` (`escape.rs`): `\` becomes `\\`, and an `&` that `entity_ahead` accepts becomes `\&`.
  - No autolink breaking (ADR 0001 decision 8). External images stay for images.
- **Files:** `src/shared/uri.rs`, `src/render/markdown/{inline,escape,mod,tests}.rs`, `Cargo.toml`
  (`pulldown-cmark` dev-dependency, no default features), `Cargo.lock`,
  `tests/snapshots/snapshots__rtf__text.rtf{,.diagnostics}.snap`, `REUSE.toml`, `docs/upstream.md`.
- **Tests:**
  - ` javascript:x`, `JavaScript:x`, `java\tscript:x`, `data:`, `vbscript:`, `file:`, `C:\x`, `\\srv\x`
    are refused; `https:`, `mailto:`, `notes.txt`, `../a.odt` are kept.
  - `render_losses` cases: `unsafe_link` in body and in a shown note.
  - A pulldown-cmark proptest: destinations holding `&copy;`, `&#35;`, `\`, `|`, parentheses and spaces
    parse back unchanged (`<`, `>`, `|` and controls percent-encoded), in a paragraph and a table cell.
  - `text.rtf`'s snapshot loses its `file:` destination and gains a diagnostics snapshot.
- **Credit:** `Co-authored-by: Gabriel Pereira Rodrigues
  <173825442+Creatixpy@users.noreply.github.com>`, origin firecrawl/anydoc #171
  (`src/render/markdown/escape.rs`); #171 moves to Taken. `escape.rs` gets its own REUSE annotation
  with the author's line, `uri.rs` joins `FSL-1.1-MIT AND MIT`, the new snapshot `FSL-1.1-MIT`.
- **Depends on:** lints. **Size:** about 250 lines. **Tier:** T3.

#### py-api: a `Converter` binding with limits, cancellation, typed diagnostics and snake_case error codes
- **Goal:** Python callers get the Rust API: limits, threads, cancellation, diagnostics under
  `OnLoss.REPORT`, engine identity and a stable `code` on every error. No input is copied past its
  limit.
- **Scope:**
  - `Converter(config)`: `.convert(data, format=None, *, cancel=None)` returns `Conversion`
    (`markdown`, `diagnostics`, `is_complete`). `.parse` returns serde's `Parsed`, which gains
    `.render()`. `.convert_path(path, format=None)` stats the file, refuses it past `max_input_bytes`,
    reads at most that, sniffs, then falls back to the extension. Also `.identity` and a module-level
    `sniff`.
  - Frozen, Rust-defaulted `Config`, `Limits` (every field), `OnLoss` and `CancelToken`;
    `ConfigError(ValueError)`.
  - `bytes` is borrowed with the GIL detached; `bytearray` and `memoryview` are size-checked before
    their one copy. Today the entry points take `Vec<u8>`, which copies the input before any bound.
  - Every exception carries `code`. `UnsupportedError` adds `kind` (new snake_case
    `UnsupportedKind::as_str`). Diagnostics are frozen `Diagnostic` objects, `IncompleteError`'s
    included.
  - Through `/change-core-api`: `code()` becomes `needs_ocr`, `resource_limit`, `missing_part`
    (`src/error.rs`). `impl FromStr for Format` replaces the binding's `FORMATS`; an unknown name raises
    `ValueError`.
  - `mirror_drift.py` and the change-core-api skill read `Format`'s `FromStr` and `as_str` names
    instead of `FORMATS`.
  - The free functions and `format_from_*` go. Every public name is documented; the README's Python
    block is rewritten, naming `UnsupportedError.detected` and the format-mismatch rule.
  - `check_dist.py` also requires the sdist's `<top>/Cargo.lock` and the wheel's
    `*.dist-info/sboms/*.cyclonedx.json`.
  - New `docs/support.md`: per-format conversion limits as they behave now, linked from the README
    (upstream #88).
  - Every `DiagnosticCode` no site emits at this item's commit goes before the first release, with
    its row in `diagnostics.rs`'s code table and the binding's mirror: on `38a013d`,
    `formatting_runs_capped`, `annotation_unrendered`, `image_placeholder`, `external_image`,
    `image_omitted`, `image_not_ocred`, `ocr_failed`, `white_page_filled`, `field_token` and
    `metadata_unreadable` (`unsafe_link` stays: links emits it before the release); re-check `src/`
    for each. The item that first emits a code adds it back with its `LossClass`: images the three
    image codes, ocr-channel `image_not_ocred` and `ocr_failed`, whichever later item first emits
    one of the rest.
- **Files:** `python/src/**`, `python/almiladoc/{__init__.py,_almiladoc.pyi}`,
  `python/stubtest-allowlist.txt`, `python/tests/**`, `src/{error,lib,diagnostics}.rs`,
  `scripts/check_dist.py`, `docs/support.md` (new), `README.md`, `REUSE.toml` (`docs/support.md` joins
  `FSL-1.1-MIT`), `docs/upstream.md`,
  `.claude/skills/change-core-api/{SKILL.md,scripts/mirror_drift.py}`.
- **Tests:** with `max_input_bytes=8`, a 9-byte `bytearray`, `memoryview` and file are refused unread.
  `cancel()` from another thread raises `CancelledError`. `REPORT` returns typed diagnostics. Each
  subclass carries its `code`; a mismatch carries `kind` and `detected`. A zero limit raises
  `ConfigError`. The README block runs, every public name has a `__doc__`, and stubtest passes. Rust:
  `codes_name_every_variant` asserts snake_case; every `Format` round-trips through `str::parse` and
  `as_str`. `check_dist` refuses an archive that lacks either file.
- **Credit:** `Co-authored-by: James Yang <106619425+HaoChiBao@users.noreply.github.com>`
  (firecrawl/anydoc #88); moves #88 to Taken in `docs/upstream.md`.
- **Depends on:** deps-refresh (pyo3 0.30), lints, serde. **Size:** about 900
  lines. **Tier:** T3.

#### level-counters: one bounded list-level counter for every office reader
- **Goal:** contributors get one list counter that cannot index out of range, used by DOC, DOCX, RTF,
  PPTX and ODF headings. The output does not change.
- **Scope:**
  - `src/shared/numbering.rs` gains `Level<const N: usize>`, built only by `Level::clamped` (saturates
    at N-1) or `Level::new` (returns `Option`), implementing `Index` and `IndexMut<Level<N>>` for
    `[T; N]`.
  - The same file gains `LevelCounters<N>` with `next(level, start)`, `seed(level, value)`,
    `restart(level)`, `restart_deeper(used, limit: impl Fn(Level<N>) -> Option<u32>)` and
    `value_or_start(level, start)` for `composite_label`. In `restart_deeper`, a deeper level restarts
    when its limit is `None` or greater than `used`.
  - `OFFICE_LEVELS = 9` replaces the level constants of `doc/lists.rs`, `docx/numbering.rs`,
    `pptx/cascade.rs` and `rtf/tables.rs`. ODF keeps `LIST_LEVELS = 10` (`odf/styles.rs`) as its N.
  - DOC (`doc/mod.rs`): a first-use LFO override is `seed`, and fNoRestart/ilvlRestartLim is the limit.
  - DOCX (`docx/numbering.rs`): `restart_pending` becomes not-started, a `startOverride` is `restart`,
    and `lvlRestart` is the limit.
  - RTF and PPTX pass no limit. ODF headings pass no limit: `restart-numbering` is `restart` and
    `start-value` is `seed`.
  - Each reader keeps its own sequence maps and override sets; ODF list items keep their ancestor chain.
  - The guarded definition loads (`docx/numbering.rs`, `rtf/tables.rs`) use `Level::new`; the
    `min(LEVELS - 1)` lookups and the `get(..).unwrap_or(&levels[LEVELS - 1])` fallback of
    `docx/numbering.rs`'s `at_level` use `Level::clamped`.
  - Behaviour does not change.
- **Files:** `src/shared/numbering.rs`, `src/formats/doc/{mod,lists}.rs`,
  `src/formats/docx/numbering.rs`, `docx/content.rs` only if the `ilvl` type change reaches it,
  `src/formats/odf/{text,styles}.rs`,
  `src/formats/pptx/{mod,cascade}.rs`, `src/formats/rtf/{mod,tables}.rs`, `REUSE.toml`
  (`src/shared/numbering.rs`, upstream MIT-only today, joins `FSL-1.1-MIT AND MIT`).
- **Tests:** new unit tests in `shared/numbering.rs` (they fail before, since the type does not exist):
  `clamped` saturates for N=9 and N=10 and `new` rejects N; `next` starts at `start`, then counts and
  saturates; `restart_deeper` with no limit clears every deeper level; the limits `None`, `Some(0)` and
  `Some(2)` restart levels the way DOC and DOCX do; `seed` leaves deeper levels alone; `value_or_start`
  is checked before first use, after use and after a restart. The existing DOC counter tests and the
  `docx/numbering.rs` tests stay green; snapshots and diagnostics stay byte-identical.
- **Credit:** none. **Depends on:** lints, binary-cursor (both edit `doc/mod.rs`). **Size:** about 400
  lines. **Tier:** T2.

### W5

#### shown-text: drop text the application hides, count comments and moves exactly
- **Goal:** DOCX, RTF, DOC and ODT output drops hidden text and DOC deleted text, and DOCX counts each
  omitted comment and move exactly.
- **Scope:**
  - Fixed rule (ADR 0001 decision 11). `StyleDelta` (`shared/delta.rs`) gains `hidden: Option<bool>`;
    the model `Style` never does. Read: DOCX `w:vanish` as a toggle (`w:webHidden` shows); RTF `\v` and
    `\v0` (`\plain` resets); DOC sprmCFVanish 0x083C; ODF `text:display="none"` through the cascade, and
    `text:hidden-paragraph` and `text:hidden-text` by cached `text:is-hidden`, never evaluating
    `text:condition`.
  - DOC takes upstream #174: `apply_chpx` (`doc/sprm.rs`) resolves a crate-private
    `Chp { style, hidden, deleted }`; sprmCFRMarkDel 0x0800 drops text, 0x0801 keeps it.
  - Dropped text counts `hidden_omitted` or `revision_omitted` once per maximal range per paragraph, in
    its region; textless ranges count nothing; losses inside go unreported. A note whose every
    reference drops is not parsed.
  - A deleted, moved-from or hidden paragraph mark (e.g. DOCX `w:pPr/w:rPr/w:del`, DOC's paragraph-end
    character carrying 0x0800, RTF `\par` inside `\v`) joins the next paragraph in its
    container with no separator and takes the following paragraph's properties, whose mark survives. A
    join never crosses into a table or out of a cell and counts nothing, since no text drops.
    `mark_deleted` (`docx/content.rs`) generalises.
  - DOCX `comment_omitted` counts each `w:commentReference`, not once.
  - A DOCX `w:moveFrom` inside a `w:moveFromRangeStart` range whose `w:name` a `w:moveToRangeStart`
    names counts 0 (the pair renders at the destination); a lone half counts 1. DOC has no move mark,
    so a move there is an ordinary deletion plus insertion.
  - Formatting-only changes (DOCX `*PrChange`, ODF `text:format-change`) are not applied: their content
    renders and counts nothing.
- **Files:** `src/shared/delta.rs`, `src/formats/docx/{mod,content,styles}.rs`,
  `src/formats/rtf/{mod,tables}.rs`, `src/formats/doc/{mod,sprm,stsh}.rs`,
  `src/formats/odf/{styles,text}.rs`, `tests/fixturegen/{docx,rtf,odf}.py`,
  `tests/fixtures/{docx,rtf,odt}/handmade-hidden.*`, `tests/snapshots/*handmade-hidden*`,
  `REUSE.toml` (`delta.rs` joins `FSL-1.1-MIT AND MIT`), `docs/upstream.md`.
- **Tests:**
  - DOCX, RTF and ODT `handmade-hidden` fixtures: hidden direct and by style, a joining hidden mark, a
    hidden note reference; diagnostics snapshotted.
  - DOC: #174's eleven tests on `DocBuilder`, plus sprmCFVanish direct and as 0x81.
  - RTF `{\v \'82}` under `\ansicpg932` is Ok under `OnLoss::Fail`.
  - The DOCX "comments, once" test counts 2; a move pair counts 0, an orphan `moveFrom` 1.
- **Credit:** `Co-authored-by: HanZephyr <50990263+HanZephyr@users.noreply.github.com>`; firecrawl/anydoc
  #174 (`96d5998`: `src/formats/doc/{mod,sprm}.rs`, `tests/doc_revisions.rs`); `REUSE.toml` declares
  `doc/{mod,sprm}.rs` `FSL-1.1-MIT AND MIT` with the author's line; `docs/upstream.md` moves #174 to
  Taken.
- **Depends on:** binary-cursor, level-counters. **Size:** about 500 lines. **Tier:** T3.

#### release: build, smoke, attest and publish wheels from version tags
- **Goal:** the primary consumer and public users install a signed, attested `almiladoc` release
  candidate from TestPyPI, and later finals from PyPI and crates.io.
- **Scope:**
  - `release.yml` per ADR 0003 decision 6: `v*` tags publish; dispatch and a weekly schedule rehearse
    without publishing; never cancelled; no cache; `id-token: write` only on publish jobs and `attest`.
  - Every run builds every decision-6 target: `linux` (maturin `--zig`, abi3 `manylinux_2_28`,
    `musllinux_1_2`, native arch), `native`, `sdist`, `crate`. Smoke runs in PyPA's digest-pinned image:
    CPython 3.11 and 3.15 pytest, `check_dist.py`, wheel under 12 MB.
  - `vX.Y.Z-rc.N` publishes to TestPyPI and a GitHub pre-release; a final tag publishes to PyPI, then
    crates.io, then runs `verify-published`. ADR 0003 decision 5's goldens are out of scope: OCR is not
    in this crate.
  - `release_guard.py --self-test`: tag equals workspace version, `rust-version` equals the toolchain
    minor, the README's Competing Use FAQ exists, `ci / required` is green on `main`,
    `docs/release-dates.tsv` covers earlier final tags. Notes carry the decision-7 date.
  - `staleness.py` flags pinned PyPA image digests (ADR 0003 decision 4).
  - The version becomes `0.1.0-rc.1`. The README says the candidate is on TestPyPI and its install line
    becomes `pip install -i https://test.pypi.org/simple/ almiladoc==0.1.0rc1`.
  - Maintainer steps: see the table below. The first final 0.x tag follows a green consumer proof.
- **Files:** `.github/workflows/release.yml` (new), `.github/scripts/release_guard.py` (new),
  `.github/scripts/staleness.py`, `docs/release-dates.tsv` (new, header row only; joins `FSL-1.1-MIT`),
  `Makefile` (`hygiene`), `Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.lock`, `README.md`, `REUSE.toml`,
  `tests/fixturegen/pdf.py`, the predefined-CMap fixture and snapshot, `python/tests/**`.
- **Tests:** the guard self-test: a mismatched tag, `rust-version` and a missing FAQ fail, and a
  candidate goes to TestPyPI. The staleness self-test flags a stale digest. A new fixture
  `handmade-predefined-cmap.pdf` (no ToUnicode) converts in Rust and in Python. `make actions hygiene
  locks python` pass, and a dispatched rehearsal is green before the tag.
- **Credit:** none. **Depends on:** py-api. **Size:** about 450 lines. **Tier:** T3.

#### sheet-shape: the xls sheet walk and number-format parser split into named parts
- **Goal:** contributors to the spreadsheet readers get record-family methods and named types instead of
  100-to-270-line functions and bool-coded tuples, with output unchanged.
- **Scope:** behaviour-preserving; corpus, diagnostics and fuzz replays are byte-identical.
  - xls `read_sheet` (267 lines) becomes a `SheetReader` holding the walk state (`depth`, `pending`,
    `shape`, `text_of`, `out`), one method per record family (cells, row and column layout, merges,
    drawing objects and notes, headers and footers, links) and pure body decoders; the
    substream-end report lands unchanged inside it.
  - numfmt `parse_section` (159 lines) becomes one function per token class; `resolve_number` (112
    lines) becomes region assignment, comma classification and place validation.
  - `read_biff8_string` and `read_byte_string` take `LengthPrefix { U8, U16 }` and
    `Runs { Plain, Rich }` instead of the `short` and `rich` bools.
  - One `omit_header_footer(&ParseCtx, Region)` in `sheet/grid.rs` replaces the `header: bool` helper
    in `xls.rs` and xlsb's bool loop.
  - xlsb's `Vec<(usize, Bundle, String)>` becomes `SheetEntry { position, bundle, part }`.
  - The item-level `too_many_lines` expectations lints leaves on these functions go. No new file.
- **Files:** `src/formats/sheet/{xls,xlsb,numfmt,grid}.rs`.
- **Tests:** in-module tests that do not compile before: each record-family decoder on a hand-built body
  (HEADER_FOOTER lengths to regions, MULRK to cells); each numfmt token-class function (`[h]:mm`,
  `# ?/8`, `0.00E+0`). `cargo test --locked --test it corpus`, the sheet bounds tests, the existing
  in-module tests and `make fuzz` (xls, xlsb and numfmt seeds) are unchanged; `make clippy` is green
  with no expectation left in the four files.
- **Credit:** none; numfmt's contributor line in `REUSE.toml` stays. **Depends on:** lints,
  binary-cursor; precedes units-meta. **Size:** about 450 lines. **Tier:** T2.

### W6

#### style: superscript and subscript from DOCX, ODF and EPUB as `<sup>`/`<sub>`
- **Goal:** `10<sup>-3</sup> M` and `H<sub>2</sub>O` keep their meaning in DOCX, ODT and EPUB output
  instead of flattening to `10-3 M` and `H2O`.
- **Scope** (through `/change-core-api`):
  - `Style.position: VerticalPosition { Baseline (default), Superscript, Subscript }`.
    `StyleDelta.position` is a tri-state in `merge` and `apply`, and `rebase_emphasis` leaves it alone.
    No underline, no `Config` field, fingerprint unchanged.
  - `render_text_run` (`inline.rs`): `<sup>`/`<sub>` outermost, then `~~`, `**`, `*`, then code. Content
    is escaped as styled text, and edge whitespace stays outside the tag.
  - DOCX: `w:vertAlign` in `rpr_delta` (`styles.rs`). Styles resolve it nearest-wins through `nearest`
    in this order: docDefaults, paragraph style, character style, direct (`walk_run`). A `NoteRef` is
    never wrapped.
  - ODF: `style:text-position` in `text_properties_delta` (`styles.rs`). `super` or a positive percent
    is superscript, `sub` or a negative percent is subscript, `0%` is baseline, anything else is unset.
  - EPUB: `<sup>`/`<sub>` in `merge_inline_tag` (`html.rs`). `vertical-align: super|sub|baseline` in
    `parse_declarations`, in both tiers, overrides the tag. `baseline` cancels only the element's own
    shift, so a reset sheet's `span, a { vertical-align: baseline }` keeps `<sup><a>` raised. Other
    values leave it unset.
  - `checkbox_text` leaves the public model; the renderer owns `[x]` and `[ ]`, and plain text keeps it
    privately. A crate-private `Block::plain_text` replaces `block_text` (`html.rs`). `ppt/mod.rs`
    names `position: Baseline`.
- **Files:** `src/model/{style,inline,block,mod}.rs`, `src/shared/{delta,html}.rs`,
  `src/render/markdown/{inline,tests}.rs`, `src/formats/docx/{styles,content}.rs`,
  `src/formats/odf/styles.rs`, `src/formats/ppt/mod.rs`, `tests/fixturegen/{docx,odf,epub}.py`,
  `tests/fixtures/{docx,odt,epub}/handmade-position.*` with their snapshots, `tests/it/style.rs` (new),
  `tests/it/json.rs`'s `handmade-rich.docx` JSON snapshot (runs gain `position`), the binding's
  `to_json` shape test, `REUSE.toml`.
- **Tests:** the three fixtures (direct, through a style, baseline over a superscript style) render
  identical Markdown. A pulldown-cmark proptest beside the link round trip: rendered runs parse back to
  the same text, emphasis and tags. Nesting and escaping. ODF percent values. EPUB: a `span` with super
  renders `<sup>`, `sup { vertical-align: baseline }` renders plain, a reset sheet stays raised.
  `text.docx`'s FootnoteReference snapshot stays byte-identical.
- **Credit:** none. **Depends on:** serde, py-api, links (the pulldown-cmark dev-dependency),
  shown-text (same run readers). **Size:** about 450 lines. **Tier:** T3.

#### codepages: one code-page table, exact Mac Roman, UTF-8 and Cyrillic, a reported XLS guess
- **Goal:** RTF and BIFF5 XLS files in Mac Roman, UTF-8, Cyrillic or ISO-8859 code pages convert exactly
  instead of failing `Incomplete` (RTF) or turning silently into mojibake (XLS).
- **Scope:**
  - New `src/shared/codepage.rs` takes `Codepage`, `TextLoss` and `decode` from `rtf/tables.rs` and
    adds `windows(u16)` and `charset(i32, default)`, each returning a `Codepage`: 874, 932, 936, 949,
    950, 1250-1258, 10000, 10007, 866, 20866, 21866, 28591-28599, 28603, 28605 (28591 and 28599 as
    WHATWG labels them), 54936, 65001; the charsets of `rtf/tables.rs` plus 77 (MACINTOSH) and 89
    (X_MAC_CYRILLIC). Anything else, 437 and 850 included, is windows-1252 marked assumed.
  - RTF: `codepage_control` reads `\mac` as 10000, `\pc` as 437, `\pca` as 850; `\ansicpg` still
    overrides them. Its rustdoc says a document naming no character set reads as declared windows-1252.
    Both RTF tables go.
  - XLS: CODEPAGE decodes through `windows`, 32768 read as 10000; the xls guess table goes. BIFF5 byte
    strings return `TextLoss`, and each site reporting `invalid_encoding` also reports
    `encoding_assumed` for non-ASCII text read in an assumed code page, in that cell or region; BIFF8
    strings never do.
  - DOC: `lid_encoding` yields a code page decoded through `windows`; output unchanged.
- **Files:** `src/shared/{codepage,mod}.rs`, `src/formats/rtf/{mod,tables}.rs`,
  `src/formats/sheet/xls.rs`, `src/formats/doc/mod.rs` (`lid_encoding`), `tests/fixturegen/rtf.py`, a
  generated `\mac` fixture and its snapshot, `REUSE.toml` (new files join `FSL-1.1-MIT`).
- **Tests:** `{\rtf1\mac \'8e\par}` gives é with no diagnostic; the cases for `\mac`, `\ansicpg10000`,
  `\ansicpg65001` and `\fcharset77` flip to exact text with no diagnostic; every other case that uses
  `\mac` only to reach an assumed code page moves to `\pca` with counts unchanged; `\ansicpg1361` and
  `\fcharset254` report `encoding_assumed`. XLS: a BIFF5 label `0x8E` under CODEPAGE 10000 or 32768
  reads é; under 437 it reports `encoding_assumed` in its unit; an ASCII label reports nothing. The
  fixture's snapshot.
- **Credit:** none. **Depends on:** shown-text, sheet-shape (both write these files first);
  legacy-shape follows and moves `lid_encoding` as it stands. **Size:** about 250 lines. **Tier:** T2.

### W7

#### units-meta: typed slide, sheet and spine units with Markdown spans; `max_pages` enforced
- **Goal:** slides, sheets and spine items become typed units with Markdown byte spans, slides get a
  visible boundary, and `max_pages` holds.
- **Scope:**
  - `Unit { kind: UnitKind { Page, Slide, Sheet, SpineItem }, ordinal: NonZeroU32 }` (the 1-based
    position), top-level `Block::UnitStart(Unit)`, `LinkTarget::Unit(Unit)`; serde-derived.
  - `ParseCtx::begin_unit(kind, position) -> Result<UnitScope, _>` replaces `enter_unit` everywhere
    (it takes the 0-based position; the ordinal is position + 1) and refuses ordinals past `max_pages`
    as `ResourceLimit`, so re-entry charges nothing.
  - Milestones (`Block::UnitStart`) only at the content sites, cited at `38a013d`: the pptx
    slide loop (`pptx/mod.rs:229`); ppt at its two slide sites, the slide-list walk
    (`ppt/mod.rs:585`, where a notes page re-entering its slide's unit adds none) and the recovery walk
    (`ppt/mod.rs:891`); ODF presentation pages (`odf/mod.rs:153`) and spreadsheet tables
    (`odf/table.rs:542`); the xlsx, xlsb and xls sheet loops (`sheet/xlsx.rs:97`, `sheet/xlsb.rs:105`,
    and `sheet/xls.rs:78` for shown sheets); the epub spine loop (`epub/mod.rs:76`). CSV is one Sheet.
    Every other `enter_unit` call becomes a `begin_unit` scope with no milestone: the unreadable slide
    (`pptx/mod.rs:187`), the slide relationship pre-read (`pptx/mod.rs:213`), the hidden-sheet and
    target pre-passes (`sheet/xlsx.rs:76`, `sheet/xlsb.rs:84`, and xls's hidden branch) and the
    image-unplaced report (`ppt/mod.rs:773`). Empty units keep their milestone; hidden and unreadable
    ones get none. pptx `slide_anchors` and the EPUB chapter anchors become `LinkTarget::Unit`.
  - Renderer: `---` before every slide but the first; `<a id="<kind>-<ordinal>">` only when linked;
    `Conversion::units()` yields `UnitSpan { unit, bytes }`, body only.
  - Re-adds `max_pages` (100 000) and `Limit::Pages` through config and binding.
  - `src/limits.rs` test: an exhaustive `Limit` match to an abuse prefix on disk, a `tests/it/limits.rs`
    test, or the `Allocation` case in `archive.rs`.
  - Binding (`/change-core-api`): `Conversion.units` as `(kind, ordinal, start, end)` str offsets.
  - The `max_work` rustdoc in `src/limits.rs` lists every charge site that has landed, the DOCX
    numbering-style walk to numId (one unit per child) included.
- **Files:** `src/model/{unit,block,link,mod}.rs` (`unit.rs` new),
  `src/{ctx,limits,config,converter,lib}.rs`, `src/render/markdown/{mod,anchors}.rs`, those frontends,
  `csv.rs`, `python/**`, `tests/it/units.rs` (new), `tests/it/{limits,converter}.rs`,
  `tests/fixturegen/{sheet,odf}.py`, the abuse fixture, pptx, ppt and odp snapshots, `REUSE.toml`
  (each pptx, ppt and odp snapshot it first modifies, `snapshots__odp__pres.odp.snap` included, joins
  `FSL-1.1-MIT AND MIT`; new files join `FSL-1.1-MIT`), `docs/upstream.md`.
- **Tests:** untitled PPTX, ODP and PPT slides: n-1 `---`, none leading or doubled; an empty middle slide
  keeps ordinals 1-3. Slide links past an unresolvable `sldId` and EPUB chapter links resolve. A hidden
  middle sheet: units 1 and 3. Spans, Python's on non-ASCII text included, slice their units.
  `max_pages = 2` refuses slide 3. `max_pages--pageflood--errors.odp`, committed: 100 001 `draw:page`
  elements in one `content.xml`, tripping `max_pages` at default limits (hidden sheets get no unit, and
  a PPTX that size trips `max_entry_count` first). The `Limit` map, with that fixture in its `Pages`
  row.
- **Credit:** `Co-authored-by: Mark Xian <53885024+xianjianlf2@users.noreply.github.com>` (firecrawl/anydoc
  #95); `Co-authored-by: tutu <10495375+Ready22Race@users.noreply.github.com>` (#32's ODP tests). Takes
  #10, #31, #32, #94 and #95; both authors' `REUSE.toml` lines.
- **Depends on:** lints, serde, py-api, binary-cursor, sheet-shape. **Size:** about 800 lines.
  **Tier:** T3.

### W8

#### images: place every picture as a counted `asset:N` placeholder under an image policy
- **Goal:** every picture keeps its position as `![alt](asset:N)`, resolvable against the document's
  assets and counted per region, and no rendered image fetches a URL.
- **Scope:**
  - `Config::images: ImagePolicy { Placeholder (default), AltText, Omit }`, `#[non_exhaustive]`,
    fingerprinted in `identity.rs`; no `ImageKind` (ADR 0001 decisions 5, 8 and 11).
  - `render_image` (`inline.rs`): an asset writes `![alt](asset:N)`, N its `AssetId`, link labels
    included, alt label-escaped, newlines folded; AltText writes alt; Omit writes nothing. Unavailable
    writes alt (Omit nothing). External writes `[alt](url)` through the links destination gate, alt
    alone inside a label, never `![`. `delims_of` and `shape.rs` follow the policy.
  - `render_losses` adds one per image by region: `image_placeholder`; `external_image` outside Omit;
    `image_omitted` for AltText assets and every Omit image; Unavailable nothing beyond the parse's
    `image_unavailable`. The three codes return to `src/diagnostics.rs` here with their `LossClass`
    (`Deferred`, `Omitted`, `Omitted`), since py-api removed them unemitted.
  - OLE objects (`docx/content.rs`, `pptx/mod.rs`) keep the payload asset and `embedded_object`, push no
    image and invent no alt; their preview picture renders (DOCX ShapeID match, PPTX `p:oleObj/p:pic`
    via `parse_picture`).
  - EPUB: a `figure` whose whitespace-collapsed `figcaption` text equals a contained image's alt clears
    that alt (`shared/html.rs`).
- **Files:** `src/{config,identity,converter,diagnostics}.rs`,
  `src/render/markdown/{inline,mod,shape,tests}.rs`, `src/formats/docx/content.rs`,
  `src/formats/pptx/mod.rs`, `src/shared/html.rs`, `python/src/**`, `python/almiladoc/_almiladoc.pyi`,
  `python/tests/`, `tests/fixturegen/epub.py`, image-bearing `tests/snapshots/*`, `docs/upstream.md`,
  `REUSE.toml`.
- **Tests:** `a[b]` on asset 3 renders `![a\[b\]](asset:3)`, alt, nothing per policy;
  `[![x](asset:0)](https://e.x)`; external `[x](https://e.x)`, and `javascript:` gives alt plus
  `unsafe_link`; a pulldown-cmark oracle reads the image destination and alt back; `render_losses`
  counts per policy and region; an Omit image-only paragraph leaves no block; the pinned fingerprint
  moves; `handmade-ole.docx` shows no "Embedded object:" and keeps its payload; a PPTX `oleObj` unit
  test; a new `handmade-figure.epub` shows the caption once per policy; Python `images="omit"` and
  stubtest.
- **Credit:** `Co-authored-by: Alexis Kowalski <312066465+alexiskowalski@users.noreply.github.com>`
  (firecrawl/anydoc #70, `src/render/markdown/inline.rs`; takes #70 and #63);
  `Co-authored-by: Shubham Mathur <16467235+mathurshubham@users.noreply.github.com>` (#4's dedupe,
  inverted). Moves both to Taken in `docs/upstream.md` and adds both authors' `REUSE.toml` lines. Takes
  issue #182 (no trailer): OLE previews are read, the AlternateContent branch already selected;
  `docs/upstream.md` moves #182 to Taken.
- **Depends on:** style, links, py-api, units-meta. **Size:** about 550 lines. **Tier:** T3.

#### legacy-shape: name the legacy binary codes, split the DOC reader, type cell merges
- **Goal:** public contributors read the DOC, PPT and OfficeArt readers through named codes, in modules
  that each hold one concern, with each cell's merge state held in a type. The output does not change.
- **Scope:** behaviour-preserving.
  - Named consts, matched as patterns, replace the hex literals: FIB offsets on `Fib`, sprm codes in
    `doc/sprm.rs`, OfficeArt record types and property ids in `shared/officeart.rs`, PPT record types in
    a new `ppt/records.rs`, and the TextPF and TextCF masks (`ppt/styletext.rs`). The comments that only
    label a code go.
  - `officeart::record_at` and `records` yield `Record { ver_inst, rec_type, body }` with `instance()`.
    This replaces ppt's tuple alias and the `ver_inst >> 4` tests. `TextType` replaces
    `matches!(tx_type, 0 | 6)`, and `metafile_parts` returns a struct.
  - `doc/mod.rs` is split at its section banners into `pieces`, `text`, `fkp` and `assemble`, and the
    banners go. `composite_label` moves to `lists`. `mod.rs` keeps `parse`, `Fib`, `Plc`, the stories
    and the floating shapes. Each module takes its tests and lint attributes with it.
  - `TableAccumulator` takes over the row and cell state of `build_blocks` and `flush_table`.
    `flush_shape` (`ppt/mod.rs`) reads through one run cursor. `build_edge_table` (`shared/grid.rs`)
    splits into edge, merge, chain and emit phases. No `too_many_lines` expectation stays in these
    files.
  - A per-axis `Merge { None, First, Continue, JoinOrStart }` replaces the bools in `CellProp`
    (`shared/grid.rs`) and `TapCell` (`doc/sprm.rs`). RTF `\clmgf` together with `\clmrg` maps to
    `JoinOrStart`: the cell joins an open group, or starts one if none is open, which is today's
    precedence. DOC never produces it.
- **Files:** `src/formats/doc/**`, `src/formats/ppt/**`, `src/formats/rtf/mod.rs`,
  `src/shared/{officeart,grid}.rs`, `REUSE.toml` and `.github/provenance.toml`: each module split from
  `doc/mod.rs` joins the `FSL-1.1-MIT AND MIT` list with every copyright line `doc/mod.rs` then carries
  and gets a `[named]` row to `src/formats/doc/mod.rs` where copy detection misses it; a file holding
  only new code joins `FSL-1.1-MIT`; `make provenance` judges each.
- **Tests:** grid unit tests for `JoinOrStart` on both axes, one with an open group and one without; an
  RTF unit test for `\clmgf\clmrg`; none of these compiles before the change; the TapCell assertions in
  the DOC tests are rewritten on `Merge`; corpus snapshots and diagnostics stay byte-identical and the
  fuzz replays are unchanged.
- **Credit:** none. **Depends on:** lints, binary-cursor, level-counters, shown-text and codepages (they
  write `doc/mod.rs` and `rtf/mod.rs` first), style and units-meta (they write `ppt/mod.rs` first); lands
  before legacy-style and presentation-style. **Size:** about 700 lines, not counting moved code; above
  1500, consts and modules first, then types. **Tier:** T2.

#### odf-fidelity: nest LibreOffice's split list levels under the item they continue
- **Goal:** ODP and ODT lists that LibreOffice writes in split form nest under their parent item and
  number without gaps, so they read the same as the same deck from PPTX or PPT.
- **Scope:**
  - The ODF `<text:list-item>` rule says an item that starts another list immediately and holds no text
    shows no label. In `parse_list` (`odf/text.rs`), an item whose first child is a `text:list` and that
    has no `text:p` or `text:h` child is a continuation. It gets no number and no label, and `next`,
    `list_counters` and `list_ids` do not count it.
  - Its nested blocks are appended to the preceding item of the same list. If it is the list's first
    item, they go to the last item of a `Block::List` that sits directly before it in the same container
    (an adjacent sibling `text:list`, as `pres.odp` writes it). If there is neither, they go out
    unwrapped beside the list, the way a list header's blocks do.
  - One helper does this attach both for the container's `list` arm and for nested lists inside an item.
  - The nested list's ancestor chain carries the number of the item it attaches to, so composite labels
    (`display-levels`) continue from that item.
- **Files:** `src/formats/odf/text.rs`; `tests/fixturegen/odf.py` (a new `splitlist_odp` and its call in
  `generate`); `tests/fixtures/odp/handmade-splitlist.odp` and its snapshot;
  `tests/snapshots/snapshots__odp__pres.odp.snap`; `REUSE.toml` (the new files join `FSL-1.1-MIT`).
- **Tests:** the `pres.odp` snapshot's list now reads like `pres.pptx`'s (`- Top level point`, then
  `  - Nested detail` under it, then `- Second point with emphasis`; today it reads `- - Nested detail`).
  `handmade-splitlist.odp` covers a `presentation:class="title"` frame, an ordered list with a split
  item between its items numbered 1, 2, a `display-levels="2"` level that reads `1.1`, and a leading
  continuation with no item before it, which renders unwrapped.
- **Credit:** none; no upstream item covers split lists. **Depends on:** units-meta, which rewrites
  `snapshots__odp__pres.odp.snap` with its slide boundaries; this item re-accepts that snapshot after
  it. **Size:** about 120 lines. **Tier:** T2.

### W9

#### blocks: render numbered lists ordered, unwrap table wrappers, escape after note references
- **Goal:** DOCX, ODT, RTF and DOC numbered lists become GFM ordered lists, a table inside a one-cell
  frame keeps its rows, and a note reference followed by `(` keeps its note.
- **Scope:** renderer only (ADR 0001 decision 8); no model, frontend or `shared/numbering.rs` change.
  - `write_list` (`render/markdown/mod.rs`): a `Decimal` list whose every label is `None`, `n` or `n)`
    (n = start + i) renders ordered, with `)` only when every label is `n)`; any other mix, Roman or
    alphabetic stays a labelled bullet.
  - An ordered list right after a sibling list rendered with the same delimiter takes the other
    delimiter, so CommonMark keeps its start rather than merging.
  - `layout_cell` also unwraps a table of either kind whose one origin cell holds, empty paragraphs
    aside, exactly one table, descending nested wrappers in a loop; `shape.rs` follows. `TableKind`
    stays.
  - After `Norm::NoteRef` (`inline.rs`) the next text run escapes a leading `(`, and a leading `:` when
    the reference opened its line, through a new `EscapeOpts` field (`escape.rs`).
- **Files:** `src/render/markdown/{mod,shape,inline,escape,tests}.rs`, `tests/fixturegen/docx.py`,
  `tests/fixtures/docx/handmade-{bare-number,nested}.docx` (new) and their snapshots,
  `tests/snapshots/snapshots__odt__handmade-lists.odt.snap` and any list snapshot the rule moves,
  `REUSE.toml`, `docs/upstream.md`.
- **Tests:** unit tests per rule (bare, `n)`, mixed, Roman, adjacent restart, wrapper beside text kept,
  two nested wrappers, `See[^1]\(2020)`, line-opening `[^1]\:`), all parsed back by style's
  pulldown-cmark oracle; upstream #17's three renderer tests; snapshots of `handmade-bare-number.docx`
  (#184's `%1`, `%1)`, interruption, `startOverride 9`) and `handmade-nested.docx`; the ODT list diff
  reviewed.
- **Credit:** firecrawl/anydoc #184 (`src/shared/numbering.rs`, `tests/gen_fixtures.py`) and #17
  (`src/render/markdown/{mod,tests}.rs`, `tests/gen_fixtures.py`), fixing #14. `docs/upstream.md` moves
  #14, #17 and #184 to Taken (#17 as a renderer unwrap); `REUSE.toml` declares `mod.rs`, `tests.rs`,
  `fixturegen/docx.py` and the new fixtures and snapshots `FSL-1.1-MIT AND MIT` with each author's line.
  `Co-authored-by: Chriscoveries <212684707+chriscoveries@users.noreply.github.com>` and
  `Co-authored-by: Shubham Mathur <16467235+mathurshubham@users.noreply.github.com>`.
- **Depends on:** links, style, images, odf-fidelity. **Size:** about 350 lines. **Tier:** T3.

#### legacy-style: RTF and DOC superscript, subscript and symbol-font characters
- **Goal:** RTF and DOC exponents, formulas and inserted symbols read as written (`x<sup>2</sup>`,
  `10 μm`, a ballot box), not `x2` or `10 (m`.
- **Scope:**
  - RTF `text_control` and the stylesheet (`rtf/tables.rs`): `\super` and `\sub` set position;
    `\nosupersub` and `\plain` reset it; `\up` and `\dn` leave it unset.
  - DOC `apply_chpx` (`doc/sprm.rs`): sprmCIss 0x2A48 (0, 1, 2); styles inherit through
    `apply_style_chpx`; sprmCHpsPos stays unread.
  - Fonts: `parse_fonttbl` (`rtf/tables.rs`) keeps each name and `\fcharset2`; DOC reads SttbfFfn names
    (FIB 0x112), charged per entry.
  - Routing, before `clean_text`, by the caller rule documented in `shared::symbol`: Symbol through
    `shared::symbol`; a Wingdings or Wingdings 2 ballot box becomes `Inline::Checkbox`, its match moving
    from docx `symbol_checkbox` (`content.rs`) into `shared::symbol`; any other symbol-font code emits
    `unmapped_symbol` and renders nothing. Inputs: RTF `\'hh` bytes undecoded, text and `\u` in such a
    font; DOC runs whose sprmCRgFtc0 (to U+007F) or sprmCRgFtc2 font is one, resolved in `char_style`'s
    order; DOC `(` under sprmCFSpec with sprmCSymbol 0x6A09 (ftc, xchar folded from U+F0xx), pushed as
    `(` today.
  - Note marks stay `[^n]`. Codes are checked against [MS-DOC] §2.6.1.
  - The DOC ballot box is written from [MS-DOC] and the symbol rule; upstream #176 is not taken, since
    it emits glyph text rather than `Inline::Checkbox` and ignores the style chain's sprmCSymbol, and
    `docs/upstream.md` records #176 as solved by this item's own commit, naming it.
- **Files:** `src/formats/rtf/{mod,tables}.rs`, `src/formats/doc/**`, `src/shared/symbol.rs`,
  `src/formats/docx/content.rs`, `tests/fixturegen/{rtf,doc}.py`, the `handmade-scientific.{rtf,doc}`
  fixtures, seeds and snapshots, `REUSE.toml`, `docs/upstream.md`.
- **Tests:** in-module, each failing today: every control and sprm above, direct and through a style;
  Symbol `m`, `\'b1`, `慔9` and xchar F06D give μ or ±; Wingdings 0xFE gives `[x]`; another symbol font,
  an unknown ftc and a malformed SttbfFfn emit `unmapped_symbol`; a piece Prm overrides the CHPX;
  `\up6` stays plain; a superscript `\chftn` renders `[^1]`. Both fixtures render identical Markdown;
  fixturegen `doc` gains per-run CHPX FKP and SttbfFfn writers.
- **Credit:** none. **Depends on:** style, symbols, shown-text, legacy-shape, codepages. **Size:** about
  650 lines. **Tier:** T3.

#### presentation-style: deck superscripts, subscripts and Symbol-font characters in PPTX and PPT
- **Goal:** a deck's exponents, formulas and Symbol-font letters convert as written (`m<sup>2</sup>`,
  `H<sub>2</sub>O`, `10 μm`, not `m2`, `H2O`, `10 mm`), and PPTX and PPT give the same result.
- **Scope:**
  - PPTX: `cascade::rpr_delta` reads `@baseline` into the position tri-state: above 0 is superscript,
    below 0 subscript, 0 baseline. A trailing `%` is accepted; any other value leaves the property
    unset. It cascades through the `defRPr` levels as `b` does.
  - PPTX: `TextProps` (`cascade.rs`) gains the `a:latin` and `a:sym` faces, classified when read by
    `shared::symbol`'s font-name rule; theme refs (`+mn-*`, `+mj-*`) are not routed.
    `parse_para_inlines` takes the folded `TextProps` and, before `clean_text`, routes U+F000-F0FF by
    `a:sym`, falling back to `a:latin`, and every other character by `a:latin`.
  - PPT: `parse_cf_exception` (`ppt/styletext.rs`) reads `fontRef`, `symbolFontRef` and `position`
    (signed, read as above) into `CharProps` and `MasterLevel`, where it skips them today.
    `read_document` (`ppt/mod.rs`) reads the environment's `FontCollection` (`0x03F2` > `0x07D5` >
    `FontEntityAtom` `0x0FB7` faces). `flush_shape` sets position and routes each character before
    `clean_text` by the same slot rule. A ref past the end of the collection names no face and emits
    nothing.
  - Routing calls the caller rule in `shared::symbol` and never copies it. Any other symbol face emits
    `unmapped_symbol` in the current unit and region. Out of scope: underline, units, OLE previews,
    hidden slides.
- **Files:** `src/formats/pptx/{cascade,mod}.rs`, `src/formats/ppt/{styletext,mod}.rs`,
  `tests/fixturegen/{pptx,ppt}.py`, `tests/fixtures/pptx/handmade-scripts.pptx`,
  `tests/fixtures/ppt/handmade-scripts.ppt`, their snapshots, `REUSE.toml`.
- **Tests:** a master superscript is inherited by one run and cancelled by `baseline="0"` on the next; a
  Symbol `a:latin` run's `m` and an `a:sym` U+F06D give `μ` beside a `+mn-lt` run that stays `m`; a
  Wingdings run adds `unmapped_symbol`; a PPT exception that carries size, colour and position reads
  position; a `fontRef` naming Symbol gives `μ`; one deck saved as `.pptx` and as `.ppt` gives identical
  Markdown. Each fails before.
- **Credit:** none; upstream #95 goes with units-meta. **Depends on:** symbols, binary-cursor, style,
  units-meta, images (both write `pptx/mod.rs`), legacy-shape (owns `ppt/**` before it). **Size:** about
  420 lines. **Tier:** T2.

### W10

#### pdf-model: PDF through the model, with page verdicts and page gaps
- **Goal:** PDF gets the same escaping, link policy, limits and diagnostics as every other format; a
  scanned page becomes a fillable gap, not an empty result.
- **Scope:**
  - One per-page extraction (`strip_headers_footers`, `escape_text`, `include_images`,
    `detect_underline: false`) replaces the bypass in `src/converter.rs`, `src/formats/pdf.rs` and
    `src/formats/mod.rs`.
  - Each page calls `ctx.begin_unit(UnitKind::Page, index)?` (0-based page index): Text; Blank
    (`no_text`, or flagged, reasonless and empty); else `Block::Gap { id: GapId { unit, ordinal },
    origin: OcrOrigin::Page(n), reasons }`.
  - New `src/shared/markdown.rs`: pulldown-cmark into the model, each event charged, nesting capped by
    `max_nesting_depth`. The reader takes a mode: page text maps `<sup>`, `<sub>` and `<s>` to `Style`
    and escapes other HTML; a gap fill (ocr-channel) escapes all raw HTML.
  - `Converter::inspect(Input)` for PDF only (ADR 0001 decision 4), on the pool: container facts,
    diagnostics and 1-based page units, each `Text`, `Blank` (`no_text`) or `NeedsOcr { reasons }`; any
    other format is `Unsupported`; distinct from `package::directory::inspect`.
  - A placed figure renders nothing, adding `image_omitted`.
  - `Parsed::gaps()`; `parse` accepts PDF; `UnsupportedKind::PdfDocumentModel` goes.
  - Unfilled gaps: `Fail` raises `NeedsOcr { pages, page_count }` before `Incomplete`; `Report` writes
    stable `<!-- needs-ocr page N -->` and counts `ocr_gap`.
  - `ocr_pages: PageSelection { None (default), All, Pages(BTreeSet<NonZeroU32>) }`, a parse option
    (`Input::with_ocr_pages`): a selected page becomes a `Page` gap with reason `caller_requested`
    whatever its text layer; its extracted text is discarded, never merged, and reports no loss. A page
    number past `page_count` is refused before extraction as
    the new variant `ConvertError::InvalidConfig(ConfigError::PageOutOfRange { page, page_count })`
    (ADR 0001 decision 9), code `invalid_config`: one variant for every caller-configuration error,
    reusing `Converter::new`'s `ConfigError`, mirrored through change-core-api.
  - Binding via `/change-core-api`. Above 1500 lines, `ocr_pages` splits out.
- **Files:** `src/formats/{pdf,mod}.rs`, `src/shared/markdown.rs` (new), `src/shared/mod.rs`,
  `src/model/gap.rs` (new), `src/model/{block,mod}.rs`, `src/render/markdown/mod.rs`, `src/inspect.rs`
  (new), `src/{converter,error,lib,identity}.rs`, `Cargo.{toml,lock}`, `deny.toml`, `about.toml`,
  `THIRD-PARTY-LICENSES`, `REUSE.toml`, `docs/upstream.md`, `tests/fixturegen/pdf.py`, PDF fixtures and
  snapshots, `tests/it/pdf.rs` (new), `tests/it/{snapshots,converter}.rs`,
  `python/{src/lib.rs,almiladoc/_almiladoc.pyi,tests/test_almiladoc.py}`.
- **Tests:** five pages TEXT/IMAGE/TEXT/TEXT/IMAGE: `Fail` gives `NeedsOcr { [2, 5], 5 }` despite a
  `Lost`; `Report` keeps pages 1, 3 and 4. Pinned verdicts: blank, header-only, image-only, invisible
  text, at most 10 inkless characters, running-head stamped scan, tiled scan. `<script>`, `1. x`,
  `[x](javascript:…)` and a forged marker render as DOCX does; sup, sub and strike; figures pass `Fail`;
  `ocr_pages` both ways, and a page past `page_count` refused before extraction with the ruled variant;
  a lowered `max_pages` trips; the five-page fixture's units are Page 1-5 in order, gaps included; a
  garbled-encoding page still adds `garbled_font_encoding`; output is byte-identical across two runs as
  well as at 1 and 4 threads; inspected page verdicts and 1-based page indices equal the parse's units
  and gaps on the five-page fixture, and a DOCX gives `Unsupported`.
- **Credit:** `Co-authored-by: Hill Patel <157357792+STiFLeR7@users.noreply.github.com>` (#166);
  `Co-authored-by: Larue <289724405+larue-greenboard@users.noreply.github.com>` (#153's fixture);
  `Co-authored-by: James Yang <106619425+HaoChiBao@users.noreply.github.com>` (#91); origin
  firecrawl/anydoc; takes #62, #91, #144, #153, #162 and #166.
- **Depends on:** the pdf-inspector release (External tracks); serde, py-api, links, style, units-meta,
  blocks. **Size:** about 1400 lines. **Tier:** T3.

### W11

#### ocr-channel: fill page and image gaps with the caller's OCR
- **Goal:** the caller's OCR completes flagged pages and pictures in place.
- **Scope:**
  - `Parsed::fill_gap(&mut self, GapId, GapFill) -> Result<(), GapError>` and `fail_gap(GapId)`, on the
    pool; `GapState { Unfilled, Failed, Filled(Vec<Block>) }`; gaps indexed once.
  - `GapFill::Markdown`: `max_ocr_text_bytes` (1 MiB) checked first; pdf-model's pulldown-cmark reader in
    `src/shared/markdown.rs`, read in fill mode (all raw HTML escaped, `<sup>` included), tables,
    strikethrough, emphasis, headings, lists and code only; links as label, images as
    alt; each event charged; container plus fill depth within `max_nesting_depth`; NFC.
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
       item's blocks join with a space; a layout single-cell table renders as blocks.
  - `image_not_ocred` and `ocr_failed` return to `src/diagnostics.rs` here with their `LossClass`
    (`Omitted`, `Deferred`), since py-api removed them unemitted.
  - Python mirror via `/change-core-api`: `fill_gap(gap, *, markdown | lines)`.
  - Revisit the grouping rule if a scan corpus with paragraph ground truth shows single-column pages
    missing over 5% of breaks (or over 5% after headings), more than 2% of joins wrong, or 2° of skew
    missing 5 points more breaks than the upright page (oriented geometry is then owed). A change to
    grouping after release alters OCR Markdown, which ADR 0001 decision 15 allows only outside a patch
    release.
- **Files:** new `src/ocr/{mod,fill,group,images}.rs`; `src/shared/markdown.rs` (from pdf-model);
  `src/{converter,config,identity,lib,limits,diagnostics}.rs`, `src/model/gap.rs`,
  `src/render/markdown/{mod,shape,table}.rs`; `Cargo.toml` (unconditional `unicode-normalization` and
  `pulldown-cmark`), `Cargo.lock`, `fuzz/{Cargo.toml,Cargo.lock}`, `fuzz/fuzz_targets/gap_fill.rs`
  (new), `fuzz/seeds/gap_fill/` (new), `fuzz/regressions/gap_fill/` (new); `tests/it/ocr.rs` (new),
  `tests/fixturegen/docx.py`, snapshots; `python/{src/lib.rs,almiladoc/__init__.py,
  almiladoc/_almiladoc.pyi,tests/test_almiladoc.py}`; `README.md`, `REUSE.toml`, `docs/upstream.md`.
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
    filled in reverse give document order; a nested cell; a list in a cell; a layout single-cell table;
    cell text `| <br> # h` renders escaped in one cell; under `Report` with no engine, unfilled image
    gaps give the `ImageOcr::None` Markdown, lists tight. A page gap is replaced in place; an empty fill
    leaves nothing; hand-run phases equal `convert`.
  - Pool threads equal `threads`; Python equals Rust; `gap_fill` replays.
- **Credit:** none; issue #157 moves to Taken.
- **Depends on:** pdf-model, images, units-meta. **Size:** about 1100 lines. **Tier:** T3.

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
declined, fall back as ADR 0001 decision 6 and ADR 0002 say. The first check is 2026-10-22; after it,
pdf-inspector's pull requests are checked at the start of each wave until pdf-model lands, and hayro's
and rten's when the deferred item they gate is next. Check a pull request with
`gh pr view <n> --repo <repo> --json state,mergedAt,reviewDecision`.

- **pdf-inspector release** gates pdf-model: a per-page call taking `MarkdownOptions`, header stripping
  across pages, escaped source text, a `no_text` reason and no environment reads (ADR 0001 decision 6).
  If #633-#635 remain unreleased at the first check, the maintainer forks and publishes
  `almiladoc-pdf-inspector` on an upstream tag carrying only these pull requests, and ADR 0001
  decision 6 names the published fork crate. #636 (the returned document) is not required by that decision.
- **hayro** (dormant; gates only the deferred rasterizer and local OCR). `LAB.icc`'s licence is stated
  upstream (#1497), so `NOTICE` cites it from the first hayro release that carries it. Font-loss
  warnings arrive with the first hayro release that carries #1498. Open: undecodable page content has
  no upstream warning, since upstream declined #1499 until it reworks warnings.
- **rten** (dormant; gates only the deferred local OCR). rten's AI policy bars agent-authored pull
  requests, so the maintainer opens and answers them. If they stay unmerged, an rten fork follows the
  pdf-inspector pattern.
- **lopdf** (candidate, not opened): declare `flate2` with `default-features = false` plus the backend it
  needs, so a dependent choosing `zlib-rs` drops `miniz_oxide`; pdf-inspector's lopdf pin moves after.
- **Consumer proof** (a change in the primary consumer's own repository). Trigger: the `almiladoc`
  candidate wheel on TestPyPI (release, W5) or on PyPI. It proves the consumer depends on that wheel
  instead of firecrawl-anydoc, a test asserts
  `importlib.metadata.version("almiladoc")` equals the candidate and that no `anydoc` import remains,
  and its suite is green. The first run certifies the non-PDF formats and today's PDF path only; each
  0.x minor carrying pdf-model or ocr-channel reruns it, and 1.0 needs it green on the 1.0 candidate.

| Repository | Pull request | Serves | State |
|---|---|---|---|
| firecrawl/pdf-inspector | [#631](https://github.com/firecrawl/pdf-inspector/pull/631) | no `PI_NO_MERGE` environment read | open |
| firecrawl/pdf-inspector | [#632](https://github.com/firecrawl/pdf-inspector/pull/632) | no `PDF_INSPECTOR_BCMAPS_DIR` read | open |
| firecrawl/pdf-inspector | [#633](https://github.com/firecrawl/pdf-inspector/pull/633) | per-page `no_text` reason | open |
| firecrawl/pdf-inspector | [#634](https://github.com/firecrawl/pdf-inspector/pull/634) | per-page `MarkdownOptions`, cross-page header stripping | open |
| firecrawl/pdf-inspector | [#635](https://github.com/firecrawl/pdf-inspector/pull/635) | `escape_text` option, source text escaped | open |
| firecrawl/pdf-inspector | [#636](https://github.com/firecrawl/pdf-inspector/pull/636) | the loaded document returned beside per-page Markdown (stacked on #634) | open |
| LaurenzV/hayro | [#1497](https://github.com/LaurenzV/hayro/pull/1497) | `LAB.icc`'s licence stated | merged |
| LaurenzV/hayro | [#1498](https://github.com/LaurenzV/hayro/pull/1498) | a warning when a font cannot be loaded | merged |
| LaurenzV/hayro | [#1499](https://github.com/LaurenzV/hayro/pull/1499) | a warning when a page content stream fails to decode | declined |
| robertknight/rten | [#1523](https://github.com/robertknight/rten/pull/1523) | the global pool only when no custom pool is set | merged |
| robertknight/rten | [#1524](https://github.com/robertknight/rten/pull/1524) | load-time work on the caller's pool | open |
| robertknight/rten | [#1525](https://github.com/robertknight/rten/pull/1525) | pool build failure visible to the caller | open |

## Maintainer steps

Asked only when the item that needs them is next.

| Step | Needed by | When |
|---|---|---|
| Fork and publish `almiladoc-pdf-inspector` if #633-#635 stay unreleased (first publish with a scoped crates.io token, ADR 0003 decision 9) | pdf-model | the 2026-10-22 check |
| Repository settings, release environments, pending trusted publishers on TestPyPI and PyPI, the crates.io `publish-new` token, the first candidate tag (ADR 0003 decision 9); the final 0.x tag after a green consumer proof | release | W5 |
| Agree with the consumer the PDF-path scope and how it maps `Incomplete` and `Internal` | consumer proof | when the proof starts |
| Ask the consumer whether its renderer shows `<sup>` and `<sub>` | style | W6 |
| A scan corpus with paragraph ground truth and headings marked (the consumer's) | grouping revisit, ocr-local-pipeline | when either is next |
