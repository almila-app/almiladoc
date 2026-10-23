//! What a cell's typed value costs when it is shown without the number style that formats it:
//! the style the fallback reproduces, and the ones that may hide the value or format it otherwise.

use crate::diagnostics::DiagnosticCode;
use crate::package::xml::{Element, ns};

/// The loss of showing a value without its number style, from none to the worst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FormatCost {
    None,
    /// The same number shows, as in General.
    Fallback,
    /// The style may show less or more than the value.
    Unread,
}

impl FormatCost {
    pub(super) const fn code(self) -> Option<DiagnosticCode> {
        match self {
            Self::None => None,
            Self::Fallback => Some(DiagnosticCode::NumberFormatFallback),
            Self::Unread => Some(DiagnosticCode::NumberFormatUnread),
        }
    }
}

/// What the fallback shows of a cell, by its `office:value-type`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Shown {
    /// A `float`, as a plain number.
    Float,
    /// A `percentage`, times 100 and a percent sign.
    Percentage,
    /// A `string`.
    Text,
    /// A currency, date, time or boolean, which no data style shows as the fallback does.
    Other,
}

impl Shown {
    pub(super) fn of(value_type: Option<&str>) -> Self {
        match value_type {
            Some("float") => Self::Float,
            Some("percentage") => Self::Percentage,
            Some("string") => Self::Text,
            _ => Self::Other,
        }
    }
}

/// The cost of a data style under each kind of value the fallback shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Costs {
    float: FormatCost,
    percentage: FormatCost,
    text: FormatCost,
}

impl Costs {
    pub(super) fn of(style: &Element) -> Self {
        let plain = |kind, sign| {
            if style.is(ns::NUMBER, kind) && reproduced(style, sign) {
                FormatCost::Fallback
            } else {
                FormatCost::Unread
            }
        };
        let text = if style.is(ns::NUMBER, "text-style") && !shows_content_only(style) {
            FormatCost::Unread
        } else {
            FormatCost::None
        };
        Self {
            float: plain("number-style", false),
            percentage: plain("percentage-style", true),
            text,
        }
    }

    pub(super) fn under(self, shown: Shown) -> FormatCost {
        match shown {
            Shown::Float => self.float,
            Shown::Percentage => self.percentage,
            Shown::Text => self.text,
            Shown::Other => FormatCost::Unread,
        }
    }
}

/// A style that is one plain number, and with `sign` a percent sign after it: the fallback shows
/// the same number for a float under a number style and a percentage under a percentage style.
fn reproduced(style: &Element, sign: bool) -> bool {
    let mut parts = style.child_elems();
    parts.next().is_some_and(is_plain_number)
        && (!sign || parts.next().is_some_and(is_percent_sign))
        && parts.next().is_none()
}

fn is_plain_number(part: &Element) -> bool {
    part.is(ns::NUMBER, "number")
        && part.child_elems().next().is_none()
        && part.attrs.iter().all(|attr| {
            attr.ns.as_deref() == Some(ns::NUMBER)
                && matches!(
                    attr.local.as_str(),
                    "decimal-places" | "min-integer-digits" | "grouping"
                )
        })
}

fn is_percent_sign(part: &Element) -> bool {
    part.is(ns::NUMBER, "text") && part.text() == "%"
}

fn shows_content_only(style: &Element) -> bool {
    let mut parts = style.child_elems();
    parts.next().is_some_and(|part| part.is(ns::NUMBER, "text-content")) && parts.next().is_none()
}
