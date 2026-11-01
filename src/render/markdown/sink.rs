//! The bounded writer: one Markdown buffer behind a stack of line prefixes,
//! and the meter that counts every byte it writes, or holds before writing,
//! against [`Limits::max_output_bytes`](crate::Limits::max_output_bytes).

use std::cell::Cell;

use crate::error::ConvertError;
use crate::ledger::Ledger;
use crate::limits::Limit;

const BYTES_PER_UNIT: u64 = 64;

/// The output budget of one render: bytes written to the Markdown, and bytes
/// held in [`Text`] buffers before they are written, together never past the
/// maximum; one unit of work per [`BYTES_PER_UNIT`] bytes written.
pub(crate) struct Meter<'l> {
    ledger: &'l Ledger<'l>,
    max: u64,
    written: Cell<u64>,
    held: Cell<u64>,
    #[cfg(test)]
    admitted: Cell<u64>,
}

impl<'l> Meter<'l> {
    pub(crate) fn new(ledger: &'l Ledger<'l>, max: u64) -> Self {
        Self {
            ledger,
            max,
            written: Cell::new(0),
            held: Cell::new(0),
            #[cfg(test)]
            admitted: Cell::new(0),
        }
    }

    /// Every byte admitted so far, held or written, released or not.
    #[cfg(test)]
    pub(crate) fn admitted(&self) -> u64 {
        self.admitted.get()
    }

    fn admit(&self, bytes: usize) -> Result<u64, ConvertError> {
        let bytes = bytes as u64;
        let total = self.written.get().saturating_add(self.held.get()).saturating_add(bytes);
        if total > self.max {
            return Err(ConvertError::limit(Limit::OutputBytes, self.max));
        }
        #[cfg(test)]
        self.admitted.set(self.admitted.get().saturating_add(bytes));
        Ok(bytes)
    }

    fn hold(&self, bytes: usize) -> Result<(), ConvertError> {
        let bytes = self.admit(bytes)?;
        self.held.set(self.held.get().saturating_add(bytes));
        Ok(())
    }

    fn release(&self, bytes: usize) {
        self.held.set(self.held.get().saturating_sub(bytes as u64));
    }

    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        self.ledger.charge(units)
    }

    pub(crate) fn write(&self, bytes: usize) -> Result<(), ConvertError> {
        let bytes = self.admit(bytes)?;
        let before = self.written.get();
        let after = before.saturating_add(bytes);
        self.written.set(after);
        let units = after / BYTES_PER_UNIT - before / BYTES_PER_UNIT;
        if units > 0 { self.ledger.charge(units) } else { Ok(()) }
    }
}

pub(crate) struct Text<'m> {
    buf: String,
    meter: &'m Meter<'m>,
    line: Line,
    space: String,
    floor: usize,
    note: Option<NoteEnd>,
}

#[derive(Clone, Copy)]
struct NoteEnd {
    at: usize,
    opens_line: bool,
}

#[derive(Clone, Copy, Default)]
struct Line {
    depth: usize,
    shown: usize,
    owed: Option<Owed>,
}

#[derive(Clone, Copy)]
struct Owed {
    by: usize,
    debt: Debt,
}

#[derive(Clone, Copy)]
enum Debt {
    Break,
    Space(usize),
}

#[derive(Clone, Copy)]
struct Mark {
    len: usize,
    space: usize,
    line: Line,
}

impl<'m> Text<'m> {
    pub(crate) fn new(meter: &'m Meter<'m>) -> Self {
        Self {
            buf: String::new(),
            meter,
            line: Line::default(),
            space: String::new(),
            floor: 0,
            note: None,
        }
    }

    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        self.meter.charge(units)
    }

    pub(crate) fn meter(&self) -> &'m Meter<'m> {
        self.meter
    }

    pub(crate) fn push_note_ref(&mut self, reference: &str) -> Result<(), ConvertError> {
        let before =
            self.buf.trim_end_matches(|c: char| c.is_whitespace() && !matches!(c, '\n' | '\r'));
        let opens_line = before.is_empty() || before.ends_with(['\n', '\r']);
        self.push_str(reference)?;
        self.note = Some(NoteEnd { at: self.buf.len(), opens_line });
        Ok(())
    }

    /// Whether what is pushed next directly follows a note reference, and
    /// if so whether that reference opens its line.
    pub(crate) fn after_note(&self) -> Option<bool> {
        self.note
            .filter(|note| note.at == self.buf.len() && self.line.owed.is_none())
            .map(|note| note.opens_line)
    }

    pub(crate) fn push_str(&mut self, s: &str) -> Result<(), ConvertError> {
        if self.line.depth == 0 {
            return self.append(s);
        }
        let mut lines = s.split('\n');
        if let Some(first) = lines.next() {
            self.push_line(first)?;
        }
        for line in lines {
            self.end_line();
            self.push_line(line)?;
        }
        Ok(())
    }

    pub(crate) fn push(&mut self, c: char) -> Result<(), ConvertError> {
        self.push_str(c.encode_utf8(&mut [0; 4]))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.buf
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn since(&self, mark: usize) -> &str {
        self.buf.get(mark..).unwrap_or_default()
    }

    pub(crate) fn attempt(
        &mut self,
        render: impl FnOnce(&mut Self) -> Result<bool, ConvertError>,
    ) -> Result<bool, ConvertError> {
        let mark = Mark { len: self.buf.len(), space: self.space.len(), line: self.line };
        let floor = self.floor;
        self.floor = floor.max(mark.space);
        let kept = render(self);
        self.floor = floor;
        if let Ok(false) = kept {
            self.truncate(mark);
        }
        self.drop_tails();
        kept
    }

    /// Whitespace owed at `mark` and shown since is the first thing pushed after it, so it moves back from there; owed whitespace dropped since stays where it was held.
    fn truncate(&mut self, mark: Mark) {
        let held = self.buf.len().saturating_add(self.space.len());
        if let Some(Owed { debt: Debt::Space(from), .. }) = mark.line.owed
            && self.since(mark.len).starts_with(char::is_whitespace)
        {
            let shown = mark.len.saturating_add(mark.space.saturating_sub(from));
            self.space.truncate(from);
            self.space.push_str(self.buf.get(mark.len..shown).unwrap_or_default());
        }
        self.buf.truncate(mark.len);
        self.space.truncate(mark.space);
        self.meter.release(held.saturating_sub(self.buf.len().saturating_add(self.space.len())));
        self.line = mark.line;
    }

    pub(crate) fn open_cell(&mut self) {
        self.line.depth = self.line.depth.saturating_add(1);
    }

    pub(crate) fn close_cell(&mut self) {
        let line = &mut self.line;
        if line.owed.is_some_and(|owed| owed.by == line.depth) {
            line.owed = None;
        }
        line.depth = line.depth.saturating_sub(1);
        line.shown = line.shown.min(line.depth);
        self.drop_tails();
    }

    pub(crate) fn into_string(mut self) -> String {
        let buf = std::mem::take(&mut self.buf);
        self.meter.release(buf.len());
        buf
    }

    fn append(&mut self, s: &str) -> Result<(), ConvertError> {
        self.meter.hold(s.len())?;
        self.buf.push_str(s);
        Ok(())
    }

    fn push_line(&mut self, line: &str) -> Result<(), ConvertError> {
        let rest = line.trim_start();
        self.push_space(line.strip_suffix(rest).unwrap_or_default())?;
        let core = rest.trim_end();
        if !core.is_empty() {
            self.show(core)?;
        }
        self.push_space(rest.strip_prefix(core).unwrap_or_default())
    }

    fn push_space(&mut self, ws: &str) -> Result<(), ConvertError> {
        let line = &mut self.line;
        if ws.is_empty() || line.shown < line.depth {
            return Ok(());
        }
        if line.owed.is_none() {
            line.owed = Some(Owed { by: line.depth, debt: Debt::Space(self.space.len()) });
        }
        self.meter.hold(ws.len())?;
        self.space.push_str(ws);
        Ok(())
    }

    fn show(&mut self, core: &str) -> Result<(), ConvertError> {
        match self.line.owed.map(|owed| owed.debt) {
            Some(Debt::Break) => self.append("<br>")?,
            Some(Debt::Space(from)) => {
                self.buf.push_str(self.space.get(from..).unwrap_or_default());
                self.space.truncate(from);
            }
            None => {}
        }
        self.line.owed = None;
        self.append(core)?;
        self.line.shown = self.line.depth;
        Ok(())
    }

    fn end_line(&mut self) {
        let line = &mut self.line;
        if line.shown == line.depth {
            line.owed = Some(Owed { by: line.depth, debt: Debt::Break });
            line.shown = line.depth.saturating_sub(1);
        }
        self.drop_tails();
    }

    fn drop_tails(&mut self) {
        let end = match self.line.owed {
            Some(Owed { debt: Debt::Space(from), .. }) => from,
            _ => self.space.len(),
        };
        let start = (self.floor..end).find(|&at| self.space.is_char_boundary(at)).unwrap_or(end);
        if start == end {
            return;
        }
        self.space.drain(start..end);
        self.meter.release(end.saturating_sub(start));
        if let Some(Owed { debt: Debt::Space(from), .. }) = &mut self.line.owed {
            *from = start;
        }
    }
}

impl Drop for Text<'_> {
    fn drop(&mut self) {
        self.meter.release(self.buf.len().saturating_add(self.space.len()));
    }
}

enum Prefix<'m> {
    Quote,
    Marker { first: Option<Text<'m>>, indent: usize },
}

struct Gap {
    breaks: u8,
    depth: usize,
}

pub(crate) struct Sink<'m> {
    out: String,
    meter: &'m Meter<'m>,
    prefixes: Vec<Prefix<'m>>,
    shown: usize,
    fresh: bool,
    gap: Option<Gap>,
    cr: bool,
}

impl<'m> Sink<'m> {
    pub(crate) fn new(meter: &'m Meter<'m>) -> Self {
        Self {
            out: String::new(),
            meter,
            prefixes: Vec::new(),
            shown: 0,
            fresh: true,
            gap: None,
            cr: false,
        }
    }

    pub(crate) fn meter(&self) -> &'m Meter<'m> {
        self.meter
    }

    /// Bytes written so far: a block wrote something when this grew.
    pub(crate) fn len(&self) -> usize {
        self.out.len()
    }

    pub(crate) fn gap(&mut self, breaks: u8) {
        self.gap = Some(Gap { breaks, depth: self.prefixes.len() });
    }

    pub(crate) fn drop_gap(&mut self) {
        self.gap = None;
    }

    pub(crate) fn push_quote(&mut self) {
        self.prefixes.push(Prefix::Quote);
    }

    pub(crate) fn push_marker(&mut self, marker: Text<'m>, indent: usize) {
        self.prefixes.push(Prefix::Marker { first: Some(marker), indent });
    }

    pub(crate) fn pop(&mut self) {
        self.prefixes.pop();
        self.shown = self.shown.min(self.prefixes.len());
    }

    pub(crate) fn pop_shown(&mut self) -> Result<(), ConvertError> {
        if matches!(self.prefixes.last(), Some(Prefix::Marker { first: Some(_), .. })) {
            self.flush_gap()?;
            self.open()?;
        }
        self.pop();
        Ok(())
    }

    pub(crate) fn write(&mut self, s: &str) -> Result<(), ConvertError> {
        if s.is_empty() {
            return Ok(());
        }
        self.flush_gap()?;
        let mut lines = s.split('\n');
        if let Some(first) = lines.next() {
            self.segment(first)?;
        }
        for line in lines {
            self.end_line(self.prefixes.len())?;
            self.segment(line)?;
        }
        Ok(())
    }

    pub(crate) fn write_text(&mut self, text: Text<'_>) -> Result<(), ConvertError> {
        self.write(&text.into_string())
    }

    pub(crate) fn finish(mut self) -> Result<String, ConvertError> {
        if std::mem::take(&mut self.cr) {
            self.emit("\r")?;
        }
        if !self.out.is_empty() {
            self.emit("\n")?;
        }
        Ok(self.out)
    }

    pub(super) fn flush_gap(&mut self) -> Result<(), ConvertError> {
        if let Some(Gap { breaks, depth }) = self.gap.take() {
            for _ in 0..breaks {
                self.end_line(depth)?;
            }
        }
        Ok(())
    }

    fn segment(&mut self, seg: &str) -> Result<(), ConvertError> {
        if seg.is_empty() {
            return Ok(());
        }
        if std::mem::take(&mut self.cr) {
            self.open()?;
            self.emit("\r")?;
        }
        let (seg, cr) = match seg.strip_suffix('\r') {
            Some(rest) if !self.prefixes.is_empty() => (rest, true),
            _ => (seg, false),
        };
        if !seg.is_empty() {
            self.open()?;
            self.emit(seg)?;
        }
        self.cr = cr;
        Ok(())
    }

    fn open(&mut self) -> Result<(), ConvertError> {
        let Self { out, meter, prefixes, shown, .. } = self;
        for prefix in prefixes.iter_mut().skip(*shown) {
            if let Prefix::Marker { first, .. } = prefix
                && let Some(marker) = first.take()
            {
                emit(out, meter, &marker.into_string())?;
            } else {
                emit_continuation(out, meter, prefix)?;
            }
        }
        self.shown = self.prefixes.len();
        self.fresh = false;
        Ok(())
    }

    fn end_line(&mut self, depth: usize) -> Result<(), ConvertError> {
        if std::mem::take(&mut self.cr) && depth == 0 {
            self.emit("\r")?;
        }
        if self.fresh {
            let Self { out, meter, prefixes, .. } = self;
            let open = prefixes.iter().take(depth);
            if let Some(quote) = open.clone().rposition(|p| matches!(p, Prefix::Quote)) {
                for prefix in open.take(quote) {
                    emit_continuation(out, meter, prefix)?;
                }
                emit(out, meter, ">")?;
            }
        }
        self.emit("\n")?;
        self.fresh = true;
        self.shown = 0;
        Ok(())
    }

    fn emit(&mut self, s: &str) -> Result<(), ConvertError> {
        emit(&mut self.out, self.meter, s)
    }
}

fn emit(out: &mut String, meter: &Meter<'_>, s: &str) -> Result<(), ConvertError> {
    meter.write(s.len())?;
    out.push_str(s);
    Ok(())
}

fn emit_continuation(
    out: &mut String,
    meter: &Meter<'_>,
    prefix: &Prefix<'_>,
) -> Result<(), ConvertError> {
    match prefix {
        Prefix::Quote => emit(out, meter, "> "),
        Prefix::Marker { indent, .. } => {
            meter.write(*indent)?;
            out.extend(std::iter::repeat_n(' ', *indent));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn a_kept_attempt_releases_the_tail_it_dropped_and_shows_the_one_it_owes() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut text = Text::new(&meter);
        text.open_cell();
        text.push_str("x ").unwrap();
        assert!(text.attempt(|text| text.push_str("\nb ").map(|()| true)).unwrap());
        assert_eq!(meter.held.get(), "x<br>b ".len() as u64);
        text.push_str("c").unwrap();
        text.close_cell();
        assert_eq!(text.as_str(), "x<br>b c");
    }

    fn marker<'m>(meter: &'m Meter<'m>, s: &str) -> Text<'m> {
        let mut text = Text::new(meter);
        text.push_str(s).unwrap();
        text
    }

    fn written(build: impl FnOnce(&mut Sink<'_>) -> Result<(), ConvertError>) -> String {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut sink = Sink::new(&meter);
        build(&mut sink).unwrap();
        sink.finish().unwrap()
    }

    #[test]
    fn each_line_opens_with_its_prefixes_and_an_empty_one_up_to_the_innermost_quote() {
        let md = written(|sink| {
            sink.push_marker(marker(sink.meter(), "- "), 2);
            sink.push_quote();
            sink.write("a\n\nb")?;
            sink.pop();
            sink.gap(2);
            sink.push_marker(marker(sink.meter(), "1. "), 3);
            sink.push_quote();
            sink.push_marker(marker(sink.meter(), "- "), 2);
            sink.write("c\n\nd")?;
            sink.pop();
            sink.pop();
            sink.pop();
            sink.pop();
            Ok(())
        });
        assert_eq!(md, "- > a\n  >\n  > b\n\n  1. > - c\n     >\n     >   d\n");
        let md = written(|sink| {
            sink.push_quote();
            sink.push_quote();
            sink.write("a\n\nb")
        });
        assert_eq!(md, "> > a\n> >\n> > b\n");
    }

    #[test]
    fn a_marker_shows_once_and_an_empty_item_keeps_it() {
        let md = written(|sink| {
            sink.push_marker(marker(sink.meter(), "- "), 2);
            sink.pop_shown()?;
            sink.gap(1);
            sink.push_marker(marker(sink.meter(), "[^1]: "), 4);
            sink.pop();
            sink.gap(1);
            sink.push_marker(marker(sink.meter(), "- "), 2);
            sink.write("x\ny")?;
            sink.pop_shown()?;
            Ok(())
        });
        assert_eq!(md, "- \n- x\n  y\n");
    }

    #[test]
    fn a_carriage_return_before_a_break_is_dropped_only_inside_a_container() {
        let md = written(|sink| {
            sink.write("a\r\nb\r")?;
            sink.gap(2);
            sink.push_quote();
            sink.write("c\r")?;
            sink.write("\n\r\nd\re\r")?;
            sink.pop();
            for item in ["x\r", "y"] {
                sink.gap(1);
                sink.push_marker(marker(sink.meter(), "- "), 2);
                sink.write(item)?;
                sink.pop_shown()?;
            }
            Ok(())
        });
        assert_eq!(md, "a\r\nb\r\n\n> c\n>\n> d\re\r\n- x\r\n- y\n");
        let md = written(|sink| {
            sink.push_quote();
            sink.write("x\r")?;
            sink.write("y\nz\r")
        });
        assert_eq!(md, "> x\ry\n> z\r\n");
    }

    #[test]
    fn a_gap_owed_when_nothing_follows_is_never_written() {
        let md = written(|sink| {
            sink.write("a")?;
            sink.gap(2);
            sink.push_quote();
            sink.pop();
            sink.gap(2);
            Ok(())
        });
        assert_eq!(md, "a\n");
    }

    #[test]
    fn written_and_held_bytes_share_the_maximum_and_fail_before_growing() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, 10);
        let mut sink = Sink::new(&meter);
        let mut held = Text::new(&meter);
        held.push_str("123").unwrap();
        let kept = held.attempt(|held| {
            held.push_str("45")?;
            sink.write("1234")?;
            let refused = held.push_str("12");
            assert_matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, max: 10 })
            );
            assert_eq!(held.as_str(), "12345");
            Ok(false)
        });
        assert_matches!(kept, Ok(false));
        held.push_str("12").unwrap();
        assert_matches!(
            sink.write("12"),
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, max: 10 })
        );
        assert_eq!(sink.len(), 4);
        drop(held);
        sink.write("123456").unwrap();
        assert_matches!(
            sink.write("1"),
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, max: 10 })
        );
        assert_eq!(sink.len(), 10);
    }

    #[test]
    fn the_spaces_that_indent_a_line_count_against_the_maximum() {
        let item = |max: usize| {
            let ledger = Ledger::new(u64::MAX, None);
            let meter = Meter::new(&ledger, max as u64);
            let mut sink = Sink::new(&meter);
            sink.push_marker(marker(&meter, "- "), 2);
            sink.write("x\ny").map(|()| sink.len())
        };
        let len = "- x\n  y".len();
        assert_eq!(item(len).unwrap(), len);
        assert_matches!(
            item(len - 1),
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. })
        );
    }

    #[test]
    fn written_bytes_charge_one_unit_per_sixty_four() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut sink = Sink::new(&meter);
        let mut held = Text::new(&meter);
        held.push_str(&"x".repeat(200)).unwrap();
        assert_eq!(ledger.spent(), 0);
        sink.write(&"y".repeat(63)).unwrap();
        assert_eq!(ledger.spent(), 0);
        sink.write("y").unwrap();
        assert_eq!(ledger.spent(), 1);
        sink.write_text(held).unwrap();
        assert_eq!(ledger.spent(), 4);
        let limited = Ledger::new(4, None);
        let meter = Meter::new(&limited, u64::MAX);
        let mut sink = Sink::new(&meter);
        sink.write(&"z".repeat(5 * 64 - 1)).unwrap();
        let tripped = sink.write("z");
        assert_matches!(tripped, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 4 }));
    }

    #[test]
    fn a_cancelled_token_stops_the_next_unit_written() {
        let token = crate::ledger::CancelToken::new();
        let ledger = Ledger::new(u64::MAX, Some(&token));
        let meter = Meter::new(&ledger, u64::MAX);
        let mut sink = Sink::new(&meter);
        sink.write(&"x".repeat(64)).unwrap();
        token.cancel();
        sink.write(&"x".repeat(63)).unwrap();
        assert_matches!(sink.write("x"), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_cell_line_releases_the_whitespace_it_drops() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, 12);
        let mut text = Text::new(&meter);
        text.open_cell();
        text.push_str("  ab  \n\n c d ").unwrap();
        text.close_cell();
        assert_eq!(text.as_str(), "ab<br>c d");
        text.push_str("123").unwrap();
        assert_matches!(
            text.push_str("x"),
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, max: 12 })
        );
    }

    #[test]
    fn a_take_back_restores_the_whitespace_a_cell_line_held() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut text = Text::new(&meter);
        text.open_cell();
        text.push_str("a ").unwrap();
        assert!(!text.attempt(|text| text.push_str("\nb").map(|()| false)).unwrap());
        text.open_cell();
        text.push_str(" \n ").unwrap();
        text.close_cell();
        text.push_str("c").unwrap();
        text.close_cell();
        assert_eq!(text.as_str(), "a c");
    }

    #[test]
    fn whitespace_a_line_drops_inside_an_attempt_is_released_past_what_it_takes_back() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, 32);
        let mut text = Text::new(&meter);
        text.open_cell();
        let lines = text.attempt(|text| {
            for _ in 0..4 {
                text.push_str("x          \n")?;
            }
            Ok(true)
        });
        assert!(lines.unwrap());
        assert_eq!((text.as_str(), meter.held.get()), ("x<br>x<br>x<br>x", 16));
        drop(text);
        let mut text = Text::new(&meter);
        text.open_cell();
        text.push_str("a ").unwrap();
        let taken_back = text.attempt(|text| {
            text.push_str("b\u{3000}\u{3000}\nc")?;
            assert_eq!(text.space, "\u{3000}");
            Ok(false)
        });
        assert!(!taken_back.unwrap());
        text.push_str("d").unwrap();
        text.close_cell();
        assert_eq!((text.as_str(), meter.held.get()), ("a d", 3));
    }
}
