//! ZIP archive access with decompression limits.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::package::directory::{self, ContainerFacts};
use crate::package::xml::{Dialect, Element, Marks, NodeSink, Parsed, WholeTree, parse};
use std::cell::{Cell, RefCell};
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

    pub(crate) fn unresolved(self) -> Option<(DiagnosticCode, Region)> {
        match self {
            Role::Content(region) | Role::Reported(region) => {
                Some((DiagnosticCode::UnresolvedEntity, region))
            }
            Role::Formatting | Role::Probe => None,
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

/// A ZIP-based document package (OOXML, ODF, EPUB). Each borrow of its
/// caches is scoped to one method, and none is held while an entry inflates.
pub struct Package<'a> {
    parse_ctx: &'a ParseCtx<'a>,
    kind: PackageKind,
    zip: zip::ZipArchive<Cursor<&'a [u8]>>,
    facts: ContainerFacts,
    /// Entry indices by ASCII-lowercased name, for OPC alone: its part names
    /// compare case-insensitively in ASCII (ECMA-376 Part 2).
    folded: HashMap<Vec<u8>, usize>,
    /// Decompressed parts by entry: repeated references are served from the
    /// cache instead of re-decompressing and re-charging the total-bytes
    /// budget (which would falsely trip on valid documents that reference one
    /// part many times). Bounded by `max_total_bytes`. Buffers are shared
    /// (`Rc`), so a cache hit never copies the bytes.
    cache: RefCell<HashMap<usize, Rc<[u8]>>>,
    /// Shared trees by entry, `None` for a part that could not be read or
    /// parsed. Together they hold at most `max_xml_nodes` nodes, so the
    /// cache never keeps more than one part at its limit could.
    trees: RefCell<HashMap<usize, Option<SharedTree>>>,
    cached_nodes: Cell<u64>,
}

/// A parsed part shared by every reference that names it, beside the node
/// count each reuse charges and the marks each reuse reports.
#[derive(Clone)]
struct SharedTree {
    root: Rc<Element>,
    nodes: u64,
    marks: Marks,
}

impl<'a> Package<'a> {
    pub fn open(
        parse_ctx: &'a ParseCtx<'a>,
        bytes: &'a [u8],
        kind: PackageKind,
    ) -> Result<Self, ConvertError> {
        directory::admit(bytes, parse_ctx.limits())?;
        let zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|_| ConvertError::malformed("not a readable zip archive"))?;
        let facts = directory::inspect(&zip, bytes, parse_ctx.limits())?;
        Self::assemble(parse_ctx, kind, zip, facts)
    }

    /// The same archive read as a package of `kind`, without admitting its
    /// directory again.
    pub fn reopen(&self, kind: PackageKind) -> Result<Self, ConvertError> {
        Self::assemble(self.parse_ctx, kind, self.zip.clone(), self.facts)
    }

    fn assemble(
        parse_ctx: &'a ParseCtx<'a>,
        kind: PackageKind,
        zip: zip::ZipArchive<Cursor<&'a [u8]>>,
        facts: ContainerFacts,
    ) -> Result<Self, ConvertError> {
        let folded = match kind {
            PackageKind::Opc => folded_names(&zip)?,
            PackageKind::Odf | PackageKind::Epub => HashMap::new(),
        };
        Ok(Package {
            parse_ctx,
            kind,
            zip,
            facts,
            folded,
            cache: RefCell::new(HashMap::new()),
            trees: RefCell::new(HashMap::new()),
            cached_nodes: Cell::new(0),
        })
    }

    /// The entry count and sizes the central directory declares.
    #[cfg_attr(not(test), expect(dead_code, reason = "no caller inspects a package yet"))]
    pub fn facts(&self) -> ContainerFacts {
        self.facts
    }

    fn dialect(&self) -> Dialect {
        match self.kind {
            PackageKind::Opc | PackageKind::Odf => Dialect::Xml,
            PackageKind::Epub => Dialect::Xhtml,
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        match self.kind {
            PackageKind::Opc => self.folded.get(&name.as_bytes().to_ascii_lowercase()).copied(),
            PackageKind::Odf | PackageKind::Epub => self.zip.index_for_name(name),
        }
    }

    /// Read a part's bytes. `Ok(None)` means the part is absent (a valid
    /// state for optional parts); `Err` means it exists but cannot be read.
    /// Callers apply the unified policy: skip and report by role when useful
    /// output remains, propagate when the part is the primary content.
    pub fn part(&self, name: &str) -> Result<Option<Rc<[u8]>>, ConvertError> {
        // OPC part URIs may carry a leading slash; entries never do.
        let name = name.trim_start_matches('/');
        let Some(index) = self.index_of(name) else {
            return Ok(None);
        };
        if let Some(bytes) = self.cache.borrow().get(&index) {
            return Ok(Some(Rc::clone(bytes)));
        }
        let mut zip = self.zip.clone();
        let mut file = zip
            .by_index(index)
            .map_err(|_| ConvertError::malformed_part(name, "unreadable archive entry"))?;
        let limits = self.parse_ctx.limits();
        if file.size() > limits.max_entry_bytes {
            return Err(ConvertError::limit(Limit::EntryBytes, limits.max_entry_bytes));
        }
        // zip fails a read past the declared size, so only the whole-archive
        // total can still run out mid-entry.
        let remaining_total = limits.max_total_bytes.saturating_sub(self.parse_ctx.archive_bytes());
        let cap = limits.max_entry_bytes.min(remaining_total);
        let bytes = read_entry(self.parse_ctx, &mut file, cap, name)?;
        let read = bytes.len() as u64;
        if read > cap {
            return Err(ConvertError::limit(Limit::TotalBytes, limits.max_total_bytes));
        }
        self.parse_ctx.count_archive_bytes(read);
        let bytes: Rc<[u8]> = Rc::from(bytes);
        self.cache.borrow_mut().insert(index, Rc::clone(&bytes));
        Ok(Some(bytes))
    }

    /// The first `cap` bytes of a part, whatever size it declares, counted
    /// against the entry and total budgets and never cached. An absent or
    /// unreadable part reads as absent; a fatal error propagates.
    pub fn probe_head(&self, name: &str, cap: u64) -> Result<Option<Vec<u8>>, ConvertError> {
        Ok(self.probe(name, cap)?.map(|(mut bytes, cap)| {
            bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
            bytes
        }))
    }

    /// A part of at most `cap` bytes, and within the entry and total
    /// budgets: a larger part, or one that does not read, reads as absent; a
    /// fatal error propagates.
    pub fn probe_part(&self, name: &str, cap: u64) -> Result<Option<Vec<u8>>, ConvertError> {
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
            .by_index(index)
            .map_err(|_| ConvertError::malformed_part(name, "unreadable archive entry"))
            .and_then(|mut file| read_entry(self.parse_ctx, &mut file, cap, name));
        if let Ok(bytes) = &read {
            self.parse_ctx.count_archive_bytes(bytes.len() as u64);
        }
        skip_unreadable(self.parse_ctx, Role::Probe, read.map(|bytes| Some((bytes, cap))))
    }

    /// [`Package::probe_part`], parsed as XML: a part that does not parse
    /// reads as absent too.
    pub fn probe_xml_part(&self, name: &str, cap: u64) -> Result<Option<Element>, ConvertError> {
        let Some(bytes) = self.probe_part(name, cap)? else {
            return Ok(None);
        };
        let parsed = self.parse_part(name, &bytes, &mut WholeTree).map(|parsed| Some(parsed.root));
        skip_unreadable(self.parse_ctx, Role::Probe, parsed)
    }

    /// True when a part exists, without reading (or budget-charging) it.
    pub fn has_part(&self, name: &str) -> bool {
        self.index_of(name.trim_start_matches('/')).is_some()
    }

    /// Read a part that must exist for any meaningful output.
    pub fn required_part(&self, name: &str) -> Result<Rc<[u8]>, ConvertError> {
        self.part(name)?.ok_or_else(|| ConvertError::missing_part(name))
    }

    /// Read an optional part a conventional name points at: absent is a
    /// valid state, and an unreadable part reports by `role` and reads as
    /// absent; a fatal error propagates.
    pub fn optional_part(&self, name: &str, role: Role) -> Result<Option<Rc<[u8]>>, ConvertError> {
        self.read_part(name, role, Absent::Valid)
    }

    /// Read a part a relationship or reference names: absent or unreadable,
    /// it reports by `role` and reads as absent; a fatal error propagates.
    pub fn related_part(&self, name: &str, role: Role) -> Result<Option<Rc<[u8]>>, ConvertError> {
        self.read_part(name, role, Absent::Lost)
    }

    /// [`Package::optional_part`], parsed as XML: a part that does not parse
    /// reports by `role` and reads as absent.
    pub fn optional_xml_part(
        &self,
        name: &str,
        role: Role,
    ) -> Result<Option<Element>, ConvertError> {
        self.read_xml_part(name, role, Absent::Valid, &mut WholeTree)
    }

    /// [`Package::related_part`], parsed as XML: a part that does not parse
    /// reports by `role` and reads as absent.
    pub fn related_xml_part(
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
    pub fn related_xml_part_streamed(
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
    pub fn related_xml_tree(
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
    /// meaningful output: markup it would need repaired is `Malformed`, and
    /// its decoding and references report by `role`.
    pub fn required_xml_part(&self, name: &str, role: Role) -> Result<Element, ConvertError> {
        let bytes = self.required_part(name)?;
        let parsed = self.parse_part(name, &bytes, &mut WholeTree)?;
        if parsed.marks.recovered {
            return Err(ConvertError::malformed_part(name, "ill-formed xml"));
        }
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

fn folded_names(
    zip: &zip::ZipArchive<Cursor<&[u8]>>,
) -> Result<HashMap<Vec<u8>, usize>, ConvertError> {
    let mut folded = HashMap::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip
            .by_index_data(index)
            .map_err(|_| ConvertError::malformed("unreadable entry name"))?;
        if folded.insert(entry.name_raw().to_ascii_lowercase(), index).is_some() {
            return Err(ConvertError::malformed("entry names equal after case folding"));
        }
    }
    Ok(folded)
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
        let data = package_of(&[("word/document.xml", b"<document/>")]);
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
    fn a_recovered_part_reports_by_the_reading_role_and_fails_where_required() {
        let data = package_of(&[("word/a.xml", b"<a><b>text</a>")]);
        let cases = [
            (Role::Content(Region::Note), vec![(DiagnosticCode::RecoveredXml, Region::Note, 1)]),
            (Role::Reported(Region::Body), vec![(DiagnosticCode::RecoveredXml, Region::Body, 1)]),
            (Role::Formatting, vec![(DiagnosticCode::StylePartUnreadable, Region::Container, 1)]),
            (Role::Probe, vec![]),
        ];
        for (role, expected) in cases {
            let parse_ctx = ParseCtx::default();
            let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            assert!(pkg.optional_xml_part("word/a.xml", role).unwrap().is_some());
            assert_eq!(reported(&parse_ctx), expected, "{role:?}");
            let parse_ctx = ParseCtx::default();
            let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
            let err = pkg.required_xml_part("word/a.xml", role).unwrap_err();
            assert!(
                matches!(&err, ConvertError::Malformed { part: Some(part), reason: "ill-formed xml" }
                    if part.as_str() == "word/a.xml"),
                "{role:?}: {err:?}"
            );
            assert_eq!(reported(&parse_ctx), [], "{role:?} required");
        }
    }

    #[test]
    fn a_corrupt_entry_is_neither_cached_nor_charged() {
        let data = corrupt_entry("word/document.xml");
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("word/document.xml").unwrap_err();
        assert!(matches!(err, ConvertError::Malformed { reason: "corrupt archive entry", .. }));
        assert!(pkg.cache.borrow().is_empty());
        assert_eq!(parse_ctx.archive_bytes(), 0);
        assert!(pkg.optional_part("word/document.xml", Role::Probe).unwrap().is_none());
        assert!(pkg.optional_xml_part("word/document.xml", Role::Probe).unwrap().is_none());
        assert_eq!(parse_ctx.archive_bytes(), 0);
    }

    #[test]
    fn repeated_reads_are_cached_and_charged_once() {
        let data = one_part_zip("media/a.bin", &[7u8; 4096]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        for _ in 0..5 {
            assert_eq!(pkg.part("media/a.bin").unwrap().unwrap().len(), 4096);
        }
        assert_eq!(parse_ctx.archive_bytes(), 4096, "repeated reads must not re-charge the budget");
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
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
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
    fn an_entry_past_its_declared_size_is_malformed() {
        let mut data = one_part_zip("a.bin", &[7u8; 4096]);
        let understate =
            |at: usize, data: &mut [u8]| data[at..at + 4].copy_from_slice(&16u32.to_le_bytes());
        let local = data.windows(4).position(|w| w == b"PK\x03\x04").unwrap();
        understate(local + 22, &mut data);
        let central = data.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        understate(central + 24, &mut data);
        let limits = Limits { max_entry_bytes: 64, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.part("a.bin").unwrap_err();
        assert!(
            matches!(&err, ConvertError::Malformed { part: Some(part), reason: "corrupt archive entry" }
                if part.as_str() == "a.bin"),
            "{err:?}"
        );
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
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.optional_part("word/comments.xml", Role::Probe).unwrap().is_none());
        assert!(pkg.optional_part("word/comments.xml", Role::Probe).unwrap().is_none());
        assert_eq!(parse_ctx.fuel(), 8);
        let limits = Limits { max_work: 3, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
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
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("word/comments.xml").unwrap().is_some());
        token.cancel();
        let role = Role::Content(Region::Comment);
        let read = pkg.optional_xml_part("word/comments.xml", role);
        assert!(matches!(read, Err(ConvertError::Cancelled)), "{read:?}");
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let read = pkg.related_part("word/comments.xml", role);
        assert!(matches!(read, Err(ConvertError::Cancelled)), "{read:?}");
        assert_eq!(reported(&parse_ctx), []);
    }

    #[test]
    fn leading_slash_part_names_normalize() {
        let data = one_part_zip("word/document.xml", b"<x/>");
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert!(pkg.part("/word/document.xml").unwrap().is_some());
    }

    const KINDS: [PackageKind; 3] = [PackageKind::Opc, PackageKind::Odf, PackageKind::Epub];

    #[test]
    fn exact_duplicate_names_are_malformed_in_every_kind() {
        let mut data = package_of(&[("a.xml", b"<first/>"), ("b.xml", b"<second/>")]);
        for at in data
            .windows(5)
            .enumerate()
            .filter(|(_, w)| w == b"b.xml")
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
        {
            data[at] = b'a';
        }
        for kind in KINDS {
            let err = Package::open(&ParseCtx::default(), &data, kind).err().unwrap();
            assert!(
                matches!(err, ConvertError::Malformed { reason: "duplicate entry names", .. }),
                "{kind:?}: {err:?}"
            );
        }
    }

    #[test]
    fn names_equal_after_ascii_case_folding_are_duplicates_in_opc_only() {
        let data = package_of(&[("word/a.xml", b"<lower/>"), ("word/A.xml", b"<upper/>")]);
        let err = Package::open(&ParseCtx::default(), &data, PackageKind::Opc).err().unwrap();
        assert!(
            matches!(
                err,
                ConvertError::Malformed { reason: "entry names equal after case folding", .. }
            ),
            "{err:?}"
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
        let data = one_part_zip("word/document.xml", &[7u8; 2048]);
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
    fn facts_are_the_central_directorys() {
        let data = package_of(&[("a.xml", &[b'a'; 3000]), ("b.xml", b"<b/>")]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Odf).unwrap();
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        let compressed = (0..2).map(|i| zip.by_index_data(i).unwrap().compressed_size()).sum();
        assert_eq!(
            pkg.facts(),
            ContainerFacts { entries: 2, compressed_bytes: compressed, uncompressed_bytes: 3004 }
        );
    }

    #[test]
    fn duplicates_behind_a_refused_end_record_are_malformed() {
        let mut data = package_of(&[("a.xml", b"<first/>"), ("b.xml", b"<second/>")]);
        let at: Vec<usize> =
            data.windows(5).enumerate().filter(|(_, w)| w == b"b.xml").map(|(i, _)| i).collect();
        for i in at {
            data[i] = b'a';
        }
        let end = data.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        let mut appended = data[end..end + 22].to_vec();
        appended[4] = 1;
        appended[8] = 1;
        appended[10] = 1;
        data.extend(appended);
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        assert_eq!(zip.len(), 1);
        let err = Package::open(&ParseCtx::default(), &data, PackageKind::Odf).err().unwrap();
        assert!(
            matches!(err, ConvertError::Malformed { reason: "duplicate entry names", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn compressed_sizes_past_the_directory_are_malformed() {
        let mut data = one_part_zip("word/document.xml", &[0; 4 << 20]);
        let limits = Limits { max_compression_ratio: 2, ..Limits::default() };
        let err = Package::open(&ParseCtx::new(&limits), &data, PackageKind::Opc).err().unwrap();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::CompressionRatio, max: 2 }),
            "{err:?}"
        );
        let central = data.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        data[central + 20..central + 24].copy_from_slice(&(4u32 << 20).to_le_bytes());
        assert!(zip::ZipArchive::new(Cursor::new(data.as_slice())).is_ok());
        let err = Package::open(&ParseCtx::new(&limits), &data, PackageKind::Opc).err().unwrap();
        assert!(
            matches!(
                err,
                ConvertError::Malformed { reason: "compressed sizes exceed the archive", .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_package_after_prepended_bytes_opens_where_zip_finds_it() {
        let data = [b"prefix!!".as_slice(), &one_part_zip("word/document.xml", b"<d/>")].concat();
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        assert_eq!(&*pkg.part("word/document.xml").unwrap().unwrap(), b"<d/>");
        assert_eq!(pkg.facts().entries, 1);
    }

    #[test]
    fn a_part_that_does_not_parse_is_malformed_by_name_where_required() {
        let data = package_of(&[("word/a.xml", b"<a>A < B</a>")]);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &data, PackageKind::Opc).unwrap();
        let err = pkg.required_xml_part("word/a.xml", Role::Content(Region::Body)).unwrap_err();
        assert!(
            matches!(&err, ConvertError::Malformed { part: Some(part), reason: "bare `<` in markup" }
                if part.as_str() == "word/a.xml"),
            "{err:?}"
        );
        assert!(
            pkg.optional_xml_part("word/a.xml", Role::Content(Region::Body)).unwrap().is_none()
        );
        assert_eq!(reported(&parse_ctx), [(DiagnosticCode::PartUnreadable, Region::Body, 1)]);
    }

    #[test]
    fn each_reuse_of_a_shared_tree_charges_its_nodes_and_reports_its_marks() {
        let data = package_of(&[
            ("c/a.xml", b"<a><b/>t</a>"),
            ("c/r.xml", b"<a><b>t</a>"),
            ("c/bad.xml", b"<a>A < B</a>"),
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
            assert_eq!(pkg.related_xml_tree("c/r.xml", role).unwrap().unwrap().text(), "t");
            assert!(pkg.related_xml_tree("c/bad.xml", role).unwrap().is_none());
        }
        assert_eq!(
            reported(&parse_ctx),
            [
                (DiagnosticCode::PartUnreadable, Region::Footer, 1),
                (DiagnosticCode::RecoveredXml, Region::Body, 1),
                (DiagnosticCode::RecoveredXml, Region::Footer, 1),
                (DiagnosticCode::RecoveredXml, Region::Note, 2),
            ]
        );
    }

    #[test]
    fn shared_trees_stay_within_the_node_budget_and_a_part_past_it_parses_each_time() {
        let names: Vec<String> = (0..10).map(|i| format!("c/{i}.xml")).collect();
        let parts: Vec<(&str, &[u8])> =
            names.iter().map(|name| (name.as_str(), b"<a><b/>t</a>".as_slice())).collect();
        let data = package_of(&parts);
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
        let data = package_of(&[("a.xml", b"<a t=\"&eacute;\">&nbsp;</a>")]);
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
