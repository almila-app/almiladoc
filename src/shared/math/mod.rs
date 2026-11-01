mod mathml;
mod omml;
mod tex;

#[cfg(test)]
pub(crate) use omml::omath_to_tex;

pub(crate) use mathml::{MathTex, mathml_is_display, mathml_to_tex};
pub(crate) use omml::{omath_para_to_tex, omath_para_with_damage, omath_with_damage};

use crate::model::Inline;

pub(crate) const GUESSED: char = '\u{fdd0}';

pub(crate) const DROPPED: char = '\u{fdd1}';

pub(crate) fn math_lines(inlines: &[Inline]) -> Option<Vec<String>> {
    let lines: Vec<String> = inlines
        .iter()
        .filter_map(|i| match i {
            Inline::Math(tex) => Some(Some(tex.clone())),
            Inline::LineBreak => None,
            Inline::Text { text, .. } if text.trim().is_empty() => None,
            _ => Some(None),
        })
        .collect::<Option<Vec<String>>>()?;
    (!lines.is_empty()).then_some(lines)
}

#[cfg(test)]
mod tests;
