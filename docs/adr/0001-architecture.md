# 0001. Architecture

Status: accepted
Date: 2026-10-14

## Context

- The primary consumer, a Python document-conversion service, converts untrusted documents in-process and OCRs out of process.
- Upstream, PDF bypasses the model, and recovery drops content yet returns `Ok` (https://github.com/firecrawl/anydoc/issues/187).

## Decision

1. **Distribution.**
   - Crate `almiladoc` and the unpublished abi3 cdylib `python/` (PyPI `almiladoc`) share one version, with `fuzz/` outside; both declare `FSL-1.1-MIT AND MIT` and ship ADR 0003's `LICENCE_SET`.
   - Features: default `pdf`; `serde`; `render` (hayro, png, zune-jpeg); `ocr-http` (`render`, ureq); `ocr-local` (`render`, rten crates) once ADR 0002's gate passes; `ocr-fetch` (`ocr-local`, ureq). The binding enables all but `ocr-fetch`.

2. **API.**
   - `Converter::new(Config)` validates `Config` and `Limits`, failing `ConfigError`.
   - `inspect`, `parse` and `convert` take `Input::new(bytes, format)`, optionally `with_cancel(CancelToken)`; `Format::sniff` is separate.
   - `parse` returns `Parsed { document, diagnostics, units }`, keeping its `Config` and any PDF document its extraction read; `Parsed::render()` ends `convert` with a `Conversion`, its `ocr_provenance` outside `PartialEq`.
   - `Parsed::rasterizer()` (`render`) draws ADR 0002's gaps without a second extraction.
   - Public types are `#[non_exhaustive]`; `Converter` is `Send + Sync`.

3. **Threads.** Each call that walks input runs inside `install` on a per-call pool of `config.threads` (default 1), its stack the measured per-level cost times decision 12's ceiling, so lopdf's `par_iter` (https://docs.rs/crate/lopdf/0.45.0/source/src/reader.rs#1055-1064) never reaches the global pool. ADR 0002 decision 8's `ocr-local` `recognize` alone runs outside it, on its own per-call rten pool.

4. **Inspect** reports container facts, diagnostics and units (1-based PDF pages, slides, sheets or spine items, else one), each PDF page `Text`, `Blank` (`no_text`) or `NeedsOcr { reasons }`.

5. **Model.**
   - The model gains underline, superscript and subscript, image kind, unit milestones, metadata and region-tagged side content.
   - `Block::Gap` awaits OCR under an opaque `GapId` of unit plus ordinal; `Parsed::gaps()` yields each gap's id, `OcrOrigin` and reason.
   - Both OCR paths return outcomes through ADR 0002's channel under the kept `Limits`: `fill_gap` takes lines, identity and the `Rendered` spec and diagnostics; `fail_gap` records `ocr_failed`.
   - `GapError` is `UnknownGap`, `AlreadyFilled`, `IdentityMismatch` (the first fill fixes the identity; an identity whose `RasterSpec` differs from the rendered one is refused alike) or `ResourceLimit`; a refused call changes nothing.

6. **PDF.**
   - Step 6 waits for a pdf-inspector release whose per-page call takes `MarkdownOptions`, strips headers across pages, escapes source text, reports `no_text`, returns the repaired, decrypted document and reads no environment.
   - Upstream pull requests come first; the fallback, a fork on an upstream tag carrying only them, lives in its own repository, which publishes `almiladoc-pdf-inspector` (first version by a scoped token, later by its own trusted publisher) before any `almiladoc` candidate depends on it. Either ships Adobe's BSD-3-Clause CMaps.
   - pulldown-cmark parses each escaped page into the model, `<sup>`, `<sub>`, `<u>` and `<s>` into `Style`.

7. **Adapters.** Crate-private frontends take one `ParseCtx`.
   - ZIP facts come from the central directory alone; duplicate names, in OPC after ASCII case folding, are `Malformed`.
   - XML: declared (DTD) entities never expand; an internal subset is `Malformed` in OOXML and ODF and ignored in EPUB (https://www.w3.org/TR/epub-33/#sec-xml-constraints), whose XHTML resolves HTML5 named references, others `Lost`.
   - Sniffing reads bounded container metadata, OPC relationships included. A body contradicting its label fails `Unsupported { kind: FormatMismatch { detected } }`.

8. **Serializer.**
   - `<sup>`, `<sub>` and `<u>` wrap escaped content, bare under `inline_markup = Plain`; raw HTML is only those, `<a id>` anchors, `<br>` in cells and `needs-ocr` and `ocr-failed` page comments.
   - Images render by policy as `![alt](asset:N)` placeholders, alt text or nothing; an unfilled image gap renders as its image.
   - Links keep `http`, `https`, `mailto`, anchors and relative paths, else label plus `unsafe_link`. The escaper breaks GFM autolink literals.

9. **Errors.** thiserror `ConvertError`: `Unsupported`, `Malformed`, `Encrypted`, `MissingPart`, `ResourceLimit { limit, max }`, `NeedsOcr { pages, page_count, gaps }`, `Incomplete { diagnostics }`, `Ocr { gap, origin, kind }`, `Cancelled`, `Internal`; recovery never swallows the last two or `ResourceLimit`. Input-derived fields are capped and escaped. A refused `try_reserve` is `ResourceLimit`; boundary `catch_unwind` maps panics to `Internal`.

10. **Diagnostics** are free-text-free `{ code, class, region, unit, count }`, aggregated, bounded, never dropped, classed `Omitted` (policy), `Deferred` (placeholder, gap, `ocr_failed`), `Lost` (unread) or `Info`. Default `on_loss = Fail` raises, in order, `Ocr`, `NeedsOcr`, `Incomplete`; `Report` returns the `Conversion` with `is_complete()` false; `log` goes.

11. **Content policy.** Headers, footers, comments, revisions, hidden text, images and `ImageOcr` are explicit, fingerprinted policies, defaulting to today's output except images (`Placeholder`).

12. **Limits.** Today's constants become `Limits` defaults; new ones (`max_input_bytes`, `max_compression_ratio`, `max_pages`, `max_output_bytes`, `max_work`), beside ADR 0002 decision 11's OCR limits, take fixed defaults, each documented on `Limits` with its reason, that admit ordinary large documents; one abuse fixture per limit trips it. `max_nesting_depth` bounds every recursion over input nesting, the renderer's included, below a compile-time ceiling. One saturating ledger charges deterministic `max_work` fuel and checks cancellation.

13. **PDF engine.** Limits and cancellation stop at pdf-inspector, lopdf and hayro, whose decoders allocate from stream headers (https://docs.rs/crate/hayro-syntax/0.8.0/source/src/filter/ccitt.rs); the consumer isolates PDF work in a memory-capped, recyclable worker.

14. **No global state.** `clippy.toml` disallows environment, filesystem, network, process, thread, pool and once-cell items, PyO3's included; only decision 3's pool, the binding's file and exception modules, OCR adapters and fixture tests `#![expect]` them. No panic hook; callers recycle the worker after `Internal`.

15. **Identity.**
    - Under the committed `Cargo.lock`, equal version, configuration, input and OCR lines give byte-identical output across threads, calls and, except PDF, architectures; non-PDF code calls no platform floating-point function (https://doc.rust-lang.org/1.99.0/std/primitive.f64.html#method.powi).
    - Identity does not hold while pdf-inspector's `PI_NO_MERGE` or `PDF_INSPECTOR_BCMAPS_DIR`, or rten's `RTEN_*`, is set, until the pinned releases read none.
    - `EngineIdentity`, with `ocr` and a `config_fingerprint` excluding limits, threads and cancellation, is a contract; `OcrProvenance` adds each gap's lines digest. Patch releases never change Markdown that converted before.

16. **Lints.** `unsafe_code = "forbid"` in both crates; `missing_docs` and `pedantic` warn. Outside tests, clippy denies panicking, unchecked or lossy operations, hash-order iteration, printing, exit and `allow`. Existing violations sit in shrinking per-module `#![expect]`s. Release builds keep overflow checks and unwinding.

17. **Python.**
    - Names mirror Rust's: frozen, Rust-defaulted `Limits` and `Config` taking `HttpOcr` or, once ADR 0002's gate passes, `LocalOcr`, an optional `CancelToken` per call, and `Parsed` with `render()` and versioned `to_json()`.
    - Input meets `max_input_bytes` before any copy, unlike `PyBackedBytes`'s of a `bytearray` (https://docs.rs/crate/pyo3/0.29.3/source/src/pybacked.rs#273-288); `bytes` is borrowed and the GIL released; `Parsed` owns a copy of what extraction read, within `max_input_bytes` (ADR 0002 decision 12).
    - Each error code has an `AlmiladocError` subclass, `OcrError` with `gap`, `origin`, `kind` and `retryable`.

18. **Gates.**
    - Architecture test: `model` imports only `limits`; adapters import no renderer or OCR adapter and reach the context only through `ParseCtx`; no interior-mutable `static` lies outside decision 14's modules.
    - Abuse fixtures cover every limit, the deepest converting on decision 3's pool.
    - Proptests cover the serializer and CSV, OOXML and ODF parsers.
    - Non-PDF fixtures are byte-identical across tested targets; PDF snapshots across runs and threads on one target. Fuzz targets cover every format.

19. **Order.** 1. Trim and rename, as ADR 0004's first candidate. 2. Dependency upgrades. 3. Configuration, ledger, diagnostics, errors. 4. Lints. 5. Model, serializer. 6. PDF and inspect, then the OCR port with `none`, `render` and `ocr-http`. 7. Binding.
    - The first release follows step 7 and the primary consumer's integration tests of step 6's adapters, without shims, and carries no `#![expect]` of a panicking or unchecked-indexing lint outside tests; `ocr-local` follows its gate, never blocking a release.

## Consequences

- Under `on_loss = Fail`, documents upstream converted while skipping OLE objects, VML pictures, charts or `mc:Fallback` fail `Incomplete`, per a site table owed before step 3.
- RTF labelled DOC and cross-labelled ODF, which upstream converted, fail `FormatMismatch`; callers retry with `detected`.
- Owed: measured stack costs and the pdf-inspector pull requests, and hayro's `LAB.icc` licence (ADR 0002) before the first release.

## Alternatives considered

- `Format::Image`: a consumer OCRs a standalone image itself.
- A public `markdown::render` for caller-built models: unrequired, and on the caller's stack.
- An ambient-read inventory: machinery for three known reads.
- A git fork: `cargo publish` refuses it (https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#specifying-dependencies-from-git-repositories). Vendoring: 137k lines of lint debt.
