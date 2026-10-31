use std::ops::Range;

use super::grid::SheetContent;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{
    Block, HeaderFooter, HeaderFooterCertainty as Certainty, HeaderFooterRegion as Side,
    HeaderFooterState, HeaderFooterVariant as Variant, Inline,
};
use crate::package::xml::{Element, Node, ns};
use crate::shared::binary::utf16le_units;
use crate::shared::text::clean_text;

const COLOR_BYTES: usize = 6;
const THEME_ID_BYTES: usize = 2;

type Damage = (Range<usize>, DiagnosticCode);
type Field = (&'static str, Side, Variant, Option<&'static str>);

const FIELDS: [Field; 6] = [
    ("oddHeader", Side::Header, Variant::Default, None),
    ("firstHeader", Side::Header, Variant::First, Some("differentFirst")),
    ("evenHeader", Side::Header, Variant::Even, Some("differentOddEven")),
    ("oddFooter", Side::Footer, Variant::Default, None),
    ("firstFooter", Side::Footer, Variant::First, Some("differentFirst")),
    ("evenFooter", Side::Footer, Variant::Even, Some("differentOddEven")),
];

struct PrintSlot {
    state: HeaderFooterState,
    reports: Vec<DiagnosticCode>,
    selector_uncertain: bool,
    unread_content: bool,
}

pub(super) struct PrintSettings {
    slots: Vec<PrintSlot>,
    conflicts: [u64; 2],
}

#[derive(Clone, Copy)]
pub(super) enum BinaryPrintText<'a> {
    Inherit,
    Unread,
    Utf16(Option<&'a [u8]>),
}

impl PrintSettings {
    pub(super) fn empty(ctx: &ParseCtx<'_>) -> Result<Self, ConvertError> {
        use BinaryPrintText::{Inherit, Utf16};
        Self::from_utf16(ctx, [Utf16(None), Inherit, Inherit, Utf16(None), Inherit, Inherit])
    }

    pub(super) fn unread(ctx: &ParseCtx<'_>) -> Result<Self, ConvertError> {
        use BinaryPrintText::{Inherit, Unread};
        Self::from_utf16(ctx, [Unread, Inherit, Inherit, Unread, Inherit, Inherit])
    }

    pub(super) fn from_utf16(
        ctx: &ParseCtx<'_>,
        fields: [BinaryPrintText<'_>; 6],
    ) -> Result<Self, ConvertError> {
        let mut result = Self { slots: Vec::with_capacity(FIELDS.len()), conflicts: [0; 2] };
        for ((_, side, variant, _), field) in FIELDS.into_iter().zip(fields) {
            ctx.charge(1)?;
            let mut slot = inherit(side, variant);
            match field {
                BinaryPrintText::Inherit => {}
                BinaryPrintText::Unread => {
                    slot.state.value.content = Some(Vec::new());
                    slot.state.certainty = Certainty::Uncertain;
                    slot.unread_content = true;
                    result.conflicts[side_index(side)] = 1;
                }
                BinaryPrintText::Utf16(bytes) => {
                    let (text, damage) = utf16_text(ctx, bytes.unwrap_or_default())?;
                    let sections = sections(ctx, &text, &damage, |code| slot.report(code))?;
                    slot.state.value.content = Some(paragraphs(sections));
                }
            }
            result.slots.push(slot);
        }
        Ok(result)
    }

    pub(super) fn merge(&mut self, ctx: &ParseCtx<'_>, extra: &Self) -> Result<(), ConvertError> {
        let conflicts = conflicting_sides(ctx, self, extra)?;
        for (count, conflict) in self.conflicts.iter_mut().zip(conflicts) {
            *count = count.saturating_add(u64::from(conflict));
        }
        Ok(())
    }
}

impl PrintSlot {
    fn report(&mut self, code: DiagnosticCode) {
        if matches!(code, DiagnosticCode::UnresolvedEntity | DiagnosticCode::InvalidEncoding) {
            self.state.certainty = Certainty::Uncertain;
        }
        self.unread_content |= code.class() == crate::LossClass::Lost;
        self.reports.push(code);
    }
}

fn utf16_text(ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<(String, Vec<Damage>), ConvertError> {
    ctx.charge(bytes.len() as u64)?;
    let mut text = String::new();
    let mut damage = Vec::new();
    for decoded in char::decode_utf16(utf16le_units(bytes)) {
        let invalid = decoded.is_err();
        let ch = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
        ctx.charge(ch.len_utf8() as u64)?;
        let start = text.len();
        text.push(ch);
        if invalid {
            damage.push((start..text.len(), DiagnosticCode::InvalidEncoding));
        }
    }
    Ok((text, damage))
}

pub(super) fn read(
    ctx: &ParseCtx<'_>,
    sheet: &Element,
    out: &mut SheetContent,
) -> Result<(), ConvertError> {
    if !ctx.headers_footers_requested() {
        return super::xlsx::omit_headers(ctx, sheet, out);
    }
    ctx.charge(sheet.children.len() as u64)?;
    let mut containers = sheet.find_all(ns::SML, "headerFooter");
    let kept = read_settings(ctx, containers.next())?;
    for container in containers {
        let extra = read_settings(ctx, Some(container))?;
        let conflicts = conflicting_sides(ctx, &kept, &extra)?;
        for (side, conflict) in [Side::Header, Side::Footer].into_iter().zip(conflicts) {
            if conflict {
                ctx.emit(DiagnosticCode::BlockUnread, diagnostic_region(side));
            }
        }
    }
    publish(ctx, kept, out)
}

fn conflicting_sides(
    ctx: &ParseCtx<'_>,
    kept: &PrintSettings,
    extra: &PrintSettings,
) -> Result<[bool; 2], ConvertError> {
    let mut conflicts = extra.conflicts.map(|count| count != 0);
    for (index, slot) in extra.slots.iter().enumerate() {
        if !same_effective(ctx, kept, extra, index)? {
            conflicts[side_index(slot.state.value.region)] = true;
        }
    }
    Ok(conflicts)
}

fn diagnostic_region(side: Side) -> Region {
    match side {
        Side::Header => Region::Header,
        Side::Footer => Region::Footer,
    }
}

fn side_index(side: Side) -> usize {
    match side {
        Side::Header => 0,
        Side::Footer => 1,
    }
}

fn read_settings(
    ctx: &ParseCtx<'_>,
    settings: Option<&Element>,
) -> Result<PrintSettings, ConvertError> {
    if let Some(settings) = settings {
        ctx.charge((settings.children.len() as u64).saturating_mul(FIELDS.len() as u64))?;
        ctx.charge((settings.attrs.len() as u64).saturating_mul(4))?;
    }
    let mut result = PrintSettings { slots: Vec::with_capacity(FIELDS.len()), conflicts: [0; 2] };
    for field in FIELDS {
        let default =
            if field.3.is_some() { Some(default_slot(ctx, &result, field.1)?) } else { None };
        let (slot, conflicts) = read_slot(ctx, settings, field, default)?;
        let count = &mut result.conflicts[side_index(field.1)];
        *count = count.saturating_add(conflicts);
        result.slots.push(slot);
    }
    Ok(result)
}

fn read_slot(
    ctx: &ParseCtx<'_>,
    settings: Option<&Element>,
    (local, side, variant, flag): Field,
    default: Option<&PrintSlot>,
) -> Result<(PrintSlot, u64), ConvertError> {
    ctx.charge(1)?;
    let selector =
        flag.and_then(|name| settings?.attrs.iter().find(|a| a.ns.is_none() && a.local == name));
    let active = flag.is_none() || selector.is_some_and(|a| super::xlsx::bool_attr(&a.value));
    let uncertain = selector.is_some_and(|a| a.unresolved);
    if !active && !uncertain {
        return Ok((inherit(side, variant), 0));
    }
    let mut items = settings.into_iter().flat_map(|s| s.find_all(ns::SML, local));
    let mut slot = read_item(ctx, items.next(), side, variant)?;
    let conflicts = duplicate_items(ctx, items, &slot)?;
    if uncertain {
        let default = default.ok_or(ConvertError::Internal)?;
        slot.selector_uncertain = slot.unread_content
            || default.unread_content
            || !slot.state.same_content(&default.state, |work| ctx.charge(work))?;
        slot.state.value.content = None;
        slot.state.certainty = Certainty::Known;
        slot.reports.clear();
        slot.unread_content = false;
    }
    Ok((slot, conflicts))
}

fn inherit(side: Side, variant: Variant) -> PrintSlot {
    PrintSlot {
        state: HeaderFooterState {
            value: HeaderFooter { region: side, variant, content: None },
            certainty: Certainty::Known,
        },
        reports: Vec::new(),
        selector_uncertain: false,
        unread_content: false,
    }
}

fn read_item(
    ctx: &ParseCtx<'_>,
    item: Option<&Element>,
    side: Side,
    variant: Variant,
) -> Result<PrintSlot, ConvertError> {
    let mut slot = inherit(side, variant);
    let sections = if let Some(item) = item {
        read_text_with(ctx, item, |code| slot.report(code))?
    } else {
        std::array::from_fn(|_| String::new())
    };
    slot.state.value.content = Some(paragraphs(sections));
    Ok(slot)
}

fn duplicate_items<'a>(
    ctx: &ParseCtx<'_>,
    items: impl Iterator<Item = &'a Element>,
    kept: &PrintSlot,
) -> Result<u64, ConvertError> {
    let mut conflicts = 0_u64;
    for item in items {
        let extra = read_item(ctx, Some(item), kept.state.value.region, kept.state.value.variant)?;
        let same = extra.state.same_content(&kept.state, |work| ctx.charge(work))?;
        if !same || extra.unread_content || kept.unread_content {
            conflicts = conflicts.saturating_add(1);
        }
    }
    Ok(conflicts)
}

fn default_slot<'a>(
    ctx: &ParseCtx<'_>,
    settings: &'a PrintSettings,
    side: Side,
) -> Result<&'a PrintSlot, ConvertError> {
    ctx.charge(settings.slots.len() as u64)?;
    settings
        .slots
        .iter()
        .find(|slot| {
            slot.state.value.region == side && slot.state.value.variant == Variant::Default
        })
        .ok_or(ConvertError::Internal)
}

fn effective<'a>(
    ctx: &ParseCtx<'_>,
    settings: &'a PrintSettings,
    index: usize,
) -> Result<(&'a HeaderFooterState, bool), ConvertError> {
    let slot = settings.slots.get(index).ok_or(ConvertError::Internal)?;
    let value = if slot.state.value.content.is_some() {
        slot
    } else {
        default_slot(ctx, settings, slot.state.value.region)?
    };
    Ok((&value.state, slot.selector_uncertain || value.unread_content))
}

fn same_effective(
    ctx: &ParseCtx<'_>,
    kept: &PrintSettings,
    extra: &PrintSettings,
    index: usize,
) -> Result<bool, ConvertError> {
    let (kept, kept_unread) = effective(ctx, kept, index)?;
    let (extra, extra_unread) = effective(ctx, extra, index)?;
    let same = extra.same_content(kept, |work| ctx.charge(work))?;
    Ok(same && !kept_unread && !extra_unread)
}

pub(super) fn publish(
    ctx: &ParseCtx<'_>,
    settings: PrintSettings,
    out: &mut SheetContent,
) -> Result<(), ConvertError> {
    for (side, count) in [Side::Header, Side::Footer].into_iter().zip(settings.conflicts) {
        ctx.emit_count(DiagnosticCode::BlockUnread, diagnostic_region(side), count);
    }
    for slot in settings.slots {
        let region = diagnostic_region(slot.state.value.region);
        if slot.selector_uncertain {
            ctx.emit(DiagnosticCode::UnresolvedEntity, region);
        }
        ctx.charge(slot.reports.len() as u64)?;
        let mut reports = Vec::new();
        for code in slot.reports {
            if matches!(
                code,
                DiagnosticCode::UnresolvedEntity
                    | DiagnosticCode::InvalidEncoding
                    | DiagnosticCode::FieldToken
                    | DiagnosticCode::FormattingUnreadable
            ) {
                reports.push(code);
            } else {
                ctx.emit(code, region);
            }
        }
        out.headers_footers.push((slot.state, reports));
    }
    Ok(())
}

fn paragraphs(sections: [String; 3]) -> Vec<Block> {
    sections
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(|s| Block::Paragraph(vec![Inline::plain(s)]))
        .collect()
}

fn read_text_with(
    ctx: &ParseCtx<'_>,
    item: &Element,
    mut report: impl FnMut(DiagnosticCode),
) -> Result<[String; 3], ConvertError> {
    let (text, damage) = item.direct_text(ctx)?;
    for node in &item.children {
        ctx.charge(1)?;
        if matches!(node, Node::Elem(_)) {
            report(DiagnosticCode::BlockUnread);
        }
    }
    let (text, damage) = xstring(ctx, &text, &damage)?;
    sections(ctx, &text, &damage, report)
}

fn sections(
    ctx: &ParseCtx<'_>,
    text: &str,
    damage: &[Damage],
    mut report: impl FnMut(DiagnosticCode),
) -> Result<[String; 3], ConvertError> {
    ctx.charge(text.len() as u64)?;
    let mut sections: [String; 3] = std::array::from_fn(|_| String::new());
    let mut section = 1;
    let mut at = 0;
    let mut faults = damage;
    while at < text.len() {
        while faults.first().is_some_and(|(r, _)| r.end <= at) {
            faults = &faults[1..];
        }
        if let Some((fault, code)) = faults.first().filter(|(r, _)| r.start == at) {
            sections[section].push_str(text.get(fault.clone()).ok_or(ConvertError::Internal)?);
            report(*code);
            at = fault.end;
            continue;
        }
        let rest = text.get(at..).ok_or(ConvertError::Internal)?;
        let Some(c) = rest.chars().next() else { break };
        at += c.len_utf8();
        if c != '&' {
            sections[section].push(c);
            continue;
        }
        if faults.first().is_some_and(|(r, _)| r.start == at) {
            continue;
        }
        let rest = text.get(at..).ok_or(ConvertError::Internal)?;
        let Some(code) = rest.chars().next() else { break };
        at += code.len_utf8();
        match code {
            'L' => section = 0,
            'C' => section = 1,
            'R' => section = 2,
            '&' => sections[section].push('&'),
            'G' => report(ctx.unplaced_image_code()),
            _ => {
                if let Some(token) = field_token(code) {
                    ctx.charge(token.len() as u64)?;
                    sections[section].push_str(token);
                    report(DiagnosticCode::FieldToken);
                } else {
                    let rest = text.get(at..).ok_or(ConvertError::Internal)?;
                    at += format_length(code, rest, at, faults);
                    report(DiagnosticCode::FormattingUnreadable);
                }
            }
        }
    }
    normalize_sections(ctx, &mut sections)?;
    Ok(sections)
}

fn field_token(code: char) -> Option<&'static str> {
    Some(match code {
        'P' => "{page}",
        'N' => "{pages}",
        'D' => "{date}",
        'T' => "{time}",
        'F' => "{file}",
        'Z' => "{path}",
        'A' => "{sheet}",
        _ => return None,
    })
}

fn normalize_sections(ctx: &ParseCtx<'_>, sections: &mut [String; 3]) -> Result<(), ConvertError> {
    for text in sections {
        ctx.charge((text.len() as u64).max(1).saturating_mul(2))?;
        let cleaned = clean_text(text);
        cleaned.trim().clone_into(text);
    }
    Ok(())
}

fn format_length(code: char, rest: &str, offset: usize, damage: &[Damage]) -> usize {
    if code == '"' {
        let mut start = 0;
        for (fault, _) in damage {
            let end = fault.start.saturating_sub(offset);
            if let Some(at) = rest.get(start..end).and_then(|s| s.find('"')) {
                return start + at + 1;
            }
            start = fault.end.saturating_sub(offset);
        }
        rest.get(start..).and_then(|s| s.find('"')).map_or(0, |at| start + at + 1)
    } else {
        let rest = damage
            .first()
            .and_then(|(r, _)| rest.get(..r.start.saturating_sub(offset)))
            .unwrap_or(rest);
        match code {
            '0'..='9' => rest.bytes().take(2).take_while(u8::is_ascii_digit).count(),
            'K' => {
                let hex =
                    rest.get(..COLOR_BYTES).filter(|s| s.bytes().all(|b| b.is_ascii_hexdigit()));
                let theme = rest.get(..COLOR_BYTES).filter(|s| {
                    s.get(..THEME_ID_BYTES).is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
                        && matches!(s.as_bytes().get(THEME_ID_BYTES), Some(b'+' | b'-'))
                        && s.get(THEME_ID_BYTES + 1..)
                            .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
                });
                hex.or(theme).map_or(0, str::len)
            }
            _ => 0,
        }
    }
}

fn xstring(
    ctx: &ParseCtx<'_>,
    text: &str,
    damage: &[Range<usize>],
) -> Result<(String, Vec<Damage>), ConvertError> {
    ctx.charge(text.len() as u64)?;
    let mut decoded = String::new();
    let mut projected = Vec::new();
    let mut at = 0;
    let mut faults = damage.iter().peekable();
    while at < text.len() {
        if let Some(fault) = faults.peek().filter(|r| r.start == at) {
            let start = decoded.len();
            decoded.push_str(text.get((*fault).clone()).ok_or(ConvertError::Internal)?);
            projected.push((start..decoded.len(), DiagnosticCode::UnresolvedEntity));
            at = fault.end;
            faults.next();
            continue;
        }
        let end = faults.peek().map_or(text.len(), |r| r.start);
        let rest = text.get(at..end).ok_or(ConvertError::Internal)?;
        if let Some(unit) = escaped_unit(rest) {
            at += 7;
            let paired = (0xD800..=0xDBFF)
                .contains(&unit)
                .then(|| text.get(at..end).and_then(escaped_unit))
                .flatten()
                .filter(|low| (0xDC00..=0xDFFF).contains(low));
            let value = if let Some(low) = paired {
                at += 7;
                0x10000 + ((u32::from(unit) - 0xD800) << 10) + u32::from(low) - 0xDC00
            } else {
                u32::from(unit)
            };
            let start = decoded.len();
            decoded.push(char::from_u32(value).unwrap_or(char::REPLACEMENT_CHARACTER));
            if char::from_u32(value).is_none() {
                projected.push((start..decoded.len(), DiagnosticCode::InvalidEncoding));
            }
        } else {
            let c = rest.chars().next().ok_or(ConvertError::Internal)?;
            decoded.push(c);
            at += c.len_utf8();
        }
    }
    Ok((decoded, projected))
}

fn escaped_unit(text: &str) -> Option<u16> {
    let hex = text.strip_prefix("_x")?.get(..5)?.strip_suffix('_')?;
    hex.bytes().all(|b| b.is_ascii_hexdigit()).then(|| u16::from_str_radix(hex, 16).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeaderFooterPolicy;
    use crate::limits::{Limit, Limits};
    use crate::package::xml::{self, Dialect, WholeTree};

    #[test]
    fn print_text_copies_charge_utf8_bytes_and_propagate_cancellation() {
        let tree =
            xml::parse(&ParseCtx::default(), "<h>é</h>".as_bytes(), Dialect::Xml, &mut WholeTree)
                .unwrap()
                .root;
        let item = tree.child_elems().next().unwrap();
        let ctx = ParseCtx::default();
        let mut reports = Vec::new();
        assert_eq!(
            read_text_with(&ctx, item, |code| reports.push(code)).unwrap(),
            ["", "é", ""].map(str::to_owned)
        );
        assert_eq!(reports, []);
        assert_eq!(ctx.fuel(), 16);
        let limits = Limits { max_work: 15, ..Limits::default() };
        assert_eq!(
            read_text_with(&ParseCtx::new(&limits), item, |_| {}),
            Err(ConvertError::limit(Limit::Work, 15))
        );
        assert_eq!(
            read_item(&ParseCtx::new(&limits), Some(item), Side::Header, Variant::Default)
                .map(|_| ()),
            Err(ConvertError::limit(Limit::Work, 15))
        );
        assert_eq!(
            xstring(&ParseCtx::default(), "_xD800_", &[]).unwrap(),
            ("�".to_owned(), vec![(0..3, DiagnosticCode::InvalidEncoding)])
        );
        let token = crate::CancelToken::default();
        token.cancel();
        assert_eq!(
            read_text_with(&ParseCtx::default().with_cancel(Some(&token)), item, |_| {}),
            Err(ConvertError::Cancelled)
        );
        assert_eq!(
            read_item(
                &ParseCtx::default().with_cancel(Some(&token)),
                Some(item),
                Side::Header,
                Variant::Default,
            )
            .map(|_| ()),
            Err(ConvertError::Cancelled)
        );
    }

    type BudgetRow = (&'static str, &'static str, [Option<&'static str>; 6], [u64; 4], usize, u64);

    const PRINT_BUDGET_ROWS: [BudgetRow; 9] = [
        ("absent", "", [Some(""), None, None, Some(""), None, None], [0; 4], 0, 18),
        (
            "selected",
            "<headerFooter><oddHeader>é&amp;P</oddHeader></headerFooter>",
            [Some("é{page}"), None, None, Some(""), None, None],
            [0; 4],
            1,
            66,
        ),
        (
            "duplicate equal",
            "<headerFooter><oddHeader>é</oddHeader><oddHeader>é</oddHeader></headerFooter>",
            [Some("é"), None, None, Some(""), None, None],
            [0; 4],
            0,
            70,
        ),
        (
            "duplicate different",
            "<headerFooter><oddHeader>é</oddHeader><oddHeader>B</oddHeader></headerFooter>",
            [Some("é"), None, None, Some(""), None, None],
            [1, 0, 0, 0],
            0,
            64,
        ),
        (
            "extra inherited",
            "<headerFooter><oddHeader>é</oddHeader></headerFooter><headerFooter><oddHeader>é</oddHeader></headerFooter>",
            [Some("é"), None, None, Some(""), None, None],
            [0; 4],
            0,
            154,
        ),
        (
            "extra clear",
            "<headerFooter><oddHeader>é</oddHeader></headerFooter><headerFooter/>",
            [Some("é"), None, None, Some(""), None, None],
            [1, 0, 0, 0],
            0,
            114,
        ),
        (
            "uncertain equal",
            "<headerFooter differentFirst=\"&flag;\"><oddHeader>é</oddHeader><firstHeader>é</firstHeader></headerFooter>",
            [Some("é"), None, None, Some(""), None, None],
            [0; 4],
            0,
            75,
        ),
        (
            "uncertain different",
            "<headerFooter differentFirst=\"&flag;\"><oddHeader>é</oddHeader><firstHeader>B</firstHeader></headerFooter>",
            [Some("é"), None, None, Some(""), None, None],
            [0, 0, 1, 0],
            0,
            69,
        ),
        (
            "full reader",
            concat!(
                "<headerFooter xmlns:x=\"urn:decoy\" differentFirst=\"&flag;\" x:differentOddEven=\"1\">",
                "<x:oddHeader>decoy</x:oddHeader><oddHeader>é&amp;P</oddHeader><oddHeader>B</oddHeader>",
                "<firstHeader>First</firstHeader><evenHeader>unused</evenHeader><oddFooter>F</oddFooter><firstFooter>Tail</firstFooter>",
                "</headerFooter><headerFooter differentOddEven=\"1\"><oddHeader>é&amp;P</oddHeader>",
                "<evenHeader>E</evenHeader><oddFooter>F</oddFooter><evenFooter>E2</evenFooter></headerFooter>"
            ),
            [Some("é{page}"), None, None, Some("F"), None, None],
            [2, 1, 1, 1],
            1,
            426,
        ),
    ];

    fn print_budget_sheet(body: &str) -> Element {
        let xml = format!("<worksheet xmlns=\"{}\">{body}</worksheet>", ns::SML);
        xml::parse(&ParseCtx::default(), xml.as_bytes(), Dialect::Xml, &mut WholeTree).unwrap().root
    }

    fn assert_print_budget_result(
        ctx: &ParseCtx<'_>,
        out: &SheetContent,
        expected: [Option<&str>; 6],
        counts: [u64; 4],
        reports: usize,
    ) {
        assert_eq!(out.headers_footers.len(), FIELDS.len());
        for (((state, _), text), (_, side, variant, _)) in
            out.headers_footers.iter().zip(expected).zip(FIELDS)
        {
            assert_eq!((state.value.region, state.value.variant), (side, variant));
            assert_eq!(
                state.value.content,
                text.map(|s| paragraphs([String::new(), s.to_owned(), String::new()]))
            );
            assert!(matches!(state.certainty, Certainty::Known));
        }
        assert_eq!(
            out.headers_footers
                .iter()
                .flat_map(|(_, codes)| codes.iter().copied())
                .collect::<Vec<_>>(),
            vec![DiagnosticCode::FieldToken; reports]
        );
        let actual: Vec<_> =
            ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect();
        let expected: Vec<_> = [
            (DiagnosticCode::BlockUnread, Region::Header),
            (DiagnosticCode::BlockUnread, Region::Footer),
            (DiagnosticCode::UnresolvedEntity, Region::Header),
            (DiagnosticCode::UnresolvedEntity, Region::Footer),
        ]
        .into_iter()
        .zip(counts)
        .filter(|(_, count)| *count != 0)
        .map(|((code, region), count)| (code, region, count))
        .collect();
        assert_eq!(actual.len(), expected.len());
        for value in expected {
            assert!(actual.contains(&value), "{actual:?} missing {value:?}");
        }
    }

    #[test]
    fn print_reader_work_budget_covers_retained_and_discarded_settings() {
        for (name, body, expected, counts, reports, work) in PRINT_BUDGET_ROWS {
            let tree = print_budget_sheet(body);
            let sheet = tree.find(ns::SML, "worksheet").unwrap();
            let limits = Limits { max_work: work, ..Limits::default() };
            let ctx = ParseCtx::new(&limits).with_headers_footers(HeaderFooterPolicy::Emit);
            let mut out = SheetContent::on(super::super::grid::OOXML_GRID, u64::MAX, u64::MAX);
            assert_eq!(read(&ctx, sheet, &mut out), Ok(()), "{name}");
            assert_eq!(ctx.fuel(), work, "{name}");
            assert_print_budget_result(&ctx, &out, expected, counts, reports);
            let limits = Limits { max_work: work - 1, ..Limits::default() };
            let ctx = ParseCtx::new(&limits).with_headers_footers(HeaderFooterPolicy::Emit);
            let mut out = SheetContent::on(super::super::grid::OOXML_GRID, u64::MAX, u64::MAX);
            assert_eq!(
                read(&ctx, sheet, &mut out),
                Err(ConvertError::limit(Limit::Work, work - 1)),
                "{name}"
            );
        }
    }

    #[test]
    fn print_state_comparison_prices_repeated_utf8_even_when_uncertain() {
        let value = HeaderFooter {
            region: Side::Header,
            variant: Variant::Default,
            content: Some(paragraphs([String::new(), "é".repeat(64), String::new()])),
        };
        let known = HeaderFooterState { value: value.clone(), certainty: Certainty::Known };
        let uncertain = HeaderFooterState { value, certainty: Certainty::Uncertain };
        for other in [&known, &uncertain] {
            let limits = Limits { max_work: 266, ..Limits::default() };
            let ctx = ParseCtx::new(&limits);
            for work in [133, 266] {
                assert_eq!(
                    known.same_content(other, |units| ctx.charge(units)),
                    Ok(matches!(other.certainty, Certainty::Known))
                );
                assert_eq!(ctx.fuel(), work);
            }
            let limits = Limits { max_work: 132, ..Limits::default() };
            let ctx = ParseCtx::new(&limits);
            assert_eq!(
                known.same_content(other, |units| ctx.charge(units)),
                Err(ConvertError::limit(Limit::Work, 132))
            );
        }
    }
}
