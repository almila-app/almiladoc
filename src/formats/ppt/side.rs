use super::records as rt;
use super::scan;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::model::{Block, HeaderFooterRegion as Side, Inline};
use crate::shared::binary::{get_u16, utf16le_units};
use crate::shared::officeart;
use crate::shared::sides::PageSides;

const COMMENT_TAG: &str = "___PPT10";
const TAG_NAME_INSTANCE: u16 = 0;

const HAS_DATE: u16 = 0x0001;
const HAS_TODAY_DATE: u16 = 0x0002;
const HAS_USER_DATE: u16 = 0x0004;
const HAS_SLIDE_NUMBER: u16 = 0x0008;
const HAS_HEADER: u16 = 0x0010;
const HAS_FOOTER: u16 = 0x0020;
const FLAGS_AT: usize = 2;

const USER_DATE_INSTANCE: u16 = 0;
const HEADER_INSTANCE: u16 = 1;
const FOOTER_INSTANCE: u16 = 2;

const PLACEMENT_DATE: u8 = 7;
const PLACEMENT_SLIDE_NUMBER: u8 = 8;
const PLACEMENT_FOOTER: u8 = 9;
const PLACEMENT_HEADER: u8 = 10;
const PLACEMENT_AT: usize = 4;

const FIELD_MARK: char = '*';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Date,
    SlideNumber,
    Footer,
    Header,
}

impl Kind {
    pub(super) fn side(self) -> Side {
        if self == Self::Header { Side::Header } else { Side::Footer }
    }

    pub(super) fn placed(placeholder: &[u8]) -> Option<Self> {
        match *placeholder.get(PLACEMENT_AT)? {
            PLACEMENT_DATE => Some(Self::Date),
            PLACEMENT_SLIDE_NUMBER => Some(Self::SlideNumber),
            PLACEMENT_FOOTER => Some(Self::Footer),
            PLACEMENT_HEADER => Some(Self::Header),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Shown {
    date: bool,
    slide_number: bool,
    footer: bool,
    header: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Settings {
    shown: Shown,
    today: bool,
    strings: [Option<Vec<u16>>; 3],
}

impl Settings {
    pub(super) fn read(parse_ctx: &ParseCtx<'_>, container: &[u8]) -> Result<Self, ConvertError> {
        let records = scan(parse_ctx, container)?;
        let flags = records
            .iter()
            .find(|r| r.rec_type == rt::HEADERS_FOOTERS_ATOM)
            .and_then(|atom| get_u16(atom.body, FLAGS_AT))
            .unwrap_or(0);
        let flag = |bit: u16| flags & bit != 0;
        let string = |instance: u16| {
            records
                .iter()
                .find(|r| {
                    r.rec_type == rt::CSTRING
                        && r.instance() == instance
                        && has_text(utf16le_units(r.body))
                })
                .map(|r| utf16le_units(r.body).collect::<Vec<u16>>())
        };
        let today = flag(HAS_TODAY_DATE);
        let strings = [USER_DATE_INSTANCE, HEADER_INSTANCE, FOOTER_INSTANCE].map(string);
        let [user_date, header, footer] = strings.each_ref().map(Option::is_some);
        Ok(Self {
            shown: Shown {
                date: flag(HAS_DATE) && (today || flag(HAS_USER_DATE) && user_date),
                slide_number: flag(HAS_SLIDE_NUMBER),
                footer: flag(HAS_FOOTER) && footer,
                header: flag(HAS_HEADER) && header,
            },
            today,
            strings,
        })
    }
}

impl Shown {
    fn mark(&mut self, kind: Kind) {
        match kind {
            Kind::Date => self.date = true,
            Kind::SlideNumber => self.slide_number = true,
            Kind::Footer => self.footer = true,
            Kind::Header => self.header = true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Defaults {
    pub(super) slides: Settings,
    pub(super) notes: Settings,
}

#[derive(Debug, Default)]
pub(super) struct PageFacts {
    pub(super) own: Option<Settings>,
    drawn: Shown,
    drawn_blocks: Vec<(Kind, Vec<Block>, Diagnostics)>,
    pub(super) comments: bool,
}

impl PageFacts {
    pub(super) fn draw(
        &mut self,
        kind: Kind,
        text: &str,
        blocks: Option<(Vec<Block>, Diagnostics)>,
    ) {
        if text.chars().any(|c| c != FIELD_MARK && is_text(c)) {
            self.drawn.mark(kind);
            if let Some((blocks, held)) = blocks
                && !self.drawn_blocks.iter().any(|(drawn, ..)| *drawn == kind)
            {
                self.drawn_blocks.push((kind, blocks, held));
            }
        }
    }

    pub(super) fn sides(
        &mut self,
        parse_ctx: &ParseCtx<'_>,
        default: &Settings,
        notes: bool,
    ) -> Result<PageSides, ConvertError> {
        let settings = self.own.clone().unwrap_or_else(|| default.clone());
        let mut sides = PageSides::default();
        for kind in [Kind::Date, Kind::Footer, Kind::SlideNumber, Kind::Header] {
            let side = kind.side();
            let set = match kind {
                Kind::Date => settings.shown.date,
                Kind::Footer => settings.shown.footer,
                Kind::SlideNumber => settings.shown.slide_number,
                Kind::Header => notes && settings.shown.header,
            };
            let drawn = self.drawn_blocks.iter().position(|(drawn, ..)| *drawn == kind);
            sides.read(parse_ctx, side, || match (set, drawn) {
                (true, _) => {
                    Ok(vec![Block::Paragraph(vec![settings.text(parse_ctx, kind, side)?])])
                }
                (false, Some(at)) => {
                    let (_, blocks, held) = self.drawn_blocks.swap_remove(at);
                    parse_ctx.report_held(&held)?;
                    Ok(blocks)
                }
                (false, None) => Ok(Vec::new()),
            })?;
        }
        Ok(sides)
    }

    pub(super) fn report(self, parse_ctx: &ParseCtx<'_>, default: &Settings, notes: bool) {
        let shown = self.own.map_or(default.shown, |own| own.shown);
        let drawn = self.drawn;
        let footers = [
            shown.date || drawn.date,
            shown.slide_number || drawn.slide_number,
            shown.footer || drawn.footer,
        ];
        let header = notes && shown.header || drawn.header;
        parse_ctx.emit_count(
            DiagnosticCode::FooterOmitted,
            Region::Footer,
            footers.into_iter().map(u64::from).sum(),
        );
        parse_ctx.emit_count(DiagnosticCode::HeaderOmitted, Region::Header, u64::from(header));
        parse_ctx.emit_count(
            DiagnosticCode::CommentOmitted,
            Region::Comment,
            u64::from(self.comments),
        );
    }
}

pub(super) fn tag_has_comment(parse_ctx: &ParseCtx<'_>, tag: &[u8]) -> Result<bool, ConvertError> {
    let Some(blob) = comment_blob(parse_ctx, tag)? else { return Ok(false) };
    for record in officeart::records(blob) {
        parse_ctx.charge(1)?;
        if record.rec_type == rt::COMMENT10 {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn comment_blob<'a>(
    parse_ctx: &ParseCtx<'_>,
    tag: &'a [u8],
) -> Result<Option<&'a [u8]>, ConvertError> {
    let records = scan(parse_ctx, tag)?;
    let named = records
        .iter()
        .find(|r| r.rec_type == rt::CSTRING && r.instance() == TAG_NAME_INSTANCE)
        .is_some_and(|name| utf16le_units(name.body).eq(COMMENT_TAG.encode_utf16()));
    let blob = records.iter().find(|r| r.rec_type == rt::BINARY_TAG_DATA_BLOB);
    let Some(blob) = blob.filter(|_| named) else {
        return Ok(None);
    };
    Ok(Some(blob.body))
}

impl Settings {
    fn text(
        &self,
        parse_ctx: &ParseCtx<'_>,
        kind: Kind,
        side: Side,
    ) -> Result<Inline, ConvertError> {
        let index = match kind {
            Kind::SlideNumber => return Ok(token(parse_ctx, "{page}", side)),
            Kind::Date if self.today => return Ok(token(parse_ctx, "{date}", side)),
            Kind::Date => 0,
            Kind::Header => 1,
            Kind::Footer => 2,
        };
        let units = self.strings.get(index).cloned().flatten().unwrap_or_default();
        parse_ctx.charge(units.len() as u64)?;
        let mut invalid = 0u64;
        let text: String = char::decode_utf16(units)
            .map(|c| {
                c.unwrap_or_else(|_| {
                    invalid = invalid.saturating_add(1);
                    char::REPLACEMENT_CHARACTER
                })
            })
            .filter(|&c| c != '\0')
            .collect();
        if invalid > 0 {
            parse_ctx.emit(DiagnosticCode::InvalidEncoding, side.region());
        }
        Ok(Inline::plain(text.trim()))
    }
}

fn token(parse_ctx: &ParseCtx<'_>, token: &str, side: Side) -> Inline {
    parse_ctx.emit(DiagnosticCode::FieldToken, side.region());
    Inline::plain(token)
}

pub(super) fn field_token(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Date => Some("{date}"),
        Kind::SlideNumber => Some("{page}"),
        Kind::Footer | Kind::Header => None,
    }
}

pub(super) const FIELD: char = FIELD_MARK;

fn is_text(c: char) -> bool {
    c != '\0' && !c.is_whitespace()
}

fn has_text(units: impl Iterator<Item = u16>) -> bool {
    char::decode_utf16(units).any(|c| c.is_err() || c.is_ok_and(is_text))
}
