use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::package::xml::Element;
use std::cell::Cell;

#[derive(Default)]
pub(crate) struct DamageScope(Cell<bool>);

impl DamageScope {
    pub(crate) fn report(&self, ctx: &ParseCtx<'_>, region: Region, damaged: bool) {
        if damaged && !self.0.replace(true) {
            ctx.emit(DiagnosticCode::UnresolvedEntity, region);
        }
    }

    pub(crate) fn reset(&self) {
        self.0.set(false);
    }
}

pub(crate) fn attribute_unresolved(element: &Element, namespace: &str, name: &str) -> bool {
    element
        .attrs
        .iter()
        .find(|attr| attr.local == name && attr.ns.as_deref() == Some(namespace))
        .or_else(|| element.attrs.iter().find(|attr| attr.local == name && attr.ns.is_none()))
        .is_some_and(|attr| attr.unresolved)
}

pub(crate) fn attribute_damage<'a>(
    element: &'a Element,
    namespace: &str,
    name: &str,
) -> &'a [std::ops::Range<usize>] {
    element
        .attrs
        .iter()
        .find(|attr| attr.local == name && attr.ns.as_deref() == Some(namespace))
        .or_else(|| element.attrs.iter().find(|attr| attr.local == name && attr.ns.is_none()))
        .and_then(|attr| attr.value_damage.as_deref())
        .map_or(&[], Vec::as_slice)
}
