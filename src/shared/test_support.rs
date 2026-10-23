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

pub(crate) fn positions(data: &[u8], needle: &[u8]) -> Vec<usize> {
    data.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect()
}
