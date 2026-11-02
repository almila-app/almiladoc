# Conversion limits

Per-format retained content and diagnostic codes; [README](../README.md#rust) explains loss classes/OnLoss, [Limits](../src/limits.rs) owns defaults, and [ADR 0001](adr/0001-architecture.md) owns rendering constraints. Code sets are checked both ways; format attribution still requires review.

## All formats

- **Headers/footers:** default Omit counts `header_omitted`/`footer_omitted` without reading bodies; Emit writes rich labelled quotes where values change, with losses in that unit's side region. Dynamic printed fields use tokens (`field_token`), fixed/locked fields keep stored content; side↔body links and ordinary side-note references remain `dangling_reference` (ADR 0001 decision 11).
- **Comments:** Omit counts eligible references/declared parts without reading bodies, reply metadata or authors; Emit writes first-reference-numbered `almiladoc-comment-N` footnotes independently of notes, nesting replies as immediate-parent quotes. Selected replies retain whole threads, unrelated definitions stay unread, first duplicate ids win and damaged parent graphs preserve text with diagnostics; anchors stay thread-local, ordinary/nested-comment references import no other story, emitted side stories can share comments and comment assets follow body/notes/headers/footers.
- **Revisions:** default Accept retains insertions, Reject retains deletions (Word/RTF/ODF); excluded paragraph marks join surviving text while preserving supported lists/tables/formatting. Unused change definitions cause no loss; policies select stored text, never reconstruct old formatting.
- **Hidden:** default Omit reports `hidden_omitted`, Emit selects supported hidden content and reports its ordinary losses: slides, sheets/axes/controls, Word/RTF text, ODF sections/text, DrawingML objects and EPUB `display: none`. Empty hidden spreadsheet drawings stay silent; omitted structures cause no image, comment-body or other content loss.
- **Pictures:** default Placeholder writes `![label](asset:N)` (`image_placeholder`, Deferred), unavailable sources `[label]`, and external images gated text links; labels use verified alt or `Image`, AltText writes labels and Omit nothing (`image_omitted`, Omitted). Read failures are `image_unavailable` (Lost) only when bytes are required; unplaced images are Lost except under Omit, hidden/deleted images have only their policy omission, and assets survive every policy except unread PDF pictures.
- **Image OCR:** `ImageOcr::All` identifies PNG/JPEG by bytes and places text gaps after their block, joined like other cell blocks; image rendering still follows its policy. Unfilled/failed image gaps emit `ocr_gap`/`ocr_failed` (Deferred) without text or NeedsOcr; other images emit `image_not_ocred` (Omitted), and repeated note ids add no gap.
- **Tables:** flatten cell lists/nested rows with `<br>`, nested-row cells with ` / `; a single-cell frame containing one table and only invisible paragraphs unwraps through the nesting limit. Merges show once with blank covered cells and missing headers become empty headers; opt-in OCR MarkdownTables preserves spans as safe HTML.
- **Raised/lowered text:** DOC/DOCX/PPT/PPTX/RTF/ODF/EPUB/PDF use `<sup>`/`<sub>`; other formats silently use baseline text.
- **Escaping:** protect source Markdown syntax (`1\. `, `[^1]\(`, line-start `[^1]\:`); emphasis uses `**`, `_`, `~~`, closes at line ends and encodes a neighboring character only when needed for flanking (`a&#98;_c_`); math escapes a bare `$`, backtick and unmatched brace, writes a space after each `<` a non-space follows (after the math when it ends in `<`) and each `]` before `(`, `[` or `:` (`$< b>$`, `$[a] (b)$`) and closes a trailing control space or backslash with `{}`; display math indents each body line four columns, so no line opens a block, and drops blank lines; text sharing one Markdown inline context with math (a paragraph or heading with its link labels and list-marker label, or a whole table, which a reader without tables reads as one paragraph) escapes each `$`, backtick, bracket and `<` a non-space follows.
- **Lists:** ordered decimals must increase by one and fit nine digits, bare or consistently suffixed `)`; other numbering (including EPUB numbers below 1) uses bullets with source labels. Adjacent sibling lists of the same kind alternate `-`/`*` or `.`/`)` to stay separate.
- **Links/notes:** nested links split at nearest targets; unsafe schemes/network paths keep labels with `unsafe_link`, missing anchors/notes with `dangling_reference`. First eligible note definition wins, including empty, and later definitions emit `duplicate_note` without contributing content, anchors or image losses.
- **Packages:** missing/unreadable referenced XML emits `part_unreadable`, or Malformed when required; malformed nesting/attributes/prefixes, undecodable bytes and unknown declared encodings are unreadable. Literal unresolved entities in selected text/retained link targets emit `unresolved_entity`; unused definitions, omitted content and undisplayed metadata emit none.
- **Style parts:** formatting-only unreadability is `style_part_unreadable`, undecodable EPUB CSS `invalid_style_encoding`; hiding/numbering risks upgrade these to Lost as specified below. `conventional_part_used` reports unresolved references recovered through conventional part names.
- **Units:** slide/sheet/chapter/page ordinals are 1-based positions, preserving hidden/unreadable/skipped entries (CSV is one sheet); beyond 64 units per code/region, highest ordinals fold into one unitless count. Omitted hidden sheets/slides and unreadable units lack ranges except surviving PPT slide-list text.
- **Alternate content:** DOCX/PPTX take first supported namespace choice, else fallback; unread chart/diagram/ink emits `graphic_unread`, picture/text `image_unplaced`/`block_unread` unless fallback shows it.
- **Charts:** DOCX/PPTX table cached series using the first series with category labels; differing/overflowing later labels emit `graphic_unread`.

## Word

- DOC/DOCX/DOCM retain body text, headings, numbered lists, tables, notes and hyperlinks; DOCX adds text boxes, TeX equations, charts/SmartArt text and OLE/ActiveX previews, retaining payload assets; DOCX MathType (MTEF v5) and Equation Editor 3.x (MTEF v3) objects show their TeX instead of the preview, and an undecodable one keeps its preview with `embedded_object`.
- DOCX VML `v:fill` frame/tile pictures apply unless their own `on`, else shape `filled`, disables them, ranked after other drawing/object-preview pictures; unused ones emit `image_unplaced`. Object preview selection matches shape id to `o:OLEObject` ShapeID, `w:objectEmbed`/`w:objectLink` shapeId or `w:control` shapeid; otherwise use the first picture outside VML shapes, labelled only by drawing/shape description.
- Symbol/Wingdings/Webdings/Zapf Dingbats text and inserted symbols map to drawn characters, ballot boxes to checkboxes; DOC chooses font by character/script from its font table and reads field instructions undecoded. Instructions cause no loss unless they render text boxes/charts/diagrams/display equations.

| Lost code | Scope |
|---|---|
| `embedded_object` | Both: unconverted OLE content |
| `image_unavailable`, `image_unplaced` | Both: unread/undecodable bytes; pictures without output positions, including floating ones |
| `graphic_unread`, `form_control_unread` | Both: unread shapes/charts and checkbox/drop-down fields; DOCX also movies/ActiveX |
| `unmapped_symbol` | Both: unmapped symbol glyphs; DOC also U+F000–F0FF in text/unresolved fonts; DOCX also MTEF content characters without a Unicode form |
| `block_unread` | DOCX rows/cells inside content controls, custom XML or alternate content; math in declined choices |
| `imported_part_unread`, `ruby_unread` | DOCX altChunk/subDoc; ruby text |
| `field_value_unread`, `link_target_unread` | DOCX unstored page/date values; missing hyperlink relationship unless label is URL |
| `text_box_unread` | DOC body text boxes |
| `truncated_records` | DOC body/note piece-table text within cbMac but beyond stream end |
| `invalid_encoding` | DOC shown text/retained link targets that do not decode |
| `part_unreadable` | DOC note/run/paragraph/list tables, selected note refs inside surrogate pairs; either reader's styles where text may be hidden/numbered, and shown header/footer data (below) |

- **DOCX styles:** unread styles are Lost with a numbering part, inflation failure, or under hidden Omit when malformed XML contains non-disabled `vanish` outside comments/CDATA/PI/DTD; missing/wrong-root styles without numbering remain Info. **DOC styles:** unread styles are Info only without list tables when the record has no parsed base/dependent style and is a type without run formatting (neither paragraph, character nor table); otherwise Lost.
- **Comments:** Emit reads referenced DOCX comments/extended reply relationships and DOC annotation stories/reply metadata, with independent numbering per body/reply; unread selected bodies report in comment region.
- **Omitted:** `header_omitted`/`footer_omitted`, `comment_omitted`, `revision_omitted`, `hidden_omitted` follow their policies.
- **Header/footer Emit:** sections inherit unnamed/empty default, first (`titlePg`/DOC sprmSFTitlePage) and even (`evenAndOddHeaders`/DOC fFacingPages) stories, but missing first/even stories stay blank rather than use a nonblank default; removed section marks join the next section. Unread shown parts/spans/settings, or DOC PlcfFldHdr tables unable to read/nest a field whose token needs its lock, emit `part_unreadable` in side region, retaining partial content and forcing repetition.

| Info code | Scope |
|---|---|
| `numbering_unresolved`, `table_span_repaired` | DOCX missing numbering becomes unnumbered; oversized merges repaired |
| `formatting_unreadable` | Unread run formatting; DOC invalid character-style references/properties and once/font for shown text retained in unresolved fonts |
| `style_part_unreadable`, `invalid_style_encoding` | Unread style tables under conditions above; undecodable style names |
| `style_cycle` | DOC keeps acyclic prefix; DOCX cycles fail Malformed |

## PowerPoint

- PPT/PPS/POT/PPTX/PPTM/PPSX/PPSM retain slide units, level-2 titles, one `---` between nonempty bodies and quoted speaker notes; ownerless PPT notes precede slides outside unit ranges. PPTX resolves master/layout/slide cascades and reads tables/equations/chart/SmartArt text and `p:oleObj/p:pic` previews labelled by frame description, retaining payload assets; MathType and Equation Editor objects show as TeX as in Word.
- **Comments:** Emit reads legacy/modern PPTX threads/replies and binary PPT comments, retaining resolved content. **Omitted:** `comment_omitted` for declared parts/records, `hidden_omitted` for hidden slides and PPTX DrawingML objects.

| Lost code | Scope |
|---|---|
| `unit_unreadable`, `embedded_object`, `form_control_unread` | Both: listed unread slides, OLE content, controls |
| `image_unavailable`, `link_target_unread` | Both: missing/undecodable pictures; targets absent from labels; PPT also unread picture stores |
| `graphic_unread`, `unmapped_symbol` | Both: unread media/graphics; unmapped glyphs; PPTX also declined-choice charts, nested notes-shape text and MTEF content characters without a Unicode form |
| `image_unplaced` | PPTX picture-filled shapes, slide backgrounds, declined-choice pictures; every shown PPT picture |
| `block_unread` | PPTX declined-choice text |
| `recovered_stream_order`, `invalid_encoding` | PPT raw order after unusable persist directory may include superseded edits; undecodable text |

- **Running content:** Omit counts visible master/layout content/backgrounds and header placeholders as `header_omitted`; footer/date/slide-number placeholders as `footer_omitted`. PPT counts notes headers/drawn nonempty header placeholders and each shown footer/date/number once/page, whether from settings or drawn placeholders.
- **Header/footer Emit:** header is layout/master/theme background resolved against the holding part, then visible non-placeholder master/layout shapes (`showMasterSp`, PPT fMasterObjects through title master); PPT master text renders, pictures remain `image_unplaced`. Footer is date/footer/number placeholders (PPT settings else drawn); notes side content opens its quote, numbers/dates become tokens; PPT unread formatting is Info `formatting_unreadable`.

## Excel

- XLSX/XLSM/XLSB/XLS render selected sheets as tables, named if workbook has multiple sheets, with cached formula results and applicable number formats; dates/times silently use ISO text. Cell-anchored checkboxes render; hyperlinks retain labels only.

| Lost code | Scope |
|---|---|
| `unit_unreadable`, `cell_value_unreadable` | Listed unread sheets; undecodable/missing cached/out-of-bounds cells |
| `number_format_unread` | Date/time/currency/literal formats falling back to General, or unread declarations that may hide values (below) |
| `link_target_unread`, `form_control_unread` | Targets absent from labels; controls other than checkboxes |
| `image_unplaced`, `graphic_unread` | Sheet pictures; charts/text shapes |
| `part_unreadable`, `truncated_records` | XLSX/XLSB styles/drawings; early-ending XLS streams |
| `invalid_encoding`, `encoding_assumed` | Undecodable text; XLS undeclared code pages |

- **Unread formats:** XLS malformed FORMAT/short XF, XLSX numFmt without code/id; valid ids are leading-space/optional-plus decimal u32, so abc/negative/overflow ids are unknown. An unknown-id record could declare any otherwise codeless id (XLS: 5–8, 23–26, 41–44, 63–66, 164–392); judge what its code would hide under General, absent codes Lost, while readable codes win regardless of record order.
- **Header/footer Emit:** default/first/even print quotes persist until changed, empty clears; BIFF5/7 XLS has default only in workbook encoding, judging damage after removing print codes. Binary readers ignore closed custom views, XLSB extension scopes too; dynamic fields are tokens without host date/path/layout reads, print pictures `image_unplaced` (Omit: `image_omitted`).
- **Print diagnostics:** Info `field_token`/`formatting_unreadable` for dynamic fields/discarded formatting; Lost `block_unread` for nested/duplicate-conflicting print definitions, unread fields or settings in unclosed/unknown binary scopes, plus `invalid_encoding`/`encoding_assumed` for shown BIFF5/7 text.
- **Comments:** Emit places XLSX legacy/threaded comments, XLSB comments and XLS cell notes; threaded entries replace only matching legacy placeholders, and hidden cells follow HiddenPolicy. **Omitted:** `comment_omitted`, `header_omitted`/`footer_omitted` under Omit/unsupported reader and `hidden_omitted` for hidden sheets/rows/columns/controls/meaningful unsupported drawings.
- **Info:** `number_format_fallback` when General shows the same number, including potential ids of unknown-id XLSX numFmt whose code shows only its number; `table_span_repaired` for unread/out-of-sheet merges.

## OpenDocument

- ODT/ODS/ODP retain text/headings/lists/tables/notes, TeX equations and stored index text; slide titles are level 2, notes quoted. Cells show stored text:p/text:h even empty, otherwise typed values without number styles; resolve style from cell→row→shown-copy columns via `table:default-cell-style-name`/parents, with unresolved number-style names costing nothing.
- Under hidden Omit, spreadsheets omit hidden sheets/rows/columns including hidden groups, keeping visible merge portions unless origin repeats; text/presentation/nested tables retain collapsed rows.

| Lost code | Scope |
|---|---|
| `cell_value_unreadable` | Unresolved entities corrupting selected typed cached values |
| `block_unread`, `text_box_unread` | Numbered paragraphs/non-paragraph title-frame blocks; text boxes outside paragraphs |
| `image_unplaced`, `image_unavailable` | Pictures outside paragraph text; missing bytes |
| `embedded_object` | OLE/floating frames/applets/non-formula/non-chart objects and unplaced frame formulas |
| `graphic_unread` | Charts/3D/media and non-slide drawing text |
| `number_format_unread` | Omitted styles that could change/hide values (below) |
| `form_control_unread` | Controls except cell-anchored, table-declared non-mixed checkboxes |
| `unmapped_symbol`, `part_unreadable` | Glyphs without TeX forms; present unread text/presentation styles.xml or spreadsheet styles with hiding risk |

- **Number styles:** nonstrings lose omitted styles except float number:number-style with one plain number:number, or percentage number:percentage-style with one plain number plus `%`; strings lose any text style beyond number:text-content alone. With unread spreadsheet styles.xml, unresolved cell/row/column/parent/data styles also lose because that part may define them; unchanged plain float/percentage fallback is Info `number_format_fallback`.
- **Text Header/footer Emit:** before shown page-flow paragraphs/headings/tables (top-level/list/section) starting a master, emit its default/first/left variants plus `style:next-style-name` chain, named by page style; first master starts the document, but cells/notes/frames/omitted content start none. Main `style:display=false` is blank, empty clears, missing/disabled first/left inherits default, fixed fields keep text.
- **Other running content:** spreadsheets use each visible sheet's master, left/center/right header regions in order; slides emit shown header/footer/date declarations, running frames, non-placeholder master shapes and page-style bitmap backgrounds; notes frames open quotes. Omit reports these as `header_omitted`/`footer_omitted`, counting presentation master shapes/background once/master where visible and including slide/notes page-number frames.
- **Comments/revisions:** Emit preserves annotation blocks excluding creator/date, with cross-unit loext:parent-name replies and no duplicate annotation-end comments; `comment_omitted`, `revision_omitted`, `hidden_omitted` follow policies, including text/section display, slides and sheet axes. Reject restores deletion payloads at markers and removes insertion intervals across structure, joining first/last restored paragraphs to neighbors but keeping intermediates separate; invalid selected refs fail Malformed, omitted hidden refs do not.
- **Styles:** unread spreadsheet styles.xml is Info `style_part_unreadable` unless inflation fails or, under hidden Omit, malformed XML may hide sheets/text: table-properties `table:display=false` or text-properties `text:display=none` outside comments/CDATA/PI/DTD, including entity-bearing display/unfinished attributes, upgrade to `part_unreadable`. Unstyled values follow the number-style rule above; `table_span_repaired` reports unaccounted covered cells, and style cycles fail Malformed.

## Rich Text Format

- RTF retains text/lists/tables/notes/links/equations and policy-controlled image assets; Symbol/Wingdings/Webdings/Zapf Dingbats map drawn glyphs/checkboxes, U+F020–F0FF in text fonts uses the last symbol font. Instructions/bookmarks/list labels/equations/picture properties remain undecoded, including fallback `\listtext` labels using the code page.

| Lost code | Scope |
|---|---|
| `unclosed_structure`, `encoding_assumed`, `invalid_encoding` | Unclosed groups; missing/unsupported code pages; undecodable text |
| `field_value_unread`, `form_control_unread`, `embedded_object` | Page/date/time words; checkbox/drop-down fields; objects |
| `image_unavailable`, `unmapped_symbol` | Unsupported pictures; unmapped glyphs/private-use codes without earlier symbol font |

- **Comments:** Emit reads isolated annotation stories with unique source ids; author tokens resolve explicit parents only when unambiguous, repeated authors retain every comment and ambiguous parents emit `dangling_reference`. Word's hidden annotation wrapper does not hide bodies, but explicit body hiding follows HiddenPolicy; Omit reports `comment_omitted`.
- **Headers/footers:** Emit selects defined/inherited `\header`/`\headerr`/`\headerl`/`\headerf` and footer groups per `\sect`, honoring `\facingp`/`\titlepg` anywhere in the body; unshown, hidden/revision-omitted or skipped stories stay unread. Omit reports `header_omitted`/`footer_omitted`; revision/hidden policies report `revision_omitted`/`hidden_omitted`.
- **Info:** `unmatched_close` for unmatched closes, `style_cycle` retaining acyclic prefixes, `invalid_style_encoding` for names and `formatting_unreadable` once/font for shown `\fN` text absent from a nonempty font table.

## EPUB

- EPUB follows spine units, retaining headings/paragraphs/lists/tables/images/book links/MathML; metadata keeps OPF title, ordered creators and date strings, flags unresolved metadata entities and reports `unresolved_entity` only if a damaged title renders as a heading.
- CSS applies bold/italic/strike/inline vertical-align/display:none from inline/screen-relevant styles; UTF-16 BOM else UTF-8 decodes linked sheets. Featureless media all/screen/no-type queries apply, print/speech/other or malformed type tails do not, and not inverts well-formed queries; feature queries and supports/layer/container/scope are unsettled and contribute only display that shows, other at-rules nothing.
- Nested rules skip; charset/namespace/import end at semicolons, stray semicolons join and invalidate the next rule, and imports are never followed. **Hidden:** Omit reports `hidden_omitted`; Emit ignores display suppression so CSS damage costs only formatting, while script/style/non-content destinations remain excluded.

| Lost code | Scope |
|---|---|
| `unit_unreadable` | Missing/unread spine chapters |
| `part_unreadable` | Per linking screen-relevant chapter: uninflatable CSS, or under hidden Omit undecodable CSS whose replacement character outside comments is in a declaration name or screen-read display rule/ancestor condition, risking hidden text |
| `image_unavailable`, `graphic_unread` | Missing image parts; audio/video |
| `embedded_object` | object/embed/iframe, retaining fallback text |
| `block_unread`, `unmapped_symbol` | List/table text outside walked items/rows; glyphs without TeX forms |

Info `style_part_unreadable` reports missing linked CSS; `invalid_style_encoding` reports undecodable CSS without text-loss risk.

## CSV

One table; BOM encoding else UTF-8 else windows-1252, detected comma/semicolon/tab/pipe delimiter. Lost `encoding_assumed` reports windows-1252 fallback; `invalid_encoding` reports undecodable UTF-16.

## PDF

- unpdf reads page units with headings/paragraphs/lists/spanned tables/fixed-pitch code blocks and bold/italic/strike/super/subscript; URI links keep their label emphasis and pass the link rules (ftp, tel, javascript and other unsafe schemes keep labels with `unsafe_link`); internal destinations and other actions keep labels only. A page where unpdf could not keep every link target reports one `link_target_unread`, so default conversion fails Incomplete; unread annotations, an unindexable page box or an overflowing link index clear all its targets, while a link over a fixed-pitch code block loses only its own target and a spent target-size or lookup bound loses every target after it. Composite-font codes unmapped by both ToUnicode and font data are reported only where clip/page box shows the run, including missing codes inside otherwise readable runs.
- Running heads/folios are non-headings of ≤80 nonspace characters, recurring digit-insensitively on at least half of text pages, beside other text on two, with stable numbers of any length or bare numbers stepping with page ordinal; a page repeating the text with other numbers does not count as a recurrence. At least one recurrence must have ordinary text, and only leading/trailing occurrences drop. unpdf's own bare page-number removal is reported as `footer_omitted` (below).
- `inspect` returns NeedsOcr for matching reasons below, otherwise Blank without shown text, else Text; running content is judged across every page, including caller-selected gaps.
- A scan carrying one non-recurring mid-page stamp stays Text; select its pages with with_ocr_pages or use `PdfScanPolicy::CoverageFirst`.
- `PdfScanPolicy::CoverageFirst` adds `ImageBacked` for every page whose images cover ≥50% of the visible page (the MediaBox when the CropBox has no area inside it), whatever text it holds: OCR replaces that text, including body text beside a large figure. It never flags fewer pages than the default.

| GapReason | Condition |
|---|---|
| `UnreadableContent` | Undecodable stream or unparseable page |
| `InvisibleText` | Dropped unreadable OCR layer, ≥90% invisible shown characters, or no readable text with clipped text |
| `GarbledText` | No readable text and font-suppressed runs |
| `ImageBacked` | Painted image without readable text; or ≥50% coverage with all text confined to margins or each text recurring as a running head/folio does, with any copies on its page agreeing in their numbers, needing neither other text beside it nor ordinary-text recurrence; or ≥90% coverage with all text recurring digit-insensitively elsewhere; under `CoverageFirst`, also ≥50% coverage with any text |
| `VectorDrawn` | No text/images, but filled paths |
| `UnreadableText` | None above, yet shown non-whitespace text reads nothing and is not all page numbers unpdf removed |
| `CallerRequested` | Selected by with_ocr_pages, after other reasons; discarded text costs no loss |

- **Gap output:** default Fail raises NeedsOcr for every unfilled/failed page before Incomplete; Report writes `<!-- needs-ocr page N -->` or `<!-- ocr-failed page N -->`. Markdown fills retain GFM tables/strike/emphasis/headings/lists/quotes/code, HTML as text, links as labels/images as alt, TeX math (`\(..\)`, `\[..\]`, `$..$`, `$$..$$`, `math` fences) as math (display math is a block, splitting its paragraph, inline in a heading or cell; empty math stays text); Lines fills stay literal; [ADR 0002](adr/0002-ocr-port.md) owns other fill types/bounds.
- **Requests:** default raster paints crop boxes with Rotate as RGB PNG, largest dpi within media-box pixel cap, concurrent pages on the call/batch pool; a page that cannot be painted or an encoded image over `RasterSpec.max_bytes` is an `OcrRefusal` (`Unpaintable`, `OverBytes`) in its list position and `OcrSummary.refused`, and remains a gap, and without `raster` page gaps are never asked. `ocr_gap`/`ocr_failed` are Deferred; painting losses count only after fill and against the latest request, which a refusal resets.
- **Fidelity:** a painted page's request carries per-kind `RasterFidelity` (text runs, images, inline images, shadings, soft masks, content streams, substituted runs). Annotation appearances, Lab/DeviceN colours and missing stand-in glyphs are not observed, so zero counts are not a completeness claim.
- **Lost:** `unit_unreadable` counts declared pages missing from the tree; `raster_incomplete` counts unpainted runs only where clip/page box shows them, images, inline images and streams, excluding shadings/soft masks. `link_target_unread` counts one per page whose links unpdf could not all keep. `garbled_font_encoding` counts suppressed font runs, wholly unmapped composite/simple-binary codes on otherwise readable pages and missing codes within readable runs; deliberately empty mappings (for example emoji extra glyphs) cost none.
- **Omitted:** `image_omitted` counts painted images on readable pages, `header_omitted`/`footer_omitted` dropped leading/trailing running heads/folios, and one `footer_omitted` per page for the bare page numbers unpdf removes from any page margin (top and side included), under Omit and Emit alike. Emit retains running content as painted side stories; an absent value clears, and pages awaiting OCR emit none.
