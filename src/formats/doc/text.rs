//! Text reading: CPs count UTF-16 units, scalars decode in place from the
//! pieces of one story as a charged cursor walks them.

use super::pieces::{Pcd, PieceTable, Prm};
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::shared::binary::get_u16;
use std::ops::Range;

pub(super) struct TextView<'w> {
    word_doc: &'w [u8],
    pieces: PieceTable<'w>,
}

/// One decoded scalar: its first CP, the file offset and piece of its leading unit, and
/// whether it stands in for units that did not decode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Scalar {
    pub(super) c: char,
    pub(super) cp: usize,
    pub(super) fc: u32,
    pub(super) piece: u32,
    pub(super) replaced: bool,
}

/// CPs of a piece whose bytes the stream does not hold.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Gap {
    pub(super) cps: Range<usize>,
    pub(super) piece: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Event {
    Scalar(Scalar),
    Gap(Gap),
}

/// A piece the cursor stands in: its geometry and its bytes, when the stream holds them.
#[derive(Clone, Copy)]
struct Placed<'w> {
    index: usize,
    pcd: Pcd,
    bytes: Option<&'w [u8]>,
}

/// Units charged at once while decoding.
const CHUNK: u64 = 1 << 16;

/// [MS-DOC] 2.9.73 `FcCompressed`: the characters of bytes 0x80-0x9F; every other byte is its own.
#[rustfmt::skip]
const COMPRESSED_80_9F: [char; 32] = [
    '\u{80}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{8e}', '\u{8f}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{9e}', '\u{178}',
];

fn compressed_char(byte: u8) -> char {
    COMPRESSED_80_9F.get(usize::from(byte.wrapping_sub(0x80))).copied().unwrap_or(char::from(byte))
}

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

fn units(range: &Range<usize>) -> u64 {
    u64::try_from(range.end.saturating_sub(range.start)).unwrap_or(u64::MAX)
}

impl<'w> TextView<'w> {
    pub(super) fn new(word_doc: &'w [u8], pieces: PieceTable<'w>) -> Self {
        Self { word_doc, pieces }
    }

    fn placed(&self, index: usize) -> Placed<'w> {
        let pcd = self.pieces.pcd(index);
        Placed { index, pcd, bytes: pcd.bytes().and_then(|range| self.word_doc.get(range)) }
    }

    pub(super) fn pcd(&self, index: u32) -> Pcd {
        self.pieces.pcd(index as usize)
    }

    pub(super) fn prm(&self, index: u32) -> Prm<'w> {
        self.pieces.prm(index as usize)
    }

    /// Refuse `range` before any of it decodes when the CPs its readable pieces hold exceed
    /// the work left, charging the seek and each piece it looks at.
    pub(super) fn preflight(
        &self,
        parse_ctx: &ParseCtx<'_>,
        range: &Range<usize>,
    ) -> Result<(), ConvertError> {
        if range.is_empty() {
            return Ok(());
        }
        parse_ctx.charge(1)?;
        let mut readable = 0u64;
        for index in self.pieces.find(range.start)..self.pieces.len() {
            parse_ctx.charge(1)?;
            let piece = self.placed(index);
            let cps = piece.pcd.cp_range();
            if piece.bytes.is_some() {
                readable = readable
                    .saturating_add(units(&(cps.start.max(range.start)..cps.end.min(range.end))));
            }
            if cps.end >= range.end {
                break;
            }
        }
        if readable > parse_ctx.work_left() {
            return Err(ConvertError::limit(Limit::Work, parse_ctx.limits().max_work));
        }
        Ok(())
    }

    /// A cursor over the scalars of `range` in `story`: the scalars whose first CP lies in
    /// it, a surrogate pair decoding whole across pieces but never across the story's
    /// bounds. The seek is charged.
    pub(super) fn cursor<'v>(
        &'v self,
        parse_ctx: &ParseCtx<'_>,
        story: &Range<usize>,
        range: &Range<usize>,
    ) -> Result<Cursor<'v, 'w>, ConvertError> {
        let start = range.start.max(story.start);
        let end = range.end.min(story.end);
        let mut cursor = Cursor {
            view: self,
            story_end: story.end,
            end,
            cp: start,
            piece: None,
            paid: 0,
            chunk: CHUNK,
        };
        if start >= end {
            cursor.cp = end;
            return Ok(cursor);
        }
        parse_ctx.charge(1)?;
        cursor.piece = Some(self.placed(self.pieces.find(start)));
        if start > story.start && cursor.unit(parse_ctx, start)?.is_some_and(is_low) {
            parse_ctx.charge(1)?;
            let before = self.placed(self.pieces.find(start - 1));
            if unit_of(&before, start - 1).is_some_and(is_high) {
                cursor.cp = start + 1;
            }
        }
        Ok(cursor)
    }
}

/// The UTF-16 unit at `cp` of `piece`, when it holds one and its bytes are present.
fn unit_of(piece: &Placed<'_>, cp: usize) -> Option<u16> {
    let bytes = piece.bytes.filter(|_| !piece.pcd.compressed)?;
    get_u16(bytes, cp.checked_sub(piece.pcd.cps.0)? * 2)
}

/// A charged walk over the scalars of one range of a story.
#[derive(Clone)]
pub(super) struct Cursor<'v, 'w> {
    view: &'v TextView<'w>,
    story_end: usize,
    end: usize,
    cp: usize,
    piece: Option<Placed<'w>>,
    /// Units charged and not yet read.
    paid: u64,
    /// The most units charged at once.
    chunk: u64,
}

impl Cursor<'_, '_> {
    /// A cursor from here that charges again each unit it reads, one at a time, since a
    /// lookahead may stop at any unit.
    pub(super) fn fork(&self) -> Self {
        Self { paid: 0, chunk: 1, ..self.clone() }
    }

    /// Charge reading one more unit, a chunk of the units up to `limit` at a time.
    fn pay(
        &mut self,
        parse_ctx: &ParseCtx<'_>,
        at: usize,
        limit: usize,
    ) -> Result<(), ConvertError> {
        if self.paid == 0 {
            let ahead = units(&(at..limit)).clamp(1, self.chunk);
            parse_ctx.charge(ahead)?;
            self.paid = ahead;
        }
        self.paid -= 1;
        Ok(())
    }

    /// The unit at `cp` of the current piece, charged as read.
    fn unit(&mut self, parse_ctx: &ParseCtx<'_>, cp: usize) -> Result<Option<u16>, ConvertError> {
        let Some(piece) = self.piece else {
            return Ok(None);
        };
        let unit = unit_of(&piece, cp);
        if unit.is_some() {
            self.pay(parse_ctx, cp, piece.pcd.cps.1.min(self.end))?;
        }
        Ok(unit)
    }

    pub(super) fn next(&mut self, parse_ctx: &ParseCtx<'_>) -> Result<Option<Event>, ConvertError> {
        if self.cp >= self.end {
            return Ok(None);
        }
        let mut piece = match self.piece {
            Some(piece) if self.cp < piece.pcd.cps.1 => piece,
            current => {
                parse_ctx.charge(1)?;
                let next = self.view.placed(current.map_or(0, |piece| piece.index + 1));
                self.piece = Some(next);
                next
            }
        };
        let index = u32::try_from(piece.index).unwrap_or(u32::MAX);
        let cp = self.cp;
        let at = cp - piece.pcd.cps.0;
        let fc = u32::try_from(piece.pcd.fc + at * piece.pcd.width()).unwrap_or(u32::MAX);
        let Some(bytes) = piece.bytes else {
            let cps = cp..piece.pcd.cps.1.min(self.end);
            self.cp = cps.end;
            return Ok(Some(Event::Gap(Gap { cps, piece: index })));
        };
        let scalar = |c: char, replaced| Scalar { c, cp, fc, piece: index, replaced };
        if piece.pcd.compressed {
            self.pay(parse_ctx, cp, piece.pcd.cps.1.min(self.end))?;
            self.cp += 1;
            let byte = bytes.get(at).copied().unwrap_or_default();
            return Ok(Some(Event::Scalar(scalar(compressed_char(byte), false))));
        }
        let Some(unit) = self.unit(parse_ctx, cp)? else {
            self.cp += 1;
            return Ok(Some(Event::Scalar(scalar(char::REPLACEMENT_CHARACTER, true))));
        };
        let mut trailing = None;
        if is_high(unit) && cp + 1 < self.story_end {
            if cp + 1 >= piece.pcd.cps.1 {
                parse_ctx.charge(1)?;
                piece = self.view.placed(piece.index + 1);
                self.piece = Some(piece);
            }
            trailing = self.unit(parse_ctx, cp + 1)?.filter(|&low| is_low(low));
        }
        let decoded = match trailing {
            Some(low) => char::decode_utf16([unit, low]).next().and_then(Result::ok),
            None => char::from_u32(u32::from(unit)),
        };
        self.cp += 1 + usize::from(trailing.is_some());
        let c = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
        Ok(Some(Event::Scalar(scalar(c, decoded.is_none()))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::doc::pieces::Coverage;
    use crate::formats::doc::tests::utf16;
    use crate::ledger::CancelToken;
    use crate::limits::Limits;
    use crate::shared::binary::get_u32;

    const MAPPED_BYTES: [u8; 24] = [
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x8B, 0x8C, 0x91, 0x92, 0x93, 0x94,
        0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9F,
    ];

    const MAPPED_CHARS: &str = "\u{201A}\u{0192}\u{201E}\u{2026}\u{2020}\u{2021}\u{02C6}\u{2030}\
        \u{0160}\u{2039}\u{0152}\u{2018}\u{2019}\u{201C}\u{201D}\u{2022}\
        \u{2013}\u{2014}\u{02DC}\u{2122}\u{0161}\u{203A}\u{0153}\u{0178}";

    /// A Clx of pieces `(cp_end, fc, compressed)`.
    fn clx(pieces: &[(usize, usize, bool)]) -> Vec<u8> {
        let to_u32 = |n: usize| u32::try_from(n).unwrap();
        let cps: Vec<u32> = [0].into_iter().chain(pieces.iter().map(|p| to_u32(p.0))).collect();
        let pcds: Vec<(u32, u16)> = pieces
            .iter()
            .map(|&(_, fc, compressed)| {
                let fc = to_u32(fc);
                (if compressed { (fc * 2) | 0x4000_0000 } else { fc }, 0)
            })
            .collect();
        crate::formats::doc::tests::clx(&[], &cps, &pcds)
    }

    /// The text of `word_doc` through the pieces, `cbMac` past the stream.
    fn view<'w>(word_doc: &'w [u8], table: &'w [u8]) -> TextView<'w> {
        let plc = &table[5..];
        let terminal_cp = get_u32(plc, (plc.len() - 4) / 12 * 4).unwrap() as usize;
        let coverage = Coverage { terminal_cp, cb_mac: u32::MAX as usize };
        let pieces = PieceTable::parse(&ParseCtx::default(), table, 0, table.len(), coverage);
        TextView::new(word_doc, pieces.unwrap())
    }

    /// Every event of `range` in `story`.
    fn walk(
        parse_ctx: &ParseCtx<'_>,
        text: &TextView<'_>,
        story: Range<usize>,
        range: Range<usize>,
    ) -> Result<Vec<Event>, ConvertError> {
        let mut cursor = text.cursor(parse_ctx, &story, &range)?;
        let mut events = Vec::new();
        while let Some(event) = cursor.next(parse_ctx)? {
            events.push(event);
        }
        Ok(events)
    }

    fn scalars(events: &[Event]) -> Vec<(char, usize, u32, u32, bool)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Scalar(s) => Some((s.c, s.cp, s.fc, s.piece, s.replaced)),
                Event::Gap(_) => None,
            })
            .collect()
    }

    #[test]
    fn each_compressed_byte_is_the_character_the_specification_maps_it_to() {
        let word_doc: Vec<u8> = (0..=u8::MAX).collect();
        let table = clx(&[(256, 0, true)]);
        let events = walk(&ParseCtx::default(), &view(&word_doc, &table), 0..256, 0..256).unwrap();
        let chars: Vec<char> = scalars(&events).iter().map(|s| s.0).collect();
        assert_eq!(chars.len(), 256);
        for byte in 0..=u8::MAX {
            let mapped = MAPPED_BYTES.iter().position(|&b| b == byte);
            let expected =
                mapped.map_or(char::from(byte), |at| MAPPED_CHARS.chars().nth(at).unwrap());
            assert_eq!(chars[usize::from(byte)], expected, "{byte:#x}");
        }
    }

    #[test]
    fn each_scalar_keeps_its_cp_file_offset_and_piece_across_piece_kinds() {
        let mut word_doc = vec![0u8; 0x40];
        word_doc[0x10..0x12].copy_from_slice(&[b'a', 0x93]);
        let units = [0x00E9u16, 0xD83D, 0xDE00, 0xD800];
        word_doc[0x20..0x28].copy_from_slice(&utf16(&units));
        word_doc[0x30] = b'z';
        let table = clx(&[(2, 0x10, true), (6, 0x20, false), (7, 0x30, true), (9, 0x50, true)]);
        let events = walk(&ParseCtx::default(), &view(&word_doc, &table), 0..9, 0..9).unwrap();
        assert_eq!(
            scalars(&events),
            [
                ('a', 0, 0x10, 0, false),
                ('\u{201C}', 1, 0x11, 0, false),
                ('\u{E9}', 2, 0x20, 1, false),
                ('\u{1F600}', 3, 0x22, 1, false),
                ('\u{FFFD}', 5, 0x26, 1, true),
                ('z', 6, 0x30, 2, false),
            ]
        );
        assert_eq!(events.last(), Some(&Event::Gap(Gap { cps: 7..9, piece: 3 })));
    }

    /// `a`, a pair split over two UTF-16 pieces, `b`, and a compressed piece.
    fn split_pair() -> (Vec<u8>, Vec<u8>) {
        let word_doc = [utf16(&[0x61, 0xD83D]), utf16(&[0xDE00, 0x62]), b"\xBD".to_vec()].concat();
        (word_doc, clx(&[(2, 0, false), (4, 4, false), (5, 8, true)]))
    }

    #[test]
    fn a_pair_decodes_whole_across_pieces_but_not_across_a_story_or_into_compressed_text() {
        let (word_doc, table) = split_pair();
        let text = view(&word_doc, &table);
        let parse_ctx = ParseCtx::default();
        let whole = scalars(&walk(&parse_ctx, &text, 0..5, 0..5).unwrap());
        assert_eq!(whole[1], ('\u{1F600}', 1, 2, 0, false));
        assert_eq!(whole[2], ('b', 3, 6, 1, false));
        let cut = scalars(&walk(&parse_ctx, &text, 0..2, 0..2).unwrap());
        assert_eq!(cut, [('a', 0, 0, 0, false), ('\u{FFFD}', 1, 2, 0, true)]);
        let after = scalars(&walk(&parse_ctx, &text, 2..5, 2..5).unwrap());
        assert_eq!(after[0], ('\u{FFFD}', 2, 4, 1, true));
        let word_doc = [utf16(&[0x61, 0xD83D]), vec![0xDE]].concat();
        let table = clx(&[(2, 0, false), (3, 4, true)]);
        let text = view(&word_doc, &table);
        let mixed = scalars(&walk(&parse_ctx, &text, 0..3, 0..3).unwrap());
        assert_eq!(mixed[1..], [('\u{FFFD}', 1, 2, 0, true), ('\u{DE}', 2, 4, 1, false)]);
    }

    #[test]
    fn a_range_owns_the_pairs_it_starts_and_reads_past_its_end_to_finish_one() {
        let (word_doc, table) = split_pair();
        let text = view(&word_doc, &table);
        let parse_ctx = ParseCtx::default();
        let left = scalars(&walk(&parse_ctx, &text, 0..5, 0..2).unwrap());
        assert_eq!(left, [('a', 0, 0, 0, false), ('\u{1F600}', 1, 2, 0, false)]);
        let right = scalars(&walk(&parse_ctx, &text, 0..5, 2..5).unwrap());
        assert_eq!(right, [('b', 3, 6, 1, false), ('\u{BD}', 4, 8, 2, false)]);
        let lone = utf16(&[0x61, 0xDE00, 0x62]);
        let table = clx(&[(3, 0, false)]);
        let text = view(&lone, &table);
        let kept = scalars(&walk(&parse_ctx, &text, 0..3, 1..3).unwrap());
        assert_eq!(kept[0], ('\u{FFFD}', 1, 2, 0, true));
    }

    #[test]
    fn a_walk_charges_its_seek_each_piece_it_enters_and_each_unit_it_reads() {
        let (word_doc, table) = split_pair();
        let text = view(&word_doc, &table);
        let fuel = |story: Range<usize>, range: Range<usize>| {
            let parse_ctx = ParseCtx::default();
            walk(&parse_ctx, &text, story, range).unwrap();
            parse_ctx.fuel()
        };
        assert_eq!(fuel(0..5, 0..5), 1 + 2 + 5, "a seek, two pieces entered and five units");
        assert_eq!(fuel(0..5, 0..2), 1 + 1 + 3, "the trailing unit read past the end");
        assert_eq!(fuel(0..5, 2..5), 1 + 1 + 1 + 3, "the leading unit read before the start");
        assert_eq!(fuel(0..5, 2..2), 0);
        let word_doc = [utf16(&[0x61, 0xD83D]), b"x".to_vec()].concat();
        let table = clx(&[(2, 0, false), (3, 4, true)]);
        let parse_ctx = ParseCtx::default();
        walk(&parse_ctx, &view(&word_doc, &table), 0..3, 0..3).unwrap();
        assert_eq!(parse_ctx.fuel(), 1 + 1 + 3, "the piece a peek enters is not entered again");
    }

    #[test]
    fn each_chunk_is_charged_right_before_its_units_decode() {
        let chunk = usize::try_from(CHUNK).unwrap();
        let mut units = vec![u16::from(b'b'); chunk + 1];
        units[chunk - 1..].copy_from_slice(&[0xD83D, 0xDE00]);
        let word_doc = [vec![b'a'; chunk + 1], utf16(&units)].concat();
        let table = clx(&[(chunk + 1, 0, true), (2 * chunk + 2, chunk + 1, false)]);
        let text = view(&word_doc, &table);
        let parse_ctx = ParseCtx::default();
        let mut cursor = text.cursor(&parse_ctx, &(0..2 * chunk + 2), &(0..2 * chunk + 2)).unwrap();
        let mut runs: Vec<(u64, usize)> = Vec::new();
        while cursor.next(&parse_ctx).unwrap().is_some() {
            match runs.last_mut() {
                Some((fuel, count)) if *fuel == parse_ctx.fuel() => *count += 1,
                _ => runs.push((parse_ctx.fuel(), 1)),
            }
        }
        let expected = [
            (1 + CHUNK, chunk),
            (1 + CHUNK + 1, 1),
            (1 + CHUNK + 1 + 1 + CHUNK, chunk - 1),
            (1 + CHUNK + 1 + 1 + CHUNK + 1, 1),
        ];
        assert_eq!(runs, expected);
    }

    #[test]
    fn cancellation_after_a_chunk_is_admitted_stops_decoding_at_the_next_chunk() {
        let chunk = usize::try_from(CHUNK).unwrap();
        let cases = [
            (vec![b'a'; 2 * chunk + 1], true),
            (utf16(&vec![u16::from(b'b'); 2 * chunk + 1]), false),
        ];
        for (word_doc, compressed) in cases {
            let table = clx(&[(2 * chunk + 1, 0, compressed)]);
            let text = view(&word_doc, &table);
            let token = CancelToken::new();
            let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
            let mut cursor =
                text.cursor(&parse_ctx, &(0..2 * chunk + 1), &(0..2 * chunk + 1)).unwrap();
            let mut decoded = 0;
            let stopped = loop {
                match cursor.next(&parse_ctx) {
                    Ok(Some(_)) => {
                        token.cancel();
                        decoded += 1;
                    }
                    other => break other.map(drop),
                }
            };
            assert_eq!(stopped, Err(ConvertError::Cancelled));
            assert_eq!((decoded, parse_ctx.fuel()), (chunk, 1 + 2 * CHUNK));
        }
    }

    #[test]
    fn a_fork_charges_each_unit_it_reads_and_polls_cancellation_at_each() {
        let chunk = usize::try_from(CHUNK).unwrap();
        let word_doc = vec![b'a'; 2 * chunk];
        let table = clx(&[(2 * chunk, 0, true)]);
        let text = view(&word_doc, &table);
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        let range = 0..2 * chunk;
        let mut cursor = text.cursor(&parse_ctx, &range, &range).unwrap();
        cursor.next(&parse_ctx).unwrap();
        let mut ahead = cursor.fork();
        let before = parse_ctx.fuel();
        for _ in 0..3 {
            ahead.next(&parse_ctx).unwrap();
        }
        assert_eq!(parse_ctx.fuel() - before, 3);
        token.cancel();
        assert_eq!(ahead.next(&parse_ctx), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_walk_over_gaps_alone_polls_cancellation_at_each_piece() {
        let pieces: Vec<(usize, usize, bool)> = (1..=4).map(|cp| (cp, 0x100, true)).collect();
        let table = clx(&pieces);
        let text = view(&[], &table);
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        let mut cursor = text.cursor(&parse_ctx, &(0..4), &(0..4)).unwrap();
        assert_eq!(cursor.next(&parse_ctx).unwrap(), Some(Event::Gap(Gap { cps: 0..1, piece: 0 })));
        token.cancel();
        assert_eq!(cursor.next(&parse_ctx), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_range_past_the_work_left_is_refused_before_any_of_it_decodes() {
        let word_doc = b"abcd";
        let table = clx(&[(4, 0, true), (8, 0, true), (10, 0x40, true)]);
        let text = view(word_doc, &table);
        for (max_work, outcome) in
            [(4 + 8 - 1, Err(ConvertError::limit(Limit::Work, 11))), (4 + 8, Ok(()))]
        {
            let limits = Limits { max_work, ..Limits::DEFAULT };
            let parse_ctx = ParseCtx::new(&limits);
            assert_eq!(text.preflight(&parse_ctx, &(0..10)), outcome, "max_work {max_work}");
            assert_eq!(parse_ctx.fuel(), 4, "the seek and three pieces");
        }
        for (range, fuel) in [(5..6, 2), (0..4, 2)] {
            let parse_ctx = ParseCtx::default();
            text.preflight(&parse_ctx, &range).unwrap();
            assert_eq!(parse_ctx.fuel(), fuel, "the seek and one piece for {range:?}");
        }
    }

    #[test]
    fn aliases_of_one_stretch_past_the_default_work_are_refused_from_metadata_alone() {
        let stretch = 1usize << 20;
        let word_doc = vec![b'a'; stretch];
        let pieces: Vec<(usize, usize, bool)> = (1..=512).map(|i| (i * stretch, 0, true)).collect();
        let table = clx(&pieces);
        let text = view(&word_doc, &table);
        let parse_ctx = ParseCtx::default();
        let cps = 0..512 * stretch;
        assert_eq!(cps.len(), 1 << 29);
        let refused = text.preflight(&parse_ctx, &cps).err();
        assert_eq!(refused, Some(ConvertError::limit(Limit::Work, Limits::DEFAULT.max_work)));
        assert_eq!(parse_ctx.fuel(), 1 + 512);
    }
}
