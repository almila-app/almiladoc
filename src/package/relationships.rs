//! OPC relationship parts, typed.

use crate::diagnostics::Region;
use crate::error::ConvertError;
use crate::package::archive::{Package, Role};
use crate::package::xml::{Element, normalize_ooxml_uri, ns};
use std::collections::HashMap;
use std::rc::Rc;

/// Well-known OPC relationship types (Transitional form; Strict types are
/// normalized onto these when the rels part is read).
pub mod rel_type {
    pub const OFFICE_DOCUMENT: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
    pub const STYLES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
    pub const NUMBERING: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
    pub const FOOTNOTES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
    pub const ENDNOTES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    Internal,
    External,
}

#[derive(Debug, Clone)]
pub struct Relationship {
    pub target: String,
    pub rel_type: String,
    pub mode: TargetMode,
}

#[derive(Debug, Default)]
pub struct Relationships {
    map: HashMap<String, Relationship>,
    reported: bool,
}

impl Relationships {
    pub(crate) fn from_root(root: &Element) -> Self {
        let mut map = HashMap::new();
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
        Relationships { map, reported: false }
    }

    pub fn get(&self, id: &str) -> Option<&Relationship> {
        self.map.get(id)
    }

    /// Internal-mode target for an id, the common case for parts.
    pub fn internal_target(&self, id: &str) -> Option<&str> {
        self.map.get(id).filter(|r| r.mode == TargetMode::Internal).map(|r| r.target.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Relationship)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// True when the part exists but could not be read and its read reported
    /// the loss by role, so the sites that resolve through it stay silent.
    pub(crate) fn reported(&self) -> bool {
        self.reported
    }

    /// The internal-mode relationship of a given type, lowest id first so
    /// the pick is deterministic when a producer emits duplicates.
    pub fn first_of_type(&self, rel_type: &str) -> Option<&Relationship> {
        self.map
            .iter()
            .filter(|(_, r)| r.rel_type == rel_type && r.mode == TargetMode::Internal)
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, r)| r)
    }
}

/// Read a relationships part. An absent, unreadable, or corrupt part yields
/// an empty map (absent is valid - many parts simply have no relationships;
/// an unreadable one whose `role` reports it is marked
/// [`reported`](Relationships::reported)). Fatal errors always propagate.
pub fn read_rels(pkg: &Package, part: &str, role: Role) -> Result<Relationships, ConvertError> {
    let Some(root) = pkg.optional_xml_part(part, role)? else {
        return Ok(Relationships {
            map: HashMap::new(),
            reported: role.unreadable().is_some() && pkg.has_part(part),
        });
    };
    Ok(Relationships::from_root(&root))
}

/// A loaded internal relationship target: its resolved part path and bytes.
pub type RelTarget = (String, Rc<[u8]>);

/// Load an internal relationship target's bytes, resolved against the part
/// the relationship appears in. Unknown ids, external targets, and
/// unresolvable or unreadable targets read as `None`, whose loss the caller
/// reports; fatal errors always propagate.
pub fn rel_target_bytes(
    pkg: &Package,
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
pub fn rel_target_path(rels: &Relationships, base_part: &str, rel_id: &str) -> Option<String> {
    let rel = rels.get(rel_id).filter(|rel| rel.mode == TargetMode::Internal)?;
    crate::package::path::resolve(base_part, &rel.target).ok().map(|target| target.path)
}

/// Conventional rels part name for a part (`word/document.xml` ->
/// `word/_rels/document.xml.rels`).
pub fn rels_part_for(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ParseCtx;
    use crate::diagnostics::DiagnosticCode;
    use crate::package::archive::PackageKind;
    use std::io::{Cursor, Write};

    const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="t" Target="a.xml"/></Relationships>"#;

    fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in parts {
            w.start_file(*name, stored).unwrap();
            w.write_all(bytes).unwrap();
        }
        let mut data = w.finish().unwrap().into_inner();
        let start = zip::ZipArchive::new(Cursor::new(data.as_slice()))
            .unwrap()
            .by_name("bad.rels")
            .unwrap()
            .data_start()
            .unwrap();
        data[usize::try_from(start).unwrap()] ^= 1;
        data
    }

    #[test]
    fn only_a_reported_unreadable_part_is_marked() {
        let bytes = package(&[("good.rels", RELS.as_bytes()), ("bad.rels", RELS.as_bytes())]);
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
}
