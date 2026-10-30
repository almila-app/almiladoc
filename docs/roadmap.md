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

No wave is open.

Before first publication prove native PDF and real-consumer integration together, and measure pool stack costs (ADR 0001).

## External tracks

- **unpdf image coverage:** union area is exact up to 256 image paints and sampled beyond, so crafted strips can keep a page under `CoverageFirst`'s half, leaving it to the default rule; adopt the release that measures it exactly (a sweep line).
- **Offline OCR quality:** evaluate native OCR on representative consumer scans once a labelled corpus has paragraph ground truth and marked headings.
- **Consumer proof:** after the candidate wheel reaches TestPyPI/PyPI, the consumer replaces its old converter without shims, asserts `importlib.metadata.version("almiladoc")` equals the candidate, asserts no old-converter import remains and passes its suite; first proof covers all formats and PDF through the model, every later 0.x minor repeats it, and 1.0 proves its own candidate.
- **Consumer agreement:** at proof start agree PDF scope, `Incomplete`/`Internal` handling, two-phase transport, prompt and concurrency; existing Xiaomi endpoint/settings need no additional live run before the first final tag (owner decision).

## Maintainer steps

As each release step becomes due: green consumer proof → final 0.x tag. At the first final, switch the README install lines to PyPI and crates.io. Before first final, supply `CRATES_IO_PUBLISH_NEW_TOKEN` (`publish-new`) in `crates-io` (ADR 0003 decision 7).
