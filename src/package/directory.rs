//! The ZIP central directory: every end record the zip reader could use is
//! held to the entry limit before it parses one, and the directory it parsed
//! is judged before any entry is opened.

use crate::error::ConvertError;
use crate::limits::{Limit, Limits};
use crate::shared::binary::{get_u16, get_u32, get_u64};
use std::io::Cursor;

const END: &[u8; 4] = b"PK\x05\x06";
const END_LEN: usize = 22;
const ZIP64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_END: &[u8; 4] = b"PK\x06\x06";
const ZIP64_END_OVERHEAD: usize = 12;
const HEADER_LEN: u64 = 46;
const ENCRYPTED: u16 = 1;
const STRONG_ENCRYPTION: u16 = 1 << 6;
const RATIO_FLOOR: u64 = 1024 * 1024;

/// Holds the entry count of every end record `zip::ZipArchive::new` could
/// read, including those it falls back to, to `max_entry_count`.
pub(crate) fn admit(bytes: &[u8], limits: &Limits) -> Result<(), ConvertError> {
    let max = limits.max_entry_count;
    for at in 0..bytes.len().saturating_sub(3) {
        let count = match bytes.get(at..at + 4) {
            Some(signature) if signature == END => classic_count(bytes, at),
            Some(signature) if signature == ZIP64_END => zip64_count(bytes, at),
            _ => None,
        };
        if count.is_some_and(|count| count > max) {
            return Err(ConvertError::limit(Limit::EntryCount, max));
        }
    }
    Ok(())
}

/// The counts of a classic end record whose comment fits the input and which
/// zip reads without a ZIP64 record.
fn classic_count(bytes: &[u8], at: usize) -> Option<u64> {
    if classic_end(bytes, at)? {
        return None;
    }
    Some(get_u16(bytes, at + 8)?.max(get_u16(bytes, at + 10)?).into())
}

/// Whether the classic end record at `at` is one zip reads through a ZIP64
/// record, or `None` when its comment overruns the input.
fn classic_end(bytes: &[u8], at: usize) -> Option<bool> {
    let comment = usize::from(get_u16(bytes, at + 20)?);
    if at.checked_add(END_LEN)?.checked_add(comment)? > bytes.len() {
        return None;
    }
    let may_be_zip64 = get_u16(bytes, at + 10)? == u16::MAX
        || get_u32(bytes, at + 12)? == u32::MAX
        || get_u32(bytes, at + 16)? == u32::MAX;
    let locator = at.checked_sub(ZIP64_LOCATOR_LEN);
    Some(may_be_zip64 && locator.is_some_and(|l| bytes.get(l..l + 4) == Some(ZIP64_LOCATOR)))
}

/// The counts of a ZIP64 end record that ends at a locator followed by a
/// classic end record zip reads through it.
fn zip64_count(bytes: &[u8], at: usize) -> Option<u64> {
    let size = usize::try_from(get_u64(bytes, at + 4)?).ok()?;
    let locator = at.checked_add(ZIP64_END_OVERHEAD)?.checked_add(size)?;
    let end = locator.checked_add(ZIP64_LOCATOR_LEN)?;
    if bytes.get(locator..locator.checked_add(4)?) != Some(ZIP64_LOCATOR)
        || bytes.get(end..end.checked_add(4)?) != Some(END)
        || !classic_end(bytes, end)?
    {
        return None;
    }
    Some(get_u64(bytes, at + 24)?.max(get_u64(bytes, at + 32)?))
}

/// Judges the central directory zip parsed, by its own reading of each
/// entry: no duplicate names, no encrypted entry, declared compressed sizes
/// that fit the bytes before the directory, and `max_compression_ratio`.
pub(crate) fn inspect(
    zip: &zip::ZipArchive<Cursor<&[u8]>>,
    bytes: &[u8],
    limits: &Limits,
) -> Result<(), ConvertError> {
    let corrupt = || ConvertError::malformed("corrupt central directory");
    let ratio = limits.max_compression_ratio;
    let room = zip.central_directory_start().saturating_sub(zip.offset());
    let mut total = 0u64;
    let mut next = zip.central_directory_start();
    for index in 0..zip.len() {
        let entry = zip.by_index_data(index).map_err(|_| corrupt())?;
        if entry.central_header_start() != next {
            return Err(ConvertError::malformed("duplicate entry names"));
        }
        next = header_end(bytes, next).ok_or_else(corrupt)?;
        if entry.flags().as_u16() & (ENCRYPTED | STRONG_ENCRYPTION) != 0 {
            return Err(ConvertError::Encrypted);
        }
        let (compressed, uncompressed) = (entry.compressed_size(), entry.size());
        total = total.saturating_add(compressed);
        if total > room {
            return Err(ConvertError::malformed("compressed sizes exceed the archive"));
        }
        if uncompressed >= RATIO_FLOOR && uncompressed > compressed.saturating_mul(ratio) {
            return Err(ConvertError::limit(Limit::CompressionRatio, ratio));
        }
    }
    Ok(())
}

fn header_end(bytes: &[u8], at: u64) -> Option<u64> {
    let start = usize::try_from(at).ok()?;
    let lengths: Option<u64> =
        [28, 30, 32].into_iter().map(|field| get_u16(bytes, start + field).map(u64::from)).sum();
    at.checked_add(HEADER_LEN)?.checked_add(lengths?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::test_support::{positions, stored, zip_package, zip_package_with};
    use std::assert_matches;
    use std::io::Write;

    const HEADER: &[u8; 4] = b"PK\x01\x02";

    fn put_u16(data: &mut [u8], at: usize, value: u16) {
        data[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(data: &mut [u8], at: usize, value: u32) {
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn sized(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                b'0' + u8::from(state & 1 == 1)
            })
            .collect()
    }

    fn entries(data: &[u8]) -> usize {
        zip::ZipArchive::new(Cursor::new(data)).unwrap().len()
    }

    fn inspected(data: &[u8], limits: &Limits) -> Result<(), ConvertError> {
        admit(data, limits)?;
        let zip = zip::ZipArchive::new(Cursor::new(data)).unwrap();
        inspect(&zip, data, limits)
    }

    fn limit_err(result: Result<(), ConvertError>, limit: Limit, max: u64) {
        assert_matches!(
            result.err(),
            Some(ConvertError::ResourceLimit { limit: l, max: m }) if l == limit && m == max
        );
    }

    fn over_count(data: &[u8], max: u64) {
        let limits = Limits { max_entry_count: max, ..Limits::DEFAULT };
        limit_err(admit(data, &limits), Limit::EntryCount, max);
    }

    fn malformed(result: Result<(), ConvertError>, reason: &str) {
        assert_matches!(result.err(), Some(ConvertError::Malformed { reason: r, .. }) if r == reason);
    }

    #[test]
    fn inspect_reads_the_sizes_zip_reads() {
        let data = zip_package(&[("a.xml", [b'a'; 3000].as_slice()), ("b.bin", &sized(5000))]);
        assert_eq!((inspected(&data, &Limits::DEFAULT), entries(&data)), (Ok(()), 2));
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        let uncompressed: u64 = (0..2).map(|i| zip.by_index_data(i).unwrap().size()).sum();
        assert_eq!(uncompressed, 8000);
    }

    fn with_zip64_end(mut data: Vec<u8>, count: u64) -> Vec<u8> {
        let end = positions(&data, END)[0];
        let (start, size) = (get_u32(&data, end + 16).unwrap(), get_u32(&data, end + 12).unwrap());
        let mut record = ZIP64_END.to_vec();
        record.extend_from_slice(&44u64.to_le_bytes());
        record.extend_from_slice(&[45, 0, 45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        for value in [count, count, size.into(), start.into()] {
            record.extend_from_slice(&value.to_le_bytes());
        }
        let mut locator = ZIP64_LOCATOR.to_vec();
        locator.extend_from_slice(&0u32.to_le_bytes());
        locator.extend_from_slice(&u64::try_from(end).unwrap().to_le_bytes());
        locator.extend_from_slice(&1u32.to_le_bytes());
        let mut classic = data.split_off(end);
        put_u16(&mut classic, 8, u16::MAX);
        put_u16(&mut classic, 10, u16::MAX);
        data.extend(record);
        data.extend(locator);
        data.extend(classic);
        data
    }

    #[test]
    fn a_zip64_end_record_gives_the_count() {
        let data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        let zip64 = with_zip64_end(data.clone(), 2);
        assert_eq!((inspected(&zip64, &Limits::DEFAULT), entries(&zip64)), (Ok(()), 2));
        over_count(&zip64, 1);
        let mut on_this_disk = zip64;
        let record = positions(&on_this_disk, ZIP64_END)[0];
        on_this_disk[record + 24..record + 32].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            (inspected(&on_this_disk, &Limits::DEFAULT), entries(&on_this_disk)),
            (Ok(()), 2)
        );
        over_count(&on_this_disk, 1);
        limit_err(
            admit(&with_zip64_end(data, 100_001), &Limits::DEFAULT),
            Limit::EntryCount,
            100_000,
        );
    }

    #[test]
    fn any_one_zip64_marker_sends_the_count_to_the_zip64_record() {
        let data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        let zip64 = with_zip64_end(data, 2);
        let end = positions(&zip64, END)[0];
        for (field, width) in [(10, 2), (12, 4), (16, 4)] {
            let mut marked = zip64.clone();
            put_u16(&mut marked, end + 8, 1);
            put_u16(&mut marked, end + 10, 1);
            marked[end + field..end + field + width].fill(0xFF);
            assert_eq!(
                (inspected(&marked, &Limits::DEFAULT), entries(&marked)),
                (Ok(()), 2),
                "{field}"
            );
            over_count(&marked, 1);
        }
    }

    #[test]
    fn a_zip64_record_counts_only_before_a_real_end_record() {
        let tail_of = |count| {
            let data = zip_package(&[("a.xml", b"a")]);
            let zip64 = with_zip64_end(data, count);
            zip64[positions(&zip64, ZIP64_END)[0]..].to_vec()
        };
        let mut tail = tail_of(100_001);
        let end = positions(&tail, END)[0];
        tail[end..end + 4].copy_from_slice(b"XXXX");
        let data = zip_package_with(&[("a.xml", b"a".as_slice()), ("b.bin", &tail)], stored());
        assert_eq!((inspected(&data, &Limits::DEFAULT), entries(&data)), (Ok(()), 2));
        let mut classic = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        classic = with_zip64_end(classic, 3);
        let end = positions(&classic, END)[0];
        put_u16(&mut classic, end + 8, 2);
        put_u16(&mut classic, end + 10, 2);
        assert_eq!(entries(&classic), 2);
        admit(&classic, &Limits { max_entry_count: 2, ..Limits::DEFAULT }).unwrap();
        limit_err(admit(&tail_of(100_001), &Limits::DEFAULT), Limit::EntryCount, 100_000);
    }

    #[test]
    fn a_zip64_package_after_prepended_bytes_is_read_where_zip_finds_it() {
        let data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        let prefixed = [b"prefix!!".as_slice(), &with_zip64_end(data, 2)].concat();
        let zip = zip::ZipArchive::new(Cursor::new(prefixed.as_slice())).unwrap();
        assert_eq!(zip.offset(), 8);
        assert_eq!((inspected(&prefixed, &Limits::DEFAULT), zip.len()), (Ok(()), 2));
        over_count(&prefixed, 1);
    }

    #[test]
    fn classic_counts_are_held_unless_zip_reads_a_zip64_record() {
        let data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        let end = positions(&data, END)[0];
        let limits = Limits { max_entry_count: 2, ..Limits::DEFAULT };
        admit(&data, &limits).unwrap();
        for field in [8, 10] {
            let mut patched = data.clone();
            put_u16(&mut patched, end + field, 3);
            limit_err(admit(&patched, &limits), Limit::EntryCount, 2);
        }
        let mut zip64 = with_zip64_end(data, 2);
        admit(&zip64, &limits).unwrap();
        let locator = positions(&zip64, ZIP64_LOCATOR)[0];
        zip64[locator] = b'X';
        limit_err(admit(&zip64, &limits), Limit::EntryCount, 2);
        let name = format!("{}{}", "PK\u{6}\u{7}", "a".repeat(16));
        let lookalike = zip_package_with(&[("a.xml", b"a"), (name.as_str(), b"b")], stored());
        let end = positions(&lookalike, END)[0];
        assert_eq!(lookalike.get(end - 20..end - 16), Some(ZIP64_LOCATOR.as_slice()));
        assert_eq!(entries(&lookalike), 2);
        over_count(&lookalike, 1);
    }

    #[test]
    fn an_end_record_counts_only_when_its_comment_fits() {
        let data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        let limits = Limits { max_entry_count: 1, ..Limits::DEFAULT };
        let end = positions(&data, END)[0];
        let mut overrun = data;
        put_u16(&mut overrun, end + 20, 1);
        admit(&overrun, &limits).unwrap();
        let mut fits = overrun;
        fits.push(0);
        limit_err(admit(&fits, &limits), Limit::EntryCount, 1);
    }

    #[test]
    fn every_end_record_zip_could_fall_back_to_is_held() {
        let names: Vec<String> = (0..200).map(|i| format!("{i}.xml")).collect();
        let parts: Vec<(&str, &[u8])> =
            names.iter().map(|n| (n.as_str(), b"x".as_slice())).collect();
        let first = zip_package(&parts);
        let mut second = zip_package(&[("abcdefghi", b"y")]);
        let header = positions(&second, HEADER)[0];
        put_u16(&mut second, header + 28, 4);
        put_u16(&mut second, header + 30, 5);
        second[header + 50..header + 55].copy_from_slice(&[0x75, 0x70, 1, 0, 1]);
        let end = positions(&second, END)[0];
        let start = get_u32(&second, end + 16).unwrap() + u32::try_from(first.len()).unwrap();
        put_u32(&mut second, end + 16, start);
        let data = [first, second].concat();
        assert_eq!(entries(&data), 200);
        over_count(&data, 50);
        admit(&data, &Limits { max_entry_count: 200, ..Limits::DEFAULT }).unwrap();
    }

    #[test]
    fn either_encryption_flag_on_any_entry_is_encrypted() {
        let data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b")]);
        for header in positions(&data, HEADER) {
            for flag in [ENCRYPTED, STRONG_ENCRYPTION] {
                let mut patched = data.clone();
                put_u16(&mut patched, header + 8, flag);
                assert_matches!(
                    inspected(&patched, &Limits::DEFAULT),
                    Err(ConvertError::Encrypted)
                );
            }
        }
    }

    #[test]
    fn the_ratio_holds_entries_of_at_least_one_mebibyte() {
        let floor = usize::try_from(RATIO_FLOOR).unwrap();
        let limits = Limits { max_compression_ratio: 2, ..Limits::DEFAULT };
        let mixed = zip_package(&[("a.bin", &sized(floor))]);
        limit_err(inspected(&mixed, &limits), Limit::CompressionRatio, 2);
        inspected(&mixed, &Limits { max_compression_ratio: 100, ..Limits::DEFAULT }).unwrap();
        let under = zip_package(&[("a.bin", &vec![0; floor - 1])]);
        inspected(&under, &limits).unwrap();
        let flat = zip_package_with(&[("a.bin", &vec![0; floor])], stored());
        inspected(&flat, &Limits { max_compression_ratio: 1, ..Limits::DEFAULT }).unwrap();
    }

    fn with_extra(extra: &[u8], sizes: (u32, u32)) -> Vec<u8> {
        let name = "a".repeat(1 + extra.len());
        let mut data = zip_package_with(&[(name.as_str(), &sized(4000))], stored());
        let header = positions(&data, HEADER)[0];
        put_u16(&mut data, header + 28, 1);
        put_u16(&mut data, header + 30, u16::try_from(extra.len()).unwrap());
        put_u32(&mut data, header + 20, sizes.0);
        put_u32(&mut data, header + 24, sizes.1);
        data[header + 47..header + 47 + extra.len()].copy_from_slice(extra);
        data
    }

    fn zip64_field(values: &[u64]) -> Vec<u8> {
        let mut field = vec![1, 0];
        field.extend_from_slice(&u16::try_from(values.len() * 8).unwrap().to_le_bytes());
        for value in values {
            field.extend_from_slice(&value.to_le_bytes());
        }
        field
    }

    #[test]
    fn a_full_zip64_field_fills_only_the_sentinel_sizes() {
        let data = with_extra(&zip64_field(&[1000, 5 << 20, 0]), (u32::MAX, 2 << 20));
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        assert_eq!(zip.by_index_data(0).unwrap().compressed_size(), 1000);
        limit_err(
            inspected(&data, &Limits { max_compression_ratio: 100, ..Limits::DEFAULT }),
            Limit::CompressionRatio,
            100,
        );
        let wide = Limits { max_compression_ratio: 3000, ..Limits::DEFAULT };
        assert_eq!(inspected(&data, &wide), Ok(()));
    }

    #[test]
    fn the_last_of_repeated_zip64_fields_gives_the_sizes() {
        let extra = [zip64_field(&[100, 100]), zip64_field(&[4 << 20, 1000])].concat();
        let data = with_extra(&extra, (u32::MAX, u32::MAX));
        limit_err(
            inspected(&data, &Limits { max_compression_ratio: 100, ..Limits::DEFAULT }),
            Limit::CompressionRatio,
            100,
        );
        let zip = zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap();
        let entry = zip.by_index_data(0).unwrap();
        assert_eq!((entry.compressed_size(), entry.size()), (1000, 4 << 20));
        let wide = Limits { max_compression_ratio: 5000, ..Limits::DEFAULT };
        assert_eq!(inspected(&data, &wide), Ok(()));
    }

    #[test]
    fn compressed_sizes_must_fit_before_the_directory() {
        let data = zip_package_with(&[("a.bin", &sized(4000)), ("b.bin", &sized(4000))], stored());
        let room =
            zip::ZipArchive::new(Cursor::new(data.as_slice())).unwrap().central_directory_start();
        let header = positions(&data, HEADER)[1];
        let mut fits = data.clone();
        put_u32(&mut fits, header + 20, u32::try_from(room).unwrap() - 4000);
        inspected(&fits, &Limits::DEFAULT).unwrap();
        let mut past = data;
        put_u32(&mut past, header + 20, u32::try_from(room).unwrap() - 3999);
        malformed(inspected(&past, &Limits::DEFAULT), "compressed sizes exceed the archive");
        let prefixed = [b"prefix!!".as_slice(), &past].concat();
        malformed(inspected(&prefixed, &Limits::DEFAULT), "compressed sizes exceed the archive");
    }

    #[test]
    fn central_headers_chain_past_extra_fields_and_comments() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["a.xml", "b.xml", "c.xml"] {
            let mut options = zip::write::FullFileOptions::default().with_file_comment("note");
            options.add_extra_field(0x5455, [1, 0, 0, 0, 0], true).unwrap();
            writer.start_file(name, options).unwrap();
            writer.write_all(b"x").unwrap();
        }
        let data = writer.finish().unwrap().into_inner();
        for header in positions(&data, HEADER) {
            assert!(get_u16(&data, header + 30).unwrap() > 0);
            assert_eq!(get_u16(&data, header + 32), Some(4));
        }
        assert_eq!((inspected(&data, &Limits::DEFAULT), entries(&data)), (Ok(()), 3));
    }

    #[test]
    fn an_entry_zip_deduplicated_is_a_duplicate_name() {
        let mut data = zip_package(&[("a.xml", b"a"), ("b.xml", b"b"), ("c.xml", b"c")]);
        for at in positions(&data, b"c.xm") {
            data[at] = b'a';
        }
        malformed(inspected(&data, &Limits::DEFAULT), "duplicate entry names");
    }
}
