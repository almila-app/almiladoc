use crate::ctx::ParseCtx;
use crate::diagnostics::{Diagnostic, DiagnosticCode, Diagnostics, LossClass, Region};
use crate::ledger::CancelToken;
use crate::model::{Block, Document, UnitKind};
use std::io::{Cursor, Write};
use std::num::NonZeroU32;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

pub(crate) trait DiagnosticRow {
    fn of(diagnostic: &Diagnostic) -> Self;
}

impl DiagnosticRow for (DiagnosticCode, Region, u64) {
    fn of(diagnostic: &Diagnostic) -> Self {
        (diagnostic.code, diagnostic.region, diagnostic.count)
    }
}

impl DiagnosticRow for (DiagnosticCode, Region, Option<u32>, u64) {
    fn of(diagnostic: &Diagnostic) -> Self {
        let unit = diagnostic.unit.map(NonZeroU32::get);
        (diagnostic.code, diagnostic.region, unit, diagnostic.count)
    }
}

impl DiagnosticRow for (DiagnosticCode, LossClass, Region, u64) {
    fn of(diagnostic: &Diagnostic) -> Self {
        (diagnostic.code, diagnostic.class, diagnostic.region, diagnostic.count)
    }
}

pub(crate) fn diagnostic_rows<R: DiagnosticRow>(diagnostics: &Diagnostics) -> Vec<R> {
    diagnostics.iter().map(|diagnostic| R::of(&diagnostic)).collect()
}

pub(crate) fn cancelled(token: &CancelToken) -> ParseCtx<'_> {
    token.cancel();
    ParseCtx::default().with_cancel(Some(token))
}

pub(crate) fn unit_starts(document: &Document) -> Vec<(UnitKind, u32)> {
    document
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::UnitStart(unit) => Some((unit.kind, unit.ordinal.get())),
            _ => None,
        })
        .collect()
}

pub(crate) fn unit_ordinals(document: &Document) -> Vec<u32> {
    unit_starts(document).into_iter().map(|(_, ordinal)| ordinal).collect()
}

pub(crate) fn zip_package<N: AsRef<str>, B: AsRef<[u8]>>(parts: &[(N, B)]) -> Vec<u8> {
    zip_package_with(parts, SimpleFileOptions::default())
}

pub(crate) fn zip_package_with<N: AsRef<str>, B: AsRef<[u8]>>(
    parts: &[(N, B)],
    options: SimpleFileOptions,
) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in parts {
        writer.start_file(name.as_ref(), options).unwrap();
        writer.write_all(body.as_ref()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

pub(crate) fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
}

pub(crate) fn corrupt_entry(data: &mut [u8], name: &str) {
    let start = zip::ZipArchive::new(Cursor::new(&*data))
        .unwrap()
        .by_name(name)
        .unwrap()
        .data_start()
        .unwrap();
    data[usize::try_from(start).unwrap()] ^= 1;
}

fn stored_block(block: &[u8]) -> Vec<u8> {
    let len = u16::try_from(block.len()).unwrap().to_le_bytes();
    [&[0][..], &len, &[!len[0], !len[1]], block].concat()
}

pub(crate) fn broken_deflate_stream(produced: &[u8]) -> Vec<u8> {
    let blocks = produced.chunks(usize::from(u16::MAX)).flat_map(stored_block);
    blocks.chain([0b111]).collect()
}

pub(crate) fn stored_deflate_stream<'a>(blocks: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let blocks = blocks.into_iter().flat_map(stored_block);
    blocks.chain([1, 0, 0, 0xFF, 0xFF]).collect()
}

pub(crate) fn empty_deflate_blocks(len: usize) -> Vec<u8> {
    stored_deflate_stream(std::iter::repeat_n([].as_slice(), len / 5))
}

pub(crate) fn positions(data: &[u8], needle: &[u8]) -> Vec<usize> {
    data.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect()
}

/// MTEF v5 of variables in one line, as `MathType` writes it.
pub(crate) fn mathtype(inline: bool, mtcodes: &[u16]) -> Vec<u8> {
    let chars = mtcodes.iter().flat_map(|code| [&[2, 0, 131][..], &code.to_le_bytes()].concat());
    [&[5, 1, 0, 6, 0][..], b"DSMT4\0", &[u8::from(inline), 10, 1, 0]]
        .concat()
        .into_iter()
        .chain(chars)
        .chain([0, 0])
        .collect()
}

/// MTEF v3 of variables in one line, as Equation Editor 3.x writes it.
pub(crate) fn equation_editor(mtcodes: &[u16]) -> Vec<u8> {
    let chars = mtcodes.iter().flat_map(|code| [&[2, 131][..], &code.to_le_bytes()].concat());
    [3, 1, 1, 3, 0, 10, 1].into_iter().chain(chars).chain([0, 0]).collect()
}

/// The CFB an equation object embeds, its `Equation Native` stream an EQNOLEFILEHDR of
/// `header_bytes` declaring `declared` MTEF bytes before `mtef`.
pub(crate) fn equation_object(header_bytes: u16, declared: usize, mtef: &[u8]) -> Vec<u8> {
    let declared = u32::try_from(declared).unwrap();
    let header =
        [&header_bytes.to_le_bytes()[..], &[0; 6], &declared.to_le_bytes(), &[0; 16]].concat();
    one_stream_cfb("Equation Native", &[header, mtef.to_vec()].concat())
}

pub(crate) fn one_stream_cfb(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    ole.create_stream(name).unwrap().write_all(bytes).unwrap();
    ole.flush().unwrap();
    ole.into_inner().into_inner()
}
