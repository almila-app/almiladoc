//! The bounded writer: one Markdown buffer behind a stack of line prefixes,
//! and the meter that counts every byte it writes, or holds before writing,
//! against [`Limits::max_output_bytes`](crate::Limits::max_output_bytes) and
//! charges the work ledger a unit per 64 bytes written.

use std::cell::Cell;

use crate::error::ConvertError;
use crate::ledger::Ledger;
use crate::limits::Limit;

/// Markdown bytes written per unit of work.
const BYTES_PER_UNIT: u64 = 64;

/// The output budget of one render: bytes written to the Markdown, and bytes
/// held in [`Text`] buffers before they are written, together never past the
/// maximum; one unit of work per [`BYTES_PER_UNIT`] bytes written.
pub(crate) struct Meter<'l> {
    ledger: &'l Ledger<'l>,
    max: u64,
    written: Cell<u64>,
    held: Cell<u64>,
}

impl<'l> Meter<'l> {
    pub(crate) fn new(ledger: &'l Ledger<'l>, max: u64) -> Self {
        Meter { ledger, max, written: Cell::new(0), held: Cell::new(0) }
    }

    fn admit(&self, bytes: usize) -> Result<u64, ConvertError> {
        let bytes = bytes as u64;
        let total = self.written.get().saturating_add(self.held.get()).saturating_add(bytes);
        if total > self.max {
            return Err(ConvertError::limit(Limit::OutputBytes, self.max));
        }
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

    fn write(&self, bytes: usize) -> Result<(), ConvertError> {
        let bytes = self.admit(bytes)?;
        let before = self.written.get();
        let after = before.saturating_add(bytes);
        self.written.set(after);
        let units = after / BYTES_PER_UNIT - before / BYTES_PER_UNIT;
        if units > 0 { self.ledger.charge(units) } else { Ok(()) }
    }
}

/// Rendered text held before it is written: a paragraph to trim, a table
/// cell, a list marker.
pub(crate) struct Text<'m> {
    buf: String,
    meter: &'m Meter<'m>,
}

impl<'m> Text<'m> {
    pub(crate) fn new(meter: &'m Meter<'m>) -> Self {
        Text { buf: String::new(), meter }
    }

    pub(crate) fn meter(&self) -> &'m Meter<'m> {
        self.meter
    }

    pub(crate) fn push_str(&mut self, s: &str) -> Result<(), ConvertError> {
        self.meter.hold(s.len())?;
        self.buf.push_str(s);
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

    /// What was pushed since `mark`, a length this text had.
    pub(crate) fn since(&self, mark: usize) -> &str {
        self.buf.get(mark..).unwrap_or_default()
    }

    /// Drop what was pushed since `mark`, a length this text had.
    pub(crate) fn truncate(&mut self, mark: usize) {
        let before = self.buf.len();
        self.buf.truncate(mark);
        self.meter.release(before.saturating_sub(self.buf.len()));
    }

    /// Trim the whitespace at both edges of what was pushed since `mark`.
    pub(crate) fn trim_since(&mut self, mark: usize) {
        let tail = self.since(mark);
        let lead = tail.len().saturating_sub(tail.trim_start().len());
        let kept = tail.trim().len();
        self.buf.drain(mark..mark.saturating_add(lead));
        self.meter.release(lead);
        self.truncate(mark.saturating_add(kept));
    }

    pub(crate) fn into_string(mut self) -> String {
        let buf = std::mem::take(&mut self.buf);
        self.meter.release(buf.len());
        buf
    }
}

impl Drop for Text<'_> {
    fn drop(&mut self) {
        self.meter.release(self.buf.len());
    }
}

/// What opens the lines of a block quote or a list item.
enum Prefix<'m> {
    /// `> ` on every line, `>` alone on an empty one.
    Quote,
    /// The marker on the first line that shows anything, spaces as wide as
    /// `indent` on the lines after it, nothing on an empty one.
    Marker { first: Option<Text<'m>>, indent: usize },
}

/// Line breaks owed before the next byte written, the empty lines among them
/// prefixed as at `depth` prefixes.
struct Gap {
    breaks: u8,
    depth: usize,
}

/// The Markdown being written: each line opens with the prefixes of the
/// containers it sits in, written as the line's first byte arrives.
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
        Sink {
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

    /// Owe `breaks` line breaks before the next byte, as between blocks or
    /// list items; owing them again replaces them.
    pub(crate) fn gap(&mut self, breaks: u8) {
        self.gap = Some(Gap { breaks, depth: self.prefixes.len() });
    }

    /// Forget the line breaks owed, as after the last block of a container.
    pub(crate) fn drop_gap(&mut self) {
        self.gap = None;
    }

    pub(crate) fn push_quote(&mut self) {
        self.prefixes.push(Prefix::Quote);
    }

    /// Open the lines that follow with `marker`, then with spaces as wide as
    /// `indent`.
    pub(crate) fn push_marker(&mut self, marker: Text<'m>, indent: usize) {
        self.prefixes.push(Prefix::Marker { first: Some(marker), indent });
    }

    /// Close the innermost prefix; a marker no line showed is dropped.
    pub(crate) fn pop(&mut self) {
        self.prefixes.pop();
        self.shown = self.shown.min(self.prefixes.len());
    }

    /// Close the innermost prefix, writing its marker on a line of its own
    /// when no line showed it: a list item keeps its marker when empty.
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

    /// The Markdown, ending in a line break unless it is empty.
    pub(crate) fn finish(mut self) -> Result<String, ConvertError> {
        if std::mem::take(&mut self.cr) {
            self.emit("\r")?;
        }
        if !self.out.is_empty() {
            self.emit("\n")?;
        }
        Ok(self.out)
    }

    fn flush_gap(&mut self) -> Result<(), ConvertError> {
        if let Some(Gap { breaks, depth }) = self.gap.take() {
            for _ in 0..breaks {
                self.end_line(depth)?;
            }
        }
        Ok(())
    }

    /// Write one line's worth of `seg`; inside a container a carriage return
    /// before a line break is dropped, as a CRLF line ending, and kept
    /// everywhere else.
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

    /// Show the prefixes this line has not shown yet.
    fn open(&mut self) -> Result<(), ConvertError> {
        let Sink { out, meter, prefixes, shown, .. } = self;
        for prefix in prefixes.iter_mut().skip(*shown) {
            match prefix {
                Prefix::Quote => emit(out, meter, "> ")?,
                Prefix::Marker { first, indent } => match first.take() {
                    Some(marker) => emit(out, meter, &marker.into_string())?,
                    None => emit_spaces(out, meter, *indent)?,
                },
            }
        }
        self.shown = self.prefixes.len();
        self.fresh = false;
        Ok(())
    }

    /// End the line; an empty one shows the prefixes up to the innermost
    /// quote among the first `depth`, that quote as `>` alone.
    fn end_line(&mut self, depth: usize) -> Result<(), ConvertError> {
        if std::mem::take(&mut self.cr) && depth == 0 {
            self.emit("\r")?;
        }
        if self.fresh {
            let Sink { out, meter, prefixes, .. } = self;
            let open = prefixes.iter().take(depth);
            if let Some(quote) = open.clone().rposition(|p| matches!(p, Prefix::Quote)) {
                for prefix in open.take(quote) {
                    match prefix {
                        Prefix::Quote => emit(out, meter, "> ")?,
                        Prefix::Marker { indent, .. } => emit_spaces(out, meter, *indent)?,
                    }
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

fn emit_spaces(out: &mut String, meter: &Meter<'_>, count: usize) -> Result<(), ConvertError> {
    meter.write(count)?;
    out.extend(std::iter::repeat_n(' ', count));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        held.push_str("12345").unwrap();
        sink.write("1234").unwrap();
        let refused = held.push_str("12");
        assert!(matches!(
            refused,
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, max: 10 })
        ));
        assert_eq!(held.as_str(), "12345");
        held.truncate(3);
        held.push_str("12").unwrap();
        assert!(sink.write("12").is_err());
        assert_eq!(sink.len(), 4);
        drop(held);
        sink.write("123456").unwrap();
        assert!(sink.write("1").is_err());
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
        assert!(matches!(
            item(len - 1),
            Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. })
        ));
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
        assert!(matches!(tripped, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 4 })));
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
        assert!(matches!(sink.write("x"), Err(ConvertError::Cancelled)));
    }

    #[test]
    fn trimming_held_text_releases_what_it_drops() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, 12);
        let mut text = Text::new(&meter);
        text.push_str("ab").unwrap();
        text.push_str("  c d \n").unwrap();
        text.trim_since(2);
        assert_eq!(text.as_str(), "abc d");
        text.push_str("1234567").unwrap();
        assert!(text.push_str("x").is_err());
    }
}
