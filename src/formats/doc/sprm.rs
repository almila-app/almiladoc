//! Sprm (single property modifier) walking and application, per MS-DOC:
//! character toggles resolve against the style in force (0 = off, 1 = on,
//! 0x80 = style's value, 0x81 = style's value inverted).

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::{Style, VerticalPosition};
use crate::shared::binary::{ByteReader, get_u16, get_u32};
use crate::shared::blockstyle::Outline;

pub(super) const SPRM_CF_RMARK_DEL: u16 = 0x0800;
pub(super) const SPRM_CF_RMARK_INS: u16 = 0x0801;
const SPRM_CF_FLD_VANISH: u16 = 0x0802;
pub(super) const SPRM_CF_DATA: u16 = 0x0806;
pub(super) const SPRM_CF_OLE2: u16 = 0x080A;
pub(super) const SPRM_CF_BOLD: u16 = 0x0835;
pub(super) const SPRM_CF_ITALIC: u16 = 0x0836;
pub(super) const SPRM_CF_STRIKE: u16 = 0x0837;
pub(super) const SPRM_CF_VANISH: u16 = 0x083C;
pub(super) const SPRM_C_ISTD: u16 = 0x4A30;
pub(super) const SPRM_C_ISS: u16 = 0x2A48;
pub(super) const SPRM_C_PIC_LOCATION: u16 = 0x6A03;
const SPRM_CF_SPEC_VANISH: u16 = 0x0818;
const SPRM_CF_SPEC: u16 = 0x0855;
const SPRM_C_ISTD_PERMUTE: u16 = 0xCA31;
const SPRM_C_PLAIN: u16 = 0x2A33;
const SPRM_C_MAJORITY: u16 = 0xCA47;
const SPRM_C_DISP_FLD_RMARK: u16 = 0xCA62;
const SPRM_C_IDSL_RMARK_DEL: u16 = 0x4867;
const SPRM_C_LBC_CRJ: u16 = 0x2879;
const SPRM_C_PBI_IBULLET: u16 = 0x6887;
const SPRM_C_PBI_GRF: u16 = 0x4888;
const SPRM_C_RG_FTC0: u16 = 0x4A4F;
const SPRM_C_RG_FTC1: u16 = 0x4A50;
const SPRM_C_RG_FTC2: u16 = 0x4A51;
const SPRM_C_FTC_BI: u16 = 0x4A5E;
const SPRM_C_SYMBOL: u16 = 0x6A09;
const SPRM_C_IDCT_HINT: u16 = 0x286F;
const SPRM_CF_BIDI: u16 = 0x085A;
const SPRM_CF_COMPLEX_SCRIPTS: u16 = 0x0882;
pub(super) const SPRM_PF_IN_TABLE: u16 = 0x2416;
pub(super) const SPRM_PF_TTP: u16 = 0x2417;
pub(super) const SPRM_PF_INNER_TABLE_CELL: u16 = 0x244B;
pub(super) const SPRM_PF_INNER_TTP: u16 = 0x244C;
pub(super) const SPRM_P_ILVL: u16 = 0x260A;
pub(super) const SPRM_P_OUT_LVL: u16 = 0x2640;
pub(super) const SPRM_P_ILFO: u16 = 0x460B;
pub(super) const SPRM_P_HUGE_PAPX: u16 = 0x6646;
pub(super) const SPRM_P_ITAP: u16 = 0x6649;
pub(super) const SPRM_P_DTAP: u16 = 0x664A;
pub(super) const SPRM_T_TABLE_HEADER: u16 = 0x3404;
pub(super) const SPRM_T_DEF_TABLE: u16 = 0xD608;
pub(super) const SPRM_T_VERT_MERGE: u16 = 0xD62B;

/// The `sprmPIlfo` operand of a paragraph whose list numbering is suppressed.
const ILFO_SUPPRESSED: u16 = 0xF801;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Chp {
    pub style: Style,
    pub hidden: bool,
    pub deleted: bool,
    pub fonts: Fonts,
    pub direct: Direct,
}

impl Chp {
    /// These properties with the ones a style reference or `sprmCPlain` leaves as `run` has them.
    fn keeping(self, run: Self) -> Self {
        Self { deleted: run.deleted, direct: run.direct, ..self }
    }
}

/// A run's font indices into the font table, by slot ([MS-DOC] `sprmCRgFtc0`, `sprmCRgFtc1`,
/// `sprmCRgFtc2` and `sprmCFtcBi`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Fonts<T = u16> {
    pub ascii: T,
    pub east_asia: T,
    pub other: T,
    pub bidi: T,
}

impl<T> Fonts<T> {
    fn slot_mut(&mut self, sprm: u16) -> Option<&mut T> {
        match sprm {
            SPRM_C_RG_FTC0 => Some(&mut self.ascii),
            SPRM_C_RG_FTC1 => Some(&mut self.east_asia),
            SPRM_C_RG_FTC2 => Some(&mut self.other),
            SPRM_C_FTC_BI => Some(&mut self.bidi),
            _ => None,
        }
    }
}

impl Fonts<Option<u16>> {
    fn then(self, child: Self) -> Self {
        Self {
            ascii: child.ascii.or(self.ascii),
            east_asia: child.east_asia.or(self.east_asia),
            other: child.other.or(self.other),
            bidi: child.bidi.or(self.bidi),
        }
    }

    fn over(self, below: Fonts) -> Fonts {
        Fonts {
            ascii: self.ascii.unwrap_or(below.ascii),
            east_asia: self.east_asia.unwrap_or(below.east_asia),
            other: self.other.unwrap_or(below.other),
            bidi: self.bidi.unwrap_or(below.bidi),
        }
    }
}

/// A `sprmCSymbol` operand: the character `xchar` in the font `ftc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Symbol {
    pub ftc: u16,
    pub xchar: u16,
}

/// [MS-DOC] `sprmCIdctHint` values.
pub(super) const HINT_EAST_ASIA: u8 = 1;
pub(super) const HINT_COMPLEX: u8 = 2;

/// The properties only direct formatting sets, which a style reference and `sprmCPlain` keep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Direct {
    pub symbol: Option<Symbol>,
    pub hint: u8,
    pub bidi: bool,
    pub complex: bool,
}

// [MS-DOC] 2.9.327 ToggleOperand.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum BoolOp {
    #[default]
    Identity,
    Invert,
    SetFalse,
    SetTrue,
}

impl BoolOp {
    fn of(operand: &[u8]) -> Option<Self> {
        match operand.first() {
            Some(0) => Some(Self::SetFalse),
            Some(1) => Some(Self::SetTrue),
            Some(0x80) => Some(Self::Identity),
            Some(0x81) => Some(Self::Invert),
            _ => None,
        }
    }

    fn apply(self, value: bool) -> bool {
        match self {
            Self::Identity => value,
            Self::Invert => !value,
            Self::SetFalse => false,
            Self::SetTrue => true,
        }
    }

    fn then(self, child: Self) -> Self {
        match (child.apply(self.apply(false)), child.apply(self.apply(true))) {
            (false, true) => Self::Identity,
            (true, false) => Self::Invert,
            (false, false) => Self::SetFalse,
            (true, true) => Self::SetTrue,
        }
    }
}

/// The character properties one style layer changes, and how, so that layers compose without
/// the sprms that made them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct CharOps {
    bold: BoolOp,
    italic: BoolOp,
    strike: BoolOp,
    hidden: BoolOp,
    position: Option<VerticalPosition>,
    fonts: Fonts<Option<u16>>,
}

impl CharOps {
    pub(super) fn then(self, child: Self) -> Self {
        Self {
            bold: self.bold.then(child.bold),
            italic: self.italic.then(child.italic),
            strike: self.strike.then(child.strike),
            hidden: self.hidden.then(child.hidden),
            position: child.position.or(self.position),
            fonts: self.fonts.then(child.fonts),
        }
    }

    pub(super) fn apply(self, chp: Chp) -> Chp {
        let style = Style {
            bold: self.bold.apply(chp.style.bold),
            italic: self.italic.apply(chp.style.italic),
            strike: self.strike.apply(chp.style.strike),
            position: self.position.unwrap_or(chp.style.position),
            ..chp.style
        };
        Chp {
            style,
            hidden: self.hidden.apply(chp.hidden),
            fonts: self.fonts.over(chp.fonts),
            ..chp
        }
    }
}

fn sprm_operand_len(sprm: u16, operand: &[u8]) -> usize {
    match sprm >> 13 {
        0 | 1 => 1,
        2 | 4 | 5 => 2,
        3 => 4,
        7 => 3,
        _ => {
            if sprm == SPRM_T_DEF_TABLE {
                get_u16(operand, 0).map_or(0, |cb| usize::from(cb) + 1)
            } else {
                operand.first().map_or(0, |&b| b as usize + 1)
            }
        }
    }
}

/// Each sprm of `grpprl` with its operand, up to the first operand that does not fit.
fn sprms(grpprl: &[u8]) -> impl Iterator<Item = (u16, &[u8])> + '_ {
    let mut reader = ByteReader::new(grpprl);
    std::iter::from_fn(move || {
        let sprm = reader.u16()?;
        Some((sprm, reader.take(sprm_operand_len(sprm, reader.rest()))?))
    })
    .fuse()
}

fn last_operand(grpprl: &[u8], sprm: u16) -> Option<&[u8]> {
    sprms(grpprl).filter(|&(found, _)| found == sprm).last().map(|(_, operand)| operand)
}

fn is_on(operand: &[u8]) -> bool {
    operand.first().is_some_and(|&v| v != 0)
}

/// Whether a CHPX sets the one-byte flag `flag`: `sprmCFOle2` marks an
/// embedded object, `sprmCFData` field data.
pub(super) fn sets_flag(chpx: &[u8], flag: u16) -> bool {
    last_operand(chpx, flag).is_some_and(is_on)
}

/// The `sprmCPicLocation` Data-stream offset in a CHPX, if any: where an
/// inline picture's PICF + OfficeArt data begins.
pub(super) fn chpx_pic_location(grpprl: &[u8]) -> Option<u32> {
    last_operand(grpprl, SPRM_C_PIC_LOCATION).and_then(|operand| get_u32(operand, 0))
}

fn vertical_position(operand: &[u8]) -> Option<VerticalPosition> {
    match operand.first() {
        Some(0) => Some(VerticalPosition::Baseline),
        Some(1) => Some(VerticalPosition::Superscript),
        Some(2) => Some(VerticalPosition::Subscript),
        _ => None,
    }
}

/// The character properties of a run: `paragraph`'s, then `grpprls` (CHPX, then the piece's Prm)
/// applied in order, their bytes charged first. A `sprmCIstd` resets all but a deletion and the
/// [`Direct`] properties to `paragraph` with its character style over it, `sprmCPlain` to
/// `paragraph` alone, and later toggles resolve against that. The flag reports a `sprmCIstd` that
/// names no character style.
pub(super) fn direct_chp(
    parse_ctx: &ParseCtx<'_>,
    paragraph: Chp,
    grpprls: [&[u8]; 2],
    character_style: impl Fn(u16) -> Option<CharOps>,
) -> Result<(Chp, bool), ConvertError> {
    parse_ctx.charge(grpprls.iter().map(|grpprl| grpprl.len() as u64).sum())?;
    let (mut base, mut chp, mut unresolved) = (paragraph, paragraph, false);
    for (sprm, operand) in grpprls.into_iter().flat_map(sprms) {
        match sprm {
            SPRM_C_ISTD => {
                if let Some(istd) = get_u16(operand, 0) {
                    let ops = character_style(istd);
                    unresolved |= ops.is_none();
                    base = ops.unwrap_or_default().apply(paragraph);
                    chp = base.keeping(chp);
                }
            }
            SPRM_C_PLAIN => (base, chp) = (paragraph, paragraph.keeping(chp)),
            SPRM_C_SYMBOL => {
                if let (Some(ftc), Some(xchar)) = (get_u16(operand, 0), get_u16(operand, 2)) {
                    chp.direct.symbol = Some(Symbol { ftc, xchar });
                }
            }
            SPRM_C_IDCT_HINT => {
                chp.direct.hint = operand.first().copied().unwrap_or(chp.direct.hint);
            }
            SPRM_CF_BIDI => toggle(operand, false, &mut chp.direct.bidi),
            SPRM_CF_COMPLEX_SCRIPTS => toggle(operand, false, &mut chp.direct.complex),
            SPRM_C_ISS => {
                chp.style.position = vertical_position(operand).unwrap_or(chp.style.position);
            }
            SPRM_CF_RMARK_DEL => toggle(operand, base.deleted, &mut chp.deleted),
            SPRM_CF_VANISH => toggle(operand, base.hidden, &mut chp.hidden),
            SPRM_CF_BOLD => toggle(operand, base.style.bold, &mut chp.style.bold),
            SPRM_CF_ITALIC => toggle(operand, base.style.italic, &mut chp.style.italic),
            SPRM_CF_STRIKE => toggle(operand, base.style.strike, &mut chp.style.strike),
            _ => {
                if let (Some(slot), Some(ftc)) = (chp.fonts.slot_mut(sprm), get_u16(operand, 0)) {
                    *slot = ftc;
                }
            }
        }
    }
    Ok((chp, unresolved))
}

fn toggle(operand: &[u8], below: bool, value: &mut bool) {
    if let Some(op) = BoolOp::of(operand) {
        *value = op.apply(below);
    }
}

/// A row's table properties from `sprmTDefTable` and its companion table
/// sprms in the row-end PAPX (TAPX).
#[derive(Debug, Clone, Default)]
pub(super) struct Tap {
    /// Cell boundary positions in twips (`rgdxaCenter`), one more entry
    /// than the cell count.
    pub boundaries: Vec<i16>,
    pub cells: Vec<TapCell>,
    /// `sprmTTableHeader`: the row repeats as a header row.
    pub header: bool,
}

/// Merge flags of one cell (TC80 `tcgrf` / `sprmTVertMerge`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TapCell {
    pub horz_first: bool,
    pub horz_cont: bool,
    pub vert_restart: bool,
    pub vert_cont: bool,
}

/// Paragraph properties a PAPX (or style PAPX) contributes.
#[derive(Debug, Clone, Default)]
pub(super) struct PapDelta {
    pub in_table: Option<bool>,
    pub ttp: Option<bool>,
    pub outline: Outline,
    pub ilfo: Option<u16>,
    pub ilvl: Option<u8>,
    /// `sprmPItap` table depth (1 = a regular table).
    pub itap: Option<i32>,
    /// `sprmPFInnerTableCell` / `sprmPFInnerTtp`: nested-table cell/row
    /// terminators.
    pub inner_cell: Option<bool>,
    pub inner_ttp: Option<bool>,
    /// Row properties, present on TTP marks.
    pub tap: Option<Tap>,
}

impl PapDelta {
    /// The list override (`ilfo`) and level a paragraph is numbered by, unless
    /// it has none or suppresses it.
    pub(super) fn numbering(&self) -> Option<(u16, usize)> {
        let ilfo = self.ilfo.unwrap_or(0);
        (ilfo != 0 && ilfo != ILFO_SUPPRESSED)
            .then_some((ilfo, usize::from(self.ilvl.unwrap_or(0))))
    }

    pub(super) fn merge(self, over: Self) -> Self {
        Self {
            in_table: over.in_table.or(self.in_table),
            ttp: over.ttp.or(self.ttp),
            outline: over.outline.or(self.outline),
            ilfo: over.ilfo.or(self.ilfo),
            ilvl: over.ilvl.or(self.ilvl),
            itap: over.itap.or(self.itap),
            inner_cell: over.inner_cell.or(self.inner_cell),
            inner_ttp: over.inner_ttp.or(self.inner_ttp),
            tap: over.tap.or(self.tap),
        }
    }
}

/// The grpprl `sprmPHugePapx` names in the Data stream, when it lies inside.
fn huge_papx<'d>(operand: &[u8], data: &'d [u8]) -> Option<&'d [u8]> {
    let off = get_u32(operand, 0)? as usize;
    let cb = get_u16(data, off)? as usize;
    data.get(off..)?.get(2..)?.get(..cb)
}

pub(super) fn apply_pap_sprms(
    parse_ctx: &ParseCtx<'_>,
    grpprl: &[u8],
    data: &[u8],
    delta: &mut PapDelta,
) -> Result<(), ConvertError> {
    for (sprm, operand) in sprms(grpprl) {
        match sprm {
            SPRM_PF_IN_TABLE => delta.in_table = Some(is_on(operand)),
            SPRM_PF_TTP => delta.ttp = Some(is_on(operand)),
            SPRM_P_HUGE_PAPX => {
                if let Some(huge) = huge_papx(operand, data) {
                    parse_ctx.charge(huge.len() as u64)?;
                    apply_pap_sprms(parse_ctx, huge, &[], delta)?;
                }
            }
            SPRM_P_OUT_LVL => {
                if let Some(&v) = operand.first() {
                    delta.outline = Outline::from_operand(v);
                }
            }
            SPRM_P_ILVL => delta.ilvl = operand.first().copied(),
            SPRM_P_ILFO => delta.ilfo = get_u16(operand, 0),
            SPRM_P_ITAP => delta.itap = get_u32(operand, 0).map(u32::cast_signed),
            SPRM_P_DTAP => {
                if let Some(d) = get_u32(operand, 0).map(u32::cast_signed) {
                    delta.itap = Some(delta.itap.unwrap_or(0).saturating_add(d));
                }
            }
            SPRM_PF_INNER_TABLE_CELL => delta.inner_cell = Some(is_on(operand)),
            SPRM_PF_INNER_TTP => delta.inner_ttp = Some(is_on(operand)),
            SPRM_T_DEF_TABLE => {
                if let Some(tap) = parse_tdef_table(operand) {
                    let header = delta.tap.as_ref().is_some_and(|t| t.header);
                    delta.tap = Some(Tap { header, ..tap });
                }
            }
            SPRM_T_TABLE_HEADER => {
                let on = is_on(operand);
                match &mut delta.tap {
                    Some(tap) => tap.header = on,
                    None if on => delta.tap = Some(Tap { header: true, ..Default::default() }),
                    None => {}
                }
            }
            SPRM_T_VERT_MERGE => {
                if let (Some(&itc), Some(&flag)) = (operand.get(1), operand.get(2))
                    && let Some(tap) = &mut delta.tap
                    && let Some(cell) = tap.cells.get_mut(itc as usize)
                {
                    cell.vert_cont = flag == 0x01;
                    cell.vert_restart = flag == 0x03;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Parse a `TDefTableOperand`: cb, NumberOfColumns, `rgdxaCenter`
/// boundaries, then TC80 records (which may be fewer than the columns).
fn parse_tdef_table(operand: &[u8]) -> Option<Tap> {
    const TC80_SIZE: usize = 20;
    let columns = *operand.get(2)? as usize;
    if columns > 63 {
        return None;
    }
    let boundaries = (0..=columns)
        .map(|i| get_u16(operand, 3 + i * 2).map(u16::cast_signed))
        .collect::<Option<Vec<_>>>()?;
    let mut cells = vec![TapCell::default(); columns];
    let tc_base = 3 + (columns + 1) * 2;
    for (i, cell) in cells.iter_mut().enumerate() {
        let Some(tcgrf) = get_u16(operand, tc_base + i * TC80_SIZE) else {
            break; // fewer TC80s than columns: defaults apply
        };
        // TCGRF: horzMerge (bits 0-1), textFlow (2-4), vertMerge (5-6).
        let horz = tcgrf & 0x3;
        cell.horz_cont = horz == 1;
        cell.horz_first = horz >= 2;
        let vert = (tcgrf >> 5) & 0x3;
        cell.vert_cont = vert == 1;
        cell.vert_restart = vert == 3;
    }
    Some(Tap { boundaries, cells, header: false })
}

// [MS-DOC] 2.9.336 UpxChpx disallows these modifiers and those sprmCIstd preserves (2.6.1).
const FORBIDDEN_IN_STYLE: [u16; 21] = [
    SPRM_CF_RMARK_DEL,
    SPRM_CF_RMARK_INS,
    SPRM_CF_FLD_VANISH,
    SPRM_CF_DATA,
    SPRM_CF_OLE2,
    SPRM_CF_SPEC_VANISH,
    SPRM_CF_SPEC,
    SPRM_C_ISTD,
    SPRM_C_ISTD_PERMUTE,
    SPRM_C_PLAIN,
    SPRM_C_MAJORITY,
    SPRM_C_DISP_FLD_RMARK,
    SPRM_C_IDSL_RMARK_DEL,
    SPRM_C_LBC_CRJ,
    SPRM_C_PBI_IBULLET,
    SPRM_C_PBI_GRF,
    SPRM_C_PIC_LOCATION,
    SPRM_C_SYMBOL,
    SPRM_C_IDCT_HINT,
    SPRM_CF_BIDI,
    SPRM_CF_COMPLEX_SCRIPTS,
];

/// A style's CHPX grpprl as one layer, and whether it held a sprm a style may not. Within the
/// layer the last valid operand of a property decides; toggles resolve against the layer below.
pub(super) fn compile_style_chpx(grpprl: &[u8]) -> (CharOps, bool) {
    let mut ops = CharOps::default();
    let mut forbidden = false;
    for (sprm, operand) in sprms(grpprl) {
        match sprm {
            SPRM_C_ISS => ops.position = vertical_position(operand).or(ops.position),
            SPRM_CF_VANISH => ops.hidden = BoolOp::of(operand).unwrap_or(ops.hidden),
            SPRM_CF_BOLD => ops.bold = BoolOp::of(operand).unwrap_or(ops.bold),
            SPRM_CF_ITALIC => ops.italic = BoolOp::of(operand).unwrap_or(ops.italic),
            SPRM_CF_STRIKE => ops.strike = BoolOp::of(operand).unwrap_or(ops.strike),
            _ => match ops.fonts.slot_mut(sprm) {
                Some(slot) => *slot = get_u16(operand, 0).or(*slot),
                None => forbidden |= FORBIDDEN_IN_STYLE.contains(&sprm),
            },
        }
    }
    (ops, forbidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::test_support::cancelled;

    const ALL: [BoolOp; 4] = [BoolOp::Identity, BoolOp::Invert, BoolOp::SetFalse, BoolOp::SetTrue];

    #[test]
    fn a_toggle_layer_composes_child_after_parent_in_every_pairing() {
        use BoolOp::{Identity as I, Invert as N, SetFalse as F, SetTrue as T};
        let composed = [[I, N, F, T], [N, I, F, T], [F, T, F, T], [T, F, F, T]];
        for (parent, row) in ALL.into_iter().zip(composed) {
            for (child, expected) in ALL.into_iter().zip(row) {
                assert_eq!(parent.then(child), expected, "{parent:?} then {child:?}");
                for value in [false, true] {
                    let sequential = child.apply(parent.apply(value));
                    assert_eq!(expected.apply(value), sequential, "{parent:?} {child:?} {value}");
                }
            }
        }
    }

    #[test]
    fn a_toggle_operand_is_one_of_four_operations() {
        use BoolOp::{Identity, Invert, SetFalse, SetTrue};
        let ops: Vec<_> = [0, 1, 0x80, 0x81, 2, 0x7F, 0xFF].map(|v| BoolOp::of(&[v])).to_vec();
        let expected =
            [Some(SetFalse), Some(SetTrue), Some(Identity), Some(Invert), None, None, None];
        assert_eq!(ops, expected);
        assert_eq!(BoolOp::of(&[]), None);
    }

    type Property = fn(Chp) -> bool;

    /// The toggle sprms by their low byte, each with the property it sets.
    const TOGGLES: [(u8, Property); 4] = [
        (0x35, |chp| chp.style.bold),
        (0x36, |chp| chp.style.italic),
        (0x37, |chp| chp.style.strike),
        (0x3C, |chp| chp.hidden),
    ];

    fn layer(grpprl: &[u8]) -> CharOps {
        let (ops, forbidden) = compile_style_chpx(grpprl);
        assert!(!forbidden);
        ops
    }

    #[test]
    fn within_a_layer_the_last_valid_operand_decides_and_a_repeat_inverts_once() {
        let bold = |grpprl: &[u8]| layer(grpprl).apply(Chp::default()).style.bold;
        let bold_over = |grpprl: &[u8], value| {
            let over = Chp { style: Style { bold: value, ..Style::PLAIN }, ..Chp::default() };
            layer(grpprl).apply(over).style.bold
        };
        assert!(bold(&[0x35, 0x08, 1]));
        assert!(!bold(&[0x35, 0x08, 1, 0x35, 0x08, 0]));
        assert!(bold(&[0x35, 0x08, 0, 0x35, 0x08, 1]));
        for invalid in [2, 0x7F, 0xFF] {
            assert!(bold(&[0x35, 0x08, 1, 0x35, 0x08, invalid]), "{invalid:#x} after on");
            assert!(bold_over(&[0x35, 0x08, 0x81, 0x35, 0x08, invalid], false), "{invalid:#x}");
        }
        assert!(bold_over(&[0x35, 0x08, 0x81, 0x35, 0x08, 0x81], false));
        assert!(!bold_over(&[0x35, 0x08, 0x81, 0x35, 0x08, 0x81], true));
        let inherited = layer(&[0x35, 0x08, 0x81]);
        assert_eq!(inherited.then(inherited).apply(Chp::default()), Chp::default());
        for (sprm, set) in TOGGLES {
            for invalid in [2, 0x7F] {
                let on = layer(&[sprm, 0x08, 1, sprm, 0x08, invalid]).apply(Chp::default());
                assert!(set(on), "{sprm:#x}: {invalid:#x} after on");
                let off = layer(&[sprm, 0x08, 1, sprm, 0x08, 0, sprm, 0x08, invalid]);
                assert!(!set(off.apply(Chp::default())), "{sprm:#x}: {invalid:#x} after off");
            }
        }
    }

    #[test]
    fn layers_compose_every_property_independently_and_position_is_unset_until_named() {
        use crate::model::VerticalPosition::{Baseline, Subscript, Superscript};
        let parent = layer(&[0x35, 0x08, 1, 0x3C, 0x08, 1, 0x48, 0x2A, 1]);
        let child = layer(&[0x36, 0x08, 1, 0x37, 0x08, 1, 0x3C, 0x08, 0x81]);
        let both = parent.then(child).apply(Chp::default());
        assert_eq!(
            both.style,
            Style { bold: true, italic: true, strike: true, position: Superscript, ..Style::PLAIN }
        );
        assert!(!both.hidden);
        for (operand, position) in [(0, Baseline), (2, Subscript)] {
            let named = layer(&[0x48, 0x2A, operand]);
            assert_eq!(parent.then(named).apply(Chp::default()).style.position, position);
            assert_eq!(named.then(layer(&[])).apply(Chp::default()).style.position, position);
        }
        assert_eq!(layer(&[0x48, 0x2A, 0x81]), CharOps::default());
        assert_eq!(
            layer(&[0x48, 0x2A, 2, 0x48, 0x2A, 6]).then(CharOps::default()),
            layer(&[0x48, 0x2A, 2])
        );
        let last = layer(&[0x48, 0x2A, 1, 0x48, 0x2A, 2]).apply(Chp::default());
        assert_eq!(last.style.position, Subscript);
    }

    #[test]
    fn a_style_layer_reports_the_forbidden_sprms_and_keeps_the_rest() {
        let (ops, forbidden) = compile_style_chpx(&[0x35, 0x08, 1, 0x00, 0x08, 1, 0x36, 0x08, 1]);
        assert!(forbidden);
        assert_eq!(ops, layer(&[0x35, 0x08, 1, 0x36, 0x08, 1]));
        assert!(!compile_style_chpx(&[0x35, 0x08, 1, 0x48, 0x2A, 1, 0x0B, 0x46, 2, 0]).1);
    }

    fn direct(grpprls: [&[u8]; 2], styles: &[(u16, CharOps)], paragraph: Chp) -> (Chp, bool) {
        direct_chp(&ParseCtx::default(), paragraph, grpprls, |istd| {
            styles.iter().find(|&&(at, _)| at == istd).map(|&(_, ops)| ops)
        })
        .unwrap()
    }

    #[test]
    fn a_direct_toggle_resolves_against_the_properties_the_last_style_reference_left() {
        let bold_paragraph = Chp { style: Style { bold: true, ..Style::PLAIN }, ..Chp::default() };
        let styles = [(7, layer(&[0x36, 0x08, 1])), (8, layer(&[0x35, 0x08, 0x81]))];
        let after = |chpx: &[u8]| direct([chpx, &[]], &styles, bold_paragraph).0.style;
        let (bold, italic) = (|style: Style| style.bold, |style: Style| style.italic);
        assert!(bold(after(&[0x35, 0x08, 0x80])));
        assert!(!bold(after(&[0x35, 0x08, 0x81])));
        assert!(!bold(after(&[0x35, 0x08, 0, 0x30, 0x4A, 8, 0])));
        assert!(
            bold(after(&[0x35, 0x08, 0, 0x30, 0x4A, 7, 0])) && italic(after(&[0x30, 0x4A, 7, 0]))
        );
        assert!(!italic(after(&[0x36, 0x08, 1, 0x30, 0x4A, 8, 0])));
        let restyle = [0x30, 0x4A, 1, 0];
        for (sprm, set) in TOGGLES {
            let styles = [(1, layer(&[sprm, 0x08, 1]))];
            let after = |chpx: &[u8], prm: &[u8]| direct([chpx, prm], &styles, Chp::default()).0;
            let restyled = after(&restyle, &[]);
            let others = TOGGLES.iter().filter(|&&(other, _)| other != sprm);
            assert!(set(restyled), "{sprm:#x}");
            assert!(!others.clone().any(|&(_, other)| other(restyled)), "{sprm:#x}");
            for (operand, shown) in [(0x80, true), (0x81, false)] {
                let toggle = &[sprm, 0x08, operand][..];
                let direct_off = &[sprm, 0x08, 0][..];
                let none = &[][..];
                let placements = [
                    ("in the run", [&restyle[..], toggle].concat(), none),
                    ("in the piece", restyle.to_vec(), toggle),
                    (
                        "after direct formatting in the run",
                        [&restyle[..], direct_off, toggle].concat(),
                        none,
                    ),
                    (
                        "after direct formatting in the piece",
                        [&restyle[..], direct_off].concat(),
                        toggle,
                    ),
                ];
                for (place, chpx, prm) in placements {
                    assert_eq!(set(after(&chpx, prm)), shown, "{sprm:#x} {operand:#x} {place}");
                }
            }
        }
    }

    #[test]
    fn a_style_reference_resets_all_but_a_deletion_and_a_second_grpprl_continues_the_first() {
        let styles = [(1, layer(&[0x36, 0x08, 1]))];
        let paragraph = Chp::default();
        let deleted_hidden_bold = [0x00, 0x08, 1, 0x3C, 0x08, 1, 0x35, 0x08, 1];
        let (chp, unresolved) =
            direct([&deleted_hidden_bold, &[0x30, 0x4A, 1, 0]], &styles, paragraph);
        assert_eq!(
            (chp.deleted, chp.hidden, chp.style.bold, chp.style.italic),
            (true, false, false, true)
        );
        assert!(!unresolved);
        let (chp, unresolved) = direct([&[0x30, 0x4A, 1, 0], &[0x35, 0x08, 1]], &styles, paragraph);
        assert_eq!((chp.style.bold, chp.style.italic, unresolved), (true, true, false));
        let (chp, unresolved) = direct([&[0x35, 0x08, 1], &[0x30, 0x4A, 9, 0]], &styles, paragraph);
        assert_eq!((chp, unresolved), (paragraph, true));
        let missing_then_found: [[&[u8]; 2]; 2] = [
            [&[0x30, 0x4A, 9, 0, 0x30, 0x4A, 1, 0], &[]],
            [&[0x30, 0x4A, 9, 0], &[0x30, 0x4A, 1, 0]],
        ];
        for grpprls in missing_then_found {
            let (chp, unresolved) = direct(grpprls, &styles, paragraph);
            assert_eq!((chp.style.italic, unresolved), (true, true), "{grpprls:?}");
        }
    }

    #[test]
    fn a_direct_replay_is_charged_its_bytes_before_any_sprm_is_walked() {
        use crate::ledger::CancelToken;
        use crate::limits::{Limit, Limits};
        use std::assert_matches;
        let lookups = std::cell::Cell::new(0);
        let replay = |parse_ctx: &ParseCtx<'_>| {
            let grpprls: [&[u8]; 2] = [&[0x30, 0x4A, 1, 0], &[0x30, 0x4A, 1, 0, 0x35, 0x08, 1]];
            direct_chp(parse_ctx, Chp::default(), grpprls, |_| {
                lookups.set(lookups.get() + 1);
                None
            })
        };
        let parse_ctx = ParseCtx::default();
        assert!(replay(&parse_ctx).unwrap().1);
        assert_eq!((parse_ctx.fuel(), lookups.replace(0)), (11, 2));
        let limits = Limits { max_work: 10, ..Limits::DEFAULT };
        let refused = replay(&ParseCtx::new(&limits));
        assert_matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 10 }));
        let token = CancelToken::new();
        let cancelled = replay(&cancelled(&token));
        assert_matches!(cancelled, Err(ConvertError::Cancelled));
        assert_eq!(lookups.get(), 0);
    }

    #[test]
    fn vertical_position_is_set_by_the_last_valid_operand_and_reset_by_a_style_reference() {
        use crate::model::VerticalPosition::{Baseline, Subscript, Superscript};
        let raised =
            Chp { style: Style { position: Superscript, ..Style::PLAIN }, ..Chp::default() };
        let styles = [(1, layer(&[]))];
        for (operand, position) in [(0, Baseline), (1, Superscript), (2, Subscript)] {
            let chpx = [0x48, 0x2A, operand];
            assert_eq!(direct([&chpx, &[]], &styles, Chp::default()).0.style.position, position);
            assert_eq!(direct([&[], &chpx], &styles, Chp::default()).0.style.position, position);
        }
        for chpx in [&[][..], &[0x48, 0x2A], &[0x48, 0x2A, 0x81], &[0x45, 0x48, 6, 0]] {
            assert_eq!(direct([chpx, &[]], &styles, raised).0, raised);
        }
        let reset = direct([&[0x48, 0x2A, 2, 0x30, 0x4A, 1, 0], &[]], &styles, raised).0;
        assert_eq!(reset, raised);
    }

    fn walked(grpprl: &[u8]) -> Vec<(u16, Vec<u8>)> {
        sprms(grpprl).map(|(sprm, operand)| (sprm, operand.to_vec())).collect()
    }

    #[test]
    fn a_walk_stops_at_the_first_sprm_whose_operand_does_not_fit() {
        assert_eq!(walked(&[0x35, 0x08, 1, 0x36, 0x08]), [(0x0835, vec![1])]);
        assert_eq!(walked(&[0x35, 0x08, 1, 0x0B, 0x46, 2]), [(0x0835, vec![1])]);
        assert_eq!(walked(&[0x35]), []);
        assert_eq!(walked(&[0x0B, 0x46, 2, 0]), [(0x460B, vec![2, 0])]);
    }

    #[test]
    fn the_last_occurrence_of_a_sprm_decides_a_flag_and_a_picture_location() {
        let ole2 = |chpx: &[u8]| sets_flag(chpx, SPRM_CF_OLE2);
        assert!(ole2(&[0x0A, 0x08, 0, 0x0A, 0x08, 1]));
        assert!(!ole2(&[0x0A, 0x08, 1, 0x0A, 0x08, 0]));
        assert!(ole2(&[0x0A, 0x08, 0x80]));
        assert!(!ole2(&[0x35, 0x08, 1]));
        let location = |at: u8| [0x03, 0x6A, at, 0, 0, 0];
        assert_eq!(chpx_pic_location(&[&location(1)[..], &location(2)].concat()), Some(2));
        assert_eq!(chpx_pic_location(&[&location(1)[..], &[0x35, 0x08, 1]].concat()), Some(1));
        assert_eq!(chpx_pic_location(&[0x35, 0x08, 1]), None);
        assert_eq!(chpx_pic_location(&[0x03, 0x6A, 1, 0]), None);
    }

    #[test]
    fn a_table_header_flag_makes_the_row_properties_only_when_set() {
        let header_after = |grpprl: &[u8], tap: Option<Tap>| {
            let mut delta = PapDelta { tap, ..PapDelta::default() };
            apply_pap_sprms(&ParseCtx::default(), grpprl, &[], &mut delta).unwrap();
            delta.tap.map(|tap| tap.header)
        };
        let header_row = Some(Tap { header: true, ..Tap::default() });
        assert_eq!(header_after(&[0x04, 0x34, 1], None), Some(true));
        assert_eq!(header_after(&[0x04, 0x34, 0], None), None);
        assert_eq!(header_after(&[0x04, 0x34, 1], Some(Tap::default())), Some(true));
        assert_eq!(header_after(&[0x04, 0x34, 0], header_row), Some(false));
    }

    #[test]
    fn a_table_definition_takes_its_two_byte_length_and_one_more() {
        assert_eq!(walked(&[0x08, 0xD6, 5]), [(0xD608, vec![])]);
        assert_eq!(walked(&[0x08, 0xD6, 2, 0, 9, 7]), [(0xD608, vec![2, 0, 9])]);
        assert_eq!(walked(&[0x08, 0xD6, 4, 0, 9, 7]), []);
        assert_eq!(walked(&[0x2B, 0xD6, 2, 7, 8]), [(0xD62B, vec![2, 7, 8])]);
    }

    #[test]
    fn a_paragraph_is_numbered_unless_its_list_is_absent_or_suppressed() {
        let numbering = |ilfo: Option<u16>, ilvl: Option<u8>| {
            PapDelta { ilfo, ilvl, ..PapDelta::default() }.numbering()
        };
        for (case, ilfo, ilvl, expected) in [
            ("no ilfo", None, Some(2), None),
            ("ilfo zero", Some(0), Some(2), None),
            ("suppressed", Some(ILFO_SUPPRESSED), Some(2), None),
            ("level defaults to zero", Some(3), None, Some((3, 0))),
            ("named level", Some(3), Some(2), Some((3, 2))),
            ("one below suppressed", Some(ILFO_SUPPRESSED - 1), Some(15), Some((0xF800, 15))),
        ] {
            assert_eq!(numbering(ilfo, ilvl), expected, "{case}");
        }
    }
}
