//! ZIP archive access with decompression limits.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::package::xml::{Element, parse_xml};
use std::collections::HashMap;
use std::io::{Cursor, ErrorKind, Read};
use std::rc::Rc;

/// The family of a ZIP-based package: OPC, OpenDocument or EPUB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    /// Open Packaging Conventions: DOCX, PPTX, XLSX and XLSB.
    Opc,
    /// OpenDocument: ODT, ODS and ODP.
    Odf,
    /// EPUB's Open Container Format.
    Epub,
}

/// A ZIP-based document package (OOXML, ODF, EPUB).
pub struct Package<'a> {
    parse_ctx: &'a ParseCtx<'a>,
    #[expect(dead_code, reason = "no reader chooses an XML dialect by package kind yet")]
    kind: PackageKind,
    zip: zip::ZipArchive<Cursor<&'a [u8]>>,
    total_read: u64,
    /// Decompressed parts by normalized name: repeated references are served
    /// from the cache instead of re-decompressing and re-charging the
    /// total-bytes budget (which would falsely trip on valid documents that
    /// reference one part many times). Bounded by `max_total_bytes`. Buffers
    /// are shared (`Rc`), so a cache hit never copies the bytes.
    cache: HashMap<String, Rc<[u8]>>,
}

impl<'a> Package<'a> {
    pub fn open(
        parse_ctx: &'a ParseCtx<'a>,
        bytes: &'a [u8],
        kind: PackageKind,
    ) -> Result<Self, ConvertError> {
        let zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|_| ConvertError::malformed("not a readable zip archive"))?;
        let max = parse_ctx.limits().max_entry_count;
        if zip.len() as u64 > max {
            return Err(ConvertError::limit(Limit::EntryCount, max));
        }
        Ok(Package { parse_ctx, kind, zip, total_read: 0, cache: HashMap::new() })
    }

    /// Read a part's bytes. `Ok(None)` means the part is absent (a valid
    /// state for optional parts); `Err` means it exists but cannot be read.
    /// Callers apply the unified policy: skip + log when useful output
    /// remains, propagate when the part is the primary content.
    pub fn part(&mut self, name: &str) -> Result<Option<Rc<[u8]>>, ConvertError> {
        // OPC part URIs may carry a leading slash; entries never do.
        let name = name.trim_start_matches('/');
        if let Some(bytes) = self.cache.get(name) {
            return Ok(Some(Rc::clone(bytes)));
        }
        let mut file = match self.zip.by_name(name) {
            Ok(f) => f,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(_) => return Err(ConvertError::malformed_part(name, "unreadable archive entry")),
        };
        let limits = self.parse_ctx.limits();
        if file.size() > limits.max_entry_bytes {
            return Err(ConvertError::limit(Limit::EntryBytes, limits.max_entry_bytes));
        }
        // The declared size can lie; read through a hard-capped reader. The
        // cap is whichever budget has less room: the per-entry limit or what
        // remains of the whole-archive total.
        let remaining_total = limits.max_total_bytes.saturating_sub(self.total_read);
        let cap = limits.max_entry_bytes.min(remaining_total);
        let bytes = read_entry(&mut file, cap, name)?;
        let read = bytes.len() as u64;
        if read > cap {
            return Err(if remaining_total < limits.max_entry_bytes {
                ConvertError::limit(Limit::TotalBytes, limits.max_total_bytes)
            } else {
                ConvertError::limit(Limit::EntryBytes, limits.max_entry_bytes)
            });
        }
        self.total_read += read;
        let bytes: Rc<[u8]> = Rc::from(bytes);
        self.cache.insert(name.to_string(), Rc::clone(&bytes));
        Ok(Some(bytes))
    }

    /// Read a part that must exist for any meaningful output.
    /// True when a part exists, without reading (or budget-charging) it.
    pub fn has_part(&self, name: &str) -> bool {
        self.zip.index_for_name(name.trim_start_matches('/')).is_some()
    }

    pub fn required_part(&mut self, name: &str) -> Result<Rc<[u8]>, ConvertError> {
        self.part(name)?.ok_or_else(|| ConvertError::missing_part(name))
    }

    /// Read an optional part under the unified recovery policy: absent is a
    /// valid state (`Ok(None)`, silent); an unreadable part is skipped with a
    /// log (`Ok(None)`); fatal resource-limit errors always propagate.
    pub fn optional_part(&mut self, name: &str) -> Result<Option<Rc<[u8]>>, ConvertError> {
        skip_unreadable(name, self.part(name))
    }

    /// Read and parse an optional XML part under the unified recovery policy:
    /// absent -> `Ok(None)`; unreadable or corrupt -> skipped with a log;
    /// fatal resource-limit errors always propagate.
    pub fn optional_xml_part(&mut self, name: &str) -> Result<Option<Element>, ConvertError> {
        let Some(bytes) = self.optional_part(name)? else {
            return Ok(None);
        };
        skip_unreadable(name, parse_xml(self.parse_ctx, &bytes).map(Some))
    }

    /// Read and parse an XML part that must exist and parse for any
    /// meaningful output.
    pub fn required_xml_part(&mut self, name: &str) -> Result<Element, ConvertError> {
        let bytes = self.required_part(name)?;
        parse_xml(self.parse_ctx, &bytes)
    }
}

fn read_entry<R: Read>(
    file: &mut zip::read::ZipFile<'_, R>,
    cap: u64,
    name: &str,
) -> Result<Vec<u8>, ConvertError> {
    let declared = file.size();
    read_capped(file, declared, cap, name, "corrupt archive entry")
}

fn skip_unreadable<T>(
    name: &str,
    read: Result<Option<T>, ConvertError>,
) -> Result<Option<T>, ConvertError> {
    match read {
        Err(e) if e.is_fatal() => Err(e),
        Err(e) => {
            log::warn!("skipping unreadable part {name}: {e}");
            Ok(None)
        }
        read => read,
    }
}

/// Reads at most `cap + 1` bytes of `part` after reserving
/// `min(declared, cap)`, so an overrun shows as a length above `cap` and a
/// refused allocation is never mistaken for a corrupt input.
pub(crate) fn read_capped(
    reader: impl Read,
    declared: u64,
    cap: u64,
    part: &str,
    reason: &'static str,
) -> Result<Vec<u8>, ConvertError> {
    let reserve = declared.min(cap);
    let mut bytes = Vec::new();
    usize::try_from(reserve)
        .ok()
        .and_then(|n| bytes.try_reserve_exact(n).ok())
        .ok_or_else(|| ConvertError::limit(Limit::Allocation, reserve))?;
    let bound = cap.saturating_add(1);
    reader.take(bound).read_to_end(&mut bytes).map_err(|e| match e.kind() {
        ErrorKind::OutOfMemory => ConvertError::limit(Limit::Allocation, bound),
        _ => ConvertError::malformed_part(part, reason),
    })?;
    Ok(bytes)
}

/// A zip-open failure on OOXML input may actually be an OLE compound file:
/// an encrypted package, or a legacy binary document with the wrong
/// extension.
pub fn probe_ole(bytes: &[u8]) -> Option<ConvertError> {
    const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if !bytes.starts_with(&OLE_MAGIC) {
        return None;
    }
    let cursor = Cursor::new(bytes);
    if let Ok(file) = cfb::CompoundFile::open(cursor)
        && (file.exists("EncryptionInfo") || file.exists("EncryptedPackage"))
    {
        return Some(ConvertError::Encrypted);
    }
    Some(ConvertError::malformed("OLE compound document where an OOXML package was expected"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use std::io::Write;

    fn one_part_zip(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(bytes).unwrap();
        w.finish().unwrap().into_inner()
    }

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
            assert!(
                matches!(err, ConvertError::ResourceLimit { limit: Limit::Allocation, max: 65 }),
                "{err:?}"
            );
        }
    }

    #[test]
    fn a_refused_reservation_is_a_resource_limit() {
        let huge = u64::try_from(isize::MAX).unwrap() + 1;
        let err = read_capped(Cursor::new(b"x"), huge, huge, "a.bin", "corrupt").unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::Allocation, max } if max == huge),
            "{err:?}"
        );
    }

    #[test]
    fn other_read_failures_stay_malformed() {
        for prefix in [b"".as_slice(), b"partial content".as_slice()] {
            let reader = failing_after(prefix, ErrorKind::InvalidData);
            let err = read_capped(reader, 4, 64, "word/document.xml", "corrupt").unwrap_err();
            assert!(
                matches!(&err, ConvertError::Malformed { part: Some(part), reason: "corrupt" }
                    if part.as_str() == "word/document.xml"),
                "{err:?}"
            );
        }
    }

    #[test]
    fn an_entry_read_reserves_its_declared_size() {
        let bytes = one_part_zip("a.bin", &[7u8; 1000]);
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let mut file = zip.by_name("a.bin").unwrap();
        assert_eq!(read_entry(&mut file, 4096, "a.bin").unwrap().capacity(), 1000);
    }

    #[test]
    fn a_capped_read_stops_one_byte_past_the_cap() {
        let bytes = read_capped(Cursor::new([7u8; 100]), 10, 64, "a.bin", "corrupt").unwrap();
        assert_eq!(bytes.len(), 65);
        let bytes = read_capped(Cursor::new([7u8; 10]), 100, 64, "a.bin", "corrupt").unwrap();
        assert_eq!(bytes.len(), 10);
    }

    #[test]
    fn optional_reads_propagate_fatal_errors_and_skip_the_rest() {
        let refused = || Err::<Option<()>, _>(ConvertError::limit(Limit::Allocation, 64));
        assert!(matches!(
            skip_unreadable("a.xml", refused()),
            Err(ConvertError::ResourceLimit { limit: Limit::Allocation, max: 64 })
        ));
        for fatal in [ConvertError::Cancelled, ConvertError::Internal] {
            assert!(skip_unreadable::<()>("a.xml", Err(fatal)).is_err());
        }
        let corrupt = Err(ConvertError::malformed_part("a.xml", "corrupt"));
        assert!(matches!(skip_unreadable::<()>("a.xml", corrupt), Ok(None)));
        assert!(matches!(skip_unreadable("a.xml", Ok(Some(1))), Ok(Some(1))));
    }

    #[test]
    fn a_corrupt_entry_is_neither_cached_nor_charged() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("word/document.xml", stored).unwrap();
        writer.write_all(b"<document/>").unwrap();
        let mut data = writer.finish().unwrap().into_inner();
        let start = zip::ZipArchive::new(Cursor::new(data.as_slice()))
            .unwrap()
            .by_name("word/document.xml")
            .unwrap()
            .data_start()
            .unwrap();
        data[usize::try_from(start).unwrap()] ^= 1;
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("word/document.xml").unwrap_err();
        assert!(matches!(err, ConvertError::Malformed { reason: "corrupt archive entry", .. }));
        assert!(pkg.cache.is_empty());
        assert_eq!(pkg.total_read, 0);
        assert!(pkg.optional_part("word/document.xml").unwrap().is_none());
        assert!(pkg.optional_xml_part("word/document.xml").unwrap().is_none());
        assert_eq!(pkg.total_read, 0);
    }

    #[test]
    fn repeated_reads_are_cached_and_charged_once() {
        let data = one_part_zip("media/a.bin", &[7u8; 4096]);
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for _ in 0..5 {
            assert_eq!(pkg.part("media/a.bin").unwrap().unwrap().len(), 4096);
        }
        assert_eq!(pkg.total_read, 4096, "repeated reads must not re-charge the budget");
    }

    #[test]
    fn total_budget_exhaustion_reports_max_total_bytes() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["a.bin", "b.bin"] {
            w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(&[7u8; 4096]).unwrap();
        }
        let data = w.finish().unwrap().into_inner();
        let limits = Limits { max_total_bytes: 4096 + 100, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("a.bin").unwrap().is_some());
        let err = pkg.part("b.bin").unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::TotalBytes, max: 4196 }),
            "expected max_total_bytes, got: {err}"
        );
    }

    #[test]
    fn entry_count_and_size_limits_are_the_contexts() {
        let data = one_part_zip("a.bin", &[7u8; 4096]);
        let mut limits = Limits { max_entry_bytes: 4096, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        assert!(Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap().part("a.bin").is_ok());
        limits.max_entry_bytes = 4095;
        let parse_ctx = ParseCtx::new(&limits);
        let err =
            Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap().part("a.bin").unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 4095 }));

        let limits = Limits { max_entry_count: 1, ..Limits::default() };
        assert!(Package::open(&ParseCtx::new(&limits), &data, PackageKind::Opc).is_ok());
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["a.bin", "b.bin"] {
            w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        }
        let two = w.finish().unwrap().into_inner();
        let err = Package::open(&ParseCtx::new(&limits), &two, PackageKind::Opc).err().unwrap();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryCount, max: 1 }));
    }

    #[test]
    fn an_entry_past_its_declared_size_is_held_to_the_contexts_entry_limit() {
        let mut data = one_part_zip("a.bin", &[7u8; 4096]);
        let understate =
            |at: usize, data: &mut [u8]| data[at..at + 4].copy_from_slice(&16u32.to_le_bytes());
        let local = data.windows(4).position(|w| w == b"PK\x03\x04").unwrap();
        understate(local + 22, &mut data);
        let central = data.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        understate(central + 24, &mut data);
        let limits = Limits { max_entry_bytes: 64, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("a.bin").unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 64 }));
    }

    #[test]
    fn leading_slash_part_names_normalize() {
        let data = one_part_zip("word/document.xml", b"<x/>");
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("/word/document.xml").unwrap().is_some());
    }
}
