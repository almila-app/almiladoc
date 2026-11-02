use super::records as rt;
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::shared::binary::ByteReader;
use crate::shared::inflate::{inflate_record, zlibbed};
use crate::shared::math::Equation;
use crate::shared::officeart::record_at;
use crate::shared::ole_equation::cfb_equation;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::io::Read;

const STORAGE: &str = "ExOleObjStg";
const CORRUPT_STORAGE: &str = "corrupt OLE object";
const UNCOMPRESSED: u16 = 0;
const COMPRESSED: u16 = 1;

/// The `PowerPoint Document` stream and the offsets its persist directory gives each object.
pub(super) struct Persisted<'a> {
    pub(super) persist: &'a HashMap<u32, usize>,
    pub(super) data: &'a [u8],
}

/// An equation object's `ExOleObjStg`: the compound file it embeds, stored or zlib-compressed.
pub(super) struct OleObject {
    instance: u16,
    body: Vec<u8>,
    decoded: OnceCell<Option<Equation>>,
}

impl OleObject {
    pub(super) fn read(
        parse_ctx: &ParseCtx<'_>,
        persisted: &Persisted<'_>,
        persist_ref: u32,
    ) -> Result<Option<Self>, ConvertError> {
        let offset = persisted.persist.get(&persist_ref).copied();
        let Some(record) = offset
            .and_then(|offset| record_at(persisted.data, offset))
            .filter(|record| record.rec_type == rt::EX_OLE_OBJ_STG)
        else {
            return Ok(None);
        };
        parse_ctx.charge(record.body.len() as u64)?;
        let body = record.body.to_vec();
        Ok(Some(Self { instance: record.instance(), body, decoded: OnceCell::new() }))
    }

    /// Decoded at the first reference, a failure too; each later one charges a unit and the TeX
    /// it copies.
    pub(super) fn equation(
        &self,
        parse_ctx: &ParseCtx<'_>,
    ) -> Result<Option<Equation>, ConvertError> {
        if let Some(decoded) = self.decoded.get() {
            let copied = decoded.as_ref().map_or(0, |equation| equation.tex.len() as u64);
            parse_ctx.charge(1 + copied)?;
            return Ok(decoded.clone());
        }
        let decoded = self.decoded(parse_ctx)?;
        Ok(self.decoded.get_or_init(|| decoded).clone())
    }

    fn decoded(&self, parse_ctx: &ParseCtx<'_>) -> Result<Option<Equation>, ConvertError> {
        let storage = match self.instance {
            UNCOMPRESSED => Cow::Borrowed(self.body.as_slice()),
            COMPRESSED => match self.inflated(parse_ctx)? {
                Some(bytes) => Cow::Owned(bytes),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        cfb_equation(parse_ctx, &storage)
    }

    fn inflated(&self, parse_ctx: &ParseCtx<'_>) -> Result<Option<Vec<u8>>, ConvertError> {
        let mut body = ByteReader::new(&self.body);
        let Some(size) = body.u32().map(u64::from) else {
            return Ok(None);
        };
        let packed = body.rest();
        let source = zlibbed(packed).take(size);
        inflate_record(parse_ctx, source, packed.len() as u64, size, STORAGE, CORRUPT_STORAGE)
    }
}

#[cfg(test)]
mod tests {
    use super::{COMPRESSED, OleObject, Persisted, UNCOMPRESSED};
    use crate::ctx::ParseCtx;
    use crate::error::ConvertError;
    use crate::ledger::CancelToken;
    use crate::limits::Limits;
    use crate::shared::officeart::record;
    use crate::shared::test_support::{equation_object, mathtype, zlib};
    use std::assert_matches;
    use std::cell::OnceCell;
    use std::collections::HashMap;

    fn compressed(storage: &[u8]) -> OleObject {
        let size = u32::try_from(storage.len()).unwrap().to_le_bytes();
        let body = [&size[..], &zlib(storage)].concat();
        OleObject { instance: COMPRESSED, body, decoded: OnceCell::new() }
    }

    #[test]
    fn a_cancelled_inflation_stops_within_its_first_chunk() {
        let object = compressed(&vec![0; 3 << 20]);
        let token = CancelToken::new();
        token.cancel();
        let limits = Limits { max_compression_ratio: u64::MAX, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits).with_cancel(Some(&token));
        assert_matches!(object.equation(&parse_ctx), Err(ConvertError::Cancelled));
        assert_eq!(parse_ctx.inflated_bytes(), 64 * 1024);
        let stored = OleObject { instance: UNCOMPRESSED, ..object };
        assert_matches!(stored.equation(&ParseCtx::default()), Ok(None));
    }

    #[test]
    fn later_references_reuse_the_first_decoding_for_a_unit_and_the_tex_they_copy() {
        let mtef = mathtype(false, &[0x78; 40]);
        let cases = [
            (equation_object(28, mtef.len(), &mtef), "x".repeat(40)),
            (b"ole".into(), String::new()),
        ];
        for (storage, tex) in cases {
            let object = compressed(&storage);
            let parse_ctx = ParseCtx::default();
            let read = |object: &OleObject| {
                let equation = object.equation(&parse_ctx).unwrap();
                (
                    equation.map(|equation| equation.tex).unwrap_or_default(),
                    parse_ctx.inflated_bytes(),
                )
            };
            assert_eq!(read(&object), (tex.clone(), storage.len() as u64));
            let fuel = parse_ctx.fuel();
            assert_eq!(read(&object), (tex.clone(), storage.len() as u64));
            assert_eq!(parse_ctx.fuel(), fuel + 1 + tex.len() as u64);
        }
    }

    #[test]
    fn an_object_is_found_through_the_persist_directory_and_charged_by_its_record() {
        let stream =
            [&[0; 8][..], &record(1 << 4, 0x1011, &[7; 100]), &record(0, 0x1010, &[7; 100])]
                .concat();
        let persist = HashMap::from([(5, 8), (6, 116), (7, 9999)]);
        let persisted = Persisted { persist: &persist, data: &stream };
        let read = |persist_ref| {
            let parse_ctx = ParseCtx::default();
            let object = OleObject::read(&parse_ctx, &persisted, persist_ref);
            (object.unwrap().map(|object| (object.instance, object.body.len())), parse_ctx.fuel())
        };
        assert_eq!(read(5), (Some((1, 100)), 100));
        for missing in [6, 7, 8] {
            assert_eq!(read(missing), (None, 0), "{missing}");
        }
    }
}
