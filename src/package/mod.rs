//! Shared package layer for ZIP-based formats (DOCX, PPTX, ODF, EPUB):
//! limited archive access, namespace-aware XML, typed relationships, and
//! OPC/EPUB target resolution.

pub(crate) mod archive;
pub(crate) mod directory;
pub(crate) mod path;
pub(crate) mod relationships;
pub(crate) mod xml;

pub(crate) use archive::{Package, PackageKind};
