//! OfficeArt (MS-ODRAW) blip extraction shared by the legacy binary
//! formats: DOC picture data (`PICF` + OfficeArt records in the Data
//! stream) and the PPT `Pictures` stream (a sequence of BStore file
//! blocks), and the record walk their drawing readers share. Drawing
//! geometry is out of scope.

use crate::limits::{Limit, LimitExceeded};
use crate::shared::binary::{ByteReader, get_u16, get_u32};
use std::borrow::Cow;
use std::io::Read;

pub(crate) const DGG_CONTAINER: u16 = 0xF000;
pub(crate) const BSTORE_CONTAINER: u16 = 0xF001;
pub(crate) const DG_CONTAINER: u16 = 0xF002;
pub(crate) const SPGR_CONTAINER: u16 = 0xF003;
pub(crate) const SP_CONTAINER: u16 = 0xF004;
pub(crate) const FBSE: u16 = 0xF007;
pub(crate) const FSP: u16 = 0xF00A;
pub(crate) const FOPT: u16 = 0xF00B;
pub(crate) const CLIENT_DATA: u16 = 0xF011;
const BLIP_EMF: u16 = 0xF01A;
const BLIP_WMF: u16 = 0xF01B;
const BLIP_JPEG: u16 = 0xF01D;
const BLIP_PNG: u16 = 0xF01E;
pub(crate) const TERTIARY_FOPT: u16 = 0xF122;

pub(crate) const LTXID: u16 = 0x0080;
pub(crate) const PIB: u16 = 0x0104;
pub(crate) const FILL_TYPE: u16 = 0x0180;
pub(crate) const FILL_BLIP: u16 = 0x0186;
pub(crate) const FILL_STYLE_BOOLEAN_PROPERTIES: u16 = 0x01BF;

/// The `recVer` of a container record.
pub(crate) const CONTAINER_VERSION: u16 = 0xF;

const JPEG_RGB_TWO_UIDS: u16 = 0x46B;
const JPEG_CMYK_TWO_UIDS: u16 = 0x6E3;
const PNG_TWO_UIDS: u16 = 0x6E1;
const EMF_TWO_UIDS: u16 = 0x3D5;
const WMF_TWO_UIDS: u16 = 0x217;

const COMPRESSION_DEFLATE: u8 = 0x00;

const PROPERTY_LEN: usize = 6;
const FBSE_NAME_LEN_AT: usize = 33;
const FBSE_HEADER_LEN: usize = 36;

/// An OfficeArt record: the same 8-byte header the PPT record stream uses,
/// then its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Record<'a> {
    pub(crate) ver_inst: u16,
    pub(crate) rec_type: u16,
    pub(crate) body: &'a [u8],
}

impl Record<'_> {
    pub(crate) fn instance(self) -> u16 {
        self.ver_inst >> 4
    }

    pub(crate) fn version(self) -> u16 {
        self.ver_inst & 0xF
    }
}

/// The OfficeArt record at `off`.
pub(crate) fn record_at(data: &[u8], off: usize) -> Option<Record<'_>> {
    let mut record = ByteReader::new(data.get(off..)?);
    let ver_inst = record.u16()?;
    let rec_type = record.u16()?;
    let len = usize::try_from(record.u32()?).ok()?;
    Some(Record { ver_inst, rec_type, body: record.take(len)? })
}

/// The records laid out back to back in `data`, up to the first that does
/// not fit.
pub(crate) fn records(data: &[u8]) -> impl Iterator<Item = Record<'_>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let record = record_at(data, pos)?;
        pos += 8 + record.body.len();
        Some(record)
    })
}

/// The `(property id, value)` pairs of an `OfficeArt` property table (`OPT` or
/// tertiary `OPT`) of `count` entries, up to the first that does not fit.
pub(crate) fn properties(count: u16, table: &[u8]) -> impl Iterator<Item = (u16, u32)> + '_ {
    let (entries, _) = table.as_chunks::<PROPERTY_LEN>();
    entries
        .iter()
        .take(usize::from(count))
        .map_while(|entry| Some((get_u16(entry, 0)? & 0x3FFF, get_u32(entry, 2)?)))
}

/// An extracted picture payload.
pub(crate) struct Blip<'a> {
    pub(crate) media_type: &'static str,
    pub(crate) extension: &'static str,
    pub(crate) bytes: Cow<'a, [u8]>,
}

/// Decode one blip record (`recType` 0xF01A–0xF01F). Metafile blips may be
/// deflate-compressed; the output is bounded by the declared uncompressed
/// size, and inflating past `max_bytes` is [`Limit::EntryBytes`].
pub(crate) fn decode_blip(
    record: Record<'_>,
    max_bytes: u64,
) -> Result<Option<Blip<'_>>, LimitExceeded> {
    let Record { rec_type, body, .. } = record;
    match rec_type {
        // Bitmap blips: rgbUid1 (16), + rgbUid2 (16) for the doubled
        // instance, then the picture bytes, with one tag byte first.
        BLIP_JPEG | BLIP_PNG => {
            let doubled =
                matches!(record.instance(), JPEG_RGB_TWO_UIDS | JPEG_CMYK_TWO_UIDS | PNG_TWO_UIDS);
            let start = if doubled { 32 } else { 16 } + 1;
            let Some(bytes) = body.get(start..) else { return Ok(None) };
            let (media_type, extension) =
                if rec_type == BLIP_JPEG { ("image/jpeg", "jpg") } else { ("image/png", "png") };
            Ok(Some(Blip { media_type, extension, bytes: Cow::Borrowed(bytes) }))
        }
        // Metafile blips: rgbUid (16/32), then a 34-byte metafile header
        // (cbSize, bounds, ptSize, cbSave, compression, filter).
        BLIP_EMF | BLIP_WMF => {
            let doubled = matches!(record.instance(), EMF_TWO_UIDS | WMF_TWO_UIDS);
            let header = if doubled { 32 } else { 16 };
            let Some(Metafile { cb_size, compression, data }) = metafile_parts(body.get(header..))
            else {
                return Ok(None);
            };
            let (media_type, extension) =
                if rec_type == BLIP_EMF { ("image/emf", "emf") } else { ("image/wmf", "wmf") };
            let bytes = match compression {
                COMPRESSION_DEFLATE => match inflate(data, cb_size, max_bytes)? {
                    Some(out) => Cow::Owned(out),
                    None => return Ok(None),
                },
                _ => Cow::Borrowed(data),
            };
            Ok(Some(Blip { media_type, extension, bytes }))
        }
        _ => Ok(None),
    }
}

/// A metafile blip's header fields this reader reads, and the data after it.
#[derive(Debug, PartialEq, Eq)]
struct Metafile<'a> {
    cb_size: u64,
    compression: u8,
    data: &'a [u8],
}

fn metafile_parts(header_and_data: Option<&[u8]>) -> Option<Metafile<'_>> {
    let mut header = ByteReader::new(header_and_data?);
    let cb_size = header.u32()?;
    header.skip(28)?;
    let compression = header.u8()?;
    header.skip(1)?;
    Some(Metafile { cb_size: u64::from(cb_size), compression, data: header.rest() })
}

fn inflate(data: &[u8], cb_size: u64, max_bytes: u64) -> Result<Option<Vec<u8>>, LimitExceeded> {
    let mut out = Vec::new();
    let mut decoder =
        flate2::read::DeflateDecoder::new(data).take(cb_size.min(max_bytes.saturating_add(1)));
    if decoder.read_to_end(&mut out).is_err() {
        return Ok(None);
    }
    if out.len() as u64 > max_bytes {
        return Err(LimitExceeded { limit: Limit::EntryBytes, max: max_bytes });
    }
    Ok(Some(out))
}

/// Find and decode the first blip in a run of OfficeArt records (a
/// `Pictures` stream block sequence, or an inline shape container),
/// descending into containers. Bounded traversal: record counts and
/// nesting beyond any real drawing abort the search.
pub(crate) fn first_blip(data: &[u8], max_bytes: u64) -> Result<Option<Blip<'_>>, LimitExceeded> {
    // (cursor, end) ranges into `data`.
    let mut stack: Vec<(usize, usize)> = vec![(0, data.len())];
    let mut visited = 0u32;
    loop {
        let Some(top) = stack.last_mut() else { return Ok(None) };
        let (cursor, end) = *top;
        if cursor >= end {
            stack.pop();
            continue;
        }
        let Some(record) = data.get(..end).and_then(|scope| record_at(scope, cursor)) else {
            stack.pop();
            continue;
        };
        let body = record.body;
        let Some(body_start) = cursor.checked_add(8) else { return Ok(None) };
        let Some(body_end) = body_start.checked_add(body.len()) else { return Ok(None) };
        top.0 = body_end;
        visited += 1;
        if visited > 10_000 || stack.len() > 16 {
            return Ok(None);
        }
        if let Some(blip) = decode_blip(record, max_bytes)? {
            return Ok(Some(blip));
        }
        if record.rec_type == FBSE {
            let Some(inner_start) =
                body_start.checked_add(fbse_blip_offset(body).unwrap_or(body.len()))
            else {
                return Ok(None);
            };
            if inner_start < body_end {
                stack.push((inner_start, body_end));
            }
            continue;
        }
        if record.version() == CONTAINER_VERSION {
            stack.push((body_start, body_end));
        }
    }
}

/// Offset of the embedded blip record inside an FBSE (0xF007) body: the
/// 36-byte header plus the entry's name.
fn fbse_blip_offset(body: &[u8]) -> Option<usize> {
    body.get(FBSE_NAME_LEN_AT).map(|&name_len| FBSE_HEADER_LEN + usize::from(name_len))
}

/// Decode the blip embedded in an FBSE (0xF007) record body, if present.
pub(crate) fn fbse_blip(body: &[u8], max_bytes: u64) -> Result<Option<Blip<'_>>, LimitExceeded> {
    let Some(blip) = fbse_blip_offset(body).and_then(|offset| record_at(body, offset)) else {
        return Ok(None);
    };
    decode_blip(blip, max_bytes)
}

#[cfg(test)]
pub(crate) fn record(ver_inst: u16, rec_type: u16, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).unwrap();
    [&ver_inst.to_le_bytes()[..], &rec_type.to_le_bytes(), &len.to_le_bytes(), body].concat()
}

/// A metafile blip record (0xF01A) whose deflated payload inflates to `inflated` zero bytes.
#[cfg(test)]
pub(crate) fn deflated_metafile_blip(inflated: usize) -> Vec<u8> {
    use std::io::Write;
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&vec![0u8; inflated]).unwrap();
    let data = encoder.finish().unwrap();
    let mut body = vec![0u8; 16];
    body.extend_from_slice(&u32::try_from(inflated).unwrap().to_le_bytes());
    body.extend_from_slice(&[0u8; 30]);
    body.extend_from_slice(&data);
    record(0x3D40, 0xF01A, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_blip_is_found_after_a_sibling_and_inside_a_container() {
        let leaf = record(0x0000, 0xF00B, &[0; 4]);
        let png = record(0x06E0, 0xF01E, &[&[0; 17][..], b"PNG"].concat());
        let flat = [leaf.clone(), png.clone()].concat();
        let container = record(0x000F, 0xF004, &[leaf, png].concat());
        for data in [flat, container] {
            let blip = first_blip(&data, 1 << 20).unwrap().unwrap();
            assert_eq!(&*blip.bytes, b"PNG");
        }
    }

    #[test]
    fn a_record_inside_a_container_is_read_only_within_it() {
        let png = [&[0; 17][..], b"PNG"].concat();
        let child = record(0x06E0, 0xF01E, &png);
        let container = record(0x000F, 0xF004, &child[..8]);
        let data = [container, png].concat();
        assert!(first_blip(&data, 1 << 20).unwrap().is_none());
    }

    #[test]
    fn a_record_is_read_only_when_its_body_fits() {
        let empty = record(0x000F, 0xF004, &[]);
        let read =
            |ver_inst, rec_type, body: &'static [u8]| Some(Record { ver_inst, rec_type, body });
        assert_eq!(record_at(&empty, 0), read(0x000F, 0xF004, &[]));
        let full = record(0x0000, 0xF00B, &[1, 2, 3]);
        assert_eq!(record_at(&full, 0), read(0x0000, 0xF00B, &[1, 2, 3]));
        assert_eq!(record_at(&full[..full.len() - 1], 0), None);
        assert_eq!(record_at(&full[..7], 0), None);
        assert_eq!(record_at(&full, full.len() + 1), None);
        let walk = [empty, full.clone(), full[..10].to_vec()].concat();
        assert_eq!(
            records(&walk).map(|record| record.rec_type).collect::<Vec<_>>(),
            [0xF004, 0xF00B]
        );
    }

    #[test]
    fn an_fbse_blip_starts_past_the_header_and_its_name() {
        let mut body = vec![0xEE; 34];
        body[33] = 5;
        assert_eq!(fbse_blip_offset(&body[..33]), None);
        assert_eq!(fbse_blip_offset(&body), Some(41));
        body[33] = u8::MAX;
        assert_eq!(fbse_blip_offset(&body), Some(36 + 255));
    }

    #[test]
    fn a_metafile_header_needs_its_compression_byte_and_the_byte_after() {
        let header = |len: usize| {
            let mut bytes = 7u32.to_le_bytes().to_vec();
            bytes.resize(len, 0xFE);
            bytes
        };
        let parts = |data: &'static [u8]| Some(Metafile { cb_size: 7, compression: 0xFE, data });
        assert_eq!(metafile_parts(Some(&header(33)[..])), None);
        assert_eq!(metafile_parts(Some(&header(34)[..])), parts(&[]));
        assert_eq!(metafile_parts(Some(&header(36)[..])), parts(&[0xFE, 0xFE]));
        assert_eq!(metafile_parts(None), None);
    }

    #[test]
    fn each_blip_record_type_decodes_to_its_media_type_past_its_uids() {
        let bitmap = |uids: usize| [&vec![0; 16 * uids + 1][..], b"pic"].concat();
        let metafile = |uids: usize| {
            [&vec![0; 16 * uids][..], &3u32.to_le_bytes(), &[0; 28], &[0xFE, 0], b"pic"].concat()
        };
        let cases = [
            (0x46A, 0xF01D, bitmap(1), "image/jpeg", "jpg"),
            (0x46B, 0xF01D, bitmap(2), "image/jpeg", "jpg"),
            (0x6E2, 0xF01D, bitmap(1), "image/jpeg", "jpg"),
            (0x6E3, 0xF01D, bitmap(2), "image/jpeg", "jpg"),
            (0x6E0, 0xF01E, bitmap(1), "image/png", "png"),
            (0x6E1, 0xF01E, bitmap(2), "image/png", "png"),
            (0x3D4, 0xF01A, metafile(1), "image/emf", "emf"),
            (0x3D5, 0xF01A, metafile(2), "image/emf", "emf"),
            (0x216, 0xF01B, metafile(1), "image/wmf", "wmf"),
            (0x217, 0xF01B, metafile(2), "image/wmf", "wmf"),
        ];
        for (instance, rec_type, body, media_type, extension) in cases {
            let record = Record { ver_inst: instance << 4, rec_type, body: &body };
            let blip = decode_blip(record, 1 << 20).unwrap().unwrap();
            assert_eq!(
                (blip.media_type, blip.extension, &*blip.bytes),
                (media_type, extension, &b"pic"[..]),
                "{rec_type:#06X} instance {instance:#05X}"
            );
        }
        let dib = Record { ver_inst: 0x7A80, rec_type: 0xF01F, body: &bitmap(1) };
        assert!(decode_blip(dib, 1 << 20).unwrap().is_none());
    }

    #[test]
    fn a_property_table_yields_only_its_declared_entries() {
        let entry = |id: u16, value: u32| [&id.to_le_bytes()[..], &value.to_le_bytes()].concat();
        let table = [entry(0x4104, 5), entry(0x0186, 1)].concat();
        assert_eq!(properties(1, &table).collect::<Vec<_>>(), [(0x0104, 5)]);
    }
}
