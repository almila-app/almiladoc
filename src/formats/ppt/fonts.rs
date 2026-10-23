//! The `FontCollectionContainer` of a deck's `Environment`: the faces that `fontRef` and
//! `symbolFontRef` index ([MS-PPT] 2.9.8).

use super::records as rt;
use super::scan;
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::shared::binary::{decode_utf16, utf16le_units};
use crate::shared::typeface::Slot;

/// Bytes of a `FontEntityAtom`'s `lfFaceName`.
const FACE_NAME_BYTES: usize = 64;

/// The slot of each face, in the order of the collection's entries.
#[derive(Debug, Default)]
pub(super) struct FontCollection(Vec<Slot>);

impl FontCollection {
    /// Read the entries of a `FontCollectionContainer` body, each charged as one record. A
    /// font reference indexes the entries in order; `recInstance` is no index.
    pub(super) fn read(parse_ctx: &ParseCtx<'_>, body: &[u8]) -> Result<Self, ConvertError> {
        let atoms = scan(parse_ctx, body)?;
        let faces = atoms.into_iter().filter(|r| r.rec_type == rt::FONT_ENTITY_ATOM);
        Ok(Self(faces.map(|atom| face_slot(atom.body)).collect()))
    }

    /// The slot a font reference sets; a reference past the end of the collection names no face.
    pub(super) fn slot(&self, font_ref: u16) -> Slot {
        self.0.get(usize::from(font_ref)).copied().unwrap_or(Slot::Plain)
    }
}

/// The slot of the NUL-terminated UTF-16 `lfFaceName` that opens a `FontEntityAtom` body.
fn face_slot(body: &[u8]) -> Slot {
    let name = body.get(..FACE_NAME_BYTES).unwrap_or(body);
    let (face, _) = decode_utf16(utf16le_units(name).take_while(|&unit| unit != 0));
    Slot::named(&face)
}
