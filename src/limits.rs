//! Fixed safety limits, applied identically to every conversion.
//!
//! These are hard caps against attack/abuse input shapes (decompression
//! bombs, pathological nesting, runaway expansion) - crossing one returns
//! [`ConvertError::ResourceLimit`](crate::ConvertError::ResourceLimit),
//! always. They are deliberately not configurable: real-world documents sit
//! orders of magnitude below every value here.

use std::fmt;

/// Maximum decompressed size of a single archive entry: 128 MiB.
pub const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum total decompressed bytes read from one archive: 512 MiB.
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum number of entries in one archive.
pub const MAX_ENTRY_COUNT: usize = 100_000;

/// Maximum XML element nesting depth.
pub const MAX_XML_DEPTH: usize = 256;

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
    /// XML element nesting depth.
    XmlDepth,
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
            Limit::XmlDepth => "max_xml_depth",
            Limit::XmlNodes => "max_xml_nodes",
            Limit::GridSlots => "max_grid_slots",
            Limit::Expansion => "max_expansion",
            Limit::ExpansionTextBytes => "max_expansion_text_bytes",
            Limit::AssetTotalBytes => "max_asset_total_bytes",
            Limit::RecordDepth => "max_record_depth",
            Limit::Records => "max_records",
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
            (Limit::XmlDepth, "max_xml_depth"),
            (Limit::XmlNodes, "max_xml_nodes"),
            (Limit::GridSlots, "max_grid_slots"),
            (Limit::Expansion, "max_expansion"),
            (Limit::ExpansionTextBytes, "max_expansion_text_bytes"),
            (Limit::AssetTotalBytes, "max_asset_total_bytes"),
            (Limit::RecordDepth, "max_record_depth"),
            (Limit::Records, "max_records"),
            (Limit::Allocation, "allocation"),
        ];
        for (limit, name) in names {
            assert_eq!(limit.as_str(), name);
            assert_eq!(limit.to_string(), name);
        }
    }
}
