use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, Inline};
use crate::ocr::GapError;

const BOX_BYTES: u64 = OcrLine::BOX_BYTES;

/// One literal horizontal text line, located in normalized image coordinates.
#[derive(Clone, PartialEq)]
pub struct OcrLine {
    text: Arc<str>,
    bounds: [f64; 4],
}

impl Eq for OcrLine {}

impl OcrLine {
    /// Location bytes each line counts against the OCR fill limit, besides its UTF-8 text.
    pub const BOX_BYTES: u64 = 32;

    /// Make a line with finite `[left, top, right, bottom]` bounds within `[0, 1]`.
    ///
    /// # Errors
    /// Returns [`GapError::InvalidLine`] for invalid or empty bounds.
    ///
    /// # Examples
    /// ```
    /// use almiladoc::OcrLine;
    /// let line = OcrLine::new("*literal*", [0.0, 0.0, 1.0, 0.1])?;
    /// assert_eq!(line.text(), "*literal*");
    /// # Ok::<(), almiladoc::GapError>(())
    /// ```
    pub fn new(text: impl Into<String>, bounds: [f64; 4]) -> Result<Self, GapError> {
        if bounds.iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            || bounds[0] >= bounds[2]
            || bounds[1] >= bounds[3]
        {
            return Err(GapError::InvalidLine);
        }
        let bounds = bounds.map(|v| if v == 0.0 { 0.0 } else { v });
        Ok(Self { text: text.into().into(), bounds })
    }

    /// The line's literal UTF-8 text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The normalized `[left, top, right, bottom]` image bounds.
    #[must_use]
    pub fn bounds(&self) -> [f64; 4] {
        self.bounds
    }
}

impl fmt::Debug for OcrLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrLine")
            .field("text_bytes", &self.text.len())
            .field("bounds", &self.bounds)
            .finish()
    }
}

pub(crate) fn blocks(ctx: &ParseCtx<'_>, lines: &[OcrLine]) -> Result<Vec<Block>, ConvertError> {
    let cap = ctx.limits().max_ocr_text_bytes;
    ctx.charge(0)?;
    let mut bytes = (lines.len() as u64).saturating_mul(BOX_BYTES);
    if bytes > cap {
        return Err(ConvertError::limit(Limit::OcrTextBytes, cap));
    }
    for line in lines {
        ctx.charge(1)?;
        bytes = bytes.saturating_add(line.text.len() as u64);
        if bytes > cap {
            return Err(ConvertError::limit(Limit::OcrTextBytes, cap));
        }
    }
    ctx.charge(bytes)?;
    let mut retained = (lines.len() as u64).saturating_mul(BOX_BYTES);
    let mut pending = vec![(0..lines.len()).collect::<Vec<_>>()];
    let mut reading = Vec::with_capacity(lines.len());
    while let Some(region) = pending.pop() {
        let (ordered, cut) = split(ctx, lines, region)?;
        if let Some(at) = cut {
            ctx.charge(ordered.len() as u64)?;
            pending.push(ordered[at..].to_vec());
            pending.push(ordered[..at].to_vec());
        } else {
            ctx.charge(ordered.len() as u64)?;
            reading.extend(ordered.into_iter().enumerate().map(|(at, index)| (index, at == 0)));
        }
    }
    paragraphs(ctx, lines, reading, &mut retained)
}

fn compare(lines: &[OcrLine], a: usize, b: usize, axis: usize) -> Ordering {
    for field in [axis, 1 - axis, 2, 3] {
        let order = lines[a].bounds[field].total_cmp(&lines[b].bounds[field]);
        if order != Ordering::Equal {
            return order;
        }
    }
    a.cmp(&b)
}

fn ordered(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    mut indices: Vec<usize>,
    axis: usize,
) -> Result<Vec<usize>, ConvertError> {
    ctx.charge(indices.len() as u64)?;
    let mut scratch = vec![0; indices.len()];
    let mut width = 1usize;
    while width < indices.len() {
        for start in (0..indices.len()).step_by(width.saturating_mul(2)) {
            let mid = start.saturating_add(width).min(indices.len());
            let end = mid.saturating_add(width).min(indices.len());
            let (mut a, mut b) = (start, mid);
            for target in &mut scratch[start..end] {
                ctx.charge(1)?;
                if a < mid && (b == end || compare(lines, indices[a], indices[b], axis).is_le()) {
                    *target = indices[a];
                    a += 1;
                } else {
                    *target = indices[b];
                    b += 1;
                }
            }
        }
        std::mem::swap(&mut scratch, &mut indices);
        width = width.saturating_mul(2);
    }
    Ok(indices)
}

fn gutter(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    indices: &[usize],
) -> Result<Option<usize>, ConvertError> {
    let (mut right, mut best) = (0.0_f64, None);
    for (at, &index) in indices.iter().enumerate() {
        ctx.charge(1)?;
        let [left, _, end, _] = lines[index].bounds;
        if at > 0
            && left > right
            && best.is_none_or(|before: usize| {
                at.abs_diff(indices.len() / 2) < before.abs_diff(indices.len() / 2)
            })
        {
            best = Some(at);
        }
        right = right.max(end);
    }
    Ok(best)
}

fn split(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    indices: Vec<usize>,
) -> Result<(Vec<usize>, Option<usize>), ConvertError> {
    let y = ordered(ctx, lines, indices, 1)?;
    if let Some(at) = section_cut(ctx, lines, &y)? {
        return Ok((y, Some(at)));
    }
    ctx.charge(y.len() as u64)?;
    let x = ordered(ctx, lines, y.clone(), 0)?;
    if let Some(at) = gutter(ctx, lines, &x)? {
        return Ok((x, Some(at)));
    }
    Ok((y, None))
}

fn bands(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    y: &[usize],
) -> Result<Vec<std::ops::Range<usize>>, ConvertError> {
    let (mut ranges, mut start, mut bottom) = (Vec::new(), 0, 0.0_f64);
    for (at, &i) in y.iter().enumerate() {
        ctx.charge(1)?;
        if at > start && lines[i].bounds[1] > bottom {
            ranges.push(start..at);
            start = at;
            bottom = 0.0;
        }
        bottom = bottom.max(lines[i].bounds[3]);
    }
    if start < y.len() {
        ranges.push(start..y.len());
    }
    Ok(ranges)
}

fn band_gutter(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    y: &[usize],
    ranges: &[std::ops::Range<usize>],
) -> Result<Option<([f64; 2], usize)>, ConvertError> {
    for (seed, range) in ranges.iter().enumerate() {
        ctx.charge(range.len() as u64)?;
        let x = ordered(ctx, lines, y[range.clone()].to_vec(), 0)?;
        if let Some(at) = gutter(ctx, lines, &x)? {
            let mut right = 0.0_f64;
            for &i in &x[..at] {
                ctx.charge(1)?;
                right = right.max(lines[i].bounds[2]);
            }
            return Ok(Some(([right, lines[x[at]].bounds[0]], seed)));
        }
    }
    Ok(None)
}

fn section_cut(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    y: &[usize],
) -> Result<Option<usize>, ConvertError> {
    let ranges = bands(ctx, lines, y)?;
    let Some(([left, right], seed)) = band_gutter(ctx, lines, y, &ranges)? else {
        return Ok(None);
    };
    let mut spanning = Vec::with_capacity(ranges.len());
    for range in &ranges {
        let mut intersects = false;
        for &i in &y[range.clone()] {
            ctx.charge(1)?;
            intersects |= lines[i].bounds[0] < right && lines[i].bounds[2] > left;
        }
        spanning.push(intersects);
    }
    ctx.charge(seed as u64)?;
    let prefix = spanning[..seed].iter().any(|&value| value);
    let mut previous = None;
    for (at, range) in ranges.iter().enumerate() {
        ctx.charge(1)?;
        let span = spanning[at] || (at < seed && prefix);
        if previous.is_some_and(|before| before != span) {
            return Ok(Some(range.start));
        }
        previous = Some(span);
    }
    Ok(None)
}

fn paragraphs(
    ctx: &ParseCtx<'_>,
    lines: &[OcrLine],
    indices: Vec<(usize, bool)>,
    retained: &mut u64,
) -> Result<Vec<Block>, ConvertError> {
    let mut blocks = Vec::new();
    let mut inlines = Vec::new();
    let mut previous: Option<[f64; 4]> = None;
    for (i, region_start) in indices {
        ctx.charge(1)?;
        let line = &lines[i];
        if previous.is_some_and(|last| region_start || paragraph_break(last, line.bounds)) {
            blocks.push(Block::Paragraph(std::mem::take(&mut inlines)));
        }
        if !inlines.is_empty() {
            inlines.push(Inline::plain(" "));
        }
        let mut text = String::new();
        for c in line.text.nfc() {
            ctx.charge(1)?;
            *retained = retained.saturating_add(c.len_utf8() as u64);
            if *retained > ctx.limits().max_ocr_text_bytes {
                return Err(ConvertError::limit(
                    Limit::OcrTextBytes,
                    ctx.limits().max_ocr_text_bytes,
                ));
            }
            text.push(c);
        }
        inlines.push(Inline::plain(text));
        previous = Some(line.bounds);
    }
    if !inlines.is_empty() {
        blocks.push(Block::Paragraph(inlines));
    }
    Ok(blocks)
}

fn paragraph_break(last: [f64; 4], next: [f64; 4]) -> bool {
    next[1] < last[1]
        || next[0] >= last[2]
        || next[2] <= last[0]
        || next[1] - last[3] > (last[3] - last[1]).max(next[3] - next[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::limits::Limits;

    fn line(text: &str, bounds: [f64; 4]) -> OcrLine {
        OcrLine::new(text, bounds).unwrap()
    }

    fn columns() -> Vec<OcrLine> {
        vec![
            line("R2", [0.6, 0.17, 1.0, 0.22]),
            line("L1", [0.0, 0.1, 0.4, 0.15]),
            line("R1", [0.6, 0.1, 1.0, 0.15]),
            line("L2", [0.0, 0.17, 0.4, 0.22]),
        ]
    }

    fn paragraphs_of(lines: &[OcrLine]) -> Vec<Block> {
        blocks(&ParseCtx::default(), lines).unwrap()
    }

    fn text_of(lines: &[OcrLine]) -> Vec<String> {
        paragraph_text(paragraphs_of(lines))
    }

    fn paragraph_text(blocks: Vec<Block>) -> Vec<String> {
        blocks
            .into_iter()
            .map(|block| match block {
                Block::Paragraph(content) => crate::model::inlines_to_plain_text(&content),
                _ => panic!("not a paragraph"),
            })
            .collect()
    }

    fn permutations(lines: &[OcrLine]) -> [Vec<OcrLine>; 3] {
        let mut reverse = lines.to_vec();
        reverse.reverse();
        let mut shuffled = lines.to_vec();
        shuffled.rotate_left(1);
        assert_ne!(shuffled, lines);
        assert_ne!(shuffled, reverse);
        [lines.to_vec(), reverse, shuffled]
    }

    fn plain(text: &str) -> Block {
        Block::Paragraph(vec![Inline::plain(text)])
    }

    #[test]
    fn normalized_bounds_are_finite_strict_and_private_to_validated_lines() {
        for bounds in [
            [f64::NAN, 0.0, 1.0, 1.0],
            [0.0, f64::INFINITY, 1.0, 1.0],
            [0.0, 0.0, f64::NAN, 1.0],
            [0.0, 0.0, 1.0, f64::NEG_INFINITY],
            [-0.1, 0.0, 1.0, 1.0],
            [0.0, 0.0, 1.1, 1.0],
            [0.2, 0.0, 0.1, 1.0],
            [0.0, 0.2, 1.0, 0.1],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 0.0],
        ] {
            assert_eq!(OcrLine::new("private", bounds), Err(GapError::InvalidLine));
        }
        let value = line("private", [-0.0, 0.1, 1.0, 0.9]);
        assert_eq!(value.bounds(), [0.0, 0.1, 1.0, 0.9]);
        assert_eq!(value.text(), "private");
        assert_eq!(format!("{value:?}"), "OcrLine { text_bytes: 7, bounds: [0.0, 0.1, 1.0, 0.9] }");
    }

    #[test]
    fn columns_keep_their_lines_and_full_width_sections_keep_their_place() {
        let header = line("H", [0.0, 0.02, 1.0, 0.07]);
        let footer = line("F", [0.0, 0.3, 1.0, 0.35]);
        for (extras, expected) in [
            (vec![], vec!["L1 L2", "R1 R2"]),
            (vec![header.clone()], vec!["H", "L1 L2", "R1 R2"]),
            (vec![footer.clone()], vec!["L1 L2", "R1 R2", "F"]),
            (vec![header, footer.clone()], vec!["H", "L1 L2", "R1 R2", "F"]),
            (
                vec![footer, line("X", [0.0, 0.4, 0.4, 0.45]), line("Y", [0.6, 0.4, 1.0, 0.45])],
                vec!["L1 L2", "R1 R2", "F", "X", "Y"],
            ),
        ] {
            let mut lines = columns();
            lines.extend(extras);
            for input in permutations(&lines) {
                assert_eq!(text_of(&input), expected);
            }
        }
    }

    #[test]
    fn partial_sections_and_gutter_footer_preserve_column_order() {
        let body = [
            line("L1", [0.05, 0.10, 0.45, 0.14]),
            line("L2", [0.05, 0.20, 0.45, 0.24]),
            line("R1", [0.55, 0.10, 0.95, 0.14]),
            line("R2", [0.55, 0.20, 0.95, 0.24]),
        ];
        for (before, after, expected) in [
            (vec![line("Title", [0.3, 0.02, 0.7, 0.06])], vec![], "Title L1 L2 R1 R2"),
            (
                vec![
                    line("Abstract", [0.1, 0.01, 0.9, 0.03]),
                    line("continues", [0.1, 0.04, 0.76, 0.06]),
                    line("ends", [0.1, 0.07, 0.36, 0.09]),
                ],
                vec![],
                "Abstract continues ends L1 L2 R1 R2",
            ),
            (vec![line("Heading", [0.051, 0.02, 0.949, 0.06])], vec![], "Heading L1 L2 R1 R2"),
            (vec![], vec![line("7", [0.48, 0.30, 0.52, 0.34])], "L1 L2 R1 R2 7"),
            (
                vec![line("Title", [0.3, 0.02, 0.7, 0.06])],
                vec![line("7", [0.48, 0.30, 0.52, 0.34])],
                "Title L1 L2 R1 R2 7",
            ),
        ] {
            let lines = before.into_iter().chain(body.clone()).chain(after).collect::<Vec<_>>();
            for input in permutations(&lines) {
                assert_eq!(text_of(&input).join(" "), expected);
            }
        }
    }

    #[test]
    fn gutter_sections_join_wrapped_text_and_keep_one_sided_body_rows() {
        let lines = [
            line("Abstract", [0.1, 0.01, 0.9, 0.03]),
            line("continues", [0.1, 0.04, 0.76, 0.06]),
            line("ends", [0.1, 0.07, 0.36, 0.09]),
            line("L1", [0.05, 0.10, 0.45, 0.14]),
            line("L2", [0.05, 0.16, 0.45, 0.20]),
            line("L3", [0.05, 0.22, 0.45, 0.26]),
            line("R1", [0.55, 0.10, 0.95, 0.14]),
            line("R2", [0.55, 0.22, 0.95, 0.26]),
            line("7", [0.48, 0.30, 0.52, 0.34]),
        ];
        for input in permutations(&lines) {
            assert_eq!(text_of(&input), ["Abstract continues ends", "L1 L2 L3", "R1", "R2", "7"]);
        }
    }

    #[test]
    fn text_is_literal_nfc_and_geometric_ties_keep_input_order() {
        let bounds = [0.1, 0.1, 0.9, 0.15];
        let lines = [line("# *e\u{301}* [x](javascript:y) <b>", bounds)];
        assert_eq!(paragraphs_of(&lines), [plain("# *é* [x](javascript:y) <b>")]);
        assert_eq!(paragraphs_of(&[]), []);
        assert_eq!(
            paragraphs_of(&[line("a", bounds), line("b", bounds)]),
            [Block::Paragraph(vec![Inline::plain("a"), Inline::plain(" "), Inline::plain("b")])]
        );
        let separated = [line("a", bounds), line("b", [0.1, 0.5, 0.9, 0.55])];
        assert_eq!(paragraphs_of(&separated), [plain("a"), plain("b")]);
    }

    #[test]
    fn count_text_normalization_and_ordering_are_bounded_before_their_work() {
        let empty = line("", [0.0, 0.0, 1.0, 1.0]);
        let limits = Limits::default();
        let ctx = ParseCtx::new(&limits);
        let many = vec![empty; usize::try_from(limits.max_ocr_text_bytes / BOX_BYTES + 1).unwrap()];
        assert_eq!(
            blocks(&ctx, &many),
            Err(ConvertError::limit(Limit::OcrTextBytes, limits.max_ocr_text_bytes))
        );
        assert_eq!(ctx.ledger().spent(), 0);
        let lines = [line("ab", [0.0, 0.0, 1.0, 0.1]), line("cde", [0.0, 0.2, 1.0, 0.3])];
        for cap in [68, 69] {
            let limits = Limits { max_ocr_text_bytes: cap, ..Limits::default() };
            assert_eq!(
                blocks(&ParseCtx::new(&limits), &lines).map(|_| ()),
                if cap == 68 { Err(ConvertError::limit(Limit::OcrTextBytes, cap)) } else { Ok(()) }
            );
        }
        let limits = Limits { max_ocr_text_bytes: 68, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(blocks(&ctx, &lines), Err(ConvertError::limit(Limit::OcrTextBytes, 68)));
        assert_eq!(ctx.ledger().spent(), 2);
        let limits = Limits { max_ocr_text_bytes: 34, ..Limits::default() };
        assert_eq!(
            blocks(&ParseCtx::new(&limits), &[line("\u{344}", [0.0, 0.0, 1.0, 1.0])]),
            Err(ConvertError::limit(Limit::OcrTextBytes, 34))
        );
        for max in [1, 2] {
            let limits = Limits { max_work: max, ..Limits::default() };
            assert_eq!(
                ordered(&ParseCtx::new(&limits), &lines, vec![0, 1], 0),
                Err(ConvertError::limit(Limit::Work, max))
            );
        }
        let limits = Limits { max_work: 1, ..Limits::default() };
        let paid = ParseCtx::new(&limits).starting_at(1);
        assert_eq!(ordered(&paid, &lines, vec![0], 0), Err(ConvertError::limit(Limit::Work, 1)));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(
            blocks(&ParseCtx::default().with_cancel(Some(&cancel)), &lines),
            Err(ConvertError::Cancelled)
        );
    }
}
