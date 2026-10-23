//! OPC relationship parts, typed.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::package::archive::{Package, Role};
use crate::package::path;
use crate::package::xml::{Element, normalize_ooxml_uri, ns};
use std::collections::BTreeMap;
use std::rc::Rc;

/// Well-known OPC relationship types (Transitional form; Strict types are
/// normalized onto these when the rels part is read).
pub(crate) mod rel_type {
    pub(crate) const OFFICE_DOCUMENT: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
    pub(crate) const STYLES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
    pub(crate) const NUMBERING: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
    pub(crate) const FOOTNOTES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
    pub(crate) const ENDNOTES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetMode {
    Internal,
    External,
}

#[derive(Debug, Clone)]
pub(crate) struct Relationship {
    pub target: String,
    pub rel_type: String,
    pub mode: TargetMode,
}

#[derive(Debug, Default)]
pub(crate) struct Relationships {
    map: BTreeMap<String, Relationship>,
    reported: bool,
}

impl Relationships {
    pub(crate) fn from_root(root: &Element) -> Self {
        let mut map = BTreeMap::new();
        for rel in root.descendants(ns::PKG_RELS, "Relationship") {
            let (Some(id), Some(target)) = (rel.attr_any("Id"), rel.attr_any("Target")) else {
                continue;
            };
            let mode = match rel.attr_any("TargetMode") {
                Some(m) if m.eq_ignore_ascii_case("external") => TargetMode::External,
                _ => TargetMode::Internal,
            };
            let rel_type = rel.attr_any("Type").unwrap_or("");
            let rel_type = normalize_ooxml_uri(rel_type).unwrap_or_else(|| rel_type.to_string());
            map.insert(id.to_string(), Relationship { target: target.to_string(), rel_type, mode });
        }
        Self { map, reported: false }
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Relationship> {
        self.map.get(id)
    }

    /// Internal-mode target for an id, the common case for parts.
    pub(crate) fn internal_target(&self, id: &str) -> Option<&str> {
        self.map.get(id).filter(|r| r.mode == TargetMode::Internal).map(|r| r.target.as_str())
    }

    /// The relationships in id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Relationship)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// True when the part exists but could not be read and its read reported
    /// the loss by role, so the sites that resolve through it stay silent.
    pub(crate) fn reported(&self) -> bool {
        self.reported
    }

    /// The internal-mode relationship of a given type, lowest id first so
    /// the pick is deterministic when a producer emits duplicates.
    pub(crate) fn first_of_type(&self, rel_type: &str) -> Option<&Relationship> {
        self.map.values().find(|r| r.rel_type == rel_type && r.mode == TargetMode::Internal)
    }
}

/// Read a relationships part. An absent, unreadable, or corrupt part yields
/// an empty map (absent is valid - many parts simply have no relationships;
/// an unreadable one whose `role` reports it is marked
/// [`reported`](Relationships::reported)). Fatal errors always propagate.
pub(crate) fn read_rels(
    pkg: &Package<'_>,
    part: &str,
    role: Role,
) -> Result<Relationships, ConvertError> {
    let Some(root) = pkg.optional_xml_part(part, role)? else {
        return Ok(Relationships {
            map: BTreeMap::new(),
            reported: role.unreadable().is_some() && pkg.has_part(part),
        });
    };
    Ok(Relationships::from_root(&root))
}

/// A loaded internal relationship target: its resolved part path and bytes.
pub(crate) type RelTarget = (String, Rc<[u8]>);

/// Load an internal relationship target's bytes, resolved against the part
/// the relationship appears in. Unknown ids, external targets, and
/// unresolvable or unreadable targets read as `None`, whose loss the caller
/// reports; fatal errors always propagate.
pub(crate) fn rel_target_bytes(
    pkg: &Package<'_>,
    rels: &Relationships,
    base_part: &str,
    rel_id: &str,
) -> Result<Option<RelTarget>, ConvertError> {
    let Some(path) = rel_target_path(rels, base_part, rel_id) else {
        return Ok(None);
    };
    let bytes = pkg.related_part(&path, Role::Reported(Region::Body))?;
    Ok(bytes.map(|bytes| (path, bytes)))
}

/// An internal relationship target's part path, resolved against the part
/// the relationship appears in: `None` for an unknown id, an external target
/// or one that does not resolve.
pub(crate) fn rel_target_path(
    rels: &Relationships,
    base_part: &str,
    rel_id: &str,
) -> Option<String> {
    path::resolve(base_part, rels.internal_target(rel_id)?).ok().map(|target| target.path)
}

pub(crate) const ROOT_RELS: &str = "_rels/.rels";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MainPart {
    pub(crate) path: String,
    pub(crate) by_convention: bool,
}

/// The part the officeDocument relationship names once it resolves to a part
/// path, else the first of `conventional` the package holds. A resolving
/// target decides even when the part is absent, so a leftover conventional
/// part never replaces it.
pub(crate) fn main_part(
    pkg: &Package<'_>,
    root: &Relationships,
    conventional: &[&str],
) -> Option<MainPart> {
    let designated = root
        .first_of_type(rel_type::OFFICE_DOCUMENT)
        .and_then(|rel| path::resolve("", &rel.target).ok())
        .map(|target| target.path)
        .filter(|part| !part.is_empty());
    if let Some(part) = designated {
        return Some(MainPart { path: part, by_convention: false });
    }
    conventional
        .iter()
        .find(|part| pkg.has_part(part))
        .map(|part| MainPart { path: (*part).to_string(), by_convention: true })
}

/// The main part's path, reporting [`DiagnosticCode::ConventionalPartUsed`]
/// when a conventional location supplied it although the package has a root
/// relationships part; `None` when the package holds no main part.
pub(crate) fn read_main_part(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    conventional: &[&str],
) -> Result<Option<String>, ConvertError> {
    let root = read_rels(pkg, ROOT_RELS, Role::Reported(Region::Body))?;
    let Some(main) = main_part(pkg, &root, conventional) else {
        return Ok(None);
    };
    if main.by_convention && pkg.has_part(ROOT_RELS) {
        parse_ctx.emit(DiagnosticCode::ConventionalPartUsed, Region::Container);
    }
    Ok(Some(main.path))
}

/// Conventional rels part name for a part (`word/document.xml` ->
/// `word/_rels/document.xml.rels`).
pub(crate) fn rels_part_for(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture XML is built with format!")]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use crate::package::archive::PackageKind;
    use crate::package::xml::parse_xml;
    use crate::shared::test_support::{
        corrupt_entry, diagnostic_rows, stored, zip_package, zip_package_with,
    };
    use std::assert_matches;

    const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="t" Target="a.xml"/></Relationships>"#;

    #[test]
    fn only_a_reported_unreadable_part_is_marked() {
        let mut bytes = zip_package_with(&[("good.rels", RELS), ("bad.rels", RELS)], stored());
        corrupt_entry(&mut bytes, "bad.rels");
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &bytes, PackageKind::Opc).unwrap();
        let role = Role::Content(Region::Body);
        let good = read_rels(&pkg, "good.rels", role).unwrap();
        assert_eq!((good.internal_target("rId1"), good.reported()), (Some("a.xml"), false));
        let bad = read_rels(&pkg, "bad.rels", role).unwrap();
        assert_eq!((bad.iter().count(), bad.reported()), (0, true));
        let absent = read_rels(&pkg, "absent.rels", role).unwrap();
        assert_eq!((absent.iter().count(), absent.reported()), (0, false));
        let silent = read_rels(&pkg, "bad.rels", Role::Reported(Region::Body)).unwrap();
        assert_eq!((silent.iter().count(), silent.reported()), (0, false));
        drop(pkg);
        let codes: Vec<_> =
            parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.count)).collect();
        assert_eq!(codes, [(DiagnosticCode::PartUnreadable, 1)]);
    }

    #[test]
    fn only_internal_resolvable_targets_have_a_part_path() {
        let xml = root_rels(&[
            ("rId1", "t", r#"Target="../media/a.png""#),
            ("rId2", "t", r#"Target="b.xml" TargetMode="External""#),
            ("rId3", "t", r#"Target="a/%2F.xml""#),
        ]);
        let parse_ctx = ParseCtx::default();
        let root = parse_xml(&parse_ctx, xml.as_bytes(), Role::Content(Region::Body)).unwrap();
        let rels = Relationships::from_root(&root);
        let paths: Vec<_> = ["rId1", "rId2", "rId3", "rId4"]
            .map(|id| rel_target_path(&rels, "word/document.xml", id))
            .into();
        assert_eq!(paths, [Some("media/a.png".to_string()), None, None, None]);
    }

    const PACKAGE_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const STRICT_OFFICE_DOCUMENT: &str =
        "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument";
    const WORD: &str = "word/document.xml";
    const SHEETS: [&str; 2] = ["xl/workbook.xml", "xl/workbook.bin"];

    fn root_rels(relationships: &[(&str, &str, &str)]) -> String {
        let body: String = relationships
            .iter()
            .map(|(id, kind, attrs)| format!(r#"<Relationship Id="{id}" Type="{kind}" {attrs}/>"#))
            .collect();
        format!(r#"<Relationships xmlns="{PACKAGE_RELS}">{body}</Relationships>"#)
    }

    fn office(target: &str) -> String {
        root_rels(&[("rId1", rel_type::OFFICE_DOCUMENT, &format!(r#"Target="{target}""#))])
    }

    fn held(root: Option<&str>, parts: &[&str]) -> Vec<u8> {
        let mut entries: Vec<(&str, &str)> = parts.iter().map(|part| (*part, "<x/>")).collect();
        entries.push(("docProps/core.xml", "<x/>"));
        entries.extend(root.map(|root| (ROOT_RELS, root)));
        zip_package(&entries)
    }

    fn main_of(root: Option<&str>, parts: &[&str], conventional: &[&str]) -> Option<MainPart> {
        let bytes = held(root, parts);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &bytes, PackageKind::Opc).unwrap();
        let rels = read_rels(&pkg, ROOT_RELS, Role::Reported(Region::Body)).unwrap();
        main_part(&pkg, &rels, conventional)
    }

    fn named(path: &str) -> MainPart {
        MainPart { path: path.into(), by_convention: false }
    }

    fn conventional(path: &str) -> MainPart {
        MainPart { path: path.into(), by_convention: true }
    }

    #[test]
    fn the_target_wins_over_a_conventional_part() {
        let root = office("custom/main.xml");
        let both = main_of(Some(&root), &["custom/main.xml", WORD], &[WORD]);
        assert_eq!(both, Some(named("custom/main.xml")));
        assert_eq!(
            main_of(Some(&root), &["custom/main.xml"], &[WORD]),
            Some(named("custom/main.xml"))
        );
        let absolute = office("/custom/main.xml");
        assert_eq!(main_of(Some(&absolute), &[WORD], &[WORD]), Some(named("custom/main.xml")));
    }

    #[test]
    fn a_target_that_resolves_to_an_absent_part_still_decides() {
        let root = office("custom/main.xml");
        assert_eq!(main_of(Some(&root), &[WORD], &[WORD]), Some(named("custom/main.xml")));
    }

    #[test]
    fn a_target_that_names_no_part_falls_back_to_the_first_conventional_part() {
        for target in ["", "/", ".", "#frag", "?q=1", "../..", "a%2Fb.xml", "%2e%2e/x.xml"] {
            let root = office(target);
            assert_eq!(
                main_of(Some(&root), &[WORD], &[WORD]),
                Some(conventional(WORD)),
                "{target:?}"
            );
        }
    }

    #[test]
    fn only_an_internal_office_document_relationship_names_the_main_part() {
        let external = root_rels(&[(
            "rId1",
            rel_type::OFFICE_DOCUMENT,
            r#"Target="custom/main.xml" TargetMode="External""#,
        )]);
        let parts = ["custom/main.xml", WORD];
        assert_eq!(main_of(Some(&external), &parts, &[WORD]), Some(conventional(WORD)));
        let other = root_rels(&[("rId1", rel_type::STYLES, r#"Target="custom/main.xml""#)]);
        assert_eq!(main_of(Some(&other), &parts, &[WORD]), Some(conventional(WORD)));
        let strict = root_rels(&[("rId1", STRICT_OFFICE_DOCUMENT, r#"Target="custom/main.xml""#)]);
        assert_eq!(main_of(Some(&strict), &[WORD], &[WORD]), Some(named("custom/main.xml")));
    }

    #[test]
    fn the_lowest_internal_id_of_a_type_comes_first_whatever_the_order_written() {
        let kind = rel_type::STYLES;
        let root = root_rels(&[
            ("rId9", kind, r#"Target="nine.xml""#),
            ("rId10", kind, r#"Target="ten.xml""#),
            ("rId0", kind, r#"Target="external.xml" TargetMode="External""#),
            ("rId3", kind, r#"Target="three.xml""#),
        ]);
        let root = parse_xml(&ParseCtx::default(), root.as_bytes(), Role::Probe).unwrap();
        let rels = Relationships::from_root(&root);
        let pick = rels.first_of_type(kind).map(|rel| rel.target.as_str());
        assert_eq!(pick, Some("ten.xml"), "ids order as strings: rId10 precedes rId3");
        let ids: Vec<_> = rels.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["rId0", "rId10", "rId3", "rId9"]);
        assert_eq!(rels.first_of_type(rel_type::NUMBERING).map(|rel| &rel.target), None);
    }

    #[test]
    fn the_lowest_id_decides_even_when_it_does_not_resolve() {
        let kind = rel_type::OFFICE_DOCUMENT;
        let root = root_rels(&[
            ("rId2", kind, r#"Target="custom/main.xml""#),
            ("rId1", kind, r#"Target="a%2Fb.xml""#),
        ]);
        assert_eq!(
            main_of(Some(&root), &["custom/main.xml", WORD], &[WORD]),
            Some(conventional(WORD))
        );
    }

    #[test]
    fn conventional_parts_are_taken_in_the_order_listed() {
        let both = ["xl/workbook.bin", "xl/workbook.xml"];
        assert_eq!(main_of(None, &both, &SHEETS), Some(conventional("xl/workbook.xml")));
        assert_eq!(
            main_of(None, &both, &[SHEETS[1], SHEETS[0]]),
            Some(conventional("xl/workbook.bin"))
        );
        assert_eq!(
            main_of(None, &["xl/workbook.bin"], &SHEETS),
            Some(conventional("xl/workbook.bin"))
        );
    }

    #[test]
    fn conventional_parts_are_found_whatever_their_case() {
        assert_eq!(main_of(None, &["Word/Document.XML"], &[WORD]), Some(conventional(WORD)));
    }

    #[test]
    fn a_package_with_no_main_part_has_none() {
        let without = root_rels(&[("rId1", rel_type::STYLES, r#"Target="styles.xml""#)]);
        assert_eq!(main_of(None, &[], &[WORD]), None);
        assert_eq!(main_of(Some(&without), &["xl/workbook.xml"], &[WORD]), None);
        assert_eq!(main_of(Some(&office("")), &[WORD], &[]), None);
        assert_eq!(main_of(Some(&office("")), &["styles.xml"], &[WORD]), None);
    }

    type Read = (Option<String>, Vec<(DiagnosticCode, Region, u64)>);

    fn read_of(root: Option<&str>, parts: &[&str], conventional: &[&str]) -> Read {
        let bytes = held(root, parts);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &bytes, PackageKind::Opc).unwrap();
        let read = read_main_part(&parse_ctx, &pkg, conventional).unwrap();
        drop(pkg);
        (read, diagnostic_rows(&parse_ctx.into_diagnostics()))
    }

    #[test]
    fn a_conventional_main_part_is_reported_beside_a_root_relationships_part() {
        let used = vec![(DiagnosticCode::ConventionalPartUsed, Region::Container, 1)];
        let without = root_rels(&[("rId1", rel_type::STYLES, r#"Target="styles.xml""#)]);
        for root in [without, office("a%2Fb.xml"), office(""), office("/")] {
            assert_eq!(read_of(Some(&root), &[WORD], &[WORD]), (Some(WORD.into()), used.clone()));
        }
        assert_eq!(read_of(None, &[WORD], &[WORD]), (Some(WORD.into()), vec![]));
    }

    #[test]
    fn a_named_or_absent_main_part_reports_nothing() {
        let parts = ["custom/main.xml", WORD];
        let named = read_of(Some(&office("custom/main.xml")), &parts, &[WORD]);
        assert_eq!(named, (Some("custom/main.xml".into()), vec![]));
        let absent = read_of(Some(&office("")), &["styles.xml"], &[WORD]);
        assert_eq!(absent, (None, vec![]));
        assert_eq!(read_of(None, &[], &[WORD]), (None, vec![]));
    }

    #[test]
    fn an_unreadable_root_relationships_part_reports_only_the_conventional_fallback() {
        let mut bytes = held(Some(&office("custom/main.xml")), &["custom/main.xml", WORD]);
        corrupt_entry(&mut bytes, ROOT_RELS);
        let parse_ctx = ParseCtx::default();
        let pkg = Package::open(&parse_ctx, &bytes, PackageKind::Opc).unwrap();
        assert_eq!(read_main_part(&parse_ctx, &pkg, &[WORD]).unwrap().as_deref(), Some(WORD));
        drop(pkg);
        let codes: Vec<_> =
            parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.count)).collect();
        assert_eq!(codes, [(DiagnosticCode::ConventionalPartUsed, 1)]);
    }

    #[test]
    fn a_fatal_error_reading_the_root_relationships_part_propagates() {
        let root = office("custom/main.xml");
        let bytes = held(Some(&root), &["custom/main.xml"]);
        let max_entry_bytes = u64::try_from(root.len()).unwrap() - 1;
        let limits = Limits { max_entry_bytes, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let pkg = Package::open(&parse_ctx, &bytes, PackageKind::Opc).unwrap();
        assert_matches!(
            read_main_part(&parse_ctx, &pkg, &[WORD]),
            Err(ConvertError::ResourceLimit { limit: Limit::EntryBytes, max }) if max == max_entry_bytes
        );
    }
}
