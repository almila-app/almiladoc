//! What a page shows of its headers, footers and comments, none of which is written:
//! `HeadersFooters` containers, drawn date, slide-number, footer and header placeholders, and
//! the `Comment10` records of a `___PPT10` binary tag.

use super::records as rt;
use super::scan;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::shared::binary::{get_u16, utf16le_units};
use crate::shared::officeart;

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

/// A kind of header or footer content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Date,
    SlideNumber,
    Footer,
    Header,
}

impl Kind {
    /// The kind a `PlaceholderAtom` body names, if it is one of the four.
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

/// Which kinds a page shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Shown {
    date: bool,
    slide_number: bool,
    footer: bool,
    header: bool,
}

impl Shown {
    /// Read a `HeadersFooters` container body, each record charged as one unit of work. The date
    /// shows when `fHasDate` is set and either `fHasTodayDate` is, or `fHasUserDate` is and the
    /// user date has text; a footer or header shows when its flag is set and its string has text.
    pub(super) fn read(parse_ctx: &ParseCtx<'_>, container: &[u8]) -> Result<Self, ConvertError> {
        let records = scan(parse_ctx, container)?;
        let flags = records
            .iter()
            .find(|r| r.rec_type == rt::HEADERS_FOOTERS_ATOM)
            .and_then(|atom| get_u16(atom.body, FLAGS_AT))
            .unwrap_or(0);
        let flag = |bit: u16| flags & bit != 0;
        let text = |instance: u16| {
            records.iter().any(|r| {
                r.rec_type == rt::CSTRING
                    && r.instance() == instance
                    && has_text(utf16le_units(r.body))
            })
        };
        Ok(Self {
            date: flag(HAS_DATE)
                && (flag(HAS_TODAY_DATE) || flag(HAS_USER_DATE) && text(USER_DATE_INSTANCE)),
            slide_number: flag(HAS_SLIDE_NUMBER),
            footer: flag(HAS_FOOTER) && text(FOOTER_INSTANCE),
            header: flag(HAS_HEADER) && text(HEADER_INSTANCE),
        })
    }

    fn mark(&mut self, kind: Kind) {
        match kind {
            Kind::Date => self.date = true,
            Kind::SlideNumber => self.slide_number = true,
            Kind::Footer => self.footer = true,
            Kind::Header => self.header = true,
        }
    }
}

/// The deck's default for each kind of page, which a page's own container replaces.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Defaults {
    pub(super) slides: Shown,
    pub(super) notes: Shown,
}

/// What one page shows that no output carries, collected while it is read and reported when it
/// ends.
#[derive(Debug, Default)]
pub(super) struct PageFacts {
    /// The page's own `HeadersFooters` container, over the deck's default.
    pub(super) own: Option<Shown>,
    drawn: Shown,
    pub(super) comments: bool,
}

impl PageFacts {
    /// Note a placeholder shape of `kind` drawn on the page, when its `text` is more than the
    /// field mark.
    pub(super) fn draw(&mut self, kind: Kind, text: &str) {
        if text.chars().any(|c| c != FIELD_MARK && is_text(c)) {
            self.drawn.mark(kind);
        }
    }

    /// Report each footer kind the page shows once, its header once on a notes page or when one
    /// is drawn, and its comments once. A slide's `fHasHeader` is ignored.
    pub(super) fn report(self, parse_ctx: &ParseCtx<'_>, default: Shown, notes: bool) {
        let shown = self.own.unwrap_or(default);
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

/// Whether a `ProgBinaryTag` container body is a `___PPT10` tag whose blob holds a `Comment10`
/// record, every record read charged as one unit of work. Any other tag is ignored.
pub(super) fn tag_has_comment(parse_ctx: &ParseCtx<'_>, tag: &[u8]) -> Result<bool, ConvertError> {
    let records = scan(parse_ctx, tag)?;
    let named = records
        .iter()
        .find(|r| r.rec_type == rt::CSTRING && r.instance() == TAG_NAME_INSTANCE)
        .is_some_and(|name| utf16le_units(name.body).eq(COMMENT_TAG.encode_utf16()));
    let blob = records.iter().find(|r| r.rec_type == rt::BINARY_TAG_DATA_BLOB);
    let Some(blob) = blob.filter(|_| named) else {
        return Ok(false);
    };
    for record in officeart::records(blob.body) {
        parse_ctx.charge(1)?;
        if record.rec_type == rt::COMMENT10 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_text(c: char) -> bool {
    c != '\0' && !c.is_whitespace()
}

fn has_text(units: impl Iterator<Item = u16>) -> bool {
    char::decode_utf16(units).any(|c| c.is_err() || c.is_ok_and(is_text))
}
