//! ZIP archive access with decompression limits.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
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

/// What a part's loss costs the output, named by the reader that asks for
/// it: it decides what an unreadable, recovered or undecodable part reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Content that renders in the region: losing it is `part_unreadable`.
    Content(Region),
    /// A part whose loss changes formatting alone.
    Formatting,
    /// A part whose loss the reader, or each consumer of the part, reports;
    /// its recovery and decoding still report in the region.
    Reported(Region),
    /// A part no content depends on.
    Probe,
}

impl Role {
    pub(crate) fn unreadable(self) -> Option<(DiagnosticCode, Region)> {
        match self {
            Role::Content(region) => Some((DiagnosticCode::PartUnreadable, region)),
            Role::Formatting => Some((DiagnosticCode::StylePartUnreadable, Region::Container)),
            Role::Reported(_) | Role::Probe => None,
        }
    }

    pub(crate) fn recovered(self) -> Option<(DiagnosticCode, Region)> {
        match self {
            Role::Content(region) | Role::Reported(region) => {
                Some((DiagnosticCode::RecoveredXml, region))
            }
            Role::Formatting => Some((DiagnosticCode::StylePartUnreadable, Region::Container)),
            Role::Probe => None,
        }
    }

    pub(crate) fn undecodable(self) -> Option<(DiagnosticCode, Region)> {
        match self {
            Role::Content(region) | Role::Reported(region) => {
                Some((DiagnosticCode::InvalidEncoding, region))
            }
            Role::Formatting => Some((DiagnosticCode::InvalidStyleEncoding, Region::Container)),
            Role::Probe => None,
        }
    }
}

pub(crate) fn report(parse_ctx: &ParseCtx, diagnostic: Option<(DiagnosticCode, Region)>) {
    if let Some((code, region)) = diagnostic {
        parse_ctx.emit(code, region);
    }
}

/// Whether an absent part is a loss: it is once a relationship or reference
/// names it, and never when only its conventional name pointed at it.
#[derive(Clone, Copy)]
enum Absent {
    Valid,
    Lost,
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
    /// Callers apply the unified policy: skip and report by role when useful
    /// output remains, propagate when the part is the primary content.
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
        let bytes = read_entry(self.parse_ctx, &mut file, cap, name)?;
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

    /// True when a part exists, without reading (or budget-charging) it.
    pub fn has_part(&self, name: &str) -> bool {
        self.zip.index_for_name(name.trim_start_matches('/')).is_some()
    }

    /// Read a part that must exist for any meaningful output.
    pub fn required_part(&mut self, name: &str) -> Result<Rc<[u8]>, ConvertError> {
        self.part(name)?.ok_or_else(|| ConvertError::missing_part(name))
    }

    /// Read an optional part a conventional name points at: absent is a
    /// valid state, and an unreadable part reports by `role` and reads as
    /// absent; a fatal error propagates.
    pub fn optional_part(
        &mut self,
        name: &str,
        role: Role,
    ) -> Result<Option<Rc<[u8]>>, ConvertError> {
        self.read_part(name, role, Absent::Valid)
    }

    /// Read a part a relationship or reference names: absent or unreadable,
    /// it reports by `role` and reads as absent; a fatal error propagates.
    pub fn related_part(
        &mut self,
        name: &str,
        role: Role,
    ) -> Result<Option<Rc<[u8]>>, ConvertError> {
        self.read_part(name, role, Absent::Lost)
    }

    /// [`Package::optional_part`], parsed as XML: a part that does not parse
    /// reports by `role` and reads as absent.
    pub fn optional_xml_part(
        &mut self,
        name: &str,
        role: Role,
    ) -> Result<Option<Element>, ConvertError> {
        self.read_xml_part(name, role, Absent::Valid)
    }

    /// [`Package::related_part`], parsed as XML: a part that does not parse
    /// reports by `role` and reads as absent.
    pub fn related_xml_part(
        &mut self,
        name: &str,
        role: Role,
    ) -> Result<Option<Element>, ConvertError> {
        self.read_xml_part(name, role, Absent::Lost)
    }

    /// Read and parse an XML part that must exist and parse for any
    /// meaningful output; its recovery and decoding report by `role`.
    pub fn required_xml_part(&mut self, name: &str, role: Role) -> Result<Element, ConvertError> {
        let bytes = self.required_part(name)?;
        parse_xml(self.parse_ctx, &bytes, role)
    }

    fn read_part(
        &mut self,
        name: &str,
        role: Role,
        absent: Absent,
    ) -> Result<Option<Rc<[u8]>>, ConvertError> {
        let read = self.part(name);
        if matches!((&read, absent), (Ok(None), Absent::Lost)) {
            report(self.parse_ctx, role.unreadable());
        }
        skip_unreadable(self.parse_ctx, role, read)
    }

    fn read_xml_part(
        &mut self,
        name: &str,
        role: Role,
        absent: Absent,
    ) -> Result<Option<Element>, ConvertError> {
        let Some(bytes) = self.read_part(name, role, absent)? else {
            return Ok(None);
        };
        skip_unreadable(self.parse_ctx, role, parse_xml(self.parse_ctx, &bytes, role).map(Some))
    }
}

fn read_entry<R: Read>(
    parse_ctx: &ParseCtx,
    file: &mut zip::read::ZipFile<'_, R>,
    cap: u64,
    name: &str,
) -> Result<Vec<u8>, ConvertError> {
    let declared = file.size();
    let mut counted = Counted { inner: file, read: 0 };
    let bytes = read_capped(&mut counted, declared, cap, name, "corrupt archive entry");
    parse_ctx.charge(counted.read.div_ceil(1024))?;
    bytes
}

struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

fn skip_unreadable<T>(
    parse_ctx: &ParseCtx,
    role: Role,
    read: Result<Option<T>, ConvertError>,
) -> Result<Option<T>, ConvertError> {
    match read {
        Err(e) if e.is_fatal() => Err(e),
        Err(_) => {
            report(parse_ctx, role.unreadable());
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
        let read = read_entry(&ParseCtx::default(), &mut file, 4096, "a.bin").unwrap();
        assert_eq!(read.capacity(), 1000);
    }

    #[test]
    fn a_capped_read_stops_one_byte_past_the_cap() {
        let bytes = read_capped(Cursor::new([7u8; 100]), 10, 64, "a.bin", "corrupt").unwrap();
        assert_eq!(bytes.len(), 65);
        let bytes = read_capped(Cursor::new([7u8; 10]), 100, 64, "a.bin", "corrupt").unwrap();
        assert_eq!(bytes.len(), 10);
    }

    fn reported(parse_ctx: &ParseCtx) -> Vec<(DiagnosticCode, Region, u64)> {
        parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect()
    }

    #[test]
    fn optional_reads_propagate_fatal_errors_and_skip_the_rest() {
        let parse_ctx = ParseCtx::default();
        let role = Role::Content(Region::Note);
        let refused = || Err::<Option<()>, _>(ConvertError::limit(Limit::Allocation, 64));
        assert!(matches!(
            skip_unreadable(&parse_ctx, role, refused()),
            Err(ConvertError::ResourceLimit { limit: Limit::Allocation, max: 64 })
        ));
        for fatal in [ConvertError::Cancelled, ConvertError::Internal] {
            assert!(skip_unreadable::<()>(&parse_ctx, role, Err(fatal)).is_err());
        }
        assert!(matches!(skip_unreadable(&parse_ctx, role, Ok(Some(1))), Ok(Some(1))));
        assert!(matches!(skip_unreadable::<()>(&parse_ctx, role, Ok(None)), Ok(None)));
        assert_eq!(reported(&parse_ctx), []);
        let corrupt = Err(ConvertError::malformed_part("a.xml", "corrupt"));
        assert!(matches!(skip_unreadable::<()>(&parse_ctx, role, corrupt), Ok(None)));
        assert_eq!(reported(&parse_ctx), [(DiagnosticCode::PartUnreadable, Region::Note, 1)]);
    }

    fn package_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in parts {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn corrupt_entry(name: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file(name, stored).unwrap();
        writer.write_all(b"<document/>").unwrap();
        let mut data = writer.finish().unwrap().into_inner();
        let start = zip::ZipArchive::new(Cursor::new(data.as_slice()))
            .unwrap()
            .by_name(name)
            .unwrap()
            .data_start()
            .unwrap();
        data[usize::try_from(start).unwrap()] ^= 1;
        data
    }

    const ROLES: [(Role, Option<(DiagnosticCode, Region)>); 5] = [
        (Role::Content(Region::Note), Some((DiagnosticCode::PartUnreadable, Region::Note))),
        (Role::Content(Region::Body), Some((DiagnosticCode::PartUnreadable, Region::Body))),
        (Role::Formatting, Some((DiagnosticCode::StylePartUnreadable, Region::Container))),
        (Role::Reported(Region::Note), None),
        (Role::Probe, None),
    ];

    #[test]
    fn an_unreadable_part_reports_by_its_role_once_per_read() {
        let corrupt = corrupt_entry("word/a.xml");
        let unparseable = package_of(&[("word/a.xml", b"<a></b></a></c>")]);
        for (role, expected) in ROLES {
            let expected: Vec<_> =
                expected.map(|(code, region)| (code, region, 1)).into_iter().collect();
            for read in 0..4 {
                let data = if read < 2 { &corrupt } else { &unparseable };
                let parse_ctx = ParseCtx::default();
                let mut pkg = Package::open(&parse_ctx, data, PackageKind::Opc).unwrap();
                match read {
                    0 => assert!(pkg.optional_part("word/a.xml", role).unwrap().is_none()),
                    1 => assert!(pkg.related_part("word/a.xml", role).unwrap().is_none()),
                    2 => assert!(pkg.optional_xml_part("word/a.xml", role).unwrap().is_none()),
                    _ => assert!(pkg.related_xml_part("word/a.xml", role).unwrap().is_none()),
                }
                assert_eq!(reported(&parse_ctx), expected, "{role:?} read {read}");
            }
        }
    }

    #[test]
    fn an_absent_part_is_a_loss_only_when_a_reference_names_it() {
        let data = package_of(&[("word/document.xml", b"<document/>")]);
        for (role, expected) in ROLES {
            let parse_ctx = ParseCtx::default();
            let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            assert!(pkg.optional_part("word/a.xml", role).unwrap().is_none());
            assert!(pkg.optional_xml_part("word/a.xml", role).unwrap().is_none());
            assert_eq!(reported(&parse_ctx), [], "{role:?}");
            assert!(pkg.related_part("word/a.xml", role).unwrap().is_none());
            assert!(pkg.related_xml_part("word/a.xml", role).unwrap().is_none());
            let expected: Vec<_> =
                expected.map(|(code, region)| (code, region, 2)).into_iter().collect();
            assert_eq!(reported(&parse_ctx), expected, "{role:?}");
            assert!(pkg.related_part("word/document.xml", role).unwrap().is_some());
            assert!(pkg.related_xml_part("word/document.xml", role).unwrap().is_some());
            assert_eq!(reported(&parse_ctx), expected, "{role:?}");
        }
    }

    #[test]
    fn a_recovered_part_reports_by_the_reading_role() {
        let data = package_of(&[("word/a.xml", b"<a><b>text</a>")]);
        let cases = [
            (Role::Content(Region::Note), vec![(DiagnosticCode::RecoveredXml, Region::Note, 1)]),
            (Role::Reported(Region::Body), vec![(DiagnosticCode::RecoveredXml, Region::Body, 1)]),
            (Role::Formatting, vec![(DiagnosticCode::StylePartUnreadable, Region::Container, 1)]),
            (Role::Probe, vec![]),
        ];
        for (role, expected) in cases {
            let parse_ctx = ParseCtx::default();
            let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            assert!(pkg.optional_xml_part("word/a.xml", role).unwrap().is_some());
            assert_eq!(reported(&parse_ctx), expected, "{role:?}");
            let parse_ctx = ParseCtx::default();
            let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            assert!(pkg.required_xml_part("word/a.xml", role).is_ok());
            assert_eq!(reported(&parse_ctx), expected, "{role:?} required");
        }
    }

    #[test]
    fn a_corrupt_entry_is_neither_cached_nor_charged() {
        let data = corrupt_entry("word/document.xml");
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("word/document.xml").unwrap_err();
        assert!(matches!(err, ConvertError::Malformed { reason: "corrupt archive entry", .. }));
        assert!(pkg.cache.is_empty());
        assert_eq!(pkg.total_read, 0);
        assert!(pkg.optional_part("word/document.xml", Role::Probe).unwrap().is_none());
        assert!(pkg.optional_xml_part("word/document.xml", Role::Probe).unwrap().is_none());
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
    fn a_read_charges_one_unit_per_started_kib_and_a_cached_read_none() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, len) in [("a.bin", 1024), ("b.bin", 1025), ("c.bin", 0)] {
            w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(&vec![7u8; len]).unwrap();
        }
        let data = w.finish().unwrap().into_inner();
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let mut spent = Vec::new();
        for name in ["a.bin", "b.bin", "c.bin", "a.bin"] {
            pkg.part(name).unwrap();
            spent.push(parse_ctx.fuel());
        }
        assert_eq!(spent, [1, 3, 3, 3]);
    }

    #[test]
    fn an_entry_failing_its_checksum_is_charged_for_what_it_inflated() {
        let mut data = one_part_zip("word/comments.xml", &[b'x'; 4096]);
        let crc = zip::ZipArchive::new(Cursor::new(data.as_slice()))
            .unwrap()
            .by_index(0)
            .unwrap()
            .crc32();
        let (stored, corrupt) = (crc.to_le_bytes(), (!crc).to_le_bytes());
        let at: Vec<usize> =
            data.windows(4).enumerate().filter(|(_, w)| *w == stored).map(|(i, _)| i).collect();
        assert_eq!(at.len(), 2);
        for i in at {
            data[i..i + 4].copy_from_slice(&corrupt);
        }
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.optional_part("word/comments.xml", Role::Probe).unwrap().is_none());
        assert!(pkg.optional_part("word/comments.xml", Role::Probe).unwrap().is_none());
        assert_eq!(parse_ctx.fuel(), 8);
        let limits = Limits { max_work: 3, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let read = pkg.optional_part("word/comments.xml", Role::Content(Region::Comment));
        assert!(
            matches!(read, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3 })),
            "{read:?}"
        );
    }

    #[test]
    fn optional_reads_propagate_a_cancellation() {
        let data = one_part_zip("word/comments.xml", b"<comments/>");
        let token = crate::ledger::CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("word/comments.xml").unwrap().is_some());
        token.cancel();
        let role = Role::Content(Region::Comment);
        let read = pkg.optional_xml_part("word/comments.xml", role);
        assert!(matches!(read, Err(ConvertError::Cancelled)), "{read:?}");
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let read = pkg.related_part("word/comments.xml", role);
        assert!(matches!(read, Err(ConvertError::Cancelled)), "{read:?}");
        assert_eq!(reported(&parse_ctx), []);
    }

    #[test]
    fn leading_slash_part_names_normalize() {
        let data = one_part_zip("word/document.xml", b"<x/>");
        let parse_ctx = ParseCtx::default();
        let mut pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("/word/document.xml").unwrap().is_some());
    }
}
