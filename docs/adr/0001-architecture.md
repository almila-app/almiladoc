# 0001. Architecture

Status: accepted
Date: 2026-10-14, revised 2026-10-26

## Context

- The primary consumer, a Python document-conversion service, converts untrusted documents in a child process and OCRs in its parent.
- PDF bypassed the model, and recovery dropped content yet returned `Ok`.

## Decision

1. **Distribution.**
   - Crate `almiladoc` and the unpublished abi3 cdylib `python/` (PyPI `almiladoc`) share one version, with `fuzz/` outside; both declare `BUSL-1.1 AND MIT` and ship ADR 0003's `LICENCE_SET`.
   - Features: default `pdf` and `raster`; `serde`; `ocr-http`. The binding enables all four. No rten dependency enters (ADR 0002 decision 8).

2. **API.**
   - `Converter::new(Config)` validates `Config` and `Limits`, failing `ConfigError`.
   - `inspect`, `parse`, `convert` and `convert_with_ocr`, which also takes ADR 0002's engine, take `Input::new(bytes, format)` or `Input::auto(bytes, fallback)`, optionally `with_cancel(CancelToken)`, which the `Parsed` keeps, and, for PDF, `with_ocr_pages(PageSelection)`; content detection, label validation and parsing share one budget; `Format::sniff` is separate.
   - `parse` returns `Parsed { document, diagnostics }`, keeping its `Config`; `Parsed::render()` ends `convert` with a `Conversion`, whose `units()` yields each unit's Markdown byte span.
   - Public types are `#[non_exhaustive]`; `Converter` is `Send + Sync`.

3. **Threads.** Each call that walks input runs inside `install` on a per-call pool of `config.threads` (default 1), its stack the measured per-level cost times decision 12's ceiling, so no work reaches the global pool: PDF extraction runs on the call's own pool thread (decision 6).

4. **Inspect** takes a PDF and reports container facts, diagnostics and 1-based page units, each `Text`, `Blank` (`no_text`) or `NeedsOcr { reasons }`; any other format is `Unsupported`.

5. **Model.**
   - The model carries resolved superscript and subscript position and typed unit milestones. Anchor targets have a document-scoped id and optional destination unit; only that unit being unread suppresses a missing-anchor loss. Readable empty units keep their original ordinals and zero-length output spans; hidden sheets and unreadable units have no milestone, bar a PPT slide whose slide-list text outlives its lost container.
   - `Block::Gap` awaits OCR under an opaque `GapId` of an optional unit plus an ordinal within it, or within the document without one; `Parsed::gaps()` walks every container in document order and yields each gap's id, `OcrOrigin` and reason.
   - The caller returns outcomes through ADR 0002's channel under the kept `Limits`: `fill_gap` takes a gap and its `GapFill`; `fail_gap` records `ocr_failed`.
   - `GapError` is `UnknownGap`, `Settled`, `Refused { limit }` or `Convert(ConvertError)`, which carries only `max_work`, `Cancelled` and `Internal`; a refused call changes nothing, fuel included.

6. **PDF.**
   - PDF through the model reads unpdf's `Document` (iyulab/unpdf, MIT, pure Rust) with default features off, page by page through `for_each_page` with a sequential `PageStreamOptions`, which starts no thread. almiladoc maps its blocks into the model, its styled runs' bold, italic, strike, superscript and subscript into `Style`, and rules each page's verdict from unpdf's page facts (text and image operators, image coverage, suppressed runs, unreadable fonts, undecodable streams).
   - It reads unpdf 0.50.0 from crates.io, which carries almiladoc's pull requests iyulab/unpdf #11, #13-#17, #19 and #20 and the maintainer's version of #12: `/Differences` glyph names on a standard-14 font; superscript, subscript and strike runs; bracket mirroring in right-to-left text; a number set against its first word opening a list item; the clef placement and false `binary_density` suppression `text.pdf` shows; page facts for invisible text in render mode 3, clipped text, margin text, whitespace-only text runs and fill operators; the lines beside a column gutter counted by baseline; and a composite font's codes its `ToUnicode` leaves out read from the font's own data, else counted. almiladoc's fork of unpdf, published while they awaited a release, is retired. Until a release carries #21, a run the clip hides entirely still reports its unread codes as `garbled_font_encoding` (roadmap item unpdf-clip).
   - unpdf carries Adobe's CMap cid2code tables, the Adobe Glyph List and PDFium's CMYK table (BSD-3-Clause), the Core14 AFM widths (Adobe's AFM notice) and the Unicode Consortium's Adobe vendor mappings (Unicode's notice).

7. **Adapters.** Crate-private frontends take one `ParseCtx`.
   - ZIP facts come from the central directory alone; duplicate names, in OPC after ASCII case folding, are `Malformed`.
   - XML: declared (DTD) entities never expand; an internal subset is `Malformed` in OOXML and ODF and ignored in EPUB (https://www.w3.org/TR/epub-33/#sec-xml-constraints), whose XHTML resolves HTML5 named references, others `Lost`.
   - A part with a mismatched or unclosed element, an ill-formed or repeated (qualified or expanded) attribute, an unbound prefix, bytes that do not decode, or a declared encoding no decoder names never parses: it is `Malformed` when required and otherwise unreadable by its reader's role. Multiple roots, top-level text and non-`Char` characters still parse.
   - Sniffing reads bounded container metadata, OPC relationships included. A body contradicting its label fails `Unsupported { kind: FormatMismatch { detected } }`.

8. **Serializer.**
   - Slide bodies that render content are separated by one thematic break; empty slides keep their milestones and linked anchors without adding separators. Unit spans exclude their own anchors, separators, boundary whitespace, the final newline and appended note definitions.
   - `<sup>` and `<sub>` wrap escaped content; raw HTML is only those, `<a id>` anchors, `<br>` in cells and `needs-ocr` and `ocr-failed` page comments.
   - Emphasis streams through one mark stack, outermost first: `<sup>` or `<sub>`, `~~`, `**`, `_`. A change of marks keeps the shared prefix open, closes the rest in reverse and opens the new rest; edge whitespace stays outside, every line end and hard break closes all marks, code spans keep theirs outside the fences, and a table cell's heading is bold beneath its own. Where a delimiter run would not flank beside a source character under CommonMark 0.31.2 section 6.2 (`~~` as `*`), that character is written as a decimal numeric reference, `a&#98;_c_`, and nowhere else; a non-ASCII character neither alphanumeric nor whitespace, and a circled or squared Latin letter (category So, alphabetic), must flank as punctuation and as not. A link nested in another splits it: the nearest kept target links its text, pieces of one source link to equal targets join, and a blank piece writes its whitespace; the URL fallback is judged on the whole link.
   - The first eligible source note definition wins, including an empty one. Selection precedes body consumption, numbering, anchors and losses. Omit prunes only image-emptied chosen notes and their dependent references; defined-empty references do not dangle.
   - Images render by policy as `![label](asset:N)` references, alt text or nothing. A label is verified source alt, or `Image`; unavailable sources keep a literal `[label]` marker. External images become text links, never fetchable image destinations. An OLE object shows its own preview picture in its place, labelled only by the source's description, else a description-only marker or nothing; its payload stays an asset no output references.
   - Blank internal link labels retain their source whitespace as plain text, whether resolved or missing. Safe external links with empty labels use the destination as their label; heading slugs follow that emitted label.
   - One destination gate serves links and external image links: after WHATWG preprocessing, `http`, `https`, `mailto`, anchors and relative paths pass, with `\` and `&` escaped; any other keeps only its label and counts `unsafe_link` (`Omitted`). AltText and Omit do not inspect unused image destinations.
   - Asset and source-free placeholders report `image_placeholder` (`Deferred`); intentional image-content omission reports `image_omitted`. Missing image bytes are `image_unavailable` (`Lost`) only when the policy requires them; unplaced pictures are Lost except under Omit. Hidden and deleted pictures report only their content-policy omission.

9. **Errors.** thiserror `ConvertError`: `Unsupported`, `Malformed`, `Encrypted`, `MissingPart`, `ResourceLimit { limit, max }`, `NeedsOcr { pages, page_count }`, `Incomplete { diagnostics }`, `InvalidConfig(ConfigError)`, `Cancelled`, `Internal`; recovery never swallows the last two or `ResourceLimit`. A caller-configuration error found during a conversion, such as an `ocr_pages` page past `page_count`, is `InvalidConfig`, reusing `Converter::new`'s `ConfigError`. Codes are snake_case (`needs_ocr`, `resource_limit`, `invalid_config`) and never renamed after the first release. Input-derived fields are capped and escaped. A refused `try_reserve` is `ResourceLimit`; boundary `catch_unwind` maps panics to `Internal`.

10. **Diagnostics** are free-text-free `{ code, class, region, unit, count }`, aggregated, bounded, never dropped, classed `Omitted` (policy, and each placed PDF figure as `image_omitted`), `Deferred` (placeholder, gap, `ocr_failed`), `Lost` (unread) or `Info`. Default `on_loss = Fail` raises `NeedsOcr` before `Incomplete`; `Report` returns the `Conversion` with `is_complete()` false; `log` goes.

11. **Content policy.** Side content is fixed: headers, footers and comments are omitted, revisions accepted, and hidden text omitted as `hidden_omitted`. `images: ImagePolicy` (default `Placeholder`) and `ImageOcr` are explicit, fingerprinted policies.

12. **Limits.** Today's constants become `Limits` defaults; new ones (`max_input_bytes`, `max_compression_ratio`, `max_pages`, `max_output_bytes`, `max_work`), beside ADR 0002 decision 11's `max_ocr_text_bytes`, take fixed defaults, each documented on `Limits` with its reason, that admit ordinary large documents; no limit ships before the site that charges it, and one abuse fixture per limit trips it. `max_nesting_depth` bounds every recursion over input nesting, the renderer's included, below a compile-time ceiling. One saturating ledger charges deterministic `max_work` fuel and checks cancellation.

13. **PDF engine.** Limits and cancellation reach unpdf only between pages: inside a page its stream decoding keeps its own fixed ratio rule and takes no caller budget. The page count meets `max_pages` before any page is read; the consumer isolates PDF work in a memory-capped, recyclable worker.

14. **No global state.** `clippy.toml` disallows, item by item, the std environment, filesystem, network, process and thread functions and types (`Path::canonicalize` included), rayon's `ThreadPoolBuilder`, global pool and parallel iterators, `thread_local!` and the once-cells, PyO3's `PyOnceLock`, `Interned` and `intern!` included; clippy takes no module path, so the architecture test bans `std::{env,fs,net,process,thread}` in non-test `src`. Only decision 3's pool and ADR 0002 decision 7's request workers, the binding's file read and exception module and test code use them: the pool and binding sites each under an item-level `#[expect]` with a reason, never an inner one; the integration tests under one reasoned crate `#![expect]`, and the fuzz harness through its manifest's allow. No panic hook; callers recycle the worker after `Internal`.

15. **Identity.**
    - Under the committed `Cargo.lock`, equal version, configuration, input and gap fills give byte-identical output across threads, calls and, except PDF, architectures; non-PDF code calls no platform floating-point function (https://doc.rust-lang.org/1.99.0/std/primitive.f64.html#method.powi).
    - `EngineIdentity`, with a `config_fingerprint` excluding limits, threads and cancellation, is a contract. Patch releases never change Markdown that converted before.

16. **Lints.** `unsafe_code = "forbid"` in every crate, the fuzz crate included; `missing_docs`, `redundant_imports`, `rust_2018_idioms`, `unreachable_pub`, `unused_qualifications`, `pedantic` and the clippy lints `assertions_on_result_states`, `clone_on_ref_ptr`, `excessive_nesting`, `let_underscore_untyped`, `needless_raw_strings`, `precedence_bits`, `redundant_clone`, `self_named_module_files` and `use_self` warn, and `-D warnings` gates them. Outside tests, a named clippy deny set (panicking, unchecked or lossy operations, hash-order iteration, printing, exit and `allow`) is fixed in place: no inner `#![expect]` or `#![allow]` in `src` or the binding, only an item-level `#[expect]` with a reason. A cast std has no conversion for (float to integer, `u64` to `f64`) is one named function of `shared::lossy` with its rounding and saturation contract. `clippy.toml` holds a function to 60 lines, a block to 6 levels of nesting and a signature to one `bool` parameter (`too-many-lines-threshold`, `excessive-nesting-threshold`, `max-fn-params-bools`); a longer function splits at a named step, and only a case or dispatch table takes an item-level `#[expect]` naming it. It also disallows the `f32` and `f64` methods whose std docs call their precision non-deterministic, and the `HashMap` and `HashSet` methods that iterate or drain in hash order; a site where order cannot escape takes an `#[expect]` naming why. Clippy cannot hold owned `HashMap::into_iter`, a hash container passed as a generic `IntoIterator`, or a foreign hash type (Fx, ahash, hashbrown), which needs its own entries in the change that introduces it. `arithmetic_side_effects` stays off and `indexing_slicing` is off crate-wide: indexing is guarded at each site, except that the binary record readers (`shared::binary`, `shared::officeart`, the DOC sprms, XLS and XLSB) deny it on their `mod` lines and read through checked views and `shared::binary::ByteReader`, the one home of little-endian decoding; release builds keep `overflow-checks = true` and unwinding.

17. **Python.**
    - Names mirror Rust's: frozen, Rust-defaulted `Limits` and `Config`, an optional `CancelToken` per call, and `Parsed` with `render()`, versioned `to_json()`, `assets()` as `(id, media_type, bytes)`, `fill_gap` and `fail_gap`; the model reaches Python only as JSON, with no model classes.
    - Input meets `max_input_bytes` before any copy, unlike `PyBackedBytes`'s of a `bytearray` (https://docs.rs/crate/pyo3/0.30.1/source/src/pybacked.rs#274-289); `bytes` is borrowed and the GIL released.
    - Each error code has a `ConvertError` subclass.

18. **Gates.**
    - Architecture test: `model` imports only `limits`; adapters import no renderer and reach the context only through `ParseCtx`; no interior-mutable `static` lies outside decision 14's modules.
    - Abuse fixtures cover every limit, the deepest converting on decision 3's pool.
    - Proptests cover the serializer, CSV and the XML parser.
    - Non-PDF fixtures are byte-identical across tested targets; PDF snapshots across runs and threads on one target.
    - Fuzz targets cover every format, and the OOXML and ODF part parsers through a fuzzed part injected into a built package; ADR 0003 decision 4's timed campaigns run each daily.

19. **Order.** 1. Trim, as ADR 0004's first candidate. 2. Dependency upgrades. 3. Configuration, ledger, diagnostics, errors and bounds. 4. Lints.
    - Steps 1 to 4 land in order; after them each feature lands with its first producer, and the binding follows step 4.
    - A `0.1.0` release candidate publishes once ADR 0002's OCR stack lands (gap fills, the engine port, the rasterizer and `ocr-http`), the primary consumer's integration tests pass on it without shims, and the first final 0.x release follows; each later 0.x minor passes those tests again.
    - 1.0 needs the deny set clean, the fuzz campaigns run and the consumer's tests green on the 1.0 candidate.

## Consequences

- Under `on_loss = Fail`, documents whose OLE objects, VML pictures, charts or `mc:Fallback` go unread fail `Incomplete`, per a site table owed before step 3.
- RTF labelled DOC and cross-labelled ODF fail `FormatMismatch`; callers retry with `detected`.
- Owed: measured stack costs before the first release.

## Alternatives considered

- `Format::Image`: a consumer OCRs a standalone image itself.
- A public `markdown::render` for caller-built models: unrequired, and on the caller's stack.
- An ambient-read inventory: machinery for three known reads.
- A first release before the OCR stack lands: the consumer would adopt today's PDF path and its `NeedsOcr` behaviour, then change both again.
- A git dependency on unpdf's unreleased fixes: `cargo publish` refuses it (https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#specifying-dependencies-from-git-repositories). Vendoring: unpdf's 50.6k lines of lint debt.
- Keeping the previous engine as a fork: on a 22-file corpus of papers, reports, slides and scans (535 pages) it refused 12 text pages as needing OCR: 8 as garbled text, 4 as scans ruled by image pixel count rather than page coverage, where a rule over unpdf's facts misjudged none; it read papers worst of the engines measured, and its upstream merged no outside pull request after 2026-08-06, so its 132k lines would be maintained alone.
- One engine's text over another's page verdicts: two parsers in the wheel. pdfboss: its own threads, raw ligatures, no page facts, one author. MuPDF (AGPL-3.0), Poppler (GPL): licence. PDFium: a native library.
