//! The document model as versioned JSON: the envelope
//! [`Parsed::to_json`](crate::Parsed::to_json) writes, through a writer that
//! counts every byte against the output and work limits.

use std::io;

use serde::Serialize;

use crate::diagnostics::Diagnostics;
use crate::error::ConvertError;
use crate::identity::EngineIdentity;
use crate::model::Document;
use crate::render::markdown::Meter;

/// The version of the JSON [`Parsed::to_json`](crate::Parsed::to_json)
/// writes, raised only when a field or `kind` is removed, renamed or changes
/// meaning: a field or `kind` added keeps it, so a reader ignores those it
/// does not know.
pub const JSON_SCHEMA_VERSION: u32 = 4;

#[derive(Serialize)]
struct Envelope<'a> {
    schema_version: u32,
    identity: &'a EngineIdentity,
    document: &'a Document,
    diagnostics: &'a Diagnostics,
}

pub(crate) fn write(
    identity: &EngineIdentity,
    document: &Document,
    diagnostics: &Diagnostics,
    meter: &Meter<'_>,
) -> Result<String, ConvertError> {
    let envelope =
        Envelope { schema_version: JSON_SCHEMA_VERSION, identity, document, diagnostics };
    let mut out = Metered { meter, json: Vec::new(), refused: None };
    let written = serde_json::to_writer(&mut out, &envelope);
    match (written, out.refused) {
        (_, Some(refused)) => Err(refused),
        (Ok(()), None) => String::from_utf8(out.json).map_err(|_| ConvertError::Internal),
        (Err(_), None) => Err(ConvertError::Internal),
    }
}

/// The JSON written so far, and the error the meter refused a write with,
/// which `serde_json` would otherwise hide inside its own.
struct Metered<'m> {
    meter: &'m Meter<'m>,
    json: Vec<u8>,
    refused: Option<ConvertError>,
}

impl io::Write for Metered<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Err(refused) = self.meter.write(buf.len()) {
            self.refused = Some(refused);
            return Err(io::Error::other("the meter refused the write"));
        }
        self.json.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::diagnostics::{DiagnosticCode, LossClass, Region};
    use crate::ledger::Ledger;
    use crate::limits::Limit;
    use crate::model::{
        Asset, AssetId, Block, Cell, CellSlot, ImageOrigin, ImageSource, Inline, LinkTarget, List,
        ListItem, MarkerKind, Note, NoteKind, Style, Table, Unit, UnitKind, VerticalPosition,
    };
    use std::num::NonZeroU32;

    fn json(document: &Document, diagnostics: &Diagnostics) -> Result<String, ConvertError> {
        let ledger = Ledger::new(u64::MAX, None);
        let identity = EngineIdentity::of(&Config::default());
        write(&identity, document, diagnostics, &Meter::new(&ledger, u64::MAX))
    }

    fn lines(json: &str) -> String {
        json.replace(r#"{"kind":"#, "\n{\"kind\":").replace(r#"{"code":"#, "\n{\"code\":")
    }

    fn depth(json: &str) -> usize {
        let (mut depth, mut deepest) = (0usize, 0);
        let (mut quoted, mut escaped) = (false, false);
        for c in json.chars() {
            match c {
                _ if escaped => escaped = false,
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                '{' | '[' if !quoted => {
                    depth += 1;
                    deepest = deepest.max(depth);
                }
                '}' | ']' if !quoted => depth -= 1,
                _ => {}
            }
        }
        deepest
    }

    fn text(text: &str) -> Inline {
        Inline::plain(text)
    }

    fn paragraph(content: &str) -> Block {
        Block::Paragraph(vec![text(content)])
    }

    fn body_image(alt: &str, source: ImageSource) -> Inline {
        Inline::Image { origin: ImageOrigin::new(Region::Body, None), alt: alt.to_string(), source }
    }

    fn model_inlines() -> Vec<Inline> {
        let styled = Style {
            bold: true,
            italic: true,
            strike: true,
            code: true,
            position: VerticalPosition::Superscript,
        };
        let lowered = Style { position: VerticalPosition::Subscript, ..Style::PLAIN };
        vec![
            Inline::Text { text: "a \"quoted\"\nline".to_string(), style: styled },
            Inline::Text { text: "2".to_string(), style: lowered },
            Inline::Link {
                content: vec![text("site")],
                target: LinkTarget::External("https://example.org".to_string()),
            },
            Inline::Link {
                content: vec![text("page")],
                target: LinkTarget::Relative("page.html".to_string()),
            },
            Inline::Link { content: vec![], target: LinkTarget::anchor("top".to_string()) },
            Inline::Link {
                content: vec![],
                target: LinkTarget::Anchor {
                    id: "chapter#top".into(),
                    unit: Some(Unit::new(UnitKind::SpineItem, NonZeroU32::MIN)),
                },
            },
            Inline::Link {
                content: vec![],
                target: LinkTarget::Unit(Unit::new(UnitKind::Slide, NonZeroU32::MIN)),
            },
            body_image("remote", ImageSource::External("https://example.org/a.png".to_string())),
            body_image("chart", ImageSource::Asset(AssetId(0))),
            body_image("", ImageSource::Unavailable),
            Inline::Image {
                origin: ImageOrigin::new(
                    Region::Note,
                    Some(Unit::new(UnitKind::Slide, NonZeroU32::MIN)),
                ),
                alt: "unread".into(),
                source: ImageSource::Unread,
            },
            Inline::Anchor("here".to_string()),
            Inline::NoteRef("n1".to_string()),
            Inline::LineBreak,
            Inline::Math("x^2".to_string()),
            Inline::Checkbox(true),
            Inline::Checkbox(false),
        ]
    }

    /// A document holding every variant of every model type, each optional
    /// field both set and unset.
    fn every_variant() -> Document {
        let list = List {
            marker: MarkerKind::LowerRoman,
            start: 3,
            items: vec![
                ListItem {
                    blocks: vec![paragraph("item")],
                    marker_label: Some("1-a)".to_string()),
                },
                ListItem { blocks: vec![], marker_label: None },
            ],
        };
        let table = Table {
            grid: vec![
                vec![
                    CellSlot::Origin(Cell::spanning(vec![paragraph("wide")], 2, 1)),
                    CellSlot::Covered { origin_row: 0, origin_col: 0 },
                ],
                vec![CellSlot::Origin(Cell::new(vec![]))],
            ],
            header_rows: 1,
        };
        Document {
            blocks: vec![
                Block::Heading {
                    level: 1,
                    anchor: Some("top".to_string()),
                    content: vec![text("Title")],
                },
                Block::heading(7, vec![]),
                Block::Paragraph(model_inlines()),
                Block::List(list),
                Block::Table(table),
                Block::BlockQuote(vec![paragraph("quoted")]),
                Block::CodeBlock {
                    lang: Some("rust".to_string()),
                    text: "fn f() {}\n".to_string(),
                },
                Block::CodeBlock { lang: None, text: String::new() },
                Block::Rule,
                Block::UnitStart(Unit::new(UnitKind::Slide, NonZeroU32::MIN)),
                Block::Math("E = mc^2".to_string()),
            ],
            notes: vec![
                Note {
                    id: "n1".to_string(),
                    kind: NoteKind::Footnote,
                    blocks: vec![paragraph("foot")],
                },
                Note { id: "n2".to_string(), kind: NoteKind::Endnote, blocks: vec![] },
            ],
            assets: vec![Asset {
                id: AssetId(0),
                media_type: "image/png".to_string(),
                origin_part: "word/media/image1.png".to_string(),
                bytes: vec![0x89, b'P', b'N', b'G'],
            }],
        }
    }

    fn inline_kinds(inlines: &[Inline], kinds: &mut Vec<&'static str>) {
        for inline in inlines {
            kinds.push(match inline {
                Inline::Text { .. } => "text",
                Inline::Link { target: LinkTarget::External(_), .. } => "link external",
                Inline::Link { target: LinkTarget::Relative(_), .. } => "link relative",
                Inline::Link { target: LinkTarget::Anchor { .. }, .. } => "link anchor",
                Inline::Link { target: LinkTarget::Unit(_), .. } => "link unit",
                Inline::Image { source: ImageSource::External(_), .. } => "image external",
                Inline::Image { source: ImageSource::Asset(_), .. } => "image asset",
                Inline::Image { source: ImageSource::Unavailable, .. } => "image unavailable",
                Inline::Image { source: ImageSource::Unread, .. } => "image unread",
                Inline::Anchor(_) => "anchor",
                Inline::NoteRef(_) => "note_ref",
                Inline::LineBreak => "line_break",
                Inline::Math(_) => "inline math",
                Inline::Checkbox(_) => "checkbox",
            });
        }
    }

    fn block_kinds(blocks: &[Block], kinds: &mut Vec<&'static str>) {
        for block in blocks {
            let kind = match block {
                Block::Heading { content, .. } => {
                    inline_kinds(content, kinds);
                    "heading"
                }
                Block::Paragraph(inlines) => {
                    inline_kinds(inlines, kinds);
                    "paragraph"
                }
                Block::List(list) => {
                    for item in &list.items {
                        block_kinds(&item.blocks, kinds);
                    }
                    "list"
                }
                Block::Table(table) => {
                    for slot in table.grid.iter().flatten() {
                        let kind = match slot {
                            CellSlot::Origin(cell) => {
                                block_kinds(&cell.blocks, kinds);
                                "origin"
                            }
                            CellSlot::Covered { .. } => "covered",
                        };
                        kinds.push(kind);
                    }
                    "table"
                }
                Block::BlockQuote(inner) => {
                    block_kinds(inner, kinds);
                    "block_quote"
                }
                Block::CodeBlock { .. } => "code_block",
                Block::Rule => "rule",
                Block::UnitStart(_) => "unit_start",
                Block::Math(_) => "math",
            };
            kinds.push(kind);
        }
    }

    fn every_kind(document: &Document) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        block_kinds(&document.blocks, &mut kinds);
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }

    #[test]
    fn the_sample_holds_every_variant() {
        assert_eq!(
            every_kind(&every_variant()),
            [
                "anchor",
                "block_quote",
                "checkbox",
                "code_block",
                "covered",
                "heading",
                "image asset",
                "image external",
                "image unavailable",
                "image unread",
                "inline math",
                "line_break",
                "link anchor",
                "link external",
                "link relative",
                "link unit",
                "list",
                "math",
                "note_ref",
                "origin",
                "paragraph",
                "rule",
                "table",
                "text",
                "unit_start",
            ]
        );
    }

    fn redacted_wire_form() -> String {
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::UnclosedStructure, Region::Note, None, 2);
        diagnostics.add(DiagnosticCode::UnclosedStructure, Region::Note, NonZeroU32::new(3), 1);
        let json = json(&every_variant(), &diagnostics).unwrap();
        let version = format!(r#""version":"{}""#, env!("CARGO_PKG_VERSION"));
        assert_eq!(json.matches(&version).count(), 1);
        lines(&json.replace(&version, r#""version":"[version]""#))
    }

    #[test]
    fn every_variant_has_its_wire_form() {
        insta::assert_snapshot!(redacted_wire_form(), @r#"
        {"schema_version":4,"identity":{"version":"[version]","config_fingerprint":"64793a7a830129ecbef0c5e222bb22b493b88b9dde158117a8a41b78855b62e5"},"document":{"blocks":[
        {"kind":"heading","value":{"level":1,"anchor":"top","content":[
        {"kind":"text","value":{"text":"Title","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}]}},
        {"kind":"heading","value":{"level":7,"anchor":null,"content":[]}},
        {"kind":"paragraph","value":[
        {"kind":"text","value":{"text":"a \"quoted\"\nline","style":{"bold":true,"italic":true,"strike":true,"code":true,"position":"superscript"}}},
        {"kind":"text","value":{"text":"2","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"subscript"}}},
        {"kind":"link","value":{"content":[
        {"kind":"text","value":{"text":"site","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}],"target":
        {"kind":"external","value":"https://example.org"}}},
        {"kind":"link","value":{"content":[
        {"kind":"text","value":{"text":"page","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}],"target":
        {"kind":"relative","value":"page.html"}}},
        {"kind":"link","value":{"content":[],"target":
        {"kind":"anchor","value":{"id":"top","unit":null}}}},
        {"kind":"link","value":{"content":[],"target":
        {"kind":"anchor","value":{"id":"chapter#top","unit":
        {"kind":"spine_item","ordinal":1}}}}},
        {"kind":"link","value":{"content":[],"target":
        {"kind":"unit","value":
        {"kind":"slide","ordinal":1}}}},
        {"kind":"image","value":{"alt":"remote","source":
        {"kind":"external","value":"https://example.org/a.png"},"origin":{"region":"body","unit":null}}},
        {"kind":"image","value":{"alt":"chart","source":
        {"kind":"asset","value":0},"origin":{"region":"body","unit":null}}},
        {"kind":"image","value":{"alt":"","source":
        {"kind":"unavailable"},"origin":{"region":"body","unit":null}}},
        {"kind":"image","value":{"alt":"unread","source":
        {"kind":"unread"},"origin":{"region":"note","unit":
        {"kind":"slide","ordinal":1}}}},
        {"kind":"anchor","value":"here"},
        {"kind":"note_ref","value":"n1"},
        {"kind":"line_break"},
        {"kind":"math","value":"x^2"},
        {"kind":"checkbox","value":true},
        {"kind":"checkbox","value":false}]},
        {"kind":"list","value":{"marker":"lower_roman","start":3,"items":[{"blocks":[
        {"kind":"paragraph","value":[
        {"kind":"text","value":{"text":"item","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}]}],"marker_label":"1-a)"},{"blocks":[],"marker_label":null}]}},
        {"kind":"table","value":{"grid":[[
        {"kind":"origin","value":{"blocks":[
        {"kind":"paragraph","value":[
        {"kind":"text","value":{"text":"wide","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}]}],"col_span":2,"row_span":1}},
        {"kind":"covered","value":{"origin_row":0,"origin_col":0}}],[
        {"kind":"origin","value":{"blocks":[],"col_span":1,"row_span":1}}]],"header_rows":1}},
        {"kind":"block_quote","value":[
        {"kind":"paragraph","value":[
        {"kind":"text","value":{"text":"quoted","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}]}]},
        {"kind":"code_block","value":{"lang":"rust","text":"fn f() {}\n"}},
        {"kind":"code_block","value":{"lang":null,"text":""}},
        {"kind":"rule"},
        {"kind":"unit_start","value":
        {"kind":"slide","ordinal":1}},
        {"kind":"math","value":"E = mc^2"}],"notes":[{"id":"n1","kind":"footnote","blocks":[
        {"kind":"paragraph","value":[
        {"kind":"text","value":{"text":"foot","style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}]}]},{"id":"n2","kind":"endnote","blocks":[]}],"assets":[{"id":0,"media_type":"image/png","origin_part":"word/media/image1.png","byte_len":4}]},"diagnostics":[
        {"code":"unclosed_structure","class":"lost","region":"note","unit":null,"count":2},
        {"code":"unclosed_structure","class":"lost","region":"note","unit":3,"count":1}]}
        "#);
    }

    #[test]
    fn field_less_enums_are_their_names() {
        let names = |json: Result<String, serde_json::Error>| json.unwrap();
        let markers = [
            MarkerKind::Bullet,
            MarkerKind::Decimal,
            MarkerKind::LowerAlpha,
            MarkerKind::UpperAlpha,
            MarkerKind::LowerRoman,
            MarkerKind::UpperRoman,
        ];
        assert_eq!(
            names(serde_json::to_string(&markers)),
            r#"["bullet","decimal","lower_alpha","upper_alpha","lower_roman","upper_roman"]"#
        );
        let positions = [
            VerticalPosition::Baseline,
            VerticalPosition::Superscript,
            VerticalPosition::Subscript,
        ];
        assert_eq!(
            names(serde_json::to_string(&positions)),
            r#"["baseline","superscript","subscript"]"#
        );
        let notes = [NoteKind::Footnote, NoteKind::Endnote];
        assert_eq!(names(serde_json::to_string(&notes)), r#"["footnote","endnote"]"#);
        for code in DiagnosticCode::ALL {
            assert_eq!(names(serde_json::to_string(code)), format!("\"{code}\""));
        }
        let classes = [LossClass::Omitted, LossClass::Deferred, LossClass::Lost, LossClass::Info];
        assert_eq!(
            names(serde_json::to_string(&classes)),
            r#"["omitted","deferred","lost","info"]"#
        );
        let regions = [
            Region::Body,
            Region::Header,
            Region::Footer,
            Region::Note,
            Region::Comment,
            Region::Metadata,
            Region::Container,
        ];
        assert_eq!(
            names(serde_json::to_string(&regions)),
            r#"["body","header","footer","note","comment","metadata","container"]"#
        );
    }

    type Wrap = fn(Block) -> Block;

    fn nested(levels: usize, wrap: Wrap) -> Document {
        let mut block = paragraph("deep");
        for _ in 0..levels {
            block = wrap(block);
        }
        Document { blocks: vec![block], ..Document::default() }
    }

    fn in_table(block: Block) -> Block {
        Block::Table(Table::from_rows(vec![vec![Cell::new(vec![block])]], 0))
    }

    fn in_list(block: Block) -> Block {
        let item = ListItem { blocks: vec![block], marker_label: None };
        Block::List(List { marker: MarkerKind::Bullet, start: 1, items: vec![item] })
    }

    fn in_quote(block: Block) -> Block {
        Block::BlockQuote(vec![block])
    }

    #[test]
    fn each_nesting_level_nests_the_json_by_its_kinds_cost() {
        let cases: [(Wrap, usize); 3] = [(in_table, 7), (in_list, 5), (in_quote, 2)];
        for (wrap, per) in cases {
            let at = |levels| depth(&json(&nested(levels, wrap), &Diagnostics::default()).unwrap());
            assert_eq!(at(11) - at(10), per);
            assert_eq!(at(10) - at(0), 10 * per);
        }
        let link = |levels| {
            let mut inline = text("deep");
            for _ in 0..levels {
                inline = Inline::Link {
                    content: vec![inline],
                    target: LinkTarget::anchor(String::new()),
                };
            }
            let document =
                Document { blocks: vec![Block::Paragraph(vec![inline])], ..Document::default() };
            depth(&json(&document, &Diagnostics::default()).unwrap())
        };
        assert_eq!(link(11) - link(10), 3);
    }

    #[test]
    fn a_refused_write_fails_with_the_meters_error() {
        let document = every_variant();
        let full = json(&document, &Diagnostics::default()).unwrap();
        let len = full.len() as u64;
        let identity = EngineIdentity::of(&Config::default());
        let with = |max_work, max_output| {
            let ledger = Ledger::new(max_work, None);
            write(&identity, &document, &Diagnostics::default(), &Meter::new(&ledger, max_output))
        };
        assert_eq!(with(u64::MAX, len), Ok(full));
        assert_eq!(with(u64::MAX, len - 1), Err(ConvertError::limit(Limit::OutputBytes, len - 1)));
        assert_eq!(
            with(len / 64 - 1, u64::MAX),
            Err(ConvertError::limit(Limit::Work, len / 64 - 1))
        );
        with(len / 64, u64::MAX).unwrap();
    }
}
