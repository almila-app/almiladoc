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

/// Read a named stream from an OLE2 compound file. A missing stream is
/// `MissingPart`; the read is hard-capped at `max_entry_bytes` so a corrupt
/// sector chain cannot expand without bound.
pub(crate) fn read_ole_stream<R: Read + std::io::Seek>(
    parse_ctx: &ParseCtx,
    ole: &mut cfb::CompoundFile<R>,
    name: &str,
) -> Result<Vec<u8>, ConvertError> {
    let max = parse_ctx.limits().max_entry_bytes;
    let stream = ole.open_stream(name).map_err(|_| ConvertError::missing_part(name))?;
    let declared = stream.len();
    let bytes = read_capped(stream, declared, max, name, "unreadable stream")?;
    if bytes.len() as u64 > max {
        return Err(ConvertError::limit(Limit::EntryBytes, max));
    }
    Ok(bytes)
}

/// Read a stream the conversion can do without: absent or unreadable is
/// `None`, and only a fatal error propagates.
pub(crate) fn optional_ole_stream<R: Read + std::io::Seek>(
    parse_ctx: &ParseCtx,
    ole: &mut cfb::CompoundFile<R>,
    name: &str,
) -> Result<Option<Vec<u8>>, ConvertError> {
    unless_fatal(read_ole_stream(parse_ctx, ole, name))
}

fn unless_fatal(read: Result<Vec<u8>, ConvertError>) -> Result<Option<Vec<u8>>, ConvertError> {
    match read {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.is_fatal() => Err(e),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn integer_reads_reject_overflowing_offsets() {
        assert_eq!(get_u16(&[1, 2, 3, 4], usize::MAX), None);
        assert_eq!(get_u32(&[1, 2, 3, 4], usize::MAX), None);
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
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 999 }));
    }

    #[test]
    fn an_optional_stream_is_none_when_absent() {
        let mut ole = one_stream_ole("Data", b"abc");
        assert_eq!(
            optional_ole_stream(&ParseCtx::default(), &mut ole, "Data").unwrap().as_deref(),
            Some(&b"abc"[..])
        );
        assert_eq!(optional_ole_stream(&ParseCtx::default(), &mut ole, "Pictures").unwrap(), None);
    }

    #[test]
    fn optional_streams_propagate_only_fatal_errors() {
        for fatal in [
            ConvertError::limit(Limit::Allocation, 64),
            ConvertError::Cancelled,
            ConvertError::Internal,
        ] {
            assert!(unless_fatal(Err(fatal)).is_err());
        }
        for skipped in
            [ConvertError::missing_part("Data"), ConvertError::malformed_part("Data", "corrupt")]
        {
            assert!(matches!(unless_fatal(Err(skipped)), Ok(None)));
        }
    }
}
