//! Presentation MathML to LaTeX, for EPUB chapters and OpenDocument
//! formula objects. A TeX annotation, when the producer kept one, is taken
//! verbatim over a reconversion of the presentation tree.

use super::tex::{Fraction, Tex, Variant, accent, big_operator, delimiter, function_name};
use crate::package::xml::{Element, Node};

/// A formula as LaTeX, with the image glyphs it could not carry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MathTex {
    /// LaTeX source without delimiters.
    pub tex: String,
    /// The `mglyph` images it would show, which have no character.
    pub glyphs: u64,
}

/// A `math` element as LaTeX source without delimiters, counting the image
/// glyphs it drops where they would show.
pub(crate) fn mathml_to_tex(math: &Element) -> MathTex {
    let mut glyphs = 0;
    let tex = sub_children(&mut glyphs, math);
    MathTex { tex: tex.finish(), glyphs }
}

/// Whether a `math` element asks for display (block) layout.
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

/// Child elements, skipping nothing: MathML layout schemata count their
/// arguments by position.
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
    }
    match e.local.as_str() {
        "semantics" => {
            if let Some(annotation) = tex_annotation(e) {
                tex.push_str(&annotation);
            } else if let Some(first) = presentation(e) {
                walk_elem(glyphs, first, tex);
            }
        }
        "mi" => identifier(e, tex),
        "mn" => tex.push_math_text(&e.text()),
        "mo" => operator(e, tex),
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
        }
        "mspace" => space(e, tex),
        "mfrac" => fraction(glyphs, e, tex),
        "msqrt" => tex.push_command("\\sqrt", &sub_children(glyphs, e)),
        "mroot" => {
            let parts = args(e);
            tex.push_macro("\\sqrt");
            if let Some(index) = parts.get(1) {
                tex.push_char('[');
                tex.push_tex(&sub(glyphs, index));
                tex.push_char(']');
            }
            if let Some(base) = parts.first() {
                tex.push_group(&sub(glyphs, base));
            }
        }
        "msup" | "msub" | "msubsup" => scripts(glyphs, e, tex),
        "munder" | "mover" | "munderover" => under_over(glyphs, e, tex),
        "mmultiscripts" => multiscripts(glyphs, e, tex),
        "mfenced" => fenced(glyphs, e, tex),
        "mtable" => table(glyphs, e, tex),
        "mphantom" => tex.push_command("\\phantom", &sub_children(&mut 0, e)),
        "menclose" => enclosure(glyphs, e, tex),
        "mglyph" => *glyphs = glyphs.saturating_add(1),
        "annotation" | "annotation-xml" | "maligngroup" | "malignmark" | "none" | "mprescripts" => {
        }
        _ => walk_children(glyphs, e, tex),
    }
}

fn space(e: &Element, tex: &mut Tex) {
    // Only the sign of the width survives: zero (the default) is
    // nothing, negative a thin backspace, anything else a space.
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
    let separators: Vec<char> =
        e.attr_any("separators").unwrap_or(",").chars().filter(|c| !c.is_whitespace()).collect();
    tex.push_macro("\\left");
    tex.push_str(open.chars().next().map_or(".", delimiter));
    for (i, part) in e.child_elems().enumerate() {
        if i > 0 {
            // The last separator repeats for any further arguments.
            let sep = separators.get(i - 1).or(separators.last());
            if let Some(sep) = sep {
                tex.push_math_char(*sep);
            }
        }
        tex.push_tex(&sub(glyphs, part));
    }
    tex.push_macro("\\right");
    tex.push_str(close.chars().next().map_or(".", delimiter));
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

fn tex_annotation(semantics: &Element) -> Option<String> {
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
        .map(|a| a.text().trim().to_string())
        .filter(|t| !t.is_empty())
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
    // Limits on a big operator or a named function take script form,
    // which LaTeX places under and over by itself.
    if is_big_operator(base) || is_named_function(base) {
        tex.push_tex(&sub(glyphs, base));
        script(glyphs, tex, '_', under);
        script(glyphs, tex, '^', over);
        return;
    }
    // An explicit accent="false" / accentunder="false" asks for a plain
    // over- or under-script even on an accent character.
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

/// An `mo` holding an n-ary operator (∑, ∫, ...).
fn is_big_operator(e: &Element) -> bool {
    e.local == "mo" && {
        let text = e.text();
        let mut chars = text.trim().chars();
        matches!((chars.next(), chars.next()), (Some(c), None) if big_operator(c).is_some())
    }
}

/// An `mi` or `mo` naming a function that takes limits (`lim`, `max`).
fn is_named_function(e: &Element) -> bool {
    matches!(e.local.as_str(), "mi" | "mo") && {
        let text = e.text();
        let text = text.trim();
        text.chars().count() > 1 && function_name(text).is_some()
    }
}
