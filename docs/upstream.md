# Upstream items

The verdict on every open issue and pull request of [firecrawl/anydoc](https://github.com/firecrawl/anydoc), judged against upstream `main` at `261fc25` and its open items as of 2026-10-14. ADR numbers refer to [`docs/adr/`](adr/).

A taken item is rewritten test-first, signed off under DCO clause (b), with its author credited by `Co-authored-by` (ADR 0004 decision 11). The commit names the upstream origin and paths, and `REUSE.toml` declares the files it changes `FSL-1.1-MIT AND MIT` with both copyright lines, adding the author's line for code taken from an unmerged pull request.

## To take

- #4 PR, EPUB metadata and figure-caption dedupe: document metadata enters the model (ADR 0001 decision 5) and duplicated figure captions are dropped; default YAML front matter is not taken, as it is not GitHub-Flavored Markdown.
- #10 issue, worksheet identity and source coordinates: worksheets become units with milestones in the model (ADR 0001 decisions 4 and 5); cell coordinates wait for a consumer that needs them.
- #14 issue, nested tables flattened into one cell: a single-cell table that only wraps another table is unwrapped, as #17 proposes; raw HTML tables are not emitted.
- #17 PR, unwrap single-cell tables that wrap a nested table: the fix for #14, rewritten as a model normalisation step without panicking paths.
- #31 issue, slides concatenated without a boundary: slides become units with milestones in the model, so an untitled slide no longer merges into the one before it (ADR 0001 decisions 4 and 5).
- #32 PR, separate slides with a thematic break: its ODP coverage and tests carry over to slide milestones (ADR 0001 decision 5); a thematic break is not used, as it cannot be told apart from a horizontal rule.
- #44 PR, parse each package part once and bound the total: a part referenced repeatedly is charged once against a document-wide work budget (ADR 0001 decision 12).
- #47 PR, single-pass RTF prelude scan and group-depth cap: RTF group nesting is bounded by the nesting limit and the prelude is scanned once (ADR 0001 decision 12).
- #54 PR, invalid EPUB spine references: a spine whose references all dangle no longer converts to an empty success, and each dangling item is reported (ADR 0001 decision 10).
- #62 issue, PDF through `to_document` with per-page extraction: PDF is parsed into the model with page units (ADR 0001 decisions 4 and 6).
- #63 issue, embedded image assets in the Markdown output: images render as `![alt](asset:N)` placeholders by default (ADR 0001 decisions 8 and 11).
- #70 PR, `asset:N` destinations for embedded images: the placeholder form ADR 0001 decision 8 chooses, rewritten on the current serializer, as the branch predates several escaping fixes.
- #88 PR, SUPPORT.md and known conversion limits: a per-format limits page is worth having, rewritten against current behaviour, which has since changed in parts it describes.
- #91 PR, `to_markdown_pages` for per-page extraction: per-page results come from `inspect` and page units in the model (ADR 0001 decisions 4 and 6) rather than a separate PDF-only function.
- #94 issue, slide boundaries for PPTX and PPT: covered by slide units and milestones (ADR 0001 decisions 4 and 5).
- #95 PR, slide boundary anchors: its design, ordinals that survive empty slides and PPT coverage, is taken as typed slide milestones in the model instead of string anchors (ADR 0001 decision 5).
- #104 issue, unbounded CSV and RTF cell and text materialisation: charged against the conversion-wide work budget and failing as `ResourceLimit` (ADR 0001 decision 12), taken through #107.
- #107 PR, bound CSV and RTF materialisation: the approach chosen for #104, rewritten onto the single budget of ADR 0001 decision 12 with fallible charges.
- #127 issue, EPUB title page missing: a chapter that cannot be read is reported instead of silently skipped (ADR 0001 decision 10); #158 removes one cause.
- #144 issue, one image-only page empties the whole PDF: a page that needs OCR becomes a gap in the model, so the readable pages keep their text under `on_loss = Report` or once OCR fills the gap (ADR 0001 decisions 5 and 10).
- #146 issue, built-in local OCR: local CPU OCR is the PP-OCRv6 adapter on rten (ADR 0002 decision 8); standalone image input stays out, as ADR 0001 rejects an image format.
- #148 PR, bound parser paths reachable from a crafted document: real crashes in the PPT, DOC and number-format paths, taken under the nesting and work limits of ADR 0001 decision 12 with its silent caps reported as diagnostics; its RTF part is replaced by #47.
- #153 PR, convert the readable pages when others need OCR: the same outcome through OCR gaps and `on_loss = Report` (ADR 0001 decisions 5 and 10) instead of page-by-page concatenation, which splits paragraphs across pages; its fixture is reused.
- #154 PR, mark `Format` as `#[non_exhaustive]`: every public type is `#[non_exhaustive]` (ADR 0001 decision 2).
- #156 issue, XLSX resource limits hit on workbooks with trailing whitespace: limit defaults must admit ordinary large documents (ADR 0001 decision 12), so empty formatted cells must not exhaust them.
- #157 issue, local OCR provider hook: a caller's own engine plugs in through the `OcrEngine` port (ADR 0002 decision 1), not a shell command.
- #158 PR, keep a part whose text carries a bare ampersand: a bare `&` is recovered losslessly instead of dropping the whole part.
- #162 issue, the OCR gate refuses born-digital PDFs: each page is classified on its own and a flagged page becomes a gap, so the readable pages convert under `on_loss = Report` or once OCR fills the gap, instead of the whole document failing (ADR 0001 decisions 4, 5 and 10).
- #166 PR, keep a text-based document despite one page needing OCR: keeping the readable text is right, but the page left out must be reported, not only logged (ADR 0001 decision 10).
- #167 issue, an allocation failure classified as malformed input: a refused allocation is `ResourceLimit` (ADR 0001 decision 9), fixed through #169.
- #169 PR, classify allocation failures as resource limits: the fix for #167 under ADR 0001 decision 9, with buffers reserved under the limit before they grow.
- #170 issue, pdf-inspector 1.14.2 lacks the right-to-left extraction fix: fixed by the pdf-inspector upgrade in the dependency step (ADR 0001 decision 19, step 2).
- #171 PR, preserve link and image destinations: destinations are corrupted today; entity escaping is taken under ADR 0001 decision 8, and backslashes are escaped rather than percent-encoded.
- #172 issue, ligature characters dropped from PDF text: glyph-name ligature recovery arrives with the pdf-inspector upgrade (ADR 0001 decision 19, step 2).
- #180 PR, slug a heading line break as the space it renders: heading slugs must match the rendered heading text so table-of-contents links resolve.
- #181 PR, update pdf-inspector for the right-to-left fix: the upgrade belongs to the dependency step (ADR 0001 decision 19, step 2), pinned to the current release with regenerated snapshots.
- #182 issue, PPTX images dropped in `mc:AlternateContent`: branch selection and OLE previews are read, and whatever stays unread is reported (ADR 0001 decisions 8 and 10).
- #184 PR, keep bare numbered levels as ordered markers: a bare `%1` level renders as an ordered list marker, chosen by the serializer.
- #187 issue, a bare `<` truncates a DOCX and returns success: truncation becomes a typed error or a reported loss, never a silent success (ADR 0001 decisions 9 and 10).
- #188 PR, reject delimiters that silently truncate text: the detector is taken, and skipped optional parts become diagnostics so its error is not swallowed (ADR 0001 decision 10).
- #189 issue, process abort on a PDF with an inline image: the lopdf upgrade in the dependency step (ADR 0001 decision 19, step 2) returns an error instead of panicking.

## Taken

- #96 issue, heading numbering in DOCM: list counters continue across numbering instances that share an abstract definition, fixed through #129; inferring heading numbers from text is not taken.
- #129 PR, continue counters across numbering instances of one abstract: the fix for #96, rewritten with a typed abstract id; a start override restarts its level once per instance, and a level override without one continues the count.
- #177 PR, preserve DOCX symbol checkbox states: a Wingdings or Wingdings 2 ballot-box `w:sym` is read as the model's checkbox inline rather than as glyph text, with the strict hex validation kept; other symbols wait for a symbol-font map.
- #179 PR, keep non-breaking hyphens: `w:noBreakHyphen` renders as a hyphen in its run's style, as the DOC and RTF readers already do; `w:softHyphen` stays dropped.

## Already fixed

- #16 PR, merged-cell spans past the used range: the sheet extent already widens to cover merges, and an oversized merge fails as `ResourceLimit`.
- #39 PR, skip hidden worksheets: the XLSX, XLSB and XLS readers already skip hidden and very hidden sheets, with tests.
- #46 PR, delimiters pairing across runs: fixed for #45 by a narrower lookahead over trailing delimiters.
- #72 PR, render XLSX number formats: the built-in number-format renderer already handles percent, currency and thousands for XLSX, XLS and XLSB.
- #90 PR, omit hidden rows and columns in XLSX: hidden rows and columns are already read and omitted; the PR adds only a duplicate test.

## Deferred

- #37 issue, Apple textutil `.doc` rejected for its MiniFAT: a real defect, but legacy `.doc` is outside the primary consumer's formats, and the repair belongs in the cfb crate.
- #42 PR, repair Apple-written zeroed MiniFAT: a correct repair, deferred with #37; it would need bounds checks and every OLE open site.
- #52 issue, HTML and MHTML input: not a format the primary consumer accepts; #147 is the base if it becomes one.
- #83 PR, positioned image files from PDF: built on an unmerged pdf-inspector fork and a side API outside the model; revisit once PDF goes through the model.
- #102 issue, passwords for encrypted OOXML: sound, but no caller supplies passwords yet, and encrypted input fails with a typed `Encrypted` error.
- #130 PR, decrypt OOXML with a supplied password: the candidate for #102 when it is needed; it also bundles an unrelated numbering fix.
- #139 issue, XLSX embedded images missing from the assets: a real gap that no ADR decision covers, and spreadsheets are not among the primary consumer's formats; #151 is the candidate.
- #147 PR, standalone HTML: the best HTML candidate, deferred until HTML is an accepted format.
- #149 PR, MHTML: stacked on #147; revisit after HTML.
- #151 PR, retain embedded worksheet images: the candidate for #139 when it is needed; it lacks a cell-anchored placeholder.
- #163 PR, preserve spreadsheet provenance: more complete than #69, but it adds spreadsheet-specific types to the format-neutral model; revisit with a consumer that needs cell provenance.
- #173 issue, multi-column PDF tables collapse: table reconstruction lives in pdf-inspector, and needs evidence from academic documents first.
- #174 PR, omit deleted revision text in DOC: spec-correct, deferred with legacy `.doc`; revisions become a content policy (ADR 0001 decision 11).
- #176 PR, preserve DOC symbol checkbox states: deferred with legacy `.doc`; it emits glyph text rather than the model's checkbox inline.

## Moot

- #11 issue, Docker support: the project ships a library and a Python wheel, with no command-line tool to package in an image.
- #23 issue, compare with Apache Tika: the benchmark harness was removed.
- #24 issue, Rust CLI binary: the command-line tool was removed.
- #29 PR, Rust binary for `cargo install`: the command-line tool was removed.
- #40 PR, multi-stage CLI image: the command-line tool and its image were removed.
- #55 PR, keep CLI output from overwriting its input: the Node command-line tool was removed, and the library writes no files.
- #75 PR, Nix flake: both of its packages were removed command-line tools.
- #77 issue, batch directory mode: a command-line feature, and the command-line tool was removed.
- #87 issue, add Xberg to the benchmark: the benchmark harness was removed.
- #89 PR, `--batch` directory conversion: the Node command-line tool was removed.
- #98 PR, list community Java bindings: third-party bindings wrap the upstream package, not this one.
- #138 issue, benchmark against Apache Tika: the benchmark harness was removed.
- #145 PR, fix the Node OCR README example: the Node package and hosted OCR were removed.
- #150 PR, list a community Swift binding: third-party bindings wrap the upstream package, not this one.
- #152 PR, fix the WebAssembly test link: the WebAssembly binding was removed.
- #155 issue, Node binding memory high-water on large XLSX: the Node binding was removed.
- #168 PR, ship the CLI as a Python console script: the command-line tool was removed.
- #183 PR, send a PDF needing OCR to a hosted service from WebAssembly: the WebAssembly binding and hosted OCR were removed.

## Declined

- #7 PR, resolve embedded asset URLs: the `asset:N` placeholder (ADR 0001 decision 8) makes a URL-rewriting output mode redundant.
- #19 PR, preserve headings across formats as an outline: stale and broad, with heuristic heading detection; an outline is derivable from heading blocks.
- #30 PR, Go bindings: the project ships the Rust crate and the Python binding only (ADR 0001 decision 1).
- #38 issue, reStructuredText input: source markup rather than a document format to convert.
- #41 issue, backslash between number and period in DOCX lists: the source has no list numbering there, so `1\.` is the correct CommonMark escape of literal text.
- #48 PR, `.wps` and `.et` extensions: WPS files are already detected by content, and mapping `.wps` to DOC misroutes Microsoft Works files.
- #53 PR, standalone HTML through the XML reader: parsing HTML as XML regresses EPUB XHTML; superseded by #147.
- #61 PR, local OCR with ocrs: local OCR is PP-OCRv6 on rten (ADR 0002 decision 8).
- #66 PR, reStructuredText reader: declined with #38.
- #69 PR, worksheet name and origin on tables: built on code since removed; superseded by #163.
- #74 issue, JVM bindings: the project ships the Rust crate and the Python binding only (ADR 0001 decision 1).
- #81 issue, duplicate numbering prefix in generated DOCX: Word itself shows both the list label and the typed number, and stripping number-like text would delete source content.
- #82 issue, PHP bindings: the project ships the Rust crate and the Python binding only (ADR 0001 decision 1).
- #86 issue, FreeMind mind maps: not a document format this library converts.
- #103 PR, passwords for encrypted OOXML: superseded by #130.
- #126 PR, per-slide anchors in PPTX: superseded by #95.
- #128 issue, `.eml` and `.msg` email: email messages are outside the document formats this library converts.
- #161 PR, OCR scanned PDFs with a vision model via LiteLLM: whole-document OCR in Python erases page boundaries; a remote engine plugs in page by page through the HTTP adapter (ADR 0002 decision 7).
- #164 PR, RFC 5322 email reader: declined with #128.
- #175 PR, pdf-inspector 1.20.0: superseded by #181, as a later release also fixes Arabic word boundaries.
- #178 issue, MOBI through libmobi: a C library under LGPL-3.0 conflicts with the pure-Rust, unsafe-free core (ADR 0001 decision 16).
- #185 PR, expand typographic ligatures: pdf-inspector already expands presentation-form ligatures, the rest arrives with the upgrade for #172, and the change would rewrite intended ligatures in other formats.
- #186 PR, render and build documents outside the crate: ADR 0001 rejects a public Markdown renderer for caller-built models.
