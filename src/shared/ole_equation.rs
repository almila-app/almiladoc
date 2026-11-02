//! `MathType` and Equation Editor objects: the CFB an `Equation.DSMT*` or `Equation.3` object
//! embeds holds its MTEF in the `Equation Native` stream behind an EQNOLEFILEHDR.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::shared::binary::{ByteReader, OptionalStream, optional_ole_stream};
use crate::shared::math::{Equation, mtef_to_tex};
use std::io::Cursor;

const EQUATION_NATIVE: &str = "Equation Native";
const EQNOLEFILEHDR_BYTES: u16 = 28;
const RESERVED_BYTES: usize = 16;

fn is_equation(prog_id: &str) -> bool {
    prog_id == "Equation.3" || prog_id.starts_with("Equation.DSMT")
}

/// `None` keeps the caller's fallback: not an equation's `ProgID`, or a payload this reader
/// cannot decode.
pub(crate) fn ole_equation(
    parse_ctx: &ParseCtx<'_>,
    prog_id: Option<&str>,
    cfb: &[u8],
) -> Result<Option<Equation>, ConvertError> {
    if !prog_id.is_some_and(is_equation) {
        return Ok(None);
    }
    parse_ctx.charge(cfb.len() as u64)?;
    let Ok(mut ole) = cfb::CompoundFile::open(Cursor::new(cfb)) else {
        return Ok(None);
    };
    let OptionalStream::Read(native) = optional_ole_stream(parse_ctx, &mut ole, EQUATION_NATIVE)?
    else {
        return Ok(None);
    };
    match mtef(&native) {
        Some(mtef) => mtef_to_tex(parse_ctx, mtef),
        None => Ok(None),
    }
}

fn mtef(native: &[u8]) -> Option<&[u8]> {
    let mut header = ByteReader::new(native);
    (header.u16()? == EQNOLEFILEHDR_BYTES).then_some(())?;
    header.u32()?;
    header.u16()?;
    let len = usize::try_from(header.u32()?).ok()?;
    header.skip(RESERVED_BYTES)?;
    header.take(len)
}

#[cfg(test)]
mod tests {
    use super::ole_equation;
    use crate::ctx::ParseCtx;
    use crate::error::ConvertError;
    use crate::limits::{Limit, Limits};
    use crate::shared::test_support::{equation_object, mathtype, one_stream_cfb};
    use std::assert_matches;

    #[test]
    fn an_object_decodes_only_under_an_equation_prog_id_with_a_whole_header() {
        let mtef = mathtype(false, &[0x78]);
        let read = |prog_id, cfb: &[u8]| {
            ole_equation(&ParseCtx::default(), prog_id, cfb).unwrap().map(|equation| equation.tex)
        };
        let whole = equation_object(28, mtef.len(), &mtef);
        for prog_id in ["Equation.3", "Equation.DSMT4", "Equation.DSMT6"] {
            assert_eq!(read(Some(prog_id), &whole).as_deref(), Some("x"), "{prog_id}");
        }
        let mathtype = Some("Equation.DSMT4");
        let fallbacks = [
            ("no ProgID", None, whole.clone()),
            ("another ProgID", Some("Equation.2"), whole.clone()),
            ("no compound file", mathtype, b"ole".to_vec()),
            ("no Equation Native", mathtype, one_stream_cfb("Contents", &whole)),
            ("a header of another size", mathtype, equation_object(27, mtef.len(), &mtef)),
            ("MTEF past its stream", mathtype, equation_object(28, mtef.len() + 1, &mtef)),
        ];
        for (name, prog_id, cfb) in fallbacks {
            assert_eq!(read(prog_id, &cfb), None, "{name}");
        }
    }

    #[test]
    fn a_stream_past_the_entry_cap_fails() {
        let mtef = mathtype(false, &[0x78]);
        let limits = Limits { max_entry_bytes: 40, ..Limits::default() };
        let read = ole_equation(
            &ParseCtx::new(&limits),
            Some("Equation.3"),
            &equation_object(28, mtef.len(), &mtef),
        );
        assert_matches!(
            read,
            Err(ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 40 })
        );
    }
}
