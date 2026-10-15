//! What a conversion could not carry into its output: [`Diagnostics`],
//! aggregated by code, region and unit, and classed by what a reader of the
//! Markdown misses.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

/// What a reader of the output misses at a diagnostic's site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LossClass {
    /// A content policy dropped it on purpose.
    Omitted,
    /// A placeholder or gap stands where it was, which the caller may fill.
    Deferred,
    /// Content the source application shows is gone, with nothing standing in.
    Lost,
    /// Every word and number still renders; formatting or a repair changed.
    Info,
}

impl LossClass {
    /// The class's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LossClass::Omitted => "omitted",
            LossClass::Deferred => "deferred",
            LossClass::Lost => "lost",
            LossClass::Info => "info",
        }
    }
}

/// Where the content a diagnostic counts sat in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Region {
    /// The main text.
    Body,
    /// Page or slide headers.
    Header,
    /// Page or slide footers.
    Footer,
    /// Footnotes, endnotes and speaker notes.
    Note,
    /// Comments and annotations.
    Comment,
    /// Document properties.
    Metadata,
    /// Parts that hold no rendered text, such as stylesheets.
    Container,
}

impl Region {
    /// The region's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Region::Body => "body",
            Region::Header => "header",
            Region::Footer => "footer",
            Region::Note => "note",
            Region::Comment => "comment",
            Region::Metadata => "metadata",
            Region::Container => "container",
        }
    }
}

macro_rules! diagnostic_codes {
    ($($(#[$doc:meta])* $variant:ident = $name:literal, $class:ident;)*) => {
        /// What a diagnostic reports; each code has one fixed [`LossClass`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum DiagnosticCode {
            $($(#[$doc])* $variant,)*
        }

        impl DiagnosticCode {
            /// The code's stable `snake_case` name.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(DiagnosticCode::$variant => $name,)*
                }
            }

            /// The class every diagnostic of this code carries.
            #[must_use]
            pub fn class(self) -> LossClass {
                match self {
                    $(DiagnosticCode::$variant => LossClass::$class,)*
                }
            }

            #[cfg(test)]
            const ALL: &[DiagnosticCode] = &[$(DiagnosticCode::$variant,)*];
        }
    };
}

diagnostic_codes! {
    /// An optional content part, stream or table that will not read or parse,
    /// or that a reference names and the package lacks.
    PartUnreadable = "part_unreadable", Lost;
    /// A listed sheet, slide, notes page or chapter that is absent or unreadable.
    UnitUnreadable = "unit_unreadable", Lost;
    /// A cell or record whose value cannot be decoded or was never cached.
    CellValueUnreadable = "cell_value_unreadable", Lost;
    /// A binary record stream that ends mid-record.
    TruncatedRecords = "truncated_records", Lost;
    /// A block, row or paragraph group the reader does not walk.
    BlockUnread = "block_unread", Lost;
    /// Markup kept after unclosed or mismatched elements or ill-formed attributes.
    RecoveredXml = "recovered_xml", Lost;
    /// Groups still open at the end of the input.
    UnclosedStructure = "unclosed_structure", Lost;
    /// Bytes that do not decode, replaced or dropped.
    InvalidEncoding = "invalid_encoding", Lost;
    /// Text of an undeclared encoding read as windows-1252.
    EncodingAssumed = "encoding_assumed", Lost;
    /// An entity reference rendered literally.
    UnresolvedEntity = "unresolved_entity", Lost;
    /// A picture whose bytes cannot be located or decoded.
    ImageUnavailable = "image_unavailable", Lost;
    /// A picture the document shows that gets no place in the output.
    ImageUnplaced = "image_unplaced", Lost;
    /// An embedded object whose content is not converted.
    EmbeddedObject = "embedded_object", Lost;
    /// A chart, diagram, ink or shape text not read.
    GraphicUnread = "graphic_unread", Lost;
    /// An imported document part or subdocument not read.
    ImportedPartUnread = "imported_part_unread", Lost;
    /// A text-box story not read.
    TextBoxUnread = "text_box_unread", Lost;
    /// Ruby base text and its reading not read.
    RubyUnread = "ruby_unread", Lost;
    /// A symbol with no Unicode character.
    UnmappedSymbol = "unmapped_symbol", Lost;
    /// A page-number or date character whose value the file does not store.
    FieldValueUnread = "field_value_unread", Lost;
    /// An external link target, not resolved, that its label does not show.
    LinkTargetUnread = "link_target_unread", Lost;
    /// A date, time, currency or literal-bearing number format shown as General.
    NumberFormatUnread = "number_format_unread", Lost;
    /// Text recovered in raw stream order.
    RecoveredStreamOrder = "recovered_stream_order", Lost;
    /// A form control's state and caption not read.
    FormControlUnread = "form_control_unread", Lost;
    /// Formatting runs capped, with the pictures, tables and list marks they carry.
    FormattingRunsCapped = "formatting_runs_capped", Lost;
    /// Text from fonts whose encoding is broken.
    GarbledFontEncoding = "garbled_font_encoding", Lost;
    /// An annotation whose appearance does not resolve.
    AnnotationUnrendered = "annotation_unrendered", Lost;
    /// Header content, or slide master and layout content, dropped by policy.
    HeaderOmitted = "header_omitted", Omitted;
    /// Footer content dropped by policy.
    FooterOmitted = "footer_omitted", Omitted;
    /// Comments dropped by policy.
    CommentOmitted = "comment_omitted", Omitted;
    /// Deleted text under accepted revisions, or inserted text under rejected ones.
    RevisionOmitted = "revision_omitted", Omitted;
    /// Hidden content dropped by policy.
    HiddenOmitted = "hidden_omitted", Omitted;
    /// A picture dropped or reduced to its alt text by policy.
    ImageOmitted = "image_omitted", Omitted;
    /// An external picture's URL, never rendered.
    ExternalImage = "external_image", Omitted;
    /// A link target outside the permitted schemes; its label renders.
    UnsafeLink = "unsafe_link", Omitted;
    /// A picture in a format image OCR does not take, kept as a picture.
    ImageNotOcred = "image_not_ocred", Omitted;
    /// A picture rendered as an asset placeholder.
    ImagePlaceholder = "image_placeholder", Deferred;
    /// A page or image gap awaiting OCR.
    OcrGap = "ocr_gap", Deferred;
    /// A gap whose OCR failed.
    OcrFailed = "ocr_failed", Deferred;
    /// A part whose loss changes formatting alone will not read or parse.
    StylePartUnreadable = "style_part_unreadable", Info;
    /// A style inheritance cycle cut at its acyclic prefix.
    StyleCycle = "style_cycle", Info;
    /// Formatting that cannot be decoded or has no model field.
    FormattingUnreadable = "formatting_unreadable", Info;
    /// A style name or stylesheet that does not decode.
    InvalidStyleEncoding = "invalid_style_encoding", Info;
    /// Numbering that names a missing definition, shown unnumbered.
    NumberingUnresolved = "numbering_unresolved", Info;
    /// A number format shown as General where the same number shows.
    NumberFormatFallback = "number_format_fallback", Info;
    /// A merge or covered cell outside any valid span.
    TableSpanRepaired = "table_span_repaired", Info;
    /// A part found by its conventional name because its reference does not resolve.
    ConventionalPartUsed = "conventional_part_used", Info;
    /// A group close with no open group.
    UnmatchedClose = "unmatched_close", Info;
    /// A link or note reference whose target exists nowhere.
    DanglingReference = "dangling_reference", Info;
    /// A second note body under an id already used.
    DuplicateNote = "duplicate_note", Info;
    /// A white raster filled without an OCR engine call.
    WhitePageFilled = "white_page_filled", Info;
    /// A page, date, time or file field rendered as its token.
    FieldToken = "field_token", Info;
    /// A metadata part that will not read; the body is unaffected.
    MetadataUnreadable = "metadata_unreadable", Info;
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One aggregated diagnostic: `count` occurrences of `code` in `region`, in
/// one `unit` (a 1-based page, slide, sheet or chapter) or across the
/// document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Diagnostic {
    /// What was reported.
    pub code: DiagnosticCode,
    /// The code's class.
    pub class: LossClass,
    /// Where the content sat.
    pub region: Region,
    /// The unit it sat in, or `None` across the document or in a folded unit.
    pub unit: Option<NonZeroU32>,
    /// Occurrences, saturating at `u64::MAX`.
    pub count: u64,
}

/// Every diagnostic of one call, aggregated in a fixed order; equal reports
/// in any order aggregate equally.
///
/// It stays bounded: past [`Diagnostics::MAX_UNITS`] units under one code and
/// region, the highest units fold into that code and region's entry with no
/// unit, which counts what it absorbs, so no occurrence is dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    entries: BTreeMap<(DiagnosticCode, Region), Units>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Units {
    across: u64,
    by_unit: BTreeMap<NonZeroU32, u64>,
}

impl Diagnostics {
    /// The most units kept apart under one code and region.
    pub const MAX_UNITS: usize = 64;

    pub(crate) fn add(
        &mut self,
        code: DiagnosticCode,
        region: Region,
        unit: Option<NonZeroU32>,
        count: u64,
    ) {
        if count == 0 {
            return;
        }
        let units = self.entries.entry((code, region)).or_default();
        let Some(unit) = unit else {
            units.across = units.across.saturating_add(count);
            return;
        };
        let kept = units.by_unit.entry(unit).or_default();
        *kept = kept.saturating_add(count);
        if units.by_unit.len() > Self::MAX_UNITS
            && let Some((_, folded)) = units.by_unit.pop_last()
        {
            units.across = units.across.saturating_add(folded);
        }
    }

    /// Every diagnostic, ordered by code, region and unit, with the entry
    /// that has no unit first.
    pub fn iter(&self) -> impl Iterator<Item = Diagnostic> + '_ {
        self.entries.iter().flat_map(|(&(code, region), units)| {
            let across = (units.across > 0).then_some((None, units.across));
            let by_unit = units.by_unit.iter().map(|(&unit, &count)| (Some(unit), count));
            across.into_iter().chain(by_unit).map(move |(unit, count)| Diagnostic {
                code,
                class: code.class(),
                region,
                unit,
                count,
            })
        })
    }

    /// True when nothing was reported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when some content was [`LossClass::Lost`].
    #[must_use]
    pub fn has_loss(&self) -> bool {
        self.entries.keys().any(|(code, _)| code.class() == LossClass::Lost)
    }

    pub(crate) fn lost_codes(&self) -> impl Iterator<Item = DiagnosticCode> + '_ {
        let mut codes: Vec<DiagnosticCode> = self
            .entries
            .keys()
            .map(|&(code, _)| code)
            .filter(|code| code.class() == LossClass::Lost)
            .collect();
        codes.dedup();
        codes.into_iter()
    }
}

/// What a conversion does once some content was [`LossClass::Lost`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OnLoss {
    /// Fail with [`ConvertError::Incomplete`](crate::ConvertError::Incomplete).
    #[default]
    Fail,
    /// Return the conversion, whose
    /// [`is_complete`](crate::Conversion::is_complete) is false.
    Report,
}

impl OnLoss {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            OnLoss::Fail => "fail",
            OnLoss::Report => "report",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CODES: [(&str, LossClass); 52] = [
        ("part_unreadable", LossClass::Lost),
        ("unit_unreadable", LossClass::Lost),
        ("cell_value_unreadable", LossClass::Lost),
        ("truncated_records", LossClass::Lost),
        ("block_unread", LossClass::Lost),
        ("recovered_xml", LossClass::Lost),
        ("unclosed_structure", LossClass::Lost),
        ("invalid_encoding", LossClass::Lost),
        ("encoding_assumed", LossClass::Lost),
        ("unresolved_entity", LossClass::Lost),
        ("image_unavailable", LossClass::Lost),
        ("image_unplaced", LossClass::Lost),
        ("embedded_object", LossClass::Lost),
        ("graphic_unread", LossClass::Lost),
        ("imported_part_unread", LossClass::Lost),
        ("text_box_unread", LossClass::Lost),
        ("ruby_unread", LossClass::Lost),
        ("unmapped_symbol", LossClass::Lost),
        ("field_value_unread", LossClass::Lost),
        ("link_target_unread", LossClass::Lost),
        ("number_format_unread", LossClass::Lost),
        ("recovered_stream_order", LossClass::Lost),
        ("form_control_unread", LossClass::Lost),
        ("formatting_runs_capped", LossClass::Lost),
        ("garbled_font_encoding", LossClass::Lost),
        ("annotation_unrendered", LossClass::Lost),
        ("header_omitted", LossClass::Omitted),
        ("footer_omitted", LossClass::Omitted),
        ("comment_omitted", LossClass::Omitted),
        ("revision_omitted", LossClass::Omitted),
        ("hidden_omitted", LossClass::Omitted),
        ("image_omitted", LossClass::Omitted),
        ("external_image", LossClass::Omitted),
        ("unsafe_link", LossClass::Omitted),
        ("image_not_ocred", LossClass::Omitted),
        ("image_placeholder", LossClass::Deferred),
        ("ocr_gap", LossClass::Deferred),
        ("ocr_failed", LossClass::Deferred),
        ("style_part_unreadable", LossClass::Info),
        ("style_cycle", LossClass::Info),
        ("formatting_unreadable", LossClass::Info),
        ("invalid_style_encoding", LossClass::Info),
        ("numbering_unresolved", LossClass::Info),
        ("number_format_fallback", LossClass::Info),
        ("table_span_repaired", LossClass::Info),
        ("conventional_part_used", LossClass::Info),
        ("unmatched_close", LossClass::Info),
        ("dangling_reference", LossClass::Info),
        ("duplicate_note", LossClass::Info),
        ("white_page_filled", LossClass::Info),
        ("field_token", LossClass::Info),
        ("metadata_unreadable", LossClass::Info),
    ];

    #[test]
    fn no_occurrence_is_no_entry() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Body, None, 0);
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Body, NonZeroU32::new(1), 0);
        assert!(diagnostics.is_empty() && !diagnostics.has_loss());
    }

    #[test]
    fn every_code_has_its_fixed_name_and_class() {
        let codes: Vec<(&str, LossClass)> =
            DiagnosticCode::ALL.iter().map(|code| (code.as_str(), code.class())).collect();
        assert_eq!(codes, CODES);
    }

    #[test]
    fn code_names_are_unique_snake_case_and_format_neutral() {
        const FORMATS: [&str; 20] = [
            "docx",
            "xlsx",
            "pptx",
            "doc",
            "xls",
            "ppt",
            "rtf",
            "odf",
            "odt",
            "ods",
            "odp",
            "epub",
            "csv",
            "pdf",
            "ole",
            "ooxml",
            "vml",
            "biff",
            "spine",
            "relationship",
        ];
        let mut seen = std::collections::BTreeSet::new();
        for code in DiagnosticCode::ALL {
            let name = code.as_str();
            assert!(seen.insert(name), "{name} twice");
            assert!(name.starts_with(|c: char| c.is_ascii_lowercase()), "{name}");
            assert!(name.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_')), "{name}");
            for word in name.split('_') {
                assert!(!FORMATS.contains(&word), "{name} names a format");
            }
            assert_eq!(code.to_string(), name);
        }
    }

    #[test]
    fn class_and_region_names_are_stable() {
        let classes = [LossClass::Omitted, LossClass::Deferred, LossClass::Lost, LossClass::Info];
        let names: Vec<&str> = classes.iter().map(|class| class.as_str()).collect();
        assert_eq!(names, ["omitted", "deferred", "lost", "info"]);
        let regions = [
            Region::Body,
            Region::Header,
            Region::Footer,
            Region::Note,
            Region::Comment,
            Region::Metadata,
            Region::Container,
        ];
        let names: Vec<&str> = regions.iter().map(|region| region.as_str()).collect();
        assert_eq!(names, ["body", "header", "footer", "note", "comment", "metadata", "container"]);
        assert_eq!([OnLoss::Fail.as_str(), OnLoss::Report.as_str()], ["fail", "report"]);
        assert_eq!(OnLoss::default(), OnLoss::Fail);
    }

    fn unit(n: u32) -> Option<NonZeroU32> {
        NonZeroU32::new(n)
    }

    #[test]
    fn only_lost_content_is_a_loss() {
        for &code in DiagnosticCode::ALL {
            let mut diagnostics = Diagnostics::default();
            diagnostics.add(code, Region::Body, None, 1);
            assert_eq!(diagnostics.has_loss(), code.class() == LossClass::Lost, "{code}");
            let lost: Vec<DiagnosticCode> = diagnostics.lost_codes().collect();
            assert_eq!(lost.is_empty(), code.class() != LossClass::Lost, "{code}");
        }
        assert!(!Diagnostics::default().has_loss());
        assert!(Diagnostics::default().is_empty());
    }

    #[test]
    fn lost_codes_name_each_code_once_in_order() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::RecoveredXml, Region::Note, None, 1);
        diagnostics.add(DiagnosticCode::StyleCycle, Region::Container, None, 1);
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Body, None, 1);
        diagnostics.add(DiagnosticCode::RecoveredXml, Region::Body, unit(2), 1);
        let lost: Vec<DiagnosticCode> = diagnostics.lost_codes().collect();
        assert_eq!(lost, [DiagnosticCode::PartUnreadable, DiagnosticCode::RecoveredXml]);
    }

    #[test]
    fn equal_reports_add_up_under_one_entry() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Note, None, 2);
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Note, None, 3);
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Note, unit(4), 1);
        let entries: Vec<Diagnostic> = diagnostics.iter().collect();
        let entry = |unit, count| Diagnostic {
            code: DiagnosticCode::PartUnreadable,
            class: LossClass::Lost,
            region: Region::Note,
            unit,
            count,
        };
        assert_eq!(entries, [entry(None, 5), entry(unit(4), 1)]);
    }

    #[test]
    fn units_past_the_bound_fold_the_highest_into_the_entry_without_one() {
        let mut diagnostics = Diagnostics::default();
        let max = u32::try_from(Diagnostics::MAX_UNITS).unwrap();
        for n in (1..=max + 2).rev() {
            diagnostics.add(DiagnosticCode::UnitUnreadable, Region::Body, unit(n), 10);
        }
        let entries: Vec<Diagnostic> = diagnostics.iter().collect();
        assert_eq!(entries.len(), Diagnostics::MAX_UNITS + 1);
        assert_eq!((entries[0].unit, entries[0].count), (None, 20));
        assert_eq!(entries[1].unit, unit(1));
        assert_eq!(entries[Diagnostics::MAX_UNITS].unit, unit(max));
    }

    #[test]
    fn a_diagnostic_holds_enums_and_integers_only() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::RecoveredXml, Region::Body, unit(3), 2);
        let Some(Diagnostic { code, class, region, unit, count }) = diagnostics.iter().next()
        else {
            panic!("one diagnostic");
        };
        let fields: (DiagnosticCode, LossClass, Region, Option<NonZeroU32>, u64) =
            (code, class, region, unit, count);
        assert_eq!(
            fields,
            (DiagnosticCode::RecoveredXml, LossClass::Lost, Region::Body, NonZeroU32::new(3), 2)
        );
    }

    #[test]
    fn counts_saturate() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::OcrGap, Region::Body, None, u64::MAX);
        diagnostics.add(DiagnosticCode::OcrGap, Region::Body, None, 2);
        diagnostics.add(DiagnosticCode::OcrGap, Region::Body, unit(1), u64::MAX);
        diagnostics.add(DiagnosticCode::OcrGap, Region::Body, unit(1), 1);
        let counts: Vec<u64> = diagnostics.iter().map(|d| d.count).collect();
        assert_eq!(counts, [u64::MAX, u64::MAX]);
    }

    fn report() -> impl Strategy<Value = (usize, usize, u32, u64)> {
        (
            0..DiagnosticCode::ALL.len(),
            0..2usize,
            0..u32::try_from(Diagnostics::MAX_UNITS * 2).unwrap(),
            prop_oneof![1..1000u64, Just(u64::MAX / 2), Just(u64::MAX)],
        )
    }

    fn aggregate(reports: &[(usize, usize, u32, u64)]) -> Diagnostics {
        let mut diagnostics = Diagnostics::default();
        for &(code, region, unit, count) in reports {
            let region = [Region::Body, Region::Note][region];
            diagnostics.add(DiagnosticCode::ALL[code], region, NonZeroU32::new(unit), count);
        }
        diagnostics
    }

    proptest! {
        #[test]
        fn aggregation_is_order_independent_saturating_and_bounded(
            reports in prop::collection::vec(report(), 0..400),
            seed in any::<u64>(),
        ) {
            let diagnostics = aggregate(&reports);
            let mut shuffled = reports.clone();
            let mut state = seed | 1;
            for i in (1..shuffled.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let j = usize::try_from(state % (i as u64 + 1)).unwrap();
                shuffled.swap(i, j);
            }
            prop_assert_eq!(&aggregate(&shuffled), &diagnostics);

            let mut expected: BTreeMap<(DiagnosticCode, Region), u64> = BTreeMap::new();
            for &(code, region, _, count) in &reports {
                let key = (DiagnosticCode::ALL[code], [Region::Body, Region::Note][region]);
                let total = expected.entry(key).or_default();
                *total = total.saturating_add(count);
            }
            let mut totals: BTreeMap<(DiagnosticCode, Region), u64> = BTreeMap::new();
            let mut units: BTreeMap<(DiagnosticCode, Region), usize> = BTreeMap::new();
            for d in diagnostics.iter() {
                prop_assert_eq!(d.class, d.code.class());
                let total = totals.entry((d.code, d.region)).or_default();
                *total = total.saturating_add(d.count);
                *units.entry((d.code, d.region)).or_default() += usize::from(d.unit.is_some());
            }
            prop_assert_eq!(totals, expected);
            prop_assert!(units.values().all(|&n| n <= Diagnostics::MAX_UNITS));
        }
    }
}
