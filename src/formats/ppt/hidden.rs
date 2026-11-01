use super::{check_record_depth, count_record, listed_elsewhere, records as rt};
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::shared::binary::{get_u16, get_u32};
use crate::shared::officeart::{self, Record};
use std::collections::BTreeSet;

const GROUP_SHAPE_FLAGS: u16 = 0x03BF;
const GROUP: u32 = 0x0001;
const HIDDEN: u32 = 0x0002;
const USE_HIDDEN: u32 = HIDDEN << 16;
const SLIDE_HIDDEN: u16 = 0x0004;
const SLIDE_FLAGS_AT: usize = 10;

pub(super) fn slide(ctx: &ParseCtx<'_>, container: &[u8]) -> Result<bool, ConvertError> {
    let mut seen = 0;
    for record in officeart::records(container) {
        count_record(ctx, &mut seen)?;
        if record.rec_type == rt::SLIDE_SHOW_INFO_ATOM {
            return Ok(get_u16(record.body, SLIDE_FLAGS_AT).is_some_and(|f| f & SLIDE_HIDDEN != 0));
        }
    }
    Ok(false)
}

pub(super) fn container(ctx: &ParseCtx<'_>, record: Record<'_>) -> Result<bool, ConvertError> {
    match record.rec_type {
        officeart::SP_CONTAINER => shape(ctx, record.body),
        officeart::SPGR_CONTAINER => {
            let mut seen = 0;
            for child in officeart::records(record.body) {
                count_record(ctx, &mut seen)?;
                if child.rec_type == officeart::SP_CONTAINER {
                    return if group(ctx, child.body)? {
                        shape(ctx, child.body)
                    } else {
                        Ok(false)
                    };
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn group(ctx: &ParseCtx<'_>, body: &[u8]) -> Result<bool, ConvertError> {
    let mut seen = 0;
    for atom in officeart::records(body) {
        count_record(ctx, &mut seen)?;
        if atom.rec_type == officeart::FSP {
            return Ok(get_u32(atom.body, 4).is_some_and(|flags| flags & GROUP != 0));
        }
    }
    Ok(false)
}

fn shape(ctx: &ParseCtx<'_>, body: &[u8]) -> Result<bool, ConvertError> {
    let mut hidden = false;
    let mut seen = 0;
    for record in officeart::records(body) {
        count_record(ctx, &mut seen)?;
        if matches!(record.rec_type, officeart::FOPT | officeart::TERTIARY_FOPT) {
            for (id, flags) in officeart::properties(record.instance(), record.body) {
                ctx.charge(1)?;
                if id == GROUP_SHAPE_FLAGS && flags & USE_HIDDEN != 0 {
                    hidden = flags & HIDDEN != 0;
                }
            }
        }
    }
    Ok(hidden)
}

pub(super) fn outline(ctx: &ParseCtx<'_>, page: &[u8]) -> Result<BTreeSet<usize>, ConvertError> {
    let mut omitted = BTreeSet::new();
    let mut shown = BTreeSet::new();
    let mut stack = vec![(page, 0, false)];
    let mut seen = 0;
    while let Some((body, depth, inherited)) = stack.pop() {
        for record in officeart::records(body) {
            count_record(ctx, &mut seen)?;
            if record.version() == officeart::CONTAINER_VERSION {
                if listed_elsewhere(record)
                    || matches!(
                        record.rec_type,
                        rt::HEADERS_FOOTERS | rt::PROG_BINARY_TAG | rt::CRYPT_SESSION10_CONTAINER
                    )
                {
                    continue;
                }
                check_record_depth(ctx, depth)?;
                stack.push((record.body, depth + 1, inherited || container(ctx, record)?));
            } else if record.rec_type == rt::OUTLINE_TEXT_REF_ATOM
                && let Some(index) = get_u32(record.body, 0).and_then(|i| usize::try_from(i).ok())
            {
                if inherited {
                    omitted.insert(index);
                } else {
                    shown.insert(index);
                }
            }
        }
    }
    omitted.retain(|index| !shown.contains(index));
    Ok(omitted)
}
