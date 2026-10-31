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

struct PrintSettings {
    slots: Vec<PrintSlot>,
    conflicts: [u64; 2],
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
        let mut conflicts = extra.conflicts.map(|count| count != 0);
        for (index, slot) in extra.slots.iter().enumerate() {
            if !same_effective(ctx, &kept, &extra, index)? {
                conflicts[side_index(slot.state.value.region)] = true;
            }
        }
        for (side, conflict) in [Side::Header, Side::Footer].into_iter().zip(conflicts) {
            if conflict {
                ctx.emit(DiagnosticCode::BlockUnread, diagnostic_region(side));
            }
        }
    }
    publish(ctx, kept, out)
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
        read_text_with(ctx, item, |code| {
            if matches!(code, DiagnosticCode::UnresolvedEntity | DiagnosticCode::InvalidEncoding) {
                slot.state.certainty = Certainty::Uncertain;
            }
            slot.unread_content |= code.class() == crate::LossClass::Lost;
            slot.reports.push(code);
        })?
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

fn publish(
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
}
