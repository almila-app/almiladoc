//! Safety limits: the defaults of [`Limits`], and the [`Limit`] a conversion
//! crossed.
//!
//! These are hard caps against abuse input shapes (decompression bombs,
//! pathological nesting, runaway expansion): crossing one returns
//! [`ConvertError::ResourceLimit`](crate::ConvertError::ResourceLimit),
//! always.

use std::fmt;

const MIB: u64 = 1024 * 1024;

/// The safety limits of one [`Converter`](crate::Converter), each crossing
/// failing [`ConvertError::ResourceLimit`](crate::ConvertError::ResourceLimit)
/// with its [`Limit`].
///
/// The defaults admit ordinary large documents.
///
/// Limits and cancellation reach the PDF engine (unpdf) only between pages:
/// inside a page its stream decoding takes no caller budget, so isolate PDF
/// work in a memory-capped worker that is recycled after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Decompressed bytes of one archive entry or compound-file stream: a
    /// decompression bomb hides in one entry.
    pub max_entry_bytes: u64,
    /// Decompressed bytes read from one archive in total: a bomb spreads over
    /// many entries, a corrupt one too, which counts what it inflated before
    /// failing its checksum or deflate stream, once however many references
    /// name it.
    pub max_total_bytes: u64,
    /// Entries in one archive: each costs memory before any is read.
    pub max_entry_count: u64,
    /// Nesting depth of input structure (XML elements in a part, RTF groups,
    /// Word fields) and of the parsed document's lists, table cells, quotes
    /// and links: recursion over it needs stack in proportion. At most
    /// [`Limits::MAX_NESTING_CEILING`].
    pub max_nesting_depth: u64,
    /// XML nodes one part holds at once: each costs about 400 bytes once
    /// parsed. An xlsx worksheet holds its structure around the rows and one
    /// row at a time, so it bounds a row, not the row count.
    pub max_xml_nodes: u64,
    /// Grid positions the tables of one parse materialize together, covered
    /// and padding ones too, and the positions a span claims in rows its
    /// table never supplies: spans, merges and repeats address far more
    /// cells than the input holds. Every DOCX, PPTX, ODF, EPUB, RTF, DOC, XLSX,
    /// XLSB and XLS table counts, and a DrawingML chart's: a header row and a
    /// row per category, each with a category cell and a cell per series,
    /// admitted before the table is built. A CSV's table does not. An xlsx,
    /// xlsb or xls worksheet's cells and the losses they report, hidden ones
    /// too, count against the positions left as its rows and records stream.
    pub max_grid_slots: u64,
    /// Content-bearing cells a span or repeat expansion produces per table.
    pub max_expansion: u64,
    /// Text bytes repeat expansion duplicates per document: a small document
    /// amplifies memory by repeating content-bearing cells.
    pub max_expansion_text_bytes: u64,
    /// Bytes of embedded assets one document retains: they are kept in
    /// memory with the document.
    pub max_asset_total_bytes: u64,
    /// Nesting depth of binary record containers: recursion over them needs
    /// stack in proportion.
    pub max_record_depth: u64,
    /// Binary records visited in one record stream: each costs time even
    /// when it yields nothing.
    pub max_records: u64,
    /// Bytes of one input, checked before any parse: everything else a
    /// conversion holds grows from it.
    pub max_input_bytes: u64,
    /// Ratio of an archive entry's decompressed to compressed size, checked on
    /// entries declaring at least 1 MiB: ordinary content compresses far less.
    pub max_compression_ratio: u64,
    /// Markdown bytes one conversion writes, counting what rendering holds
    /// before writing it (a paragraph to trim, a table's cells), and JSON
    /// bytes one serialisation writes: escaping and repeated references
    /// amplify the input, and the JSON's field names and `kind` tags amplify
    /// it several times over, as each styled run of text spells out its
    /// style.
    pub max_output_bytes: u64,
    /// Work units one conversion charges, bounding its time
    /// deterministically: XML events, RTF tokens, CSS rules an element's tag
    /// or classes name, KiB decompressed from an archive, records and cells
    /// (CSV, PPT, DOC), PDF pages, their blocks, inline runs and text bytes,
    /// style and numbering resolution, grid positions counted against
    /// [`Limits::max_grid_slots`], inlines the Markdown renderer reads, looks
    /// ahead at and writes, and 64 bytes of Markdown or JSON written. Detection shares these counters with parsing,
    /// and rendering continues the same ledger. The default, two units per
    /// byte of [`Limits::max_input_bytes`], admits every input whose cost is
    /// linear at that rate: a 256 MiB RTF of one hex picture charges
    /// 268 435 410 units and the costliest test-corpus document 7 239. Work
    /// that grows faster than the input crosses it: a PPT of 65 hyperlinks
    /// each over one text of 4 Mi UTF-16 units (8 MiB), each link charging its
    /// range read and its label compared, asks 545 259 782.
    pub max_work: u64,
    /// Units one document holds: slides, sheets, EPUB spine items and PDF
    /// pages, a CSV being one sheet. Each unit costs fixed work that its
    /// bytes and nodes do not price, such as the relationships part a `.pptx`
    /// reads per slide: a 2.7 KB `.odp` of empty `draw:page` elements declares
    /// 100 001. Each is numbered by its place in the document's own list, so
    /// a hidden, skipped or unreadable one still takes its number, and the
    /// first unit numbered past this is refused; a PDF declaring more pages is
    /// refused before any is read. At most `u32::MAX`.
    pub max_pages: u64,
}

impl Limits {
    /// The highest [`Limits::max_nesting_depth`] a [`Converter`](crate::Converter)
    /// accepts, which its threads' stack is sized for.
    pub const MAX_NESTING_CEILING: u64 = 1024;

    pub(crate) const DEFAULT: Self = Self {
        max_entry_bytes: 128 * MIB,
        max_total_bytes: 512 * MIB,
        max_entry_count: 100_000,
        max_nesting_depth: 256,
        max_xml_nodes: 2_000_000,
        max_grid_slots: 4_000_000,
        max_expansion: 4_000_000,
        max_expansion_text_bytes: 64 * MIB,
        max_asset_total_bytes: 128 * MIB,
        max_record_depth: 64,
        max_records: 16_000_000,
        max_input_bytes: 256 * MIB,
        max_compression_ratio: 1000,
        max_output_bytes: 256 * MIB,
        max_work: 2 * 256 * MIB,
        max_pages: 100_000,
    };

    pub(crate) fn fields(&self) -> [(Limit, u64); 16] {
        [
            (Limit::EntryBytes, self.max_entry_bytes),
            (Limit::TotalBytes, self.max_total_bytes),
            (Limit::EntryCount, self.max_entry_count),
            (Limit::NestingDepth, self.max_nesting_depth),
            (Limit::XmlNodes, self.max_xml_nodes),
            (Limit::GridSlots, self.max_grid_slots),
            (Limit::Expansion, self.max_expansion),
            (Limit::ExpansionTextBytes, self.max_expansion_text_bytes),
            (Limit::AssetTotalBytes, self.max_asset_total_bytes),
            (Limit::RecordDepth, self.max_record_depth),
            (Limit::Records, self.max_records),
            (Limit::InputBytes, self.max_input_bytes),
            (Limit::CompressionRatio, self.max_compression_ratio),
            (Limit::OutputBytes, self.max_output_bytes),
            (Limit::Work, self.max_work),
            (Limit::Pages, self.max_pages),
        ]
    }

    pub(crate) fn ceiling(limit: Limit) -> u64 {
        match limit {
            Limit::NestingDepth => Self::MAX_NESTING_CEILING,
            Limit::Pages => u64::from(u32::MAX),
            _ => u64::MAX,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The safety limit a conversion crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Limit {
    /// Decompressed size of one archive entry or compound-file stream.
    EntryBytes,
    /// Decompressed bytes read from one archive in total.
    TotalBytes,
    /// Entries in one archive.
    EntryCount,
    /// Nesting depth of input structure.
    NestingDepth,
    /// XML nodes in one part.
    XmlNodes,
    /// Grid positions the tables of one parse materialize.
    GridSlots,
    /// Cells a span or repeat expansion produces per table.
    Expansion,
    /// Text bytes repeat expansion duplicates per document.
    ExpansionTextBytes,
    /// Bytes of embedded assets retained in one document.
    AssetTotalBytes,
    /// Nesting depth of binary record containers.
    RecordDepth,
    /// Binary records visited in one record stream.
    Records,
    /// Bytes of one input.
    InputBytes,
    /// Ratio of an archive entry's decompressed to compressed size.
    CompressionRatio,
    /// Markdown bytes one conversion writes.
    OutputBytes,
    /// Work units one conversion charges.
    Work,
    /// Units one document holds.
    Pages,
    /// A memory reservation the allocator refused.
    Allocation,
}

impl Limit {
    /// The limit's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EntryBytes => "max_entry_bytes",
            Self::TotalBytes => "max_total_bytes",
            Self::EntryCount => "max_entry_count",
            Self::NestingDepth => "max_nesting_depth",
            Self::XmlNodes => "max_xml_nodes",
            Self::GridSlots => "max_grid_slots",
            Self::Expansion => "max_expansion",
            Self::ExpansionTextBytes => "max_expansion_text_bytes",
            Self::AssetTotalBytes => "max_asset_total_bytes",
            Self::RecordDepth => "max_record_depth",
            Self::Records => "max_records",
            Self::InputBytes => "max_input_bytes",
            Self::CompressionRatio => "max_compression_ratio",
            Self::OutputBytes => "max_output_bytes",
            Self::Work => "max_work",
            Self::Pages => "max_pages",
            Self::Allocation => "allocation",
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A limit crossed below the error layer, converted into
/// [`ConvertError::ResourceLimit`](crate::ConvertError::ResourceLimit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LimitExceeded {
    pub(crate) limit: Limit,
    pub(crate) max: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_the_snake_case_limit_names() {
        let names = [
            (Limit::EntryBytes, "max_entry_bytes"),
            (Limit::TotalBytes, "max_total_bytes"),
            (Limit::EntryCount, "max_entry_count"),
            (Limit::NestingDepth, "max_nesting_depth"),
            (Limit::XmlNodes, "max_xml_nodes"),
            (Limit::GridSlots, "max_grid_slots"),
            (Limit::Expansion, "max_expansion"),
            (Limit::ExpansionTextBytes, "max_expansion_text_bytes"),
            (Limit::AssetTotalBytes, "max_asset_total_bytes"),
            (Limit::RecordDepth, "max_record_depth"),
            (Limit::Records, "max_records"),
            (Limit::InputBytes, "max_input_bytes"),
            (Limit::CompressionRatio, "max_compression_ratio"),
            (Limit::OutputBytes, "max_output_bytes"),
            (Limit::Work, "max_work"),
            (Limit::Pages, "max_pages"),
            (Limit::Allocation, "allocation"),
        ];
        for (limit, name) in names {
            assert_eq!(limit.as_str(), name);
            assert_eq!(limit.to_string(), name);
        }
    }

    #[test]
    fn default_limits_are_the_constants() {
        let limits = Limits::default();
        assert_eq!(
            limits.fields(),
            [
                (Limit::EntryBytes, 128 * 1024 * 1024),
                (Limit::TotalBytes, 512 * 1024 * 1024),
                (Limit::EntryCount, 100_000),
                (Limit::NestingDepth, 256),
                (Limit::XmlNodes, 2_000_000),
                (Limit::GridSlots, 4_000_000),
                (Limit::Expansion, 4_000_000),
                (Limit::ExpansionTextBytes, 64 * 1024 * 1024),
                (Limit::AssetTotalBytes, 128 * 1024 * 1024),
                (Limit::RecordDepth, 64),
                (Limit::Records, 16_000_000),
                (Limit::InputBytes, 256 * 1024 * 1024),
                (Limit::CompressionRatio, 1000),
                (Limit::OutputBytes, 256 * 1024 * 1024),
                (Limit::Work, 536_870_912),
                (Limit::Pages, 100_000),
            ]
        );
    }

    #[test]
    fn only_nesting_depth_and_pages_have_a_ceiling_below_the_type() {
        for (limit, _) in Limits::default().fields() {
            let ceiling = match limit {
                Limit::NestingDepth => 1024,
                Limit::Pages => 4_294_967_295,
                _ => u64::MAX,
            };
            assert_eq!(Limits::ceiling(limit), ceiling, "{limit}");
        }
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads the abuse corpus and test sources"
    )]
    fn every_limit_is_tripped_by_an_abuse_fixture_or_a_named_test() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let abuse: Vec<String> = std::fs::read_dir(root.join("tests/fixtures/abuse"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let limits = Limits::default().fields().map(|(limit, _)| limit);
        for limit in limits.into_iter().chain([Limit::Allocation]) {
            let (source, test) = match limit {
                Limit::EntryBytes
                | Limit::TotalBytes
                | Limit::EntryCount
                | Limit::NestingDepth
                | Limit::XmlNodes
                | Limit::GridSlots
                | Limit::Expansion
                | Limit::ExpansionTextBytes
                | Limit::AssetTotalBytes
                | Limit::RecordDepth
                | Limit::CompressionRatio
                | Limit::Pages => {
                    let prefix = format!("{limit}--");
                    assert!(abuse.iter().any(|name| name.starts_with(&prefix)), "{limit}");
                    continue;
                }
                Limit::Records => (
                    "tests/it/limits.rs",
                    "a_ppt_stream_of_max_records_empty_atoms_trips_max_records",
                ),
                Limit::InputBytes => (
                    "tests/it/limits.rs",
                    "an_input_past_max_input_bytes_is_refused_before_any_parse",
                ),
                Limit::OutputBytes => {
                    ("tests/it/limits.rs", "a_csv_cell_escaped_past_max_output_bytes_trips_it")
                }
                Limit::Work => {
                    ("tests/it/limits.rs", "ppt_links_each_over_one_long_text_trip_max_work")
                }
                Limit::Allocation => {
                    ("src/package/archive.rs", "an_allocation_failure_mid_read_is_a_resource_limit")
                }
            };
            let source = std::fs::read_to_string(root.join(source)).unwrap();
            assert!(source.contains(&format!("fn {test}()")), "{limit}");
        }
    }
}
