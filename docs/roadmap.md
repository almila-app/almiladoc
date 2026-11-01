# Roadmap

Open work only; [ADRs](adr/) own decisions. Landing an item removes its section and Waves row; partial completion narrows it, and evidence correcting a wave updates it; item/wave ids stay here, outside rustdoc, README, commits and test names.

## Item rules

- **Proof:** leave `make ci` green; new tests fail before the change. Limit/context tests cover each forwarding/reading site and kill a default-substitution mutant; independent admission/in-flight checks each need a test that kills their own mutant.
- **Limits:** default-limit inputs prove abuse resistance; lowered limits prove charge sites only. First enforcement owns field rustdoc; committed abuse inputs own `tests/it/limits.rs` rows; model fields/types/diagnostics ship only with their first producer, declared once with fixed loss class and additive on non-exhaustive types.
- **Size:** about 800 hand-written changed lines maximum, excluding generated files, fixtures, snapshots and locks; split larger items at a named or natural seam.
- **Ownership:** parallel wave items share no file except REUSE path lists and `tests/it/main.rs` mod lines, whose conflicts retain both sides; other conflicts stop landing. Manifest/lock writers run serially; one item/wave touches `python/`, and each fixture generator has one owner.
- **API/fixtures:** use `change-core-api` and `add-fixture`; JSON schema bumps only for removal, rename or meaning changes. Abuse names are `<limit>--<shape>--errors.<ext>`; generate inputs over 1 MiB in tests and foreign containers through `zip::ZipWriter`/`cfb`; no third-party bytes; update only owned/named format snapshots, with separate diagnostic snapshots.
- **Support:** changes to reads/skips/reports update [support](support.md); hygiene checks the diagnostic code set both ways, not format attribution.
- **Refactors:** preserve Markdown, snapshots, diagnostics, errors, charges, API and binding; necessary behavior changes need explicit acceptance/regressions, never disguised cleanup or automatic deferral. Compact tests retain every case/defect.
- **Risk:** `Tier` is T0 mechanical (for example locks), T1 non-decision prose/new tests, T2 ordinary code, T3 public contracts/gates/design; [ADR 0004](adr/0004-maintainer-landing.md) owns landing and trailers.
- **Updates:** adopt latest stable dependencies/toolchains/runners/tools immediately on a green candidate, outside waves, as signed-off T0 commits closing Dependabot PRs with links (ADR 0003 decision 8).

## Waves

`∥` means disjoint parallel work; `then` binds tighter and means serial. Unlisted items land separately before the first wave sharing their files.

| Wave | Items | Placement |
|---|---|---|
| W7 | unpdf-clip (when upstream excludes hidden raster gaps) | |

Before first publication prove native PDF and real-consumer integration together, and measure pool stack costs (ADR 0001).

## Items

### W7

#### unpdf-clip: count only unpaintable runs the page shows

- Adopt unpdf's release whose `RasterGaps` excludes clipped/off-page unpaintable runs; a filled page then reports no `raster_incomplete` for them and ADR 0002's limitation disappears.
- Files: `Cargo.toml`, `Cargo.lock`, `fuzz/Cargo.lock`, `tests/it/raster.rs`, `THIRD-PARTY-LICENSES`, ADR 0001 decision 6, ADR 0002; depends on upstream release, about 40 lines, T2, no external credit.

## External tracks

Check open unpdf PRs at each wave start with `gh pr view <n> --repo <repo> --json state,mergedAt,reviewDecision`; 0.51.0 already carries the merged text/paint-budget fixes.

| Upstream | Needed behavior |
|---|---|
| [unpdf #24](https://github.com/iyulab/unpdf/pull/24) | exclude hidden/off-page raster gaps (unpdf-clip) |
| [unpdf #25](https://github.com/iyulab/unpdf/pull/25) | count dropped bare margin page numbers for `footer_omitted` |
| [unpdf #26](https://github.com/iyulab/unpdf/pull/26) | retain URI annotations on covered text |

- **Offline OCR quality:** evaluate native OCR on representative consumer scans once a labelled corpus has paragraph ground truth and marked headings.
- **Consumer proof:** after the candidate wheel reaches TestPyPI/PyPI, the consumer replaces its old converter without shims, asserts `importlib.metadata.version("almiladoc")` equals the candidate, asserts no old-converter import remains and passes its suite; first proof covers all formats and PDF through the model, every later 0.x minor repeats it, and 1.0 proves its own candidate.
- **Consumer agreement:** at proof start agree PDF scope, `Incomplete`/`Internal` handling, two-phase transport, prompt and concurrency; existing Xiaomi endpoint/settings need no additional live run before the first final tag (owner decision).

## Maintainer steps

As each release step becomes due: green dispatched `release.yml` rehearsal → `v0.1.0-rc.1` approved in `testpypi` → green consumer proof → final 0.x tag. Before first final, supply `CRATES_IO_PUBLISH_NEW_TOKEN` (`publish-new`) in `crates-io`; before the next final, add the previous final's row to `docs/release-dates.tsv` (ADR 0003 decisions 7/9).
