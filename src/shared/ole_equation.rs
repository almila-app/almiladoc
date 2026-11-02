//! `MathType` and Equation Editor objects: the CFB an `Equation.DSMT*` or `Equation.3` object
//! embeds holds its MTEF in the `Equation Native` stream behind an EQNOLEFILEHDR. RTF wraps that
//! CFB in an OLE1 object header, and DOC keeps it as a storage of the document's own CFB.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::shared::binary::{ByteReader, OptionalStream, charged_ole_stream, optional_ole_stream};
use crate::shared::math::{Equation, mtef_to_tex};
use std::io::{Cursor, Read, Seek};

const EQUATION_NATIVE: &str = "Equation Native";
const EQNOLEFILEHDR_BYTES: u16 = 28;
const RESERVED_BYTES: usize = 16;
const OLE1_EMBEDDED: u32 = 2;

pub(crate) fn is_equation(prog_id: &str) -> bool {
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
    cfb_equation(parse_ctx, cfb)
}

/// The equation an object's compound file holds, its `ProgID` already checked by the caller.
pub(crate) fn cfb_equation(
    parse_ctx: &ParseCtx<'_>,
    cfb: &[u8],
) -> Result<Option<Equation>, ConvertError> {
    parse_ctx.charge(cfb.len() as u64)?;
    let Ok(mut ole) = cfb::CompoundFile::open(Cursor::new(cfb)) else {
        return Ok(None);
    };
    let OptionalStream::Read(native) = optional_ole_stream(parse_ctx, &mut ole, EQUATION_NATIVE)?
    else {
        return Ok(None);
    };
    native_equation(parse_ctx, &native)
}

/// The equation a storage of the document's own compound file holds, which carries no `ProgID`:
/// its `Equation Native` stream is charged chunk by chunk as it is copied, an unreadable one's too.
pub(crate) fn pooled_equation<R: Read + Seek>(
    parse_ctx: &ParseCtx<'_>,
    ole: &mut cfb::CompoundFile<R>,
    storage: &str,
) -> Result<Option<Equation>, ConvertError> {
    let path = format!("{storage}/{EQUATION_NATIVE}");
    let OptionalStream::Read(native) = charged_ole_stream(parse_ctx, ole, &path)? else {
        return Ok(None);
    };
    native_equation(parse_ctx, &native)
}

fn native_equation(
    parse_ctx: &ParseCtx<'_>,
    native: &[u8],
) -> Result<Option<Equation>, ConvertError> {
    match mtef(native) {
        Some(mtef) => mtef_to_tex(parse_ctx, mtef),
        None => Ok(None),
    }
}

pub(crate) struct Ole1Object<'a> {
    pub class: &'a str,
    pub native: &'a [u8],
}

/// An OLE1 embedded object: its header names the class, its native data is the compound file an
/// RTF `\objdata` carries.
pub(crate) fn ole1_object(object: &[u8]) -> Option<Ole1Object<'_>> {
    let mut reader = ByteReader::new(object);
    reader.skip(4)?;
    (reader.u32()? == OLE1_EMBEDDED).then_some(())?;
    let class = std::str::from_utf8(ansi_string(&mut reader)?).ok()?;
    ansi_string(&mut reader)?;
    ansi_string(&mut reader)?;
    let len = usize::try_from(reader.u32()?).ok()?;
    Some(Ole1Object { class, native: reader.take(len)? })
}

fn ansi_string<'a>(reader: &mut ByteReader<'a>) -> Option<&'a [u8]> {
    let len = usize::try_from(reader.u32()?).ok()?;
    let bytes = reader.take(len)?;
    Some(bytes.strip_suffix(&[0]).unwrap_or(bytes))
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
    use super::{ole_equation, ole1_object, pooled_equation};
    use crate::ctx::ParseCtx;
    use crate::error::ConvertError;
    use crate::limits::{Limit, Limits};
    use crate::shared::math::mtef_to_tex;
    use crate::shared::test_support::{
        equation_native, equation_object, mathtype, ole1_data, one_stream_cfb,
    };
    use std::assert_matches;
    use std::io::{Cursor, Write};

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
    fn an_ole1_object_names_its_class_then_holds_its_native_data() {
        let whole = ole1_data("Equation.3", b"native");
        let parsed = ole1_object(&whole).unwrap();
        assert_eq!((parsed.class, parsed.native), ("Equation.3", &b"native"[..]));
        for end in 0..whole.len() {
            assert!(ole1_object(&whole[..end]).is_none(), "cut at {end}");
        }
        let named = |format: u32, class: &[u8], names: [&[u8]; 2], size: u32| {
            let string = |bytes: &[u8]| {
                let len = u32::try_from(bytes.len()).unwrap();
                [&len.to_le_bytes()[..], bytes].concat()
            };
            [
                &0x0501_u32.to_le_bytes()[..],
                &format.to_le_bytes(),
                &string(class),
                &string(names[0]),
                &string(names[1]),
                &size.to_le_bytes(),
                b"native",
            ]
            .concat()
        };
        type Parsed = Option<(&'static str, &'static [u8])>;
        let cases: [(&str, Vec<u8>, Parsed); 6] = [
            (
                "topic and item",
                named(2, b"Equation.3\0", [b"topic\0", b"item\0"], 6),
                Some(("Equation.3", b"native")),
            ),
            (
                "a class with no terminator",
                named(2, b"Equation.3", [b"", b""], 6),
                Some(("Equation.3", b"native")),
            ),
            ("no class", named(2, b"", [b"", b""], 6), Some(("", b"native"))),
            ("a linked object", named(1, b"Equation.3\0", [b"", b""], 6), None),
            ("a class that is not text", named(2, b"\xff\xfe\0", [b"", b""], 6), None),
            ("native data past the end", named(2, b"Equation.3\0", [b"", b""], 7), None),
        ];
        for (case, bytes, expected) in cases {
            let got = ole1_object(&bytes).map(|object| (object.class, object.native));
            assert_eq!(got, expected, "{case}");
        }
    }

    #[test]
    fn a_pooled_equation_is_read_without_a_prog_id_and_charged_by_its_stream() {
        let mtef = mathtype(false, &[0x78]);
        let native = equation_native(28, mtef.len(), &mtef);
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_storage_all("/ObjectPool/_7").unwrap();
        ole.create_stream("/ObjectPool/_7/Equation Native").unwrap().write_all(&native).unwrap();
        ole.create_storage_all("/ObjectPool/_8").unwrap();
        let ctx = ParseCtx::default();
        let tex = pooled_equation(&ctx, &mut ole, "ObjectPool/_7").unwrap().map(|e| e.tex);
        assert_eq!(tex.as_deref(), Some("x"));
        let decoding = ParseCtx::default();
        mtef_to_tex(&decoding, &mtef).unwrap();
        assert_eq!(ctx.fuel(), native.len() as u64 + decoding.fuel());
        for storage in ["ObjectPool/_8", "ObjectPool/_9"] {
            let ctx = ParseCtx::default();
            assert_eq!(pooled_equation(&ctx, &mut ole, storage).unwrap().map(|e| e.tex), None);
            assert_eq!(ctx.fuel(), 0, "{storage}");
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
