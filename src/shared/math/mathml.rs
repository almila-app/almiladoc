//! A producer-supplied TeX annotation takes precedence over presentation-tree conversion.

use super::tex::{Fraction, Tex, Variant, accent, big_operator, delimiter, function_name};
use crate::package::xml::{Element, Node};
use crate::shared::text::trim_tex;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MathTex {
    pub tex: String,
    /// The `mglyph` images it would show, which have no character.
    pub glyphs: u64,
    pub unresolved: bool,
}

/// A `math` element as LaTeX source without delimiters, counting the image
/// glyphs it drops where they would show.
pub(crate) fn mathml_to_tex(math: &Element) -> MathTex {
    let mut glyphs = 0;
    let tex = sub_children(&mut glyphs, math);
    let (tex, unresolved) = tex.finish_with_damage();
    MathTex { tex, glyphs, unresolved }
}

pub(crate) fn mathml_is_display(math: &Element) -> bool {
    math.attr_any("display").is_some_and(|d| d == "block")
}

fn walk_children(glyphs: &mut u64, elem: &Element, tex: &mut Tex) {
    for node in &elem.children {
        match node {
            Node::Elem(child) => walk_elem(glyphs, child, tex),
            Node::Text(_) => {}
        }
    }
}

fn args(elem: &Element) -> Vec<&Element> {
    elem.child_elems().collect()
}

fn presentation(semantics: &Element) -> Option<&Element> {
    semantics.child_elems().find(|c| !matches!(c.local.as_str(), "annotation" | "annotation-xml"))
}

fn glyphs_in(e: &Element) -> u64 {
    match e.local.as_str() {
        "mglyph" => 1,
        "mphantom" | "annotation" | "annotation-xml" => 0,
        "semantics" if tex_annotation(e).is_some() => 0,
        "semantics" => presentation(e).map_or(0, glyphs_in),
        _ => e.child_elems().map(glyphs_in).fold(0, u64::saturating_add),
    }
}

fn sub(glyphs: &mut u64, elem: &Element) -> Tex {
    let mut tex = Tex::default();
    walk_elem(glyphs, elem, &mut tex);
    tex
}

fn sub_children(glyphs: &mut u64, elem: &Element) -> Tex {
    let mut tex = Tex::default();
    walk_children(glyphs, elem, &mut tex);
    tex
}

fn walk_elem(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    if matches!(e.local.as_str(), "mi" | "mn" | "mo" | "mtext" | "ms") {
        *glyphs = glyphs.saturating_add(glyphs_in(e));
        tex.push_tex(&token(e));
        return;
    }
    match e.local.as_str() {
        "semantics" => {
            if let Some(annotation) = tex_annotation(e) {
                tex.push_tex(&annotation);
            } else if let Some(first) = presentation(e) {
                walk_elem(glyphs, first, tex);
            }
        }
        "mspace" => space(e, tex),
        "mfrac" => fraction(glyphs, e, tex),
        "msqrt" => tex.push_command("\\sqrt", &sub_children(glyphs, e)),
        "mroot" => {
            let parts = args(e);
            tex.push_sqrt(parts.get(1).map(|index| sub(glyphs, index)).as_ref());
            if let Some(base) = parts.first() {
                tex.push_group(&sub(glyphs, base));
            }
        }
        "msup" | "msub" | "msubsup" => scripts(glyphs, e, tex),
        "munder" | "mover" | "munderover" => under_over(glyphs, e, tex),
        "mmultiscripts" => multiscripts(glyphs, e, tex),
        "mfenced" => fenced(glyphs, e, tex),
        "mtable" => table(glyphs, e, tex),
        "mphantom" => {
            tex.push_command("\\phantom", &sub_children(&mut 0, e).without_unresolved());
        }
        "menclose" => enclosure(glyphs, e, tex),
        "mglyph" => *glyphs = glyphs.saturating_add(1),
        "annotation" | "annotation-xml" | "maligngroup" | "malignmark" | "none" | "mprescripts" => {
        }
        _ => walk_children(glyphs, e, tex),
    }
}

fn token(e: &Element) -> Tex {
    let mut tex = Tex::default();
    match e.local.as_str() {
        "mi" => identifier(e, &mut tex),
        "mn" => tex.push_math_text(&e.text()),
        "mo" => operator(e, &mut tex),
        "mtext" => {
            let text = e.text();
            if !text.trim().is_empty() {
                tex.push_text_mode(text.trim_matches(|c: char| c == '\n' || c == '\r'));
            }
        }
        "ms" => {
            let text = e.text();
            let quoted = format!(
                "{}{}{}",
                e.attr_any("lquote").unwrap_or("\""),
                text.trim_matches(|c: char| c == '\n' || c == '\r'),
                e.attr_any("rquote").unwrap_or("\"")
            );
            tex.push_text_mode(&quoted);
            tex.mark_unresolved(
                ["lquote", "rquote"]
                    .iter()
                    .any(|name| crate::shared::xml::attribute_any_unresolved(e, name)),
            );
        }
        _ => {}
    }
    tex.mark_unresolved(!tex.is_empty() && e.unresolved_text);
    tex
}

fn space(e: &Element, tex: &mut Tex) {
    let width = e.attr_any("width").unwrap_or("0").trim();
    let negative = width.starts_with('-') || width.starts_with("negative");
    let digits = width.trim_start_matches('-');
    let end = digits.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(digits.len());
    let magnitude: f64 = digits.get(..end).unwrap_or_default().parse().unwrap_or(1.0);
    if magnitude != 0.0 {
        tex.push_str(if negative { "\\!" } else { "\\ " });
    }
}

fn fraction(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let parts = args(e);
    let (Some(num), Some(den)) = (parts.first(), parts.get(1)) else {
        return walk_children(glyphs, e, tex);
    };
    let (num, den) = (sub(glyphs, num), sub(glyphs, den));
    let kind = if e.attr_any("bevelled").is_some_and(|b| b == "true") {
        Fraction::Linear
    } else if e.attr_any("linethickness").is_some_and(|t| matches!(t.trim(), "0" | "0px")) {
        Fraction::NoBar
    } else {
        Fraction::Bar
    };
    tex.push_fraction(kind, &num, &den);
}

fn fenced(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let open = e.attr_any("open").unwrap_or("(");
    let close = e.attr_any("close").unwrap_or(")");
    let separators: Vec<(char, bool)> =
        attribute_chars(e, "separators", ",").filter(|(c, _)| !c.is_whitespace()).collect();
    tex.push_macro("\\left");
    tex.push_str(open.chars().next().map_or(".", delimiter));
    for (i, part) in e.child_elems().enumerate() {
        if i > 0 {
            let sep = separators.get(i - 1).or(separators.last());
            if let Some((sep, damaged)) = sep {
                tex.push_math_char(*sep);
                tex.mark_unresolved(*damaged);
            }
        }
        tex.push_tex(&sub(glyphs, part));
    }
    tex.push_macro("\\right");
    tex.push_str(close.chars().next().map_or(".", delimiter));
}

fn attribute_chars<'a>(
    e: &'a Element,
    name: &str,
    fallback: &'a str,
) -> impl Iterator<Item = (char, bool)> + 'a {
    let attr = e.attrs.iter().find(|attr| attr.local == name);
    let text = attr.map_or(fallback, |attr| attr.value.as_str());
    let mut damage = attr
        .and_then(|attr| attr.value_damage.as_deref())
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .peekable();
    text.char_indices().map(move |(offset, c)| {
        while damage.next_if(|span| span.end <= offset).is_some() {}
        (c, damage.peek().is_some_and(|span| span.start <= offset))
    })
}

fn enclosure(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let notation = e.attr_any("notation").unwrap_or("longdiv");
    let inner = sub_children(glyphs, e);
    match notation.split_whitespace().next() {
        Some("box" | "roundedbox") => tex.push_command("\\boxed", &inner),
        Some("top") => tex.push_command("\\overline", &inner),
        Some("bottom" | "underline") => tex.push_command("\\underline", &inner),
        Some("horizontalstrike" | "updiagonalstrike" | "downdiagonalstrike") => {
            tex.push_command("\\cancel", &inner);
        }
        _ => tex.push_group(&inner),
    }
}

fn tex_annotation(semantics: &Element) -> Option<Tex> {
    semantics
        .child_elems()
        .filter(|a| a.local == "annotation")
        .find(|a| {
            a.attr_any("encoding").is_some_and(|enc| {
                matches!(
                    enc.trim().to_ascii_lowercase().as_str(),
                    "application/x-tex" | "tex" | "latex" | "application/x-latex"
                )
            })
        })
        .and_then(|a| {
            let text = a.text();
            let text = trim_tex(&text);
            if text.is_empty() {
                return None;
            }
            let mut tex = Tex::default();
            tex.push_str(text);
            tex.mark_unresolved(a.unresolved_text);
            Some(tex)
        })
}

fn identifier(e: &Element, tex: &mut Tex) {
    let text = e.text();
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let variant = e.attr_any("mathvariant");
    let single = text.chars().count() == 1;
    if !single && variant.is_none_or(|v| v == "normal") && text.chars().all(char::is_alphabetic) {
        if let Some(name) = function_name(text) {
            tex.push_macro(&name);
        } else {
            let mut inner = Tex::default();
            inner.push_math_text(text);
            tex.push_command("\\mathrm", &inner);
        }
        return;
    }
    let mut inner = Tex::default();
    inner.push_math_text(text);
    match variant.and_then(Variant::from_mathvariant) {
        Some(v) => tex.push_command(v.command(), &inner),
        None if variant == Some("normal") && single => tex.push_command("\\mathrm", &inner),
        None => tex.push_tex(&inner),
    }
}

fn operator(e: &Element, tex: &mut Tex) {
    let text = e.text();
    let text = text.trim();
    if text.chars().count() > 1
        && let Some(name) = function_name(text)
    {
        tex.push_macro(&name);
        return;
    }
    tex.push_math_text(text);
}

fn scripts(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let parts = args(e);
    let Some(base) = parts.first() else { return };
    let base_tex = sub(glyphs, base);
    if is_big_operator(base) {
        tex.push_tex(&base_tex);
    } else {
        tex.push_base(&base_tex);
    }
    match e.local.as_str() {
        "msup" => script(glyphs, tex, '^', parts.get(1)),
        "msub" => script(glyphs, tex, '_', parts.get(1)),
        _ => {
            script(glyphs, tex, '_', parts.get(1));
            script(glyphs, tex, '^', parts.get(2));
        }
    }
}

fn script(glyphs: &mut u64, tex: &mut Tex, mark: char, arg: Option<&&Element>) {
    let Some(arg) = arg else { return };
    tex.push_script(mark, &sub(glyphs, arg));
}

fn under_over(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let parts = args(e);
    let Some(base) = parts.first() else { return };
    let (under, over) = match e.local.as_str() {
        "munder" => (parts.get(1), None),
        "mover" => (None, parts.get(1)),
        _ => (parts.get(1), parts.get(2)),
    };
    if is_big_operator(base) || is_named_function(base) {
        tex.push_tex(&sub(glyphs, base));
        script(glyphs, tex, '_', under);
        script(glyphs, tex, '^', over);
        return;
    }
    let plain_over = e.attr_any("accent").is_some_and(|v| v == "false");
    let plain_under = e.attr_any("accentunder").is_some_and(|v| v == "false");
    let mut body = sub(glyphs, base);
    if let Some(over) = over {
        body = decorate(glyphs, over, &body, Place::Over, plain_over);
    }
    if let Some(under) = under {
        body = decorate(glyphs, under, &body, Place::Under, plain_under);
    }
    tex.push_tex(&body);
}

#[derive(Clone, Copy)]
enum Place {
    Over,
    Under,
}

/// `body` with `mark` placed over or under it: an accent command where
/// the mark is an accent character (unless `plain`), `\overset` /
/// `\underset` otherwise.
fn decorate(glyphs: &mut u64, mark: &Element, body: &Tex, place: Place, plain: bool) -> Tex {
    let mut out = Tex::default();
    let text = mark.text();
    let text = text.trim();
    if !plain
        && let Some(c) = text.chars().next()
        && text.chars().count() == 1
        && let Some(cmd) = accent(c)
    {
        let cmd = match (cmd, place) {
            ("\\underline", Place::Over) => "\\overline",
            ("\\overline", Place::Under) => "\\underline",
            ("\\underbrace", Place::Over) => "\\overbrace",
            ("\\overbrace", Place::Under) => "\\underbrace",
            (cmd, _) => cmd,
        };
        *glyphs = glyphs.saturating_add(glyphs_in(mark));
        out.push_command(cmd, body);
        return out;
    }
    out.push_macro(match place {
        Place::Over => "\\overset",
        Place::Under => "\\underset",
    });
    out.push_group(&sub(glyphs, mark));
    out.push_group(body);
    out
}

fn multiscripts(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let parts = args(e);
    let Some((base, rest)) = parts.split_first() else { return };
    let split = rest.iter().position(|p| p.local == "mprescripts").unwrap_or(rest.len());
    let (post, pre) = rest.split_at(split);
    let pre = pre.get(1..).unwrap_or(&[]);
    for pair in pre.chunks(2) {
        tex.push_str("{}");
        script_pair(glyphs, tex, pair);
    }
    tex.push_group(&sub(glyphs, base));
    for pair in post.chunks(2) {
        script_pair(glyphs, tex, pair);
    }
}

fn script_pair(glyphs: &mut u64, tex: &mut Tex, pair: &[&Element]) {
    if let Some(sub_) = pair.first().filter(|p| p.local != "none") {
        tex.push_script('_', &sub(glyphs, sub_));
    }
    if let Some(sup) = pair.get(1).filter(|p| p.local != "none") {
        tex.push_script('^', &sub(glyphs, sup));
    }
}

fn table(glyphs: &mut u64, e: &Element, tex: &mut Tex) {
    let rows: Vec<Vec<Tex>> = e
        .child_elems()
        .filter(|r| matches!(r.local.as_str(), "mtr" | "mlabeledtr"))
        .map(|row| {
            row.child_elems()
                .filter(|c| c.local == "mtd")
                .map(|cell| sub_children(glyphs, cell))
                .collect()
        })
        .collect();
    tex.push_environment("matrix", &rows);
}

fn is_big_operator(e: &Element) -> bool {
    e.local == "mo" && {
        let text = e.text();
        let mut chars = text.trim().chars();
        matches!((chars.next(), chars.next()), (Some(c), None) if big_operator(c).is_some())
    }
}

fn is_named_function(e: &Element) -> bool {
    matches!(e.local.as_str(), "mi" | "mo") && {
        let text = e.text();
        let text = text.trim();
        text.chars().count() > 1 && function_name(text).is_some()
    }
}
