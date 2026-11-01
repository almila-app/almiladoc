use super::headers::{self, BinaryPrintText, Damage, PartialPrintSettings};
use super::xls::{PrintString, SegReader, read_print_string};
use crate::ctx::ParseCtx;
use crate::diagnostics::DiagnosticCode;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{HeaderFooterRegion as Side, HeaderFooterVariant as Variant};
use crate::shared::binary::get_u16;
use crate::shared::codepage::Codepage;

pub(super) const HEADER: u16 = 0x0014;
pub(super) const FOOTER: u16 = 0x0015;
pub(super) const HEADER_FOOTER: u16 = 0x089C;
const USER_VIEW_BEGIN: u16 = 0x01AA;
const USER_VIEW_END: u16 = 0x01AB;
const START_BLOCK: u16 = 0x0852;
const END_BLOCK: u16 = 0x0853;
const START_OBJECT: u16 = 0x0854;
const END_OBJECT: u16 = 0x0855;
const FRT_HEADER_BYTES: usize = 12;
const VIEW_GUID_BYTES: usize = 16;
const VIEW_END_BYTES: usize = 2;
const EVEN_PAGES: u16 = 0x0001;
const FIRST_PAGE: u16 = 0x0002;
const VARIANTS: [(Side, Variant, u16); 4] = [
    (Side::Header, Variant::Even, EVEN_PAGES),
    (Side::Footer, Variant::Even, EVEN_PAGES),
    (Side::Header, Variant::First, FIRST_PAGE),
    (Side::Footer, Variant::First, FIRST_PAGE),
];

#[derive(Default)]
struct PrintPresence {
    header: bool,
    footer: bool,
    variants: bool,
}

#[derive(Default)]
pub(super) struct PrintRecords {
    settings: PartialPrintSettings,
    open_views: Vec<PrintPresence>,
    unknown_scope: bool,
    unresolved: PrintPresence,
}

impl PrintRecords {
    pub(super) fn eligible(&self) -> bool {
        self.open_views.is_empty() && !self.unknown_scope
    }

    pub(super) fn observe(
        &mut self,
        ctx: &ParseCtx<'_>,
        record: u16,
        body: &[u8],
    ) -> Result<(), ConvertError> {
        if self.unknown_scope {
            return Ok(());
        }
        match record {
            USER_VIEW_BEGIN => {
                ctx.charge(1)?;
                if self.open_views.len() as u64 >= ctx.limits().max_record_depth {
                    return Err(ConvertError::limit(
                        Limit::RecordDepth,
                        ctx.limits().max_record_depth,
                    ));
                }
                self.open_views
                    .try_reserve(1)
                    .map_err(|_| ConvertError::limit(Limit::Allocation, 0))?;
                self.open_views.push(PrintPresence::default());
            }
            USER_VIEW_END if body.len() >= VIEW_END_BYTES => {
                self.open_views.pop();
            }
            START_BLOCK | END_BLOCK | START_OBJECT | END_OBJECT if self.open_views.is_empty() => {
                self.unknown_scope = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn skipped(&mut self, record: u16) {
        let seen = self
            .open_views
            .last_mut()
            .filter(|_| !self.unknown_scope)
            .unwrap_or(&mut self.unresolved);
        match record {
            HEADER => seen.header = true,
            FOOTER => seen.footer = true,
            HEADER_FOOTER => seen.variants = true,
            _ => {}
        }
    }

    pub(super) fn read(
        &mut self,
        ctx: &ParseCtx<'_>,
        record: u16,
        segments: &[&[u8]],
    ) -> Result<(), ConvertError> {
        if !self.eligible() {
            self.skipped(record);
            return Ok(());
        }
        ctx.charge(segments.len() as u64)?;
        match record {
            HEADER | FOOTER => {
                let side = if record == HEADER { Side::Header } else { Side::Footer };
                let value = if segments.first().is_some_and(|bytes| bytes.is_empty()) {
                    PrintString::Utf16(Vec::new())
                } else {
                    read_print_string(ctx, &mut SegReader::new(segments), None, true)?
                };
                self.settings.absorb(ctx, side, Variant::Default, as_field(&value))?;
            }
            HEADER_FOOTER => self.variants(ctx, &mut SegReader::new(segments))?,
            _ => return Err(ConvertError::Internal),
        }
        Ok(())
    }

    /// Read a BIFF5/7 `HEADER` or `FOOTER` record, through its `CONTINUE` segments: a one-byte
    /// character count and that many bytes in the workbook's code page, with the span of each
    /// character its decoding damaged.
    pub(super) fn read_bytes(
        &mut self,
        ctx: &ParseCtx<'_>,
        record: u16,
        segments: &[&[u8]],
        codepage: Codepage,
    ) -> Result<(), ConvertError> {
        if !self.eligible() {
            self.skipped(record);
            return Ok(());
        }
        let side = match record {
            HEADER => Side::Header,
            FOOTER => Side::Footer,
            _ => return Ok(()),
        };
        ctx.charge(segments.len() as u64)?;
        let mut bytes = segments.iter().flat_map(|segment| segment.iter().copied());
        let text = match bytes.next() {
            None => BinaryPrintText::Decoded(String::new(), Vec::new()),
            Some(count) => {
                let wanted: Vec<u8> = bytes.by_ref().take(usize::from(count)).collect();
                ctx.charge(wanted.len() as u64)?;
                if wanted.len() < usize::from(count) {
                    BinaryPrintText::Unread
                } else {
                    let (text, invalid) = codepage.decode_marked(&wanted);
                    let damage = damage_spans(&text, &invalid, codepage.assumed);
                    BinaryPrintText::Decoded(text, damage)
                }
            }
        };
        self.settings.absorb(ctx, side, Variant::Default, text)
    }

    fn variants(
        &mut self,
        ctx: &ParseCtx<'_>,
        reader: &mut SegReader<'_>,
    ) -> Result<(), ConvertError> {
        let Some(header) = reader.bytes(FRT_HEADER_BYTES) else {
            return self.unread_variants(ctx);
        };
        let Some(guid) = reader.bytes(VIEW_GUID_BYTES) else {
            return self.unread_variants(ctx);
        };
        if guid.iter().any(|byte| *byte != 0) {
            return Ok(());
        }
        if get_u16(header, 0) != Some(HEADER_FOOTER) {
            return self.unread_variants(ctx);
        }
        let Some(flags) = reader.u16() else { return self.unread_variants(ctx) };
        let Some(counts) = counts(reader) else { return self.unread_variants(ctx) };
        let mut broken = false;
        for ((side, variant, mask), count) in VARIANTS.into_iter().zip(counts) {
            ctx.charge(1)?;
            let selected = flags & mask != 0;
            let value = if count == 0 {
                PrintString::Utf16(Vec::new())
            } else if broken {
                PrintString::BrokenFraming
            } else {
                read_print_string(ctx, reader, Some(count), selected)?
            };
            broken |= matches!(value, PrintString::BrokenFraming);
            let field = if selected { as_field(&value) } else { BinaryPrintText::Inherit };
            self.settings.absorb(ctx, side, variant, field)?;
        }
        Ok(())
    }

    fn unread_variants(&mut self, ctx: &ParseCtx<'_>) -> Result<(), ConvertError> {
        for (side, variant, _) in VARIANTS {
            ctx.charge(1)?;
            self.settings.absorb(ctx, side, variant, BinaryPrintText::Unread)?;
        }
        Ok(())
    }

    pub(super) fn unread_record(
        &mut self,
        ctx: &ParseCtx<'_>,
        record: u16,
    ) -> Result<(), ConvertError> {
        if !self.eligible() {
            self.skipped(record);
            return Ok(());
        }
        match record {
            HEADER => {
                self.settings.absorb(ctx, Side::Header, Variant::Default, BinaryPrintText::Unread)
            }
            FOOTER => {
                self.settings.absorb(ctx, Side::Footer, Variant::Default, BinaryPrintText::Unread)
            }
            HEADER_FOOTER => self.unread_variants(ctx),
            _ => Ok(()),
        }
    }

    pub(super) fn finish(
        mut self,
        ctx: &ParseCtx<'_>,
        out: &mut super::grid::SheetContent,
    ) -> Result<(), ConvertError> {
        let mut seen = std::mem::take(&mut self.unresolved);
        for frame in std::mem::take(&mut self.open_views) {
            ctx.charge(1)?;
            seen.header |= frame.header;
            seen.footer |= frame.footer;
            seen.variants |= frame.variants;
        }
        if seen.header {
            self.settings.absorb(ctx, Side::Header, Variant::Default, BinaryPrintText::Unread)?;
        }
        if seen.footer {
            self.settings.absorb(ctx, Side::Footer, Variant::Default, BinaryPrintText::Unread)?;
        }
        if seen.variants {
            self.unread_variants(ctx)?;
        }
        headers::publish(ctx, self.settings.finish(ctx)?, out)
    }
}

/// The spans of `text` its decoding damaged: those of bytes that did not decode, and, where the
/// code page was assumed, each other non-ASCII character.
fn damage_spans(text: &str, invalid: &[std::ops::Range<usize>], assumed: bool) -> Vec<Damage> {
    let mut damage: Vec<Damage> =
        invalid.iter().map(|span| (span.clone(), DiagnosticCode::InvalidEncoding)).collect();
    if assumed {
        damage.extend(text.char_indices().filter_map(|(at, c)| {
            let span = at..at + c.len_utf8();
            (!c.is_ascii() && !invalid.contains(&span))
                .then_some((span, DiagnosticCode::EncodingAssumed))
        }));
        damage.sort_by_key(|(span, _)| span.start);
    }
    damage
}

fn counts(reader: &mut SegReader<'_>) -> Option<[u16; 4]> {
    Some([reader.u16()?, reader.u16()?, reader.u16()?, reader.u16()?])
}

fn as_field(value: &PrintString) -> BinaryPrintText<'_> {
    match value {
        PrintString::Utf16(bytes) => BinaryPrintText::Utf16(Some(bytes)),
        PrintString::InvalidLength | PrintString::BrokenFraming => BinaryPrintText::Unread,
    }
}
