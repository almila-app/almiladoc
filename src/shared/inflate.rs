use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::{Limit, Limits};
use flate2::bufread::DeflateDecoder;
use std::io::{BufRead, Error, ErrorKind, Read, Take};

const CHUNK: usize = 64 * 1024;
const KIB: u64 = 1024;
const INPUT_UNIT: u64 = 2 * KIB;
pub(crate) const RATIO_FLOOR: u64 = 1024 * 1024;

/// Input a reader has consumed and bytes it has produced so far, those of a failed read too.
pub(crate) trait Metered: Read {
    fn consumed(&self) -> u64;
    fn produced(&self) -> u64;
}

impl<R: BufRead> Metered for DeflateDecoder<Stepped<R>> {
    fn consumed(&self) -> u64 {
        self.total_in()
    }

    fn produced(&self) -> u64 {
        self.total_out()
    }
}

impl<R: Metered> Metered for Take<R> {
    fn consumed(&self) -> u64 {
        self.get_ref().consumed()
    }

    fn produced(&self) -> u64 {
        self.get_ref().produced()
    }
}

/// Compressed input a [`CHUNK`] at a time: past each its decoder's read fails `WouldBlock` once,
/// so [`inflate_capped`] charges a stream that inflates nothing before reading on.
pub(crate) struct Stepped<R> {
    inner: R,
    left: usize,
}

pub(crate) fn deflated<R: BufRead>(source: R) -> DeflateDecoder<Stepped<R>> {
    DeflateDecoder::new(Stepped { inner: source, left: CHUNK })
}

impl<R: BufRead> Read for Stepped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.fill_buf()?.read(buf)?;
        self.consume(n);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Stepped<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.left == 0 {
            self.left = CHUNK;
            return Err(ErrorKind::WouldBlock.into());
        }
        let buf = self.inner.fill_buf()?;
        Ok(&buf[..buf.len().min(self.left)])
    }

    fn consume(&mut self, amt: usize) {
        self.left = self.left.saturating_sub(amt);
        self.inner.consume(amt);
    }
}

pub(crate) struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R> Counted<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self { inner, read: 0 }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

impl<R: Read> Metered for Counted<R> {
    fn consumed(&self) -> u64 {
        self.read
    }

    fn produced(&self) -> u64 {
        self.read
    }
}

/// Bytes the next inflation may produce: the entry limit, or what the total leaves.
pub(crate) fn room(parse_ctx: &ParseCtx<'_>) -> u64 {
    let limits = parse_ctx.limits();
    let left = limits.max_total_bytes.saturating_sub(parse_ctx.inflated_bytes());
    limits.max_entry_bytes.min(left)
}

/// Admits a whole read the closure makes at [`room`], refusing what it inflated past the entry
/// limit, else a total past its limit: a crossed limit outranks a stream that also breaks.
pub(crate) fn admitted<T>(
    parse_ctx: &ParseCtx<'_>,
    read: impl FnOnce() -> Result<T, ConvertError>,
) -> Result<T, ConvertError> {
    let before = parse_ctx.inflated_bytes();
    let read = read();
    if read.as_ref().is_err_and(ConvertError::is_fatal) {
        return read;
    }
    let limits = parse_ctx.limits();
    if parse_ctx.inflated_bytes() - before > limits.max_entry_bytes {
        return Err(ConvertError::limit(Limit::EntryBytes, limits.max_entry_bytes));
    }
    if parse_ctx.inflated_bytes() > limits.max_total_bytes {
        return Err(ConvertError::limit(Limit::TotalBytes, limits.max_total_bytes));
    }
    read
}

pub(crate) fn admit_ratio(
    limits: &Limits,
    compressed: u64,
    uncompressed: u64,
) -> Result<(), ConvertError> {
    let ratio = limits.max_compression_ratio;
    if uncompressed >= RATIO_FLOOR && uncompressed > compressed.saturating_mul(ratio) {
        return Err(ConvertError::limit(Limit::CompressionRatio, ratio));
    }
    Ok(())
}

fn reserved(declared: u64, cap: u64) -> Result<Vec<u8>, ConvertError> {
    let reserve = declared.min(cap);
    let mut bytes = Vec::new();
    usize::try_from(reserve)
        .ok()
        .and_then(|n| bytes.try_reserve_exact(n).ok())
        .ok_or_else(|| ConvertError::limit(Limit::Allocation, reserve))?;
    Ok(bytes)
}

fn failure(e: &Error, bound: u64, part: &str, reason: &'static str) -> ConvertError {
    match e.kind() {
        ErrorKind::OutOfMemory => ConvertError::limit(Limit::Allocation, bound),
        _ => ConvertError::malformed_part(part, reason),
    }
}

/// Reads at most `cap + 1` bytes of `part` after reserving `min(declared, cap)`, so an overrun
/// shows as a length above `cap` and a refused allocation is never mistaken for a corrupt input.
pub(crate) fn read_capped(
    reader: impl Read,
    declared: u64,
    cap: u64,
    part: &str,
    reason: &'static str,
) -> Result<Vec<u8>, ConvertError> {
    let mut bytes = reserved(declared, cap)?;
    let bound = cap.saturating_add(1);
    reader.take(bound).read_to_end(&mut bytes).map_err(|e| failure(&e, bound, part, reason))?;
    Ok(bytes)
}

/// [`read_capped`] counting what `source` produces, a failed stream's too, charging per KiB out
/// begun or 2 KiB in read, whichever is more, by chunk: cancellation and work act inside one entry.
pub(crate) fn inflate_capped(
    parse_ctx: &ParseCtx<'_>,
    mut source: impl Metered,
    declared: u64,
    cap: u64,
    part: &str,
    reason: &'static str,
) -> Result<Vec<u8>, ConvertError> {
    let mut bytes = reserved(declared, cap)?;
    let bound = cap.saturating_add(1);
    let (mut counted, mut charged) = (0, 0);
    loop {
        let want = bound.saturating_sub(bytes.len() as u64).min(CHUNK as u64);
        let read = (&mut source).take(want).read_to_end(&mut bytes);
        let produced = source.produced();
        parse_ctx.count_inflated_bytes(produced - counted);
        counted = produced;
        let due = (source.consumed() / INPUT_UNIT).max(produced.div_ceil(KIB));
        parse_ctx.charge(due - charged)?;
        charged = due;
        match read {
            Ok(got) if (got as u64) < want || bytes.len() as u64 >= bound => return Ok(bytes),
            Err(e) if e.kind() != ErrorKind::WouldBlock => {
                return Err(failure(&e, bound, part, reason));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::shared::test_support::{cancelled, empty_deflate_blocks, stored_deflate_stream};
    use std::assert_matches;
    use std::io::Cursor;
    use std::iter::repeat_n;

    struct FailingReader(ErrorKind);

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }
    }

    fn failing_after(prefix: &'static [u8], kind: ErrorKind) -> impl Read {
        Cursor::new(prefix).chain(FailingReader(kind))
    }

    #[test]
    fn an_allocation_failure_mid_read_is_a_resource_limit() {
        for prefix in [b"".as_slice(), b"partial content".as_slice()] {
            let reader = failing_after(prefix, ErrorKind::OutOfMemory);
            let err = read_capped(reader, 4, 64, "word/document.xml", "corrupt").unwrap_err();
            assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Allocation, max: 65 });
        }
    }

    #[test]
    fn a_refused_reservation_is_a_resource_limit() {
        let huge = u64::try_from(isize::MAX).unwrap() + 1;
        let err = read_capped(Cursor::new(b"x"), huge, huge, "a.bin", "corrupt").unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::Allocation, max } if max == huge
        );
    }

    #[test]
    fn other_read_failures_stay_malformed() {
        for prefix in [b"".as_slice(), b"partial content".as_slice()] {
            let reader = failing_after(prefix, ErrorKind::InvalidData);
            let err = read_capped(reader, 4, 64, "word/document.xml", "corrupt").unwrap_err();
            assert_matches!(
                &err,
                ConvertError::Malformed { part: Some(part), reason: "corrupt" }
                    if part.as_str() == "word/document.xml"
            );
        }
    }

    #[test]
    fn a_capped_read_stops_one_byte_past_the_cap() {
        let bytes = read_capped(Cursor::new([7u8; 100]), 10, 64, "a.bin", "corrupt").unwrap();
        assert_eq!(bytes.len(), 65);
        let bytes = read_capped(Cursor::new([7u8; 10]), 100, 64, "a.bin", "corrupt").unwrap();
        assert_eq!(bytes.len(), 10);
    }

    #[test]
    fn an_inflating_read_names_its_failures_like_a_capped_read() {
        let parse_ctx = ParseCtx::default();
        let source = Counted::new(failing_after(b"partial", ErrorKind::OutOfMemory));
        let err = inflate_capped(&parse_ctx, source, 4, 64, "a.bin", "corrupt").unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Allocation, max: 65 });
        let source = Counted::new(failing_after(b"partial", ErrorKind::InvalidData));
        let err = inflate_capped(&parse_ctx, source, 4, 64, "a.bin", "corrupt").unwrap_err();
        assert_matches!(err, ConvertError::Malformed { reason: "corrupt", .. });
        assert_eq!(parse_ctx.inflated_bytes(), 2 * 7);
    }

    #[test]
    fn compressed_input_that_inflates_to_nothing_is_charged() {
        let stream = empty_deflate_blocks(4 * CHUNK);
        let limits = Limits { max_work: 10, ..Limits::default() };
        let source = deflated(stream.as_slice());
        let err = inflate_capped(&ParseCtx::new(&limits), source, 0, 64, "a", "x").unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 10 });
        let parse_ctx = ParseCtx::default();
        let bytes =
            inflate_capped(&parse_ctx, deflated(stream.as_slice()), 0, 64, "a", "x").unwrap();
        assert_eq!((bytes.len(), parse_ctx.fuel()), (0, stream.len() as u64 / INPUT_UNIT));
    }

    #[test]
    fn a_stream_longer_than_what_it_inflates_charges_its_output() {
        let stream = stored_deflate_stream(repeat_n([7u8; 1024].as_slice(), 256));
        assert_eq!(stream.len(), 256 * (5 + 1024) + 5);
        let parse_ctx = ParseCtx::default();
        let bytes =
            inflate_capped(&parse_ctx, deflated(stream.as_slice()), 0, 1 << 20, "a", "x").unwrap();
        assert_eq!((bytes.len(), parse_ctx.fuel()), (256 * 1024, 256));
    }

    #[test]
    fn a_cancelled_inflation_leaves_compressed_input_unread() {
        let stream = empty_deflate_blocks(4 * CHUNK);
        let token = CancelToken::new();
        let mut unread = stream.as_slice();
        let err =
            inflate_capped(&cancelled(&token), deflated(&mut unread), 0, 64, "a", "x").unwrap_err();
        assert_matches!(err, ConvertError::Cancelled);
        assert_eq!(unread.len(), stream.len() - CHUNK);
    }

    #[test]
    fn an_inflating_read_stops_one_byte_past_the_cap_across_chunks() {
        let len = 3 * CHUNK;
        let parse_ctx = ParseCtx::default();
        let cap = 2 * CHUNK as u64 + 5;
        let bytes =
            inflate_capped(&parse_ctx, Counted::new(Cursor::new(vec![7u8; len])), 0, cap, "a", "x")
                .unwrap();
        assert_eq!(bytes.len() as u64, cap + 1);
        assert_eq!(parse_ctx.inflated_bytes(), cap + 1);
        assert_eq!(parse_ctx.fuel(), (cap + 1).div_ceil(KIB));
    }
}
