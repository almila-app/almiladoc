# 0001. Architecture

Status: accepted
Date: 2026-10-14

## Context

- The primary consumer, a Python document-conversion service, converts untrusted documents in-process and OCRs out of process.
- Upstream, PDF bypasses the model, and recovery drops content yet returns `Ok` (https://github.com/firecrawl/anydoc/issues/187).

## Decision

1. **Distribution.**
   - Crate `almiladoc` and the unpublished abi3 cdylib `python/` (PyPI `almiladoc`) share one version, with `fuzz/` outside; both declare `FSL-1.1-MIT AND MIT` and ship ADR 0003's `LICENCE_SET`.
   - Features: default `pdf`; `serde`. The binding enables both. No rasterizer, OCR feature or rten dependency enters until an adapter that needs it (ADR 0002).

2. **API.**
   - `Converter::new(Config)` validates `Config` and `Limits`, failing `ConfigError`.
   - `inspect`, `parse` and `convert` take `Input::new(bytes, format)`, optionally `with_cancel(CancelToken)` and, for PDF, `with_ocr_pages(PageSelection)`; `Format::sniff` is separate.
   - `parse` returns `Parsed { document, diagnostics }`, keeping its `Config`; `Parsed::render()` ends `convert` with a `Conversion`, whose `units()` yields each unit's Markdown byte span.
   - Public types are `#[non_exhaustive]`; `Converter` is `Send + Sync`.

3. **Threads.** Each call that walks input runs inside `install` on a per-call pool of `config.threads` (default 1), its stack the measured per-level cost times decision 12's ceiling, so lopdf's `par_iter` (https://docs.rs/crate/lopdf/0.45.0/source/src/reader.rs#1055-1064) never reaches the global pool.

4. **Inspect** takes a PDF and reports container facts, diagnostics and 1-based page units, each `Text`, `Blank` (`no_text`) or `NeedsOcr { reasons }`; any other format is `Unsupported`.

5. **Model.**
   - The model gains a superscript and subscript position and unit milestones; each type lands with its first producer, never earlier.
   - `Block::Gap` awaits OCR under an opaque `GapId` of unit plus ordinal; `Parsed::gaps()` yields each gap's id, `OcrOrigin` and reason.
   - The caller returns outcomes through ADR 0002's channel under the kept `Limits`: `fill_gap` takes a gap and its `GapFill`; `fail_gap` records `ocr_failed`.
   - `GapError` is `UnknownGap`, `Settled` or `Convert(ConvertError)`; a refused call changes nothing, fuel included.

6. **PDF.**
   - PDF through the model waits for a pdf-inspector release whose per-page call takes `MarkdownOptions`, strips headers across pages, escapes source text, reports `no_text` and reads no environment.
   - Upstream pull requests come first; the fallback, a fork on an upstream tag carrying only them, lives in its own repository, which publishes `almiladoc-pdf-inspector` (first version by a scoped token, later by its own trusted publisher) before any `almiladoc` candidate depends on it. Either ships Adobe's BSD-3-Clause CMaps.
   - pulldown-cmark parses each escaped page into the model, `<sup>`, `<sub>` and `<s>` into `Style`; other raw HTML stays escaped text.

7. **Adapters.** Crate-private frontends take one `ParseCtx`.
   - ZIP facts come from the central directory alone; duplicate names, in OPC after ASCII case folding, are `Malformed`.
   - XML: declared (DTD) entities never expand; an internal subset is `Malformed` in OOXML and ODF and ignored in EPUB (https://www.w3.org/TR/epub-33/#sec-xml-constraints), whose XHTML resolves HTML5 named references, others `Lost`.
   - Sniffing reads bounded container metadata, OPC relationships included. A body contradicting its label fails `Unsupported { kind: FormatMismatch { detected } }`.

8. **Serializer.**
   - `<sup>` and `<sub>` wrap escaped content; raw HTML is only those, `<a id>` anchors, `<br>` in cells and `needs-ocr` and `ocr-failed` page comments.
   - Images render by policy as `![alt](asset:N)` placeholders, alt text or nothing; an unfilled image gap adds nothing.
   - One destination gate serves links and external images: after WHATWG preprocessing, `http`, `https`, `mailto`, anchors and relative paths pass, with `\` and `&` escaped; any other keeps only its label or alt and counts `unsafe_link` (`Omitted`).

9. **Errors.** thiserror `ConvertError`: `Unsupported`, `Malformed`, `Encrypted`, `MissingPart`, `ResourceLimit { limit, max }`, `NeedsOcr { pages, page_count }`, `Incomplete { diagnostics }`, `InvalidConfig(ConfigError)`, `Cancelled`, `Internal`; recovery never swallows the last two or `ResourceLimit`. A caller-configuration error found during a conversion, such as an `ocr_pages` page past `page_count`, is `InvalidConfig`, reusing `Converter::new`'s `ConfigError`. Codes are snake_case (`needs_ocr`, `resource_limit`, `invalid_config`) and never renamed after the first release. Input-derived fields are capped and escaped. A refused `try_reserve` is `ResourceLimit`; boundary `catch_unwind` maps panics to `Internal`.

10. **Diagnostics** are free-text-free `{ code, class, region, unit, count }`, aggregated, bounded, never dropped, classed `Omitted` (policy, and each placed PDF figure as `image_omitted`), `Deferred` (placeholder, gap, `ocr_failed`), `Lost` (unread) or `Info`. Default `on_loss = Fail` raises `NeedsOcr` before `Incomplete`; `Report` returns the `Conversion` with `is_complete()` false; `log` goes.

11. **Content policy.** Side content is fixed: headers, footers and comments are omitted, revisions accepted, and hidden text omitted as `hidden_omitted`. `images: ImagePolicy` (default `Placeholder`) and `ImageOcr` are explicit, fingerprinted policies.

12. **Limits.** Today's constants become `Limits` defaults; new ones (`max_input_bytes`, `max_compression_ratio`, `max_pages`, `max_output_bytes`, `max_work`), beside ADR 0002 decision 11's OCR limits, take fixed defaults, each documented on `Limits` with its reason, that admit ordinary large documents; no limit ships before the site that charges it, and one abuse fixture per limit trips it. `max_nesting_depth` bounds every recursion over input nesting, the renderer's included, below a compile-time ceiling. One saturating ledger charges deterministic `max_work` fuel and checks cancellation.

13. **PDF engine.** Limits and cancellation stop at pdf-inspector and lopdf, whose stream decoding is unbounded (https://docs.rs/crate/lopdf/0.45.0/source/src/object.rs#874-887), as pdf-inspector's ToUnicode reader calls it (https://docs.rs/crate/pdf-inspector/1.25.2/source/src/tounicode.rs#3329); the consumer isolates PDF work in a memory-capped, recyclable worker.

14. **No global state.** `clippy.toml` disallows environment, filesystem, network, process, thread, pool and once-cell items, PyO3's included; only decision 3's pool, the binding's file and exception modules and fixture tests use them, each site under an item-level `#[expect]` with a reason, never an inner one. No panic hook; callers recycle the worker after `Internal`.

15. **Identity.**
    - Under the committed `Cargo.lock`, equal version, configuration, input and gap fills give byte-identical output across threads, calls and, except PDF, architectures; non-PDF code calls no platform floating-point function (https://doc.rust-lang.org/1.99.0/std/primitive.f64.html#method.powi).
    - Identity does not hold while pdf-inspector's `PI_NO_MERGE` or `PDF_INSPECTOR_BCMAPS_DIR` is set, until the pinned release reads neither.
    - `EngineIdentity`, with a `config_fingerprint` excluding limits, threads and cancellation, is a contract. Patch releases never change Markdown that converted before.

16. **Lints.** `unsafe_code = "forbid"` in every crate, the fuzz crate included; `missing_docs` and `pedantic` warn. Outside tests, a named clippy deny set (panicking, unchecked or lossy operations, hash-order iteration, printing, exit and `allow`) is fixed in place: no inner `#![expect]` or `#![allow]`, only an item-level `#[expect]` with a reason. Release builds keep `overflow-checks = true` and unwinding.

17. **Python.**
    - Names mirror Rust's: frozen, Rust-defaulted `Limits` and `Config`, an optional `CancelToken` per call, and `Parsed` with `render()`, versioned `to_json()`, `assets()` as `(id, media_type, bytes)`, `fill_gap` and `fail_gap`; the model reaches Python only as JSON, with no model classes.
    - Input meets `max_input_bytes` before any copy, unlike `PyBackedBytes`'s of a `bytearray` (https://docs.rs/crate/pyo3/0.29.3/source/src/pybacked.rs#273-288); `bytes` is borrowed and the GIL released.
    - Each error code has an `AlmiladocError` subclass.

18. **Gates.**
    - Architecture test: `model` imports only `limits`; adapters import no renderer and reach the context only through `ParseCtx`; no interior-mutable `static` lies outside decision 14's modules.
    - Abuse fixtures cover every limit, the deepest converting on decision 3's pool.
    - Proptests cover the serializer, CSV and the XML parser.
    - Non-PDF fixtures are byte-identical across tested targets; PDF snapshots across runs and threads on one target.
    - Fuzz targets cover every format, and the OOXML and ODF part parsers through a fuzzed part injected into a built package; ADR 0003 decision 4's timed campaigns run each daily.

19. **Order.** 1. Trim and rename, as ADR 0004's first candidate. 2. Dependency upgrades. 3. Configuration, ledger, diagnostics, errors and bounds. 4. Lints.
    - Steps 1 to 4 land in order; after them each feature lands with its first producer, and the binding follows step 4.
    - A `0.1.0` release candidate publishes, the primary consumer's integration tests pass on it without shims, and the first final 0.x release follows. PDF through the model and gap fills ship as 0.x minors, each passing those tests again.
    - 1.0 needs the deny set clean, the fuzz campaigns run and the consumer's tests green on the 1.0 candidate.

## Consequences

- Under `on_loss = Fail`, documents upstream converted while skipping OLE objects, VML pictures, charts or `mc:Fallback` fail `Incomplete`, per a site table owed before step 3.
- RTF labelled DOC and cross-labelled ODF, which upstream converted, fail `FormatMismatch`; callers retry with `detected`.
- Owed: measured stack costs before the first release; the pdf-inspector pull requests before PDF goes through the model; hayro's `LAB.icc` licence (ADR 0002) before a rasterizer ships.

## Alternatives considered

- `Format::Image`: a consumer OCRs a standalone image itself.
- A public `markdown::render` for caller-built models: unrequired, and on the caller's stack.
- An ambient-read inventory: machinery for three known reads.
- Every step in strict order before a release: holds the consumer's non-PDF wheel behind the pdf-inspector release.
- A git fork: `cargo publish` refuses it (https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#specifying-dependencies-from-git-repositories). Vendoring: 137k lines of lint debt.
