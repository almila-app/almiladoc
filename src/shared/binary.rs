//! Shared primitives for the legacy OLE2 binary formats (DOC, PPT):
//! checked little-endian integer readers and bounded compound-file stream
//! reading.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::package::archive::read_capped;
use std::io::Read;

/// Little-endian `u16` at `off`; `None` when out of bounds.
pub(crate) fn get_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..)?.get(..2)?.try_into().ok()?))
}

/// Little-endian `u32` at `off`; `None` when out of bounds.
pub(crate) fn get_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..)?.get(..4)?.try_into().ok()?))
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
    parse_ctx: &ParseCtx,
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
            OptionalStream::Read(bytes) => bytes,
            OptionalStream::Absent | OptionalStream::Unreadable => Vec::new(),
        }
    }
}

/// Read a stream the conversion can do without; only a fatal error
/// propagates.
pub(crate) fn optional_ole_stream<R: Read + std::io::Seek>(
    parse_ctx: &ParseCtx,
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

    #[test]
    fn integer_reads_reject_overflowing_offsets() {
        assert_eq!(get_u16(&[1, 2, 3, 4], usize::MAX), None);
        assert_eq!(get_u32(&[1, 2, 3, 4], usize::MAX), None);
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
        assert!(read_ole_stream(&ParseCtx::new(&limits), &mut ole, "Data").is_ok());
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
