//! `StyleTextPropAtom` and `TxMasterStyleAtom` parsing: paragraph and
//! character formatting runs keyed by text range, per MS-PPT's
//! TextPFException / TextCFException layouts. Parsing is defensive - a
//! malformed exception aborts styling for that atom, never the text.

use crate::model::VerticalPosition;
use crate::shared::binary::{get_u16, get_u32};
use crate::shared::delta::StyleDelta;

const PF_HAS_BULLET: u32 = 0x0000_0001;
/// [MS-PPT] 2.9.21 `PFMasks`: `hasBullet`, `bulletHasFont`, `bulletHasColor`
/// and `bulletHasSize` each say the `bulletFlags` field exists.
const PF_BULLET_FLAGS: u32 = 0x0000_000F;
const PF_BULLET_FONT: u32 = 0x0000_0010;
const PF_BULLET_COLOR: u32 = 0x0000_0020;
const PF_BULLET_SIZE: u32 = 0x0000_0040;
const PF_BULLET_CHAR: u32 = 0x0000_0080;
const PF_LEFT_MARGIN: u32 = 0x0000_0100;
const PF_INDENT: u32 = 0x0000_0400;
const PF_ALIGN: u32 = 0x0000_0800;
const PF_LINE_SPACING: u32 = 0x0000_1000;
const PF_SPACE_BEFORE: u32 = 0x0000_2000;
const PF_SPACE_AFTER: u32 = 0x0000_4000;
const PF_DEFAULT_TAB_SIZE: u32 = 0x0000_8000;
const PF_FONT_ALIGN: u32 = 0x0001_0000;
/// [MS-PPT] 2.9.21 `PFMasks`: `charWrap`, `wordWrap` and `overflow` each say
/// the `wrapFlags` field exists.
const PF_WRAP_FLAGS: u32 = 0x000E_0000;
const PF_TAB_STOPS: u32 = 0x0010_0000;
const PF_TEXT_DIRECTION: u32 = 0x0020_0000;
/// The fields after `bulletFlags` in [MS-PPT] 2.9.20 order, by mask bit and width; a
/// `tabStops` array also holds as many 4-byte stops as its leading count says.
const PF_FIELDS: &[(u32, usize)] = &[
    (PF_BULLET_CHAR, 2),
    (PF_BULLET_FONT, 2),
    (PF_BULLET_SIZE, 2),
    (PF_BULLET_COLOR, 4),
    (PF_ALIGN, 2),
    (PF_LINE_SPACING, 2),
    (PF_SPACE_BEFORE, 2),
    (PF_SPACE_AFTER, 2),
    (PF_LEFT_MARGIN, 2),
    (PF_INDENT, 2),
    (PF_DEFAULT_TAB_SIZE, 2),
    (PF_TAB_STOPS, 2),
    (PF_FONT_ALIGN, 2),
    (PF_WRAP_FLAGS, 2),
    (PF_TEXT_DIRECTION, 2),
];

const CF_BOLD: u32 = 0x0000_0001;
const CF_ITALIC: u32 = 0x0000_0002;
const CF_FONT_STYLE: u32 = 0x0000_FFFF;
const CF_TYPEFACE: u32 = 0x0001_0000;
const CF_SIZE: u32 = 0x0002_0000;
const CF_COLOR: u32 = 0x0004_0000;
const CF_POSITION: u32 = 0x0008_0000;
const CF_OLD_EA_TYPEFACE: u32 = 0x0020_0000;
const CF_ANSI_TYPEFACE: u32 = 0x0040_0000;
const CF_SYMBOL_TYPEFACE: u32 = 0x0080_0000;

/// A `TextTypeEnum` value, kept raw: a text type this reader does not name
/// still finds the master defaults of the same instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct TextType(pub(super) u16);

impl TextType {
    pub(super) const TITLE: Self = Self(0);
    pub(super) const BODY: Self = Self(1);
    pub(super) const CENTER_BODY: Self = Self(5);
    pub(super) const CENTER_TITLE: Self = Self(6);

    pub(super) fn is_title(self) -> bool {
        matches!(self, Self::TITLE | Self::CENTER_TITLE)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ParaProps {
    pub count: usize,
    pub depth: u16,
    /// From bulletFlags' fHasBullet, when present.
    pub bullet: Option<bool>,
}

/// Character formatting of a `TextCFException`: each property is tri-state, and `None`
/// inherits the master's per-level default.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CharStyle {
    pub delta: StyleDelta,
    /// `fontRef`, an index into the font collection.
    pub font: Option<u16>,
    /// `symbolFontRef`, an index into the font collection.
    pub symbol_font: Option<u16>,
}

impl CharStyle {
    pub(super) fn or(self, base: Self) -> Self {
        Self {
            delta: base.delta.merge(self.delta),
            font: self.font.or(base.font),
            symbol_font: self.symbol_font.or(base.symbol_font),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CharProps {
    pub count: usize,
    pub style: CharStyle,
}

/// One indent level's defaults from a `TxMasterStyleAtom`, tri-state.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MasterLevel {
    pub bullet: Option<bool>,
    pub chars: CharStyle,
}

#[derive(Debug, Default)]
pub(super) struct StyleRuns {
    pub paragraphs: Vec<ParaProps>,
    pub chars: Vec<CharProps>,
}

/// Parse a `StyleTextPropAtom` body for text of `text_len` UTF-16 units, and
/// whether its runs cover the text and every one parsed.
pub(super) fn parse_style_text(body: &[u8], text_len: usize) -> (StyleRuns, bool) {
    let mut runs = StyleRuns::default();
    let mut pos = 0usize;
    let complete = read_runs(body, &mut pos, text_len, &mut runs.paragraphs, |count, at| {
        let depth = get_u16(body, at)?;
        let (bullet, next) = parse_pf_exception(body, at + 2)?;
        Some((ParaProps { count, depth, bullet }, next))
    }) && read_runs(body, &mut pos, text_len, &mut runs.chars, |count, at| {
        let (style, next) = parse_cf_exception(body, at)?;
        Some((CharProps { count, style }, next))
    });
    (runs, complete)
}

/// Push the runs `read` takes from the counts at `pos` until they cover `text_len` units
/// and the implicit final paragraph mark; false when one is unreadable or empty.
fn read_runs<T>(
    body: &[u8],
    pos: &mut usize,
    text_len: usize,
    runs: &mut Vec<T>,
    read: impl Fn(usize, usize) -> Option<(T, usize)>,
) -> bool {
    let mut covered = 0usize;
    while covered <= text_len {
        let Some(count) = get_u32(body, *pos).map(|v| v as usize) else {
            return false;
        };
        let Some((run, next)) = read(count, *pos + 4) else {
            return false;
        };
        *pos = next;
        runs.push(run);
        covered += count;
        if count == 0 {
            return false;
        }
    }
    true
}

/// TextPFException: mask + fields in mask-bit order. Returns the bullet
/// state (if the mask carried bulletFlags) and the next offset.
fn parse_pf_exception(body: &[u8], pos: usize) -> Option<(Option<bool>, usize)> {
    let mask = get_u32(body, pos)?;
    let mut pos = pos + 4;
    let mut bullet = None;
    // The fHasBullet value is specified only when masks.hasBullet itself is set.
    if mask & PF_BULLET_FLAGS != 0 {
        let flags = get_u16(body, pos)?;
        bullet = (mask & PF_HAS_BULLET != 0).then_some(flags & 0x0001 != 0);
        pos += 2;
    }
    for &(bit, width) in PF_FIELDS {
        if mask & bit == 0 {
            continue;
        }
        if bit == PF_TAB_STOPS {
            pos += usize::from(get_u16(body, pos)?) * 4;
        }
        pos += width;
    }
    (pos <= body.len()).then_some((bullet, pos))
}

/// The shift a `position` percentage of the line height names, by its sign.
fn shift(percent: i16) -> VerticalPosition {
    match percent {
        1.. => VerticalPosition::Superscript,
        ..0 => VerticalPosition::Subscript,
        0 => VerticalPosition::Baseline,
    }
}

/// TextCFException: mask (+ optional style bitfield) + sized fields. Each
/// style bit is specified only when its own mask bit is set (per-bit
/// tri-state); strike-through is not in the 97-2003 style bits. [MS-PPT] 2.9.43 ends the
/// structure at `position`: the pp10ext, newEA, cs and pp11ext masks must be zero here.
fn parse_cf_exception(body: &[u8], pos: usize) -> Option<(CharStyle, usize)> {
    let mask = get_u32(body, pos)?;
    let mut pos = pos + 4;
    let mut field = |bit: u32, width: usize| -> Option<Option<u16>> {
        if mask & bit == 0 {
            return Some(None);
        }
        let value = get_u16(body, pos)?;
        pos += width;
        Some(Some(value))
    };
    let style = field(CF_FONT_STYLE, 2)?;
    let font = field(CF_TYPEFACE, 2)?;
    field(CF_OLD_EA_TYPEFACE, 2)?;
    field(CF_ANSI_TYPEFACE, 2)?;
    let symbol_font = field(CF_SYMBOL_TYPEFACE, 2)?;
    field(CF_SIZE, 2)?;
    field(CF_COLOR, 4)?;
    let position = field(CF_POSITION, 2)?.map(|percent| shift(percent.cast_signed()));
    let flag =
        |bit: u32| style.filter(|_| mask & bit != 0).map(|style| u32::from(style) & bit != 0);
    let delta =
        StyleDelta { bold: flag(CF_BOLD), italic: flag(CF_ITALIC), position, ..Default::default() };
    (pos <= body.len()).then_some((CharStyle { delta, font, symbol_font }, pos))
}

/// A `TxMasterStyleAtom`'s per-indent-level tri-state defaults (index =
/// depth), and whether every level it declares parsed.
pub(super) fn parse_master_style(body: &[u8], text_type: TextType) -> (Vec<MasterLevel>, bool) {
    let Some(levels) = get_u16(body, 0).map(|v| v as usize) else {
        return (Vec::new(), false);
    };
    let mut pos = 2usize;
    let mut out = Vec::new();
    for _ in 0..levels.min(10) {
        // Levels for body-family text types carry a leading depth field in
        // format version >= 9 atoms; instances >= 5 always do.
        if text_type >= TextType::CENTER_BODY {
            pos += 2;
        }
        let Some((bullet, next)) = parse_pf_exception(body, pos) else {
            return (out, false);
        };
        pos = next;
        let Some((chars, next)) = parse_cf_exception(body, pos) else {
            return (out, false);
        };
        pos = next;
        out.push(MasterLevel { bullet, chars });
    }
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::ppt::tests::{halves, words};
    use VerticalPosition::{Baseline, Subscript, Superscript};

    /// A character run's count and `TextCFException`: its mask, then `fields` as written.
    fn run(count: u32, mask: u32, fields: &[u8]) -> Vec<u8> {
        [words(&[count, mask]), fields.to_vec()].concat()
    }

    fn exception(mask: u32, fields: &[u8]) -> Vec<u8> {
        [words(&[mask]), fields.to_vec()].concat()
    }

    /// A style atom for `text_len` units whose single paragraph run and `runs` cover them.
    fn atom(text_len: u32, runs: &[Vec<u8>]) -> Vec<u8> {
        [words(&[text_len + 1]), vec![0, 0], words(&[0]), runs.concat()].concat()
    }

    const ALL_FIELDS: u32 = CF_BOLD
        | CF_TYPEFACE
        | CF_SIZE
        | CF_COLOR
        | CF_POSITION
        | CF_OLD_EA_TYPEFACE
        | CF_ANSI_TYPEFACE
        | CF_SYMBOL_TYPEFACE;

    #[test]
    fn a_run_reads_its_typeface_symbol_typeface_and_position_among_the_fields_it_skips() {
        let fields = [
            halves(&[1]),
            halves(&[0x0102, 0x0203, 0x0304, 0x0405, 0x0506]),
            words(&[0x0A0B_0C0D]),
            (-25i16).to_le_bytes().to_vec(),
        ]
        .concat();
        let runs = [
            run(2, ALL_FIELDS, &fields),
            run(2, CF_POSITION, &30i16.to_le_bytes()),
            run(1, 0, &[]),
        ];
        let (styles, complete) = parse_style_text(&atom(4, &runs), 4);
        assert!(complete);
        let chars: Vec<_> = styles
            .chars
            .iter()
            .map(|c| {
                (
                    c.count,
                    c.style.delta.bold,
                    c.style.font,
                    c.style.symbol_font,
                    c.style.delta.position,
                )
            })
            .collect();
        assert_eq!(
            chars,
            [
                (2, Some(true), Some(0x0102), Some(0x0405), Some(Subscript)),
                (2, None, None, None, Some(Superscript)),
                (1, None, None, None, None),
            ]
        );
    }

    #[test]
    fn a_position_is_raised_lowered_or_level_by_its_sign() {
        for (percent, expected) in [
            (30, Superscript),
            (1, Superscript),
            (100, Superscript),
            (i16::MAX, Superscript),
            (0, Baseline),
            (-1, Subscript),
            (-25, Subscript),
            (-100, Subscript),
            (i16::MIN, Subscript),
        ] {
            let runs = [run(2, CF_POSITION, &percent.to_le_bytes())];
            let (styles, complete) = parse_style_text(&atom(1, &runs), 1);
            assert!(complete, "{percent}");
            assert_eq!(styles.chars[0].style.delta.position, Some(expected), "{percent}");
        }
    }

    #[test]
    fn nothing_follows_position_so_the_masks_of_the_later_exceptions_read_no_bytes() {
        let later = 0x0010_0000 | 0x0100_0000 | 0x0200_0000 | 0x0400_0000;
        let runs = [
            run(2, CF_POSITION | later, &(-25i16).to_le_bytes()),
            run(2, CF_POSITION, &30i16.to_le_bytes()),
        ];
        let (styles, complete) = parse_style_text(&atom(3, &runs), 3);
        assert!(complete);
        let positions: Vec<_> = styles.chars.iter().map(|c| c.style.delta.position).collect();
        assert_eq!(positions, [Some(Subscript), Some(Superscript)]);
    }

    #[test]
    fn a_run_cut_inside_a_field_is_unreadable() {
        for (mask, fields) in [
            (CF_TYPEFACE, vec![1]),
            (CF_SYMBOL_TYPEFACE, vec![]),
            (CF_POSITION, vec![30]),
            (CF_SIZE | CF_POSITION, halves(&[12])),
            (CF_COLOR, halves(&[1])),
        ] {
            let runs = [run(1, 0, &[]), run(1, mask, &fields)];
            let (styles, complete) = parse_style_text(&atom(1, &runs), 1);
            assert!(!complete, "{mask:#x}");
            assert_eq!(styles.chars.len(), 1, "{mask:#x}");
        }
    }

    #[test]
    fn a_style_bit_is_read_only_when_its_own_mask_bit_is_set() {
        for (label, mask, style, expected) in [
            ("bold alone", CF_BOLD, 0b11, (Some(true), None)),
            ("italic alone", CF_ITALIC, 0b11, (None, Some(true))),
            ("both, bold off", CF_BOLD | CF_ITALIC, 0b10, (Some(false), Some(true))),
            ("another style bit", 0x0004, 0b11, (None, None)),
        ] {
            let runs = [run(2, mask, &halves(&[style]))];
            let (styles, complete) = parse_style_text(&atom(1, &runs), 1);
            let delta = styles.chars[0].style.delta;
            assert!(complete, "{label}");
            assert_eq!((delta.bold, delta.italic), expected, "{label}");
        }
    }

    #[test]
    fn a_run_of_no_units_ends_its_pass_incomplete_and_keeps_the_runs_before_it() {
        let empty_paragraph = [words(&[0]), vec![0, 0], words(&[0])].concat();
        let (styles, complete) =
            parse_style_text(&[empty_paragraph.clone(), empty_paragraph].concat(), 1);
        assert_eq!((complete, styles.paragraphs.len(), styles.chars.len()), (false, 1, 0));
        let runs = [run(1, 0, &[]), run(0, 0, &[]), run(2, 0, &[])];
        let (styles, complete) = parse_style_text(&atom(2, &runs), 2);
        assert_eq!((complete, styles.chars.len()), (false, 2));
    }

    #[test]
    fn a_master_level_carries_its_position_and_font_references() {
        let fields = [halves(&[7, 9]), 30i16.to_le_bytes().to_vec()].concat();
        let level = [
            words(&[0]),
            words(&[CF_TYPEFACE | CF_SYMBOL_TYPEFACE | CF_POSITION]),
            fields,
            words(&[0, 0]),
        ]
        .concat();
        let (levels, complete) =
            parse_master_style(&[&[2, 0][..], &level].concat(), TextType::BODY);
        assert!(complete);
        let seen: Vec<_> = levels
            .iter()
            .map(|l| (l.chars.font, l.chars.symbol_font, l.chars.delta.position))
            .collect();
        assert_eq!(seen, [(Some(7), Some(9), Some(Superscript)), (None, None, None)]);
    }

    #[test]
    fn a_paragraph_exception_with_every_field_ends_after_the_bytes_its_layout_gives_them() {
        let mask = PF_FIELDS.iter().fold(0, |all, &(bit, _)| all | bit);
        for stops in [0, 1, 3] {
            let fields =
                [vec![0xFF; 24], halves(&[stops]), vec![0xFF; usize::from(stops) * 4 + 6]].concat();
            let body = exception(mask, &fields);
            assert_eq!(parse_pf_exception(&body, 0), Some((None, body.len())), "{stops}");
            assert_eq!(parse_pf_exception(&body[..body.len() - 1], 0), None, "{stops}");
        }
    }

    #[test]
    fn a_paragraph_exception_reports_a_bullet_only_when_its_mask_has_one() {
        for (label, mask, flags, expected) in [
            ("set", PF_HAS_BULLET, 1, (Some(true), 6)),
            ("clear", PF_HAS_BULLET, 0, (Some(false), 6)),
            ("only other flag bits", PF_HAS_BULLET, 0xFFFE, (Some(false), 6)),
            ("the flags without hasBullet", PF_BULLET_FLAGS & !PF_HAS_BULLET, 1, (None, 6)),
        ] {
            let body = exception(mask, &[halves(&[flags]), vec![0; 4]].concat());
            assert_eq!(parse_pf_exception(&body, 0), Some(expected), "{label}");
        }
    }
}
