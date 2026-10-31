//! ZIP archive access with decompression limits.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::package::directory;
use crate::package::xml::{Dialect, Element, Marks, NodeSink, Parsed, WholeTree, parse};
use flate2::Crc;
use flate2::bufread::DeflateDecoder;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufReader, Cursor, ErrorKind, Read};
use std::rc::Rc;
use zip::CompressionMethod;

/// The family of a ZIP-based package: OPC, OpenDocument or EPUB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageKind {
    /// Open Packaging Conventions: DOCX, PPTX, XLSX and XLSB.
    Opc,
    /// OpenDocument: ODT, ODS and ODP.
    Odf,
    /// EPUB's Open Container Format.
    Epub,
}

/// What a part's loss costs the output, named by the reader that asks for
/// it: it decides what an unreadable part and an unresolved reference
/// report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Content that renders in the region: losing it is `part_unreadable`.
    Content(Region),
    /// A part whose loss changes formatting alone.
    Formatting,
    /// A part whose loss the reader, or each consumer of the part, reports;
    /// its unresolved references still report in the region.
    Reported(Region),
    Probe,
}

impl Role {
    pub(crate) fn unreadable(self) -> Option<(DiagnosticCode, Region)> {
        match self {
            Self::Content(region) => Some((DiagnosticCode::PartUnreadable, region)),
            Self::Formatting => Some((DiagnosticCode::StylePartUnreadable, Region::Container)),
            Self::Reported(_) | Self::Probe => None,
        }
    }

    pub(crate) fn unresolved(self) -> Option<(DiagnosticCode, Region)> {
        match self {
            Self::Content(region) | Self::Reported(region) => {
                Some((DiagnosticCode::UnresolvedEntity, region))
            }
            Self::Formatting | Self::Probe => None,
        }
    }
}

pub(crate) fn report(parse_ctx: &ParseCtx<'_>, diagnostic: Option<(DiagnosticCode, Region)>) {
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

/// A ZIP-based document package (OOXML, ODF, EPUB). Each borrow of its
/// caches is scoped to one method, and none is held while an entry inflates.
pub(crate) struct Package<'a> {
    parse_ctx: &'a ParseCtx<'a>,
    kind: PackageKind,
    zip: zip::ZipArchive<Cursor<&'a [u8]>>,
    /// Entry indices by stored name, ASCII-lowercased for OPC: its part names
    /// compare case-insensitively in ASCII (ECMA-376 Part 2).
    names: HashMap<Vec<u8>, usize>,
    /// Each entry's first read, which inflated once and counted what it
    /// inflated against `max_total_bytes`.
    cache: RefCell<HashMap<usize, FirstRead>>,
    /// Shared trees by entry, `None` for a part that could not be read or
    /// parsed. Together they hold at most `max_xml_nodes` nodes, so the
    /// cache never keeps more than one part at its limit could.
    trees: RefCell<HashMap<usize, Option<SharedTree>>>,
    cached_nodes: Cell<u64>,
}

/// An entry's decompressed bytes, shared (`Rc`), or the reason it failed
/// non-fatally.
type FirstRead = Result<Rc<[u8]>, &'static str>;

/// A parsed part shared by every reference that names it, beside the node
/// count each reuse charges and the marks each reuse reports.
#[derive(Clone)]
struct SharedTree {
    root: Rc<Element>,
    nodes: u64,
    marks: Marks,
}

impl<'a> Package<'a> {
    pub(crate) fn open(
        parse_ctx: &'a ParseCtx<'a>,
        bytes: &'a [u8],
        kind: PackageKind,
    ) -> Result<Self, ConvertError> {
        directory::admit(bytes, parse_ctx.limits())?;
        let zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|_| ConvertError::malformed("not a readable zip archive"))?;
        directory::inspect(&zip, bytes, parse_ctx.limits())?;
        Self::assemble(parse_ctx, kind, zip)
    }

    /// The same archive read as a package of `kind`, without admitting its
    /// directory again.
    pub(crate) fn reopen(&self, kind: PackageKind) -> Result<Self, ConvertError> {
        Self::assemble(self.parse_ctx, kind, self.zip.clone())
    }

    fn assemble(
        parse_ctx: &'a ParseCtx<'a>,
        kind: PackageKind,
        zip: zip::ZipArchive<Cursor<&'a [u8]>>,
    ) -> Result<Self, ConvertError> {
        let names = entry_names(&zip, kind)?;
        Ok(Self {
            parse_ctx,
            kind,
            zip,
            names,
            cache: RefCell::new(HashMap::new()),
            trees: RefCell::new(HashMap::new()),
            cached_nodes: Cell::new(0),
        })
    }

    fn dialect(&self) -> Dialect {
        match self.kind {
            PackageKind::Opc | PackageKind::Odf => Dialect::Xml,
            PackageKind::Epub => Dialect::Xhtml,
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        match self.kind {
            PackageKind::Opc => self.names.get(&name.as_bytes().to_ascii_lowercase()),
            PackageKind::Odf | PackageKind::Epub => self.names.get(name.as_bytes()),
        }
        .copied()
    }

    /// Read a part's bytes. `Ok(None)` means the part is absent (a valid
    /// state for optional parts); `Err` means it exists but cannot be read.
    /// Callers apply the unified policy: skip and report by role when useful
    /// output remains, propagate when the part is the primary content. An
    /// entry inflates once, and one that fails non-fatally fails again from
    /// memory at every later call.
    pub(crate) fn part(&self, name: &str) -> Result<Option<Rc<[u8]>>, ConvertError> {
        // OPC part URIs may carry a leading slash; entries never do.
        let name = name.trim_start_matches('/');
        let Some(index) = self.index_of(name) else {
            return Ok(None);
        };
        let remembered = self.cache.borrow().get(&index).cloned();
        match remembered {
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(reason)) => Err(ConvertError::malformed_part(name, reason)),
            None => self.inflate(index, name).map(Some),
        }
    }

    fn inflate(&self, index: usize, name: &str) -> Result<Rc<[u8]>, ConvertError> {
        let read = self.read_at(index, name).map(Rc::from);
        let remembered = match &read {
            Ok(bytes) => Ok(Rc::clone(bytes)),
            Err(ConvertError::Malformed { reason, .. }) => Err(*reason),
            Err(_) => return read,
        };
        self.cache.borrow_mut().insert(index, remembered);
        read
    }

    fn read_at(&self, index: usize, name: &str) -> Result<Vec<u8>, ConvertError> {
        let mut zip = self.zip.clone();
        let mut file = zip
            .by_index_raw(index)
            .map_err(|_| ConvertError::malformed_part(name, "unreadable archive entry"))?;
        let limits = self.parse_ctx.limits();
        if file.size() > limits.max_entry_bytes {
            return Err(ConvertError::limit(Limit::EntryBytes, limits.max_entry_bytes));
        }
        // A read past the declared size is malformed, so only the
        // whole-archive total can still run out mid-entry.
        let remaining_total = limits.max_total_bytes.saturating_sub(self.parse_ctx.archive_bytes());
        let cap = limits.max_entry_bytes.min(remaining_total);
        let bytes = read_entry(self.parse_ctx, &mut file, cap, name)?;
        if bytes.len() as u64 > cap {
            return Err(ConvertError::limit(Limit::TotalBytes, limits.max_total_bytes));
        }
        Ok(bytes)
    }

    /// The first `cap` bytes of a part, whatever size it declares, counted
    /// against the entry and total budgets and never cached. An absent or
    /// unreadable part reads as absent; a fatal error propagates.
    pub(crate) fn probe_head(&self, name: &str, cap: u64) -> Result<Option<Vec<u8>>, ConvertError> {
        Ok(self.probe(name, cap)?.map(|(mut bytes, cap)| {
            bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
            bytes
        }))
    }

    /// A part of at most `cap` bytes, and within the entry and total
    /// budgets: a larger part, or one that does not read, reads as absent; a
    /// fatal error propagates.
    pub(crate) fn probe_part(&self, name: &str, cap: u64) -> Result<Option<Vec<u8>>, ConvertError> {
        Ok(self.probe(name, cap)?.and_then(|(bytes, cap)| {
            u64::try_from(bytes.len()).is_ok_and(|len| len <= cap).then_some(bytes)
        }))
    }

    fn probe(&self, name: &str, cap: u64) -> Result<Option<(Vec<u8>, u64)>, ConvertError> {
        let name = name.trim_start_matches('/');
        let Some(index) = self.index_of(name) else {
            return Ok(None);
        };
        let limits = self.parse_ctx.limits();
        let remaining_total = limits.max_total_bytes.saturating_sub(self.parse_ctx.archive_bytes());
        let cap = cap.min(limits.max_entry_bytes).min(remaining_total);
        let mut zip = self.zip.clone();
        let read = zip
            .by_index_raw(index)
            .map_err(|_| ConvertError::malformed_part(name, "unreadable archive entry"))
            .and_then(|mut file| read_entry(self.parse_ctx, &mut file, cap, name));
        skip_unreadable(self.parse_ctx, Role::Probe, read.map(|bytes| Some((bytes, cap))))
    }

    /// [`Package::probe_part`], parsed as XML: a part that does not parse
    /// reads as absent too.
    pub(crate) fn probe_xml_part(
        &self,
        name: &str,
        cap: u64,
    ) -> Result<Option<Element>, ConvertError> {
        let Some(bytes) = self.probe_part(name, cap)? else {
            return Ok(None);
        };
        let parsed = self.parse_part(name, &bytes, &mut WholeTree).map(|parsed| Some(parsed.root));
        skip_unreadable(self.parse_ctx, Role::Probe, parsed)
    }

    /// True when a part exists, without reading (or budget-charging) it.
    pub(crate) fn has_part(&self, name: &str) -> bool {
        self.index_of(name.trim_start_matches('/')).is_some()
    }

    /// Read a part that must exist for any meaningful output.
    pub(crate) fn required_part(&self, name: &str) -> Result<Rc<[u8]>, ConvertError> {
        self.part(name)?.ok_or_else(|| ConvertError::missing_part(name))
    }

    /// Read an optional part a conventional name points at: absent is a
    /// valid state, and an unreadable part reports by `role` and reads as
    /// absent; a fatal error propagates.
    pub(crate) fn optional_part(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Option<Rc<[u8]>>, ConvertError> {
        self.read_part(name, role, Absent::Valid)
    }

    /// Read a part a relationship or reference names: absent or unreadable,
    /// it reports by `role` and reads as absent; a fatal error propagates.
    pub(crate) fn related_part(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Option<Rc<[u8]>>, ConvertError> {
        self.read_part(name, role, Absent::Lost)
    }

    /// [`Package::optional_part`], parsed as XML: a part that does not parse
    /// reports by `role` and reads as absent.
    pub(crate) fn optional_xml_part(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Option<Element>, ConvertError> {
        self.read_xml_part(name, role, Absent::Valid, &mut WholeTree)
    }

    /// [`Package::related_part`], parsed as XML: a part that does not parse
    /// reports by `role` and reads as absent.
    pub(crate) fn related_xml_part(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Option<Element>, ConvertError> {
        self.read_xml_part(name, role, Absent::Lost, &mut WholeTree)
    }

    /// [`Package::related_xml_part`], with every node `sink` takes handed to
    /// it as it completes instead of joining the tree. A part that does not
    /// parse reads as absent after the sink has seen part of it, so the
    /// caller discards what the sink holds.
    pub(crate) fn related_xml_part_streamed(
        &self,
        name: &str,
        role: Role,
        sink: &mut dyn NodeSink,
    ) -> Result<Option<Element>, ConvertError> {
        self.read_xml_part(name, role, Absent::Lost, sink)
    }

    /// [`Package::related_xml_part`], shared while the shared trees stay
    /// within `max_xml_nodes` in total: every reuse charges the tree's node
    /// count before it gets the tree, and every reference reports the
    /// part's loss and marks by its own role, in its region and unit. A part
    /// past that budget parses at every reference.
    pub(crate) fn related_xml_tree(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Option<Rc<Element>>, ConvertError> {
        let Some(index) = self.index_of(name.trim_start_matches('/')) else {
            return self.related_xml_part(name, role).map(|tree| tree.map(Rc::new));
        };
        let cached = self.trees.borrow().get(&index).cloned();
        let shared = match cached {
            Some(Some(shared)) => {
                self.parse_ctx.charge(shared.nodes)?;
                shared
            }
            Some(None) => {
                report(self.parse_ctx, role.unreadable());
                return Ok(None);
            }
            None => {
                let Some(parsed) = self.read_xml(name, role, Absent::Lost, &mut WholeTree)? else {
                    self.trees.borrow_mut().insert(index, None);
                    return Ok(None);
                };
                let shared = SharedTree {
                    root: Rc::new(parsed.root),
                    nodes: parsed.nodes,
                    marks: parsed.marks,
                };
                let cached_nodes = self.cached_nodes.get().saturating_add(shared.nodes);
                if cached_nodes <= self.parse_ctx.limits().max_xml_nodes {
                    self.cached_nodes.set(cached_nodes);
                    self.trees.borrow_mut().insert(index, Some(shared.clone()));
                }
                shared
            }
        };
        shared.marks.report(self.parse_ctx, role);
        Ok(Some(shared.root))
    }

    /// Read and parse an XML part that must exist and parse for any
    /// meaningful output; its unresolved references report by `role`.
    pub(crate) fn required_xml_part(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Element, ConvertError> {
        let bytes = self.required_part(name)?;
        let parsed = self.parse_part(name, &bytes, &mut WholeTree)?;
        parsed.marks.report(self.parse_ctx, role);
        Ok(parsed.root)
    }

    fn parse_part(
        &self,
        name: &str,
        bytes: &[u8],
        sink: &mut dyn NodeSink,
    ) -> Result<Parsed, ConvertError> {
        parse(self.parse_ctx, bytes, self.dialect(), sink).map_err(|e| e.within(name))
    }

    fn read_part(
        &self,
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
        &self,
        name: &str,
        role: Role,
        absent: Absent,
        sink: &mut dyn NodeSink,
    ) -> Result<Option<Element>, ConvertError> {
        Ok(self.read_xml(name, role, absent, sink)?.map(|parsed| {
            parsed.marks.report(self.parse_ctx, role);
            parsed.root
        }))
    }

    /// A part read and parsed, its loss reported by `role` and its marks
    /// left to the caller.
    fn read_xml(
        &self,
        name: &str,
        role: Role,
        absent: Absent,
        sink: &mut dyn NodeSink,
    ) -> Result<Option<Parsed>, ConvertError> {
        let Some(bytes) = self.read_part(name, role, absent)? else {
            return Ok(None);
        };
        skip_unreadable(self.parse_ctx, role, self.parse_part(name, &bytes, sink).map(Some))
    }
}

fn entry_names(
    zip: &zip::ZipArchive<Cursor<&[u8]>>,
    kind: PackageKind,
) -> Result<HashMap<Vec<u8>, usize>, ConvertError> {
    let mut names = HashMap::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip
            .by_index_data(index)
            .map_err(|_| ConvertError::malformed("unreadable entry name"))?;
        let name = match kind {
            PackageKind::Opc => entry.name_raw().to_ascii_lowercase(),
            PackageKind::Odf | PackageKind::Epub => entry.name_raw().to_vec(),
        };
        if names.insert(name, index).is_some() {
            return Err(ConvertError::malformed("entry names equal after case folding"));
        }
    }
    Ok(names)
}

/// An entry opened raw and inflated here, so the bytes a deflate stream
/// writes before it breaks count too. It reads at most one byte past `cap`;
/// a stream past its declared size or failing its checksum is `Malformed`.
fn read_entry<R: Read>(
    parse_ctx: &ParseCtx<'_>,
    file: &mut zip::read::ZipFile<'_, R>,
    cap: u64,
    name: &str,
) -> Result<Vec<u8>, ConvertError> {
    let declared = file.size();
    let bound = cap.min(declared);
    let checksum = file.crc32();
    let (bytes, inflated) = match file.compression() {
        CompressionMethod::Stored => {
            let mut counted = Counted { inner: file, read: 0 };
            (read_capped(&mut counted, declared, bound, name, CORRUPT_ENTRY), counted.read)
        }
        CompressionMethod::Deflated => {
            let mut inflater = DeflateDecoder::new(BufReader::new(file));
            (read_capped(&mut inflater, declared, bound, name, CORRUPT_ENTRY), inflater.total_out())
        }
        _ => return Err(ConvertError::malformed_part(name, "unreadable archive entry")),
    };
    parse_ctx.count_archive_bytes(inflated);
    parse_ctx.charge(inflated.div_ceil(1024))?;
    let bytes = bytes?;
    let len = bytes.len() as u64;
    let corrupt = if len > bound { bound == declared } else { crc32(&bytes) != checksum };
    if corrupt {
        return Err(ConvertError::malformed_part(name, CORRUPT_ENTRY));
    }
    Ok(bytes)
}

const CORRUPT_ENTRY: &str = "corrupt archive entry";

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(bytes);
    crc.sum()
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

/// A read's value, or `None` after reporting `role.unreadable()` for a
/// non-fatal error; a fatal error propagates.
pub(crate) fn skip_unreadable<T>(
    parse_ctx: &ParseCtx<'_>,
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

/// `bytes` opened as an OOXML package; a zip failure on an OLE compound file reports as what
/// the file is.
pub(crate) fn open_ooxml<'a>(
    parse_ctx: &'a ParseCtx<'a>,
    bytes: &'a [u8],
) -> Result<Package<'a>, ConvertError> {
    Package::open(parse_ctx, bytes, PackageKind::Opc).map_err(|e| match e {
        ConvertError::Malformed { .. } => probe_ole(bytes).unwrap_or(e),
        e => e,
    })
}

/// A zip-open failure on OOXML input may actually be an OLE compound file:
/// an encrypted package, or a legacy binary document with the wrong
/// extension.
pub(crate) fn probe_ole(bytes: &[u8]) -> Option<ConvertError> {
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
#[expect(clippy::disallowed_methods, reason = "an assertion holds for every map value")]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use crate::shared::test_support::{
        corrupt_entry, diagnostic_rows, positions, stored, zip_package, zip_package_with,
    };
    use std::assert_matches;

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
    fn an_entry_read_reserves_its_declared_size() {
        let bytes = zip_package(&[("a.bin", &[7u8; 1000])]);
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let mut file = zip.by_index_raw(0).unwrap();
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

    fn reported(parse_ctx: &ParseCtx<'_>) -> Vec<(DiagnosticCode, Region, u64)> {
        diagnostic_rows(&parse_ctx.diagnostics())
    }

    #[test]
    fn optional_reads_propagate_fatal_errors_and_skip_the_rest() {
        let parse_ctx = ParseCtx::default();
        let role = Role::Content(Region::Note);
        let refused = || Err::<Option<()>, _>(ConvertError::limit(Limit::Allocation, 64));
        assert_matches!(
            skip_unreadable(&parse_ctx, role, refused()),
            Err(ConvertError::ResourceLimit { limit: Limit::Allocation, max: 64 })
        );
        assert_matches!(
            skip_unreadable::<()>(&parse_ctx, role, Err(ConvertError::Cancelled)),
            Err(ConvertError::Cancelled)
        );
        assert_matches!(
            skip_unreadable::<()>(&parse_ctx, role, Err(ConvertError::Internal)),
            Err(ConvertError::Internal)
        );
        assert_matches!(skip_unreadable(&parse_ctx, role, Ok(Some(1))), Ok(Some(1)));
        assert_matches!(skip_unreadable::<()>(&parse_ctx, role, Ok(None)), Ok(None));
        assert_eq!(reported(&parse_ctx), []);
        let corrupt = Err(ConvertError::malformed_part("a.xml", "corrupt"));
        assert_matches!(skip_unreadable::<()>(&parse_ctx, role, corrupt), Ok(None));
        assert_eq!(reported(&parse_ctx), [(DiagnosticCode::PartUnreadable, Region::Note, 1)]);
    }

    fn corrupt_document(name: &str) -> Vec<u8> {
        let mut data = zip_package_with(&[(name, "<document/>")], stored());
        corrupt_entry(&mut data, name);
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
        let corrupt = corrupt_document("word/a.xml");
        let unparseable = zip_package(&[("word/a.xml", b"<a></b></a></c>")]);
        for (role, expected) in ROLES {
            let expected: Vec<_> =
                expected.map(|(code, region)| (code, region, 1)).into_iter().collect();
            for read in 0..4 {
                let data = if read < 2 { &corrupt } else { &unparseable };
                let parse_ctx = ParseCtx::default();
                let pkg = Package::open(&parse_ctx, data, PackageKind::Opc).unwrap();
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
        let data = zip_package(&[("word/document.xml", b"<document/>")]);
        for (role, expected) in ROLES {
            let parse_ctx = ParseCtx::default();
            let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
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
    fn a_damaged_part_is_unreadable_by_the_reading_role_and_malformed_where_required() {
        for (xml, reason) in [
            (&b"<a><b>text</a>"[..], "mismatched end tag"),
            (b"<a><b>text", "unclosed element"),
            (b"<a x='1' x='2'/>", "ill-formed attribute"),
            (b"<a xmlns:p='urn:p' xmlns:q='urn:p' p:x='1' q:x='2'/>", "repeated attribute"),
            (b"<p:a/>", "unbound prefix"),
            (b"<?xml version='1.0' encoding='ISO-8859-3'?><a>\xA5</a>", "bytes that do not decode"),
        ] {
            let data = zip_package(&[("word/a.xml", xml)]);
            for (role, expected) in ROLES {
                let expected: Vec<_> =
                    expected.map(|(code, region)| (code, region, 1)).into_iter().collect();
                let parse_ctx = ParseCtx::default();
                let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
                assert!(pkg.optional_xml_part("word/a.xml", role).unwrap().is_none());
                assert_eq!(reported(&parse_ctx), expected, "{role:?} {reason}");
                let parse_ctx = ParseCtx::default();
                let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
                let err = pkg.required_xml_part("word/a.xml", role).unwrap_err();
                assert_matches!(
                    &err,
                    ConvertError::Malformed { part: Some(part), reason: r }
                        if part.as_str() == "word/a.xml" && *r == reason,
                    "{role:?}"
                );
                assert_eq!(reported(&parse_ctx), [], "{role:?} required");
            }
        }
    }

    #[test]
    fn a_corrupt_entry_inflates_once_whoever_names_it_and_counts_what_it_inflated() {
        let data = corrupt_document("word/document.xml");
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for name in ["word/document.xml", "/Word/Document.xml", "word/document.xml"] {
            let err = pkg.part(name).unwrap_err();
            assert_matches!(
                &err,
                ConvertError::Malformed { part: Some(part), reason: "corrupt archive entry" }
                    if part.as_str() == name.trim_start_matches('/'),
                "{name}"
            );
        }
        let role = Role::Content(Region::Note);
        for _ in 0..3 {
            assert!(pkg.optional_part("word/document.xml", role).unwrap().is_none());
        }
        assert!(pkg.optional_xml_part("word/document.xml", Role::Probe).unwrap().is_none());
        assert_eq!(reported(&parse_ctx), [(DiagnosticCode::PartUnreadable, Region::Note, 3)]);
        assert_eq!((parse_ctx.archive_bytes(), parse_ctx.fuel()), (11, 1));
    }

    #[test]
    fn a_failure_is_remembered_for_its_own_entry_alone() {
        let parts = [("pre.xml", "<pre/>"), ("a.xml", "<a/>"), ("b.xml", "<bb/>")];
        let mut data = zip_package_with(&parts, stored());
        corrupt_entry(&mut data, "a.xml");
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for _ in 0..2 {
            for (name, content) in parts {
                let read = pkg.part(name);
                if name == "a.xml" {
                    assert_matches!(
                        read,
                        Err(ConvertError::Malformed { reason: "corrupt archive entry", .. })
                    );
                } else {
                    assert_eq!(&*read.unwrap().unwrap(), content.as_bytes(), "{name}");
                }
            }
        }
        assert_eq!((parse_ctx.archive_bytes(), parse_ctx.fuel()), (6 + 4 + 5, 3));
    }

    #[test]
    fn a_probe_of_a_corrupt_entry_counts_what_it_inflated() {
        let data = corrupt_document("word/document.xml");
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.probe_part("word/document.xml", 64).unwrap().is_none());
        assert_eq!(parse_ctx.archive_bytes(), 11);
        assert!(pkg.probe_head("word/document.xml", 64).unwrap().is_none());
        assert_eq!(parse_ctx.archive_bytes(), 22);
    }

    /// `sound` parts, after deflated entries whose stream inflates their
    /// prefix in a stored block, then breaks on a block of the reserved type.
    fn broken_deflate(broken: &[(&str, &[u8])], sound: &[(&str, &[u8])]) -> Vec<u8> {
        let streams = broken.iter().map(|(name, prefix)| {
            let len = u16::try_from(prefix.len()).unwrap().to_le_bytes();
            (*name, [&[0], &len[..], &[!len[0], !len[1]], prefix, &[0b111]].concat())
        });
        let sound = sound.iter().map(|(name, body)| (*name, body.to_vec()));
        let parts: Vec<_> = streams.chain(sound).collect();
        let mut data = zip_package_with(&parts, stored());
        for (name, _) in broken {
            set_method(&mut data, name, 8);
        }
        data
    }

    fn set_method(data: &mut [u8], name: &str, method: u8) {
        let mut zip = zip::ZipArchive::new(Cursor::new(data.to_vec())).unwrap();
        let file = zip.by_name(name).unwrap();
        for at in [file.header_start() + 8, file.central_header_start() + 10] {
            data[usize::try_from(at).unwrap()] = method;
        }
    }

    #[test]
    fn a_deflate_stream_that_breaks_counts_what_it_inflated_before_breaking() {
        let data = broken_deflate(&[("a.bin", &[7u8; 4000])], &[]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for _ in 0..2 {
            let err = pkg.part("a.bin").unwrap_err();
            assert_matches!(err, ConvertError::Malformed { reason: "corrupt archive entry", .. });
        }
        assert_eq!(&*pkg.probe_head("a.bin", 64).unwrap().unwrap(), [7u8; 64]);
        assert_eq!((parse_ctx.archive_bytes(), parse_ctx.fuel()), (4000 + 65, 4 + 1));
    }

    #[test]
    fn broken_deflate_streams_count_against_the_total_until_it_is_spent() {
        let broken: [(&str, &[u8]); 2] = [("a.bin", &[7u8; 4000]), ("b.bin", &[7u8; 4000])];
        let data = broken_deflate(&broken, &[("c.bin", &[7u8; 200])]);
        let limits = Limits { max_total_bytes: 2 * 4000 + 100, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for (name, _) in broken {
            assert_matches!(pkg.part(name), Err(ConvertError::Malformed { .. }), "{name}");
        }
        assert_eq!(parse_ctx.archive_bytes(), 2 * 4000);
        let err = pkg.part("c.bin").unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::TotalBytes, .. });
    }

    #[test]
    fn an_entry_of_a_compression_method_zip_cannot_read_is_unreadable() {
        let mut data = zip_package_with(&[("a.bin", "abc")], stored());
        set_method(&mut data, "a.bin", 12);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("a.bin").unwrap_err();
        assert_matches!(err, ConvertError::Malformed { reason: "unreadable archive entry", .. });
        assert_eq!(parse_ctx.archive_bytes(), 0);
    }

    #[test]
    fn corrupt_entries_count_against_the_total_until_it_is_spent() {
        let mut data = zip_package_with(
            &[("a.bin", [7u8; 4096]), ("b.bin", [7u8; 4096]), ("c.bin", [7u8; 4096])],
            stored(),
        );
        for name in ["a.bin", "b.bin", "c.bin"] {
            corrupt_entry(&mut data, name);
        }
        let limits = Limits { max_total_bytes: 2 * 4096 + 100, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for name in ["a.bin", "b.bin"] {
            let err = pkg.part(name).unwrap_err();
            assert_matches!(err, ConvertError::Malformed { reason: "corrupt archive entry", .. });
        }
        assert_eq!(parse_ctx.archive_bytes(), 2 * 4096);
        for _ in 0..2 {
            let err = pkg.part("c.bin").unwrap_err();
            assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::TotalBytes, .. });
        }
    }

    #[test]
    fn repeated_reads_are_cached_and_charged_once() {
        let data = zip_package(&[("media/a.bin", &[7u8; 4096])]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for _ in 0..5 {
            assert_eq!(pkg.part("media/a.bin").unwrap().unwrap().len(), 4096);
        }
        assert_eq!(parse_ctx.archive_bytes(), 4096, "repeated reads must not re-charge the budget");
    }

    #[test]
    fn total_budget_exhaustion_reports_max_total_bytes() {
        let data = zip_package(&[("a.bin", [7u8; 4096]), ("b.bin", [7u8; 4096])]);
        let limits = Limits { max_total_bytes: 4096 + 100, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("a.bin").unwrap().is_some());
        for _ in 0..2 {
            let err = pkg.part("b.bin").unwrap_err();
            assert_matches!(
                err,
                ConvertError::ResourceLimit { limit: Limit::TotalBytes, max: 4196 }
            );
        }
    }

    #[test]
    fn entry_count_and_size_limits_are_the_contexts() {
        let data = zip_package(&[("a.bin", &[7u8; 4096])]);
        let mut limits = Limits { max_entry_bytes: 4096, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap().part("a.bin").unwrap();
        limits.max_entry_bytes = 4095;
        let parse_ctx = ParseCtx::new(&limits);
        let err =
            Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap().part("a.bin").unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 4095 });

        let limits = Limits { max_entry_count: 1, ..Limits::default() };
        Package::open(&ParseCtx::new(&limits), &data, PackageKind::Opc).unwrap();
        let two = zip_package(&[("a.bin", ""), ("b.bin", "")]);
        let err = Package::open(&ParseCtx::new(&limits), &two, PackageKind::Opc).err().unwrap();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryCount, max: 1 });
    }

    #[test]
    fn an_entry_past_its_declared_size_is_malformed() {
        let mut data = zip_package(&[("a.bin", &[7u8; 4096])]);
        let understate =
            |at: usize, data: &mut [u8]| data[at..at + 4].copy_from_slice(&16u32.to_le_bytes());
        understate(positions(&data, b"PK\x03\x04")[0] + 22, &mut data);
        understate(positions(&data, b"PK\x01\x02")[0] + 24, &mut data);
        let limits = Limits { max_entry_bytes: 64, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("a.bin").unwrap_err();
        assert_matches!(
            &err,
            ConvertError::Malformed { part: Some(part), reason: "corrupt archive entry" }
                if part.as_str() == "a.bin"
        );
    }

    #[test]
    fn a_read_charges_one_unit_per_started_kib_and_a_cached_read_none() {
        let data = zip_package(&[
            ("a.bin", vec![7u8; 1024]),
            ("b.bin", vec![7u8; 1025]),
            ("c.bin", vec![]),
        ]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let mut spent = Vec::new();
        for name in ["a.bin", "b.bin", "c.bin", "a.bin"] {
            pkg.part(name).unwrap();
            spent.push(parse_ctx.fuel());
        }
        assert_eq!(spent, [1, 3, 3, 3]);
    }

    #[test]
    fn an_entry_failing_its_checksum_is_charged_for_what_it_inflated() {
        let mut data = zip_package(&[("word/comments.xml", &[b'x'; 4096])]);
        let crc = zip::ZipArchive::new(Cursor::new(data.as_slice()))
            .unwrap()
            .by_index(0)
            .unwrap()
            .crc32();
        let (declared, corrupt) = (crc.to_le_bytes(), (!crc).to_le_bytes());
        let at = positions(&data, &declared);
        assert_eq!(at.len(), 2);
        for i in at {
            data[i..i + 4].copy_from_slice(&corrupt);
        }
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.optional_part("word/comments.xml", Role::Probe).unwrap().is_none());
        assert!(pkg.optional_part("word/comments.xml", Role::Probe).unwrap().is_none());
        assert_eq!((parse_ctx.archive_bytes(), parse_ctx.fuel()), (4096, 4));
        let limits = Limits { max_work: 3, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let read = pkg.optional_part("word/comments.xml", Role::Content(Region::Comment));
        assert_matches!(read, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3 }));
    }

    #[test]
    fn optional_reads_propagate_a_cancellation_of_a_sound_or_damaged_part() {
        for body in ["<comments/>", "<comments><c>"] {
            let data = zip_package(&[("word/comments.xml", body)]);
            let token = crate::ledger::CancelToken::new();
            let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
            let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            assert!(pkg.part("word/comments.xml").unwrap().is_some());
            token.cancel();
            let role = Role::Content(Region::Comment);
            let read = pkg.optional_xml_part("word/comments.xml", role);
            assert_matches!(read, Err(ConvertError::Cancelled), "{body}");
            let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            let read = pkg.related_part("word/comments.xml", role);
            assert_matches!(read, Err(ConvertError::Cancelled), "{body}");
            assert_eq!(reported(&parse_ctx), [], "{body}");
        }
    }

    #[test]
    fn leading_slash_part_names_normalize() {
        let data = zip_package(&[("word/document.xml", b"<x/>")]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("/word/document.xml").unwrap().is_some());
    }

    const KINDS: [PackageKind; 3] = [PackageKind::Opc, PackageKind::Odf, PackageKind::Epub];

    fn duplicated() -> Vec<u8> {
        let mut data = zip_package(&[("a.xml", "<first/>"), ("b.xml", "<second/>")]);
        for at in positions(&data, b"b.xml") {
            data[at] = b'a';
        }
        data
    }

    #[test]
    fn exact_duplicate_names_are_malformed_in_every_kind() {
        let data = duplicated();
        for kind in KINDS {
            let err = Package::open(&ParseCtx::default(), &data, kind).err().unwrap();
            assert_matches!(
                err,
                ConvertError::Malformed { reason: "duplicate entry names", .. },
                "{kind:?}"
            );
        }
    }

    #[test]
    fn names_equal_after_ascii_case_folding_are_duplicates_in_opc_only() {
        let data = zip_package(&[("word/a.xml", b"<lower/>"), ("word/A.xml", b"<upper/>")]);
        let err = Package::open(&ParseCtx::default(), &data, PackageKind::Opc).err().unwrap();
        assert_matches!(
            err,
            ConvertError::Malformed { reason: "entry names equal after case folding", .. }
        );
        for kind in [PackageKind::Odf, PackageKind::Epub] {
            let parse_ctx = ParseCtx::default();
            let pkg = Package::open(&parse_ctx, &data, kind).unwrap();
            assert_eq!(&*pkg.part("word/a.xml").unwrap().unwrap(), b"<lower/>");
            assert_eq!(&*pkg.part("word/A.xml").unwrap().unwrap(), b"<upper/>");
        }
    }

    #[test]
    fn opc_names_resolve_after_ascii_case_folding_and_are_cached_once() {
        let data = zip_package(&[("word/document.xml", &[7u8; 2048])]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.has_part("/Word/Document.XML"));
        for name in ["word/Document.xml", "WORD/DOCUMENT.XML", "word/document.xml"] {
            assert_eq!(pkg.part(name).unwrap().unwrap().len(), 2048, "{name}");
        }
        assert_eq!(
            (parse_ctx.archive_bytes(), pkg.cache.borrow().len(), parse_ctx.fuel()),
            (2048, 1, 2)
        );
        assert!(pkg.part("word/document.xm").unwrap().is_none());
        for kind in [PackageKind::Odf, PackageKind::Epub] {
            let pkg = Package::open(&parse_ctx, &data, kind).unwrap();
            assert!(!pkg.has_part("word/Document.xml"), "{kind:?}");
            assert!(pkg.part("word/Document.xml").unwrap().is_none(), "{kind:?}");
            assert!(pkg.part("word/document.xml").unwrap().is_some(), "{kind:?}");
        }
    }

    #[test]
    fn a_name_matches_only_the_bytes_an_entry_stores() {
        let mut data = zip_package_with(&[("Pictures/X.png", b"x".as_slice())], stored());
        for at in positions(&data, b"Pictures/X") {
            data[at + 9] = 0x81;
        }
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        assert_eq!(zip.index_for_name("Pictures/ü.png"), Some(0));
        let parse_ctx = ParseCtx::default();
        for kind in KINDS {
            let pkg = Package::open(&parse_ctx, &data, kind).unwrap();
            assert!(!pkg.has_part("Pictures/ü.png"), "{kind:?}");
            assert!(pkg.part("Pictures/ü.png").unwrap().is_none(), "{kind:?}");
        }
    }

    #[test]
    fn duplicates_behind_a_refused_end_record_are_malformed() {
        let mut data = duplicated();
        let end = data.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        let mut appended = data[end..end + 22].to_vec();
        appended[4] = 1;
        appended[8] = 1;
        appended[10] = 1;
        data.extend(appended);
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        assert_eq!(zip.len(), 1);
        let err = Package::open(&ParseCtx::default(), &data, PackageKind::Odf).err().unwrap();
        assert_matches!(err, ConvertError::Malformed { reason: "duplicate entry names", .. });
    }

    #[test]
    fn compressed_sizes_past_the_directory_are_malformed() {
        let mut data = zip_package(&[("word/document.xml", &vec![0; 4 << 20])]);
        let limits = Limits { max_compression_ratio: 2, ..Limits::default() };
        let err = Package::open(&ParseCtx::new(&limits), &data, PackageKind::Opc).err().unwrap();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::CompressionRatio, max: 2 }
        );
        let central = positions(&data, b"PK\x01\x02")[0];
        data[central + 20..central + 24].copy_from_slice(&(4u32 << 20).to_le_bytes());
        zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        let err = Package::open(&ParseCtx::new(&limits), &data, PackageKind::Opc).err().unwrap();
        assert_matches!(
            err,
            ConvertError::Malformed { reason: "compressed sizes exceed the archive", .. }
        );
    }

    #[test]
    fn a_package_after_prepended_bytes_opens_where_zip_finds_it() {
        let data =
            [b"prefix!!".as_slice(), &zip_package(&[("word/document.xml", b"<d/>")])].concat();
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert_eq!(&*pkg.part("word/document.xml").unwrap().unwrap(), b"<d/>");
    }

    #[test]
    fn a_part_that_does_not_parse_is_malformed_by_name_where_required() {
        let data = zip_package(&[("word/a.xml", b"<a>A < B</a>")]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.required_xml_part("word/a.xml", Role::Content(Region::Body)).unwrap_err();
        assert_matches!(
            &err,
            ConvertError::Malformed { part: Some(part), reason: "bare `<` in markup" }
                if part.as_str() == "word/a.xml"
        );
        assert!(
            pkg.optional_xml_part("word/a.xml", Role::Content(Region::Body)).unwrap().is_none()
        );
        assert_eq!(reported(&parse_ctx), [(DiagnosticCode::PartUnreadable, Region::Body, 1)]);
    }

    #[test]
    fn each_reuse_of_a_shared_tree_charges_its_nodes_and_reports_its_marks() {
        let data = zip_package(&[
            ("c/a.xml", "<a><b/>t</a>"),
            ("c/r.xml", "<a><b/>&bogus;</a>"),
            ("c/bad.xml", "<a>A < B</a>"),
        ]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let role = Role::Reported(Region::Note);
        let first = pkg.related_xml_tree("c/a.xml", role).unwrap().unwrap();
        assert_eq!(parse_ctx.fuel(), 6);
        for name in ["c/a.xml", "/C/A.xml"] {
            let fuel = parse_ctx.fuel();
            assert!(Rc::ptr_eq(&first, &pkg.related_xml_tree(name, role).unwrap().unwrap()));
            assert_eq!(parse_ctx.fuel() - fuel, 3, "{name}");
        }
        for (name, nodes) in [("c/r.xml", 3), ("c/bad.xml", 0), ("c/absent.xml", 0)] {
            let once = pkg.related_xml_tree(name, role).unwrap().map(|tree| tree.text());
            let fuel = parse_ctx.fuel();
            assert_eq!(pkg.related_xml_tree(name, role).unwrap().map(|tree| tree.text()), once);
            assert_eq!(parse_ctx.fuel() - fuel, nodes, "{name}");
        }
        assert_eq!(pkg.trees.borrow().len(), 3);
        for role in [Role::Reported(Region::Body), Role::Content(Region::Footer)] {
            assert_eq!(pkg.related_xml_tree("c/r.xml", role).unwrap().unwrap().text(), "&bogus;");
            assert!(pkg.related_xml_tree("c/bad.xml", role).unwrap().is_none());
        }
        assert_eq!(
            reported(&parse_ctx),
            [
                (DiagnosticCode::PartUnreadable, Region::Footer, 1),
                (DiagnosticCode::UnresolvedEntity, Region::Body, 1),
                (DiagnosticCode::UnresolvedEntity, Region::Footer, 1),
                (DiagnosticCode::UnresolvedEntity, Region::Note, 2),
            ]
        );
    }

    #[test]
    fn shared_trees_stay_within_the_node_budget_and_a_part_past_it_parses_each_time() {
        let names: Vec<String> = (0..10).map(|i| format!("c/{i}.xml")).collect();
        let parts: Vec<(&str, &[u8])> =
            names.iter().map(|name| (name.as_str(), b"<a><b/>t</a>".as_slice())).collect();
        let data = zip_package(&parts);
        let limits = Limits { max_xml_nodes: 10, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let role = Role::Reported(Region::Body);
        for name in &names {
            assert_eq!(pkg.related_xml_tree(name, role).unwrap().unwrap().text(), "t", "{name}");
        }
        let cached: u64 = pkg.trees.borrow().values().flatten().map(|tree| tree.nodes).sum();
        assert_eq!(cached, 9);
        for (name, charged) in [("c/0.xml", 3), ("c/9.xml", 5), ("c/9.xml", 5)] {
            let fuel = parse_ctx.fuel();
            assert_eq!(pkg.related_xml_tree(name, role).unwrap().unwrap().text(), "t", "{name}");
            assert_eq!(parse_ctx.fuel() - fuel, charged, "{name}");
        }
    }

    #[test]
    fn only_epub_reads_html_named_references() {
        let data = zip_package(&[("a.xml", b"<a t=\"&eacute;\">&nbsp;</a>")]);
        for (kind, text, unresolved) in [
            (PackageKind::Opc, "&nbsp;", 1),
            (PackageKind::Odf, "&nbsp;", 1),
            (PackageKind::Epub, "\u{a0}", 0),
        ] {
            let parse_ctx = ParseCtx::default();
            let pkg = Package::open(&parse_ctx, &data, kind).unwrap();
            let tree = pkg.required_xml_part("a.xml", Role::Content(Region::Body)).unwrap();
            assert_eq!(tree.text(), text, "{kind:?}");
            let expected: Vec<_> = (unresolved > 0)
                .then_some((DiagnosticCode::UnresolvedEntity, Region::Body, unresolved))
                .into_iter()
                .collect();
            assert_eq!(reported(&parse_ctx), expected, "{kind:?}");
        }
    }
}
