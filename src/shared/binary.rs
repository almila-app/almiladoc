//! Shared primitives for the binary formats: the one home of little-endian
//! decoding, as bounds-checked random-access reads and a sequential
//! [`ByteReader`], and bounded OLE2 compound-file stream reading.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::package::archive::read_capped;
use std::io::Read;

/// Little-endian `u16` at `off`; `None` when out of bounds.
pub(crate) fn get_u16(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..)?.first_chunk().copied().map(u16::from_le_bytes)
}

/// Little-endian `u32` at `off`; `None` when out of bounds.
pub(crate) fn get_u32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..)?.first_chunk().copied().map(u32::from_le_bytes)
}

/// Little-endian `u64` at `off`; `None` when out of bounds.
pub(crate) fn get_u64(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..)?.first_chunk().copied().map(u64::from_le_bytes)
}

/// A sequential little-endian reader over a byte slice: every read returns
/// `None` when too few bytes remain and then leaves the position unchanged.
#[derive(Debug, Clone)]
pub(crate) struct ByteReader<'a> {
    rest: &'a [u8],
}

impl<'a> ByteReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.rest.split_first_chunk::<N>()?;
        self.rest = rest;
        Some(*head)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.array().map(|[byte]| byte)
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn f64(&mut self) -> Option<f64> {
        self.array().map(f64::from_le_bytes)
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.rest.split_at_checked(n)?;
        self.rest = rest;
        Some(head)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(drop)
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.rest
    }
}

/// The UTF-16LE code units of `bytes`; a trailing odd byte is dropped.
pub(crate) fn utf16le_units(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair))
}

/// The text of UTF-16 code units, each unpaired surrogate replaced, and
/// whether any was.
pub(crate) fn decode_utf16(units: impl IntoIterator<Item = u16>) -> (String, bool) {
    let mut replaced = false;
    let text = char::decode_utf16(units)
        .map(|unit| {
            unit.unwrap_or_else(|_| {
                replaced = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    (text, replaced)
}

/// Read a named stream from an OLE2 compound file. A missing stream is
/// `MissingPart` and an entry that is no stream `Malformed`; the read
/// is hard-capped at `max_entry_bytes` so a corrupt sector chain cannot
/// expand without bound.
pub(crate) fn read_ole_stream<R: Read + std::io::Seek>(
    parse_ctx: &ParseCtx<'_>,
    ole: &mut cfb::CompoundFile<R>,
    name: &str,
) -> Result<Vec<u8>, ConvertError> {
    let max = parse_ctx.limits().max_entry_bytes;
    let stream = ole.open_stream(name).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ConvertError::missing_part(name),
        _ => ConvertError::malformed_part(name, "unreadable stream"),
    })?;
    let declared = stream.len();
    let bytes = read_capped(stream, declared, max, name, "unreadable stream")?;
    if bytes.len() as u64 > max {
        return Err(ConvertError::limit(Limit::EntryBytes, max));
    }
    Ok(bytes)
}

/// A stream the conversion can do without, as read.
#[derive(Debug)]
pub(crate) enum OptionalStream {
    Absent,
    Unreadable,
    Read(Vec<u8>),
}

impl OptionalStream {
    /// The stream's bytes, empty when it is absent or unreadable.
    pub(crate) fn or_empty(self) -> Vec<u8> {
        match self {
            Self::Read(bytes) => bytes,
            Self::Absent | Self::Unreadable => Vec::new(),
        }
    }
}

/// Read a stream the conversion can do without; only a fatal error
/// propagates.
pub(crate) fn optional_ole_stream<R: Read + std::io::Seek>(
    parse_ctx: &ParseCtx<'_>,
    ole: &mut cfb::CompoundFile<R>,
    name: &str,
) -> Result<OptionalStream, ConvertError> {
    unless_fatal(read_ole_stream(parse_ctx, ole, name))
}

fn unless_fatal(read: Result<Vec<u8>, ConvertError>) -> Result<OptionalStream, ConvertError> {
    match read {
        Ok(bytes) => Ok(OptionalStream::Read(bytes)),
        Err(e) if e.is_fatal() => Err(e),
        Err(ConvertError::MissingPart { .. }) => Ok(OptionalStream::Absent),
        Err(_) => Ok(OptionalStream::Unreadable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;
    use std::io::{Cursor, Write};

    fn naive(bytes: &[u8], off: usize, width: usize) -> Option<u64> {
        let field = bytes.get(off..off.checked_add(width)?)?;
        Some(field.iter().rev().fold(0, |value, &byte| (value << 8) | u64::from(byte)))
    }

    #[test]
    fn random_access_reads_decode_little_endian_up_to_the_exact_end() {
        let bytes: Vec<u8> = (1..=11).collect();
        for off in 0..=bytes.len() + 1 {
            assert_eq!(get_u16(&bytes, off).map(u64::from), naive(&bytes, off, 2), "{off}");
            assert_eq!(get_u32(&bytes, off).map(u64::from), naive(&bytes, off, 4), "{off}");
            assert_eq!(get_u64(&bytes, off), naive(&bytes, off, 8), "{off}");
        }
        assert_eq!(get_u64(&bytes, 3), Some(0x0B0A_0908_0706_0504));
        assert_eq!(get_u64(&bytes, 4), None);
        for off in [usize::MAX - 1, usize::MAX] {
            assert_eq!(get_u16(&bytes, off), None);
            assert_eq!(get_u32(&bytes, off), None);
            assert_eq!(get_u64(&bytes, off), None);
        }
    }

    #[test]
    fn a_reader_over_nothing_reads_only_nothing() {
        let mut reader = ByteReader::new(&[]);
        assert_eq!(reader.u8(), None);
        assert_eq!(reader.u16(), None);
        assert_eq!(reader.u32(), None);
        assert_eq!(reader.f64().map(f64::to_bits), None);
        assert_eq!(reader.array::<1>(), None);
        assert_eq!(reader.skip(1), None);
        assert_eq!(reader.take(0), Some(&[][..]));
        assert_eq!(reader.array::<0>(), Some([]));
        assert_eq!(reader.rest(), [0u8; 0]);
    }

    #[test]
    fn a_read_that_fits_exactly_succeeds_and_one_byte_short_moves_nothing() {
        let bytes = [0x78, 0x56, 0x34, 0x12, 0xAA];
        let mut reader = ByteReader::new(&bytes);
        assert_eq!(reader.u32(), Some(0x1234_5678));
        assert_eq!(reader.u16(), None);
        assert_eq!(reader.rest(), &[0xAA][..]);
        assert_eq!(reader.u8(), Some(0xAA));
        assert_eq!(reader.rest(), [0u8; 0]);
        let mut reader = ByteReader::new(&bytes[..3]);
        assert_eq!(reader.u32(), None);
        assert_eq!(reader.take(4), None);
        assert_eq!(reader.array::<4>(), None);
        assert_eq!(reader.rest(), &bytes[..3]);
        assert_eq!(reader.take(3), Some(&bytes[..3]));
    }

    #[test]
    fn a_take_or_skip_past_any_length_fails_in_place() {
        let mut reader = ByteReader::new(&[1, 2, 3]);
        reader.skip(1).unwrap();
        assert_eq!(reader.take(usize::MAX), None);
        assert_eq!(reader.skip(usize::MAX), None);
        assert_eq!(reader.skip(3), None);
        assert_eq!(reader.rest(), &[2, 3][..]);
    }

    #[test]
    fn floats_decode_little_endian_bit_for_bit() {
        for value in [1.5f64, -0.0, f64::from_bits(0x7FF8_0000_0000_0001), f64::NEG_INFINITY] {
            let bytes = value.to_bits().to_le_bytes();
            let mut reader = ByteReader::new(&bytes);
            assert_eq!(reader.f64().map(f64::to_bits), Some(value.to_bits()));
        }
        let mut reader = ByteReader::new(&[0, 0, 0, 0, 0, 0, 0xF8, 0x3F]);
        assert_eq!(reader.f64(), Some(1.5));
    }

    #[test]
    fn sequential_reads_equal_random_access_at_every_offset() {
        let bytes: Vec<u8> = (0u8..23).map(|i| i.wrapping_mul(37).wrapping_add(5)).collect();
        for off in 0..=bytes.len() {
            let mut reader = ByteReader::new(&bytes);
            reader.skip(off).unwrap();
            assert_eq!(reader.clone().u16(), get_u16(&bytes, off), "{off}");
            assert_eq!(reader.u32(), get_u32(&bytes, off), "{off}");
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Array2,
        U8,
        U16,
        U32,
        F64,
        Take(usize),
        Skip(usize),
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::{Just, Strategy, prop_oneof};
        let len = || prop_oneof![0..12usize, Just(usize::MAX)];
        prop_oneof![
            Just(Op::Array2),
            Just(Op::U8),
            Just(Op::U16),
            Just(Op::U32),
            Just(Op::F64),
            len().prop_map(Op::Take),
            len().prop_map(Op::Skip),
        ]
    }

    proptest::proptest! {
        #[test]
        fn a_reader_matches_a_naive_model(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..40),
            ops in proptest::collection::vec(op(), 0..24),
        ) {
            let mut reader = ByteReader::new(&bytes);
            let mut pos = 0usize;
            for op in ops {
                let width = match op {
                    Op::Array2 | Op::U16 => 2,
                    Op::U8 => 1,
                    Op::U32 => 4,
                    Op::F64 => 8,
                    Op::Take(n) | Op::Skip(n) => n,
                };
                let expected =
                    pos.checked_add(width).and_then(|end| bytes.get(pos..end)).map(<[u8]>::to_vec);
                let got = match op {
                    Op::Array2 => reader.array::<2>().map(|a| a.to_vec()),
                    Op::U8 => reader.u8().map(|v| vec![v]),
                    Op::U16 => reader.u16().map(|v| v.to_le_bytes().to_vec()),
                    Op::U32 => reader.u32().map(|v| v.to_le_bytes().to_vec()),
                    Op::F64 => reader.f64().map(|v| v.to_bits().to_le_bytes().to_vec()),
                    Op::Take(n) => reader.take(n).map(<[u8]>::to_vec),
                    Op::Skip(n) => reader.skip(n).map(|()| bytes[pos..pos + n].to_vec()),
                };
                proptest::prop_assert_eq!(&got, &expected);
                if got.is_some() {
                    pos += width;
                }
                proptest::prop_assert_eq!(reader.rest(), &bytes[pos..]);
            }
        }
    }

    #[test]
    fn a_decode_says_whether_it_replaced_a_unit() {
        assert_eq!(decode_utf16([0xD800, 0xDC00, 0x61]), ("\u{10000}a".to_string(), false));
        assert_eq!(decode_utf16([0xDC00, 0x61]), ("\u{FFFD}a".to_string(), true));
        assert_eq!(decode_utf16([0x61, 0xD800]), ("a\u{FFFD}".to_string(), true));
    }

    fn one_stream_ole(name: &str, bytes: &[u8]) -> cfb::CompoundFile<Cursor<Vec<u8>>> {
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_stream(name).unwrap().write_all(bytes).unwrap();
        ole
    }

    #[test]
    fn a_stream_read_reserves_its_declared_size() {
        let mut ole = one_stream_ole("Data", &[7u8; 1000]);
        assert_eq!(
            read_ole_stream(&ParseCtx::default(), &mut ole, "Data").unwrap().capacity(),
            1000
        );
    }

    #[test]
    fn the_stream_cap_is_the_contexts() {
        let mut ole = one_stream_ole("Data", &[7u8; 1000]);
        let mut limits =
            crate::limits::Limits { max_entry_bytes: 1000, ..crate::limits::Limits::default() };
        read_ole_stream(&ParseCtx::new(&limits), &mut ole, "Data").unwrap();
        limits.max_entry_bytes = 999;
        let err = optional_ole_stream(&ParseCtx::new(&limits), &mut ole, "Data").unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 999 });
    }

    #[test]
    fn an_optional_stream_is_absent_only_when_no_entry_names_it() {
        let mut ole = one_stream_ole("Data", b"abc");
        ole.create_storage("Pictures").unwrap();
        let ctx = ParseCtx::default();
        let read = |ole: &mut cfb::CompoundFile<_>, name| optional_ole_stream(&ctx, ole, name);
        assert_matches!(read(&mut ole, "Data"), Ok(OptionalStream::Read(b)) if b == b"abc");
        assert_matches!(read(&mut ole, "Current User"), Ok(OptionalStream::Absent));
        assert_matches!(read(&mut ole, "Pictures"), Ok(OptionalStream::Unreadable));
    }

    #[test]
    fn optional_streams_propagate_only_fatal_errors() {
        assert_matches!(
            unless_fatal(Err(ConvertError::limit(Limit::Allocation, 64))),
            Err(ConvertError::ResourceLimit { limit: Limit::Allocation, max: 64 })
        );
        assert_matches!(unless_fatal(Err(ConvertError::Cancelled)), Err(ConvertError::Cancelled));
        assert_matches!(unless_fatal(Err(ConvertError::Internal)), Err(ConvertError::Internal));
        let absent = unless_fatal(Err(ConvertError::missing_part("Data")));
        assert_matches!(absent, Ok(OptionalStream::Absent));
        let corrupt = unless_fatal(Err(ConvertError::malformed_part("Data", "corrupt")));
        assert_matches!(corrupt, Ok(OptionalStream::Unreadable));
    }
}
