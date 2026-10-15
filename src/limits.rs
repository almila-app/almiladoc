//! Safety limits: the defaults of [`Limits`], and the [`Limit`] a conversion
//! crossed.
//!
//! These are hard caps against abuse input shapes (decompression bombs,
//! pathological nesting, runaway expansion): crossing one returns
//! [`ConvertError::ResourceLimit`](crate::ConvertError::ResourceLimit),
//! always.

use std::fmt;

/// Maximum decompressed size of a single archive entry: 128 MiB.
pub const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum total decompressed bytes read from one archive: 512 MiB.
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum number of entries in one archive.
pub const MAX_ENTRY_COUNT: usize = 100_000;

/// Maximum nesting depth of input structure.
pub const MAX_NESTING_DEPTH: usize = 256;

/// Maximum number of XML nodes (elements + text runs) in one part. Sized
/// from the measured worst-case DOM cost (~400 bytes/node, see the node-cap
/// memory test) so a saturating part stays around the archive budget.
pub const MAX_XML_NODES: usize = 2_000_000;

/// Maximum grid positions one spreadsheet table may materialize: 4 million.
pub const MAX_GRID_SLOTS: u64 = 4_000_000;

/// Maximum content-bearing cells a repeat expansion may produce per table.
pub const MAX_EXPANSION: u64 = 4_000_000;

/// Maximum total text bytes *duplicated* by repeat expansion per document:
/// 64 MiB. The slot budget above bounds positions; this bounds the memory a
/// small document can amplify by repeating content-bearing cells.
pub const MAX_EXPANSION_TEXT_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum total bytes of embedded assets retained in a `Document`: 128 MiB.
pub const MAX_ASSET_TOTAL_BYTES: usize = 128 * 1024 * 1024;

/// Maximum nesting depth of binary record containers (legacy PPT stream).
pub const MAX_RECORD_DEPTH: usize = 64;

/// Maximum total binary records visited in one legacy record stream.
pub const MAX_RECORDS: u64 = 16_000_000;

/// Maximum size of one input: 256 MiB.
pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum ratio of an archive entry's decompressed to compressed size.
pub const MAX_COMPRESSION_RATIO: u64 = 1000;

/// Maximum units (pages, slides, sheets, spine items) in one document.
pub const MAX_PAGES: u64 = 100_000;

/// Maximum Markdown bytes one conversion writes: 256 MiB.
pub const MAX_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum work units one conversion charges.
pub const MAX_WORK: u64 = u64::MAX;

/// The safety limits of one [`Converter`](crate::Converter), each crossing
/// failing [`ConvertError::ResourceLimit`](crate::ConvertError::ResourceLimit)
/// with its [`Limit`].
///
/// The defaults admit ordinary large documents.
///
/// Limits and cancellation stop at the PDF engine (pdf-inspector and lopdf),
/// whose decoders allocate from stream headers: isolate PDF work in a
/// memory-capped worker that is recycled after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Decompressed bytes of one archive entry or compound-file stream: a
    /// decompression bomb hides in one entry.
    pub max_entry_bytes: u64,
    /// Decompressed bytes read from one archive in total: a bomb spreads over
    /// many entries.
    pub max_total_bytes: u64,
    /// Entries in one archive: each costs memory before any is read.
    pub max_entry_count: u64,
    /// Nesting depth of input structure: recursion over it needs stack in
    /// proportion. At most [`Limits::MAX_NESTING_CEILING`].
    pub max_nesting_depth: u64,
    /// XML nodes in one part: each costs about 400 bytes once parsed.
    pub max_xml_nodes: u64,
    /// Grid positions one spreadsheet table materializes: spans and repeats
    /// address far more cells than the input holds.
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
    /// Bytes of one input: everything else a conversion holds grows from it.
    /// Not yet enforced.
    pub max_input_bytes: u64,
    /// Ratio of an archive entry's decompressed to compressed size, checked on
    /// entries declaring at least 1 MiB: ordinary content compresses far less.
    /// Not yet enforced.
    pub max_compression_ratio: u64,
    /// Units (pages, slides, sheets or spine items) in one document: each
    /// costs work and output even when empty. Not yet enforced.
    pub max_pages: u64,
    /// Markdown bytes one conversion writes: rendering amplifies repeated
    /// references. Not yet enforced.
    pub max_output_bytes: u64,
    /// Work units one conversion charges, bounding its time
    /// deterministically. Not yet enforced; its default is set from
    /// measurement when it is.
    pub max_work: u64,
}

impl Limits {
    /// The highest [`Limits::max_nesting_depth`] a [`Converter`](crate::Converter)
    /// accepts, which its threads' stack is sized for.
    pub const MAX_NESTING_CEILING: u64 = 1024;

    pub(crate) const DEFAULT: Limits = Limits {
        max_entry_bytes: MAX_ENTRY_BYTES,
        max_total_bytes: MAX_TOTAL_BYTES,
        max_entry_count: MAX_ENTRY_COUNT as u64,
        max_nesting_depth: MAX_NESTING_DEPTH as u64,
        max_xml_nodes: MAX_XML_NODES as u64,
        max_grid_slots: MAX_GRID_SLOTS,
        max_expansion: MAX_EXPANSION,
        max_expansion_text_bytes: MAX_EXPANSION_TEXT_BYTES,
        max_asset_total_bytes: MAX_ASSET_TOTAL_BYTES as u64,
        max_record_depth: MAX_RECORD_DEPTH as u64,
        max_records: MAX_RECORDS,
        max_input_bytes: MAX_INPUT_BYTES,
        max_compression_ratio: MAX_COMPRESSION_RATIO,
        max_pages: MAX_PAGES,
        max_output_bytes: MAX_OUTPUT_BYTES,
        max_work: MAX_WORK,
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
            (Limit::Pages, self.max_pages),
            (Limit::OutputBytes, self.max_output_bytes),
            (Limit::Work, self.max_work),
        ]
    }

    pub(crate) fn ceiling(limit: Limit) -> u64 {
        match limit {
            Limit::NestingDepth => Self::MAX_NESTING_CEILING,
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
    /// Grid positions one spreadsheet materializes.
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
    /// Units in one document.
    Pages,
    /// Markdown bytes one conversion writes.
    OutputBytes,
    /// Work units one conversion charges.
    Work,
    /// A memory reservation the allocator refused.
    Allocation,
}

impl Limit {
    /// The limit's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::EntryBytes => "max_entry_bytes",
            Limit::TotalBytes => "max_total_bytes",
            Limit::EntryCount => "max_entry_count",
            Limit::NestingDepth => "max_nesting_depth",
            Limit::XmlNodes => "max_xml_nodes",
            Limit::GridSlots => "max_grid_slots",
            Limit::Expansion => "max_expansion",
            Limit::ExpansionTextBytes => "max_expansion_text_bytes",
            Limit::AssetTotalBytes => "max_asset_total_bytes",
            Limit::RecordDepth => "max_record_depth",
            Limit::Records => "max_records",
            Limit::InputBytes => "max_input_bytes",
            Limit::CompressionRatio => "max_compression_ratio",
            Limit::Pages => "max_pages",
            Limit::OutputBytes => "max_output_bytes",
            Limit::Work => "max_work",
            Limit::Allocation => "allocation",
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
            (Limit::Pages, "max_pages"),
            (Limit::OutputBytes, "max_output_bytes"),
            (Limit::Work, "max_work"),
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
                (Limit::Pages, 100_000),
                (Limit::OutputBytes, 256 * 1024 * 1024),
                (Limit::Work, u64::MAX),
            ]
        );
        assert_eq!(limits.max_nesting_depth, MAX_NESTING_DEPTH as u64);
        assert_eq!(limits.max_entry_count, MAX_ENTRY_COUNT as u64);
        assert_eq!(limits.max_xml_nodes, MAX_XML_NODES as u64);
        assert_eq!(limits.max_asset_total_bytes, MAX_ASSET_TOTAL_BYTES as u64);
        assert_eq!(limits.max_record_depth, MAX_RECORD_DEPTH as u64);
    }

    #[test]
    fn only_nesting_depth_has_a_ceiling_below_the_type() {
        for (limit, _) in Limits::default().fields() {
            let ceiling = if limit == Limit::NestingDepth { 1024 } else { u64::MAX };
            assert_eq!(Limits::ceiling(limit), ceiling, "{limit}");
        }
    }
}
