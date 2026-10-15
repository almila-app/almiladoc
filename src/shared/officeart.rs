//! OfficeArt (MS-ODRAW) blip extraction shared by the legacy binary
//! formats: DOC picture data (`PICF` + OfficeArt records in the Data
//! stream) and the PPT `Pictures` stream (a sequence of BStore file
//! blocks), and the record walk their drawing readers share. Drawing
//! geometry is out of scope.

use crate::limits::{Limit, LimitExceeded};
use std::borrow::Cow;
use std::io::Read;

/// (verAndInstance, recType, body) of the OfficeArt record at `off` — the
/// same 8-byte header the PPT record stream uses.
pub(crate) fn record_at(data: &[u8], off: usize) -> Option<(u16, u16, &[u8])> {
    let rest = data.get(off..)?;
    let hdr = rest.get(..8)?;
    let ver_inst = u16::from_le_bytes([hdr[0], hdr[1]]);
    let rec_type = u16::from_le_bytes([hdr[2], hdr[3]]);
    let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    let body = rest.get(8..)?.get(..len)?;
    Some((ver_inst, rec_type, body))
}

/// The records laid out back to back in `data`, up to the first that does
/// not fit.
pub(crate) fn records(data: &[u8]) -> impl Iterator<Item = (u16, u16, &[u8])> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let (ver_inst, rec_type, body) = record_at(data, pos)?;
        pos += 8 + body.len();
        Some((ver_inst, rec_type, body))
    })
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
    ver_inst: u16,
    rec_type: u16,
    body: &[u8],
    max_bytes: u64,
) -> Result<Option<Blip<'_>>, LimitExceeded> {
    let instance = ver_inst >> 4;
    match rec_type {
        // Bitmap blips: rgbUid1 (16), + rgbUid2 (16) for the doubled
        // instance, then the picture bytes, with one tag byte first.
        0xF01D | 0xF01E => {
            let doubled = matches!(instance, 0x46B | 0x6E3 | 0x6E1);
            let start = if doubled { 32 } else { 16 } + 1;
            let Some(bytes) = body.get(start..) else { return Ok(None) };
            let (media_type, extension) =
                if rec_type == 0xF01D { ("image/jpeg", "jpg") } else { ("image/png", "png") };
            Ok(Some(Blip { media_type, extension, bytes: Cow::Borrowed(bytes) }))
        }
        // Metafile blips: rgbUid (16/32), then a 34-byte metafile header
        // (cbSize, bounds, ptSize, cbSave, compression, filter).
        0xF01A | 0xF01B => {
            let doubled = matches!(instance, 0x3D5 | 0x217);
            let header = if doubled { 32 } else { 16 };
            let Some((cb_size, compression, data)) = metafile_parts(body.get(header..)) else {
                return Ok(None);
            };
            let (media_type, extension) =
                if rec_type == 0xF01A { ("image/emf", "emf") } else { ("image/wmf", "wmf") };
            let bytes = match compression {
                // 0x00 = deflate-compressed; 0xFE = uncompressed.
                0x00 => match inflate(data, cb_size, max_bytes)? {
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

fn metafile_parts(header_and_data: Option<&[u8]>) -> Option<(u64, u8, &[u8])> {
    let header_and_data = header_and_data?;
    let cb_size = u32::from_le_bytes(header_and_data.get(..4)?.try_into().ok()?);
    Some((u64::from(cb_size), *header_and_data.get(32)?, header_and_data.get(34..)?))
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
        let Some(&(cursor, end)) = stack.last() else { return Ok(None) };
        if cursor >= end {
            stack.pop();
            continue;
        }
        let Some((ver_inst, rec_type, body)) = record_at(&data[..end], cursor) else {
            stack.pop();
            continue;
        };
        let Some(body_start) = cursor.checked_add(8) else { return Ok(None) };
        let Some(body_end) = body_start.checked_add(body.len()) else { return Ok(None) };
        stack.last_mut().unwrap().0 = body_end;
        visited += 1;
        if visited > 10_000 || stack.len() > 16 {
            return Ok(None);
        }
        if let Some(blip) = decode_blip(ver_inst, rec_type, body, max_bytes)? {
            return Ok(Some(blip));
        }
        if rec_type == 0xF007 {
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
        if ver_inst & 0xF == 0xF {
            stack.push((body_start, body_end));
        }
    }
}

/// Offset of the embedded blip record inside an FBSE (0xF007) body: the
/// 36-byte header plus the entry's name.
fn fbse_blip_offset(body: &[u8]) -> Option<usize> {
    let cb_name = *body.get(33)? as usize;
    36usize.checked_add(cb_name)
}

/// Decode the blip embedded in an FBSE (0xF007) record body, if present.
pub(crate) fn fbse_blip(body: &[u8], max_bytes: u64) -> Result<Option<Blip<'_>>, LimitExceeded> {
    let Some((ver_inst, rec_type, blip_body)) =
        fbse_blip_offset(body).and_then(|offset| record_at(body, offset))
    else {
        return Ok(None);
    };
    decode_blip(ver_inst, rec_type, blip_body, max_bytes)
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
    let mut record = 0x3D40u16.to_le_bytes().to_vec();
    record.extend_from_slice(&0xF01Au16.to_le_bytes());
    record.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    record.extend_from_slice(&body);
    record
}
