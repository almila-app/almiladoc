//! Office Math (OMML) to LaTeX. The same element tree serves docx and pptx
//! directly, and rtf through its math destinations, which mirror OMML
//! element for element: there, an attribute arrives as the element's text
//! (`{\mtype lin}`) and a run's text sits directly in the run.

use super::tex::{Fraction, Tex, accent, delimiter, function_name, symbol};
use super::{DROPPED, GUESSED};
use crate::package::xml::{Element, Node, ns};

/// One `m:oMath` as LaTeX source without delimiters.
pub(crate) fn omath_to_tex(omath: &Element) -> String {
    children_tex(omath, Mode::Math).finish()
}

/// The equations of an `m:oMathPara`, one per `m:oMath` line.
pub(crate) fn omath_para_to_tex(para: &Element) -> Vec<String> {
    let lines: Vec<String> =
        para.find_all(ns::M, "oMath").map(omath_to_tex).filter(|t| !t.is_empty()).collect();
    if lines.is_empty() {
        let whole = omath_to_tex(para);
        return if whole.is_empty() { Vec::new() } else { vec![whole] };
    }
    lines
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Math,
    /// Inside `m:fName`: run text names a function (`\sin`, `\lim`).
    FuncName,
    /// Inside an equation-array row: `&` marks an alignment point.
    Array,
}

fn walk_children(elem: &Element, tex: &mut Tex, mode: Mode) {
    for child in elem.child_elems() {
        walk_elem(child, tex, mode);
    }
}

fn walk_elem(e: &Element, tex: &mut Tex, mode: Mode) {
    if e.ns.as_deref() != Some(ns::M) {
        // Revision and bookmark markup from the host document.
        match e.local.as_str() {
            "del" | "moveFrom" | "rPr" | "pPr" => {}
            _ => walk_children(e, tex, mode),
        }
        return;
    }
    match e.local.as_str() {
        "r" => run(e, tex, mode),
        "t" => tex.push_math_text(&e.text()),
        "f" => fraction(e, tex),
        "sSup" | "sSub" | "sSubSup" | "sPre" => scripts(e, tex),
        "rad" => {
            let deg = arg(e, "deg", Mode::Math);
            tex.push_macro("\\sqrt");
            if flag(e, "radPr", "degHide") != Some(true) && !deg.is_empty() {
                tex.push_char('[');
                tex.push_tex(&deg);
                tex.push_char(']');
            }
            tex.push_group(&arg(e, "e", Mode::Math));
        }
        "d" => delimited(e, tex),
        "nary" => nary(e, tex),
        "func" => {
            if let Some(name) = e.find(ns::M, "fName") {
                walk_children(name, tex, Mode::FuncName);
            }
            tex.push_group(&arg(e, "e", Mode::Math));
        }
        "acc" => {
            let chr = marked_prop(e, "accPr", "chr")
                .and_then(|v| prop_char(&v, tex))
                .unwrap_or('\u{302}');
            tex.push_command(accent(chr).unwrap_or("\\hat"), &arg(e, "e", Mode::Math));
        }
        "bar" => {
            let top = prop(e, "barPr", "pos").is_some_and(|p| p == "top");
            let cmd = if top { "\\overline" } else { "\\underline" };
            tex.push_command(cmd, &arg(e, "e", Mode::Math));
        }
        "borderBox" => tex.push_command("\\boxed", &arg(e, "e", Mode::Math)),
        "phant" if flag(e, "phantPr", "show") == Some(false) => {
            tex.push_command("\\phantom", &arg(e, "e", Mode::Math));
        }
        "box" | "phant" => tex.push_group(&arg(e, "e", mode)),
        "groupChr" => group_char(e, tex),
        "limLow" | "limUpp" => limit(e, tex, mode),
        "m" => matrix(e, tex),
        "eqArr" => equation_array(e, tex),
        local if local.ends_with("Pr") => {}
        _ => walk_children(e, tex, mode),
    }
}

fn children_tex(elem: &Element, mode: Mode) -> Tex {
    let mut tex = Tex::default();
    walk_children(elem, &mut tex, mode);
    tex
}

/// Child argument `name` of `parent`, converted on its own.
fn arg(parent: &Element, name: &str, mode: Mode) -> Tex {
    parent.find(ns::M, name).map_or_else(Tex::default, |child| children_tex(child, mode))
}

fn scripts(e: &Element, tex: &mut Tex) {
    let part = |name: &str| arg(e, name, Mode::Math);
    match e.local.as_str() {
        "sSup" => {
            tex.push_base(&part("e"));
            tex.push_script('^', &part("sup"));
        }
        "sSub" => {
            tex.push_base(&part("e"));
            tex.push_script('_', &part("sub"));
        }
        "sSubSup" => {
            tex.push_base(&part("e"));
            tex.push_script('_', &part("sub"));
            tex.push_script('^', &part("sup"));
        }
        _ => {
            tex.push_str("{}");
            tex.push_script('_', &part("sub"));
            tex.push_script('^', &part("sup"));
            tex.push_group(&part("e"));
        }
    }
}

/// A property value without reader markers, so a marked conversion takes
/// the decisions an unmarked one does.
fn prop(parent: &Element, pr: &str, name: &str) -> Option<String> {
    let value = marked_prop(parent, pr, name)?;
    Some(value.chars().filter(|&c| !is_marker(c)).collect::<String>().trim().to_string())
}

/// A property value: `parent/pr/name/@m:val`, or the element's own text
/// where the tree came from rtf, with any reader markers in it.
fn marked_prop(parent: &Element, pr: &str, name: &str) -> Option<String> {
    parent.find(ns::M, pr)?.find(ns::M, name).map(val_or_text)
}

fn val_or_text(elem: &Element) -> String {
    elem.attr(ns::M, "val").map_or_else(|| direct_text(elem).trim().to_string(), str::to_string)
}

/// A character property's character: the first of `value` after any reader
/// markers, which go to `tex` so the output keeps the marks on what it shows.
fn prop_char(value: &str, tex: &mut Tex) -> Option<char> {
    for c in value.chars() {
        if !is_marker(c) {
            return Some(c);
        }
        tex.push_char(c);
    }
    None
}

fn is_marker(c: char) -> bool {
    c == GUESSED || c == DROPPED
}

/// An on/off property: absent is `None`; present with no value is on.
fn flag(parent: &Element, pr: &str, name: &str) -> Option<bool> {
    prop(parent, pr, name).map(|value| is_on(&value))
}

fn is_on(value: &str) -> bool {
    !matches!(value, "0" | "off" | "false")
}

fn direct_text(elem: &Element) -> String {
    elem.children
        .iter()
        .filter_map(|node| match node {
            Node::Text(t) => Some(t.as_str()),
            Node::Elem(_) => None,
        })
        .collect()
}

/// A run's text: its `m:t` children, or its own text where the tree came
/// from rtf.
fn run_text(r: &Element) -> String {
    let mut texts = r.find_all(ns::M, "t").peekable();
    if texts.peek().is_some() {
        return texts.map(Element::text).collect();
    }
    let direct = direct_text(r);
    if direct.trim().is_empty() { String::new() } else { direct }
}

fn run(r: &Element, tex: &mut Tex, mode: Mode) {
    let text = run_text(r);
    if text.is_empty() {
        return;
    }
    // Run properties sit under `m:rPr`; rtf writes them on the run itself,
    // with `sty` and `scr` as numbers in their enumeration order.
    let value = |name: &str| {
        r.find(ns::M, "rPr")
            .and_then(|p| p.find(ns::M, name))
            .or(r.find(ns::M, name))
            .map(val_or_text)
    };
    if value("nor").is_some_and(|v| is_on(&v)) {
        tex.push_text_mode(&text);
        return;
    }
    if mode == Mode::FuncName
        && let Some(name) = function_name(&text)
    {
        tex.push_macro(&name);
        return;
    }
    let font = match value("scr").as_deref() {
        Some("script" | "1") => Some("\\mathcal"),
        Some("fraktur" | "2") => Some("\\mathfrak"),
        Some("double-struck" | "3") => Some("\\mathbb"),
        Some("sans-serif" | "4") => Some("\\mathsf"),
        Some("monospace" | "5") => Some("\\mathtt"),
        _ => match value("sty").as_deref() {
            Some("p" | "0") => Some("\\mathrm"),
            Some("b" | "1") => Some("\\mathbf"),
            Some("bi" | "3") => Some("\\boldsymbol"),
            _ => None,
        },
    };
    let mut inner = Tex::default();
    match mode {
        Mode::Array => {
            for (i, piece) in text.split('&').enumerate() {
                if i > 0 {
                    inner.push_str(" & ");
                }
                inner.push_math_text(piece);
            }
        }
        _ => inner.push_math_text(&text),
    }
    match font {
        Some(cmd) => tex.push_command(cmd, &inner),
        None => tex.push_tex(&inner),
    }
}

fn fraction(e: &Element, tex: &mut Tex) {
    let kind = match prop(e, "fPr", "type").as_deref() {
        Some("lin") => Fraction::Linear,
        Some("skw") => Fraction::Skewed,
        Some("noBar") => Fraction::NoBar,
        _ => Fraction::Bar,
    };
    tex.push_fraction(kind, &arg(e, "num", Mode::Math), &arg(e, "den", Mode::Math));
}

fn delimited(e: &Element, tex: &mut Tex) {
    // An explicitly empty character means no delimiter on that side.
    let chr = |name: &str, default: char, tex: &mut Tex| -> Option<char> {
        match marked_prop(e, "dPr", name) {
            Some(v) => prop_char(&v, tex),
            None => Some(default),
        }
    };
    let beg = chr("begChr", '(', tex);
    let end = chr("endChr", ')', tex);
    tex.push_macro("\\left");
    tex.push_str(beg.map_or(".", delimiter));
    for (i, part) in e.find_all(ns::M, "e").enumerate() {
        if i > 0 {
            if let Some(sep) = chr("sepChr", '|', tex) {
                tex.push_macro("\\middle");
                tex.push_str(delimiter(sep));
            } else {
                tex.push_char(',');
            }
        }
        tex.push_tex(&children_tex(part, Mode::Math));
    }
    tex.push_macro("\\right");
    tex.push_str(end.map_or(".", delimiter));
}

fn nary(e: &Element, tex: &mut Tex) {
    let chr = marked_prop(e, "naryPr", "chr").and_then(|v| prop_char(&v, tex)).unwrap_or('∫');
    match symbol(chr) {
        Some(op) if op.starts_with('\\') => tex.push_macro(op),
        _ => {
            tex.push_macro("\\operatorname*");
            let mut inner = Tex::default();
            inner.push_math_char(chr);
            tex.push_group(&inner);
        }
    }
    let integral = matches!(chr, '∫' | '∬' | '∭' | '⨌' | '∮' | '∯' | '∰');
    match prop(e, "naryPr", "limLoc").as_deref() {
        Some("undOvr") if integral => tex.push_macro("\\limits"),
        Some("subSup") if !integral => tex.push_macro("\\nolimits"),
        _ => {}
    }
    if flag(e, "naryPr", "subHide") != Some(true) {
        let sub = arg(e, "sub", Mode::Math);
        if !sub.is_empty() {
            tex.push_script('_', &sub);
        }
    }
    if flag(e, "naryPr", "supHide") != Some(true) {
        let sup = arg(e, "sup", Mode::Math);
        if !sup.is_empty() {
            tex.push_script('^', &sup);
        }
    }
    tex.push_group(&arg(e, "e", Mode::Math));
}

fn group_char(e: &Element, tex: &mut Tex) {
    let chr =
        marked_prop(e, "groupChrPr", "chr").and_then(|v| prop_char(&v, tex)).unwrap_or('\u{23df}');
    let top = prop(e, "groupChrPr", "pos").is_some_and(|p| p == "top");
    let body = arg(e, "e", Mode::Math);
    if let Some(cmd) = accent(chr) {
        tex.push_command(cmd, &body);
    } else {
        let mut mark = Tex::default();
        mark.push_math_char(chr);
        tex.push_macro(if top { "\\overset" } else { "\\underset" });
        tex.push_group(&mark);
        tex.push_group(&body);
    }
}

fn limit(e: &Element, tex: &mut Tex, mode: Mode) {
    let upper = e.local == "limUpp";
    let base = arg(e, "e", mode);
    let lim = arg(e, "lim", Mode::Math);
    if mode == Mode::FuncName {
        // `\lim_{x \to 0}`: the operator takes its own limits.
        tex.push_tex(&base);
        tex.push_script(if upper { '^' } else { '_' }, &lim);
        return;
    }
    tex.push_macro(if upper { "\\overset" } else { "\\underset" });
    tex.push_group(&lim);
    tex.push_group(&base);
}

fn matrix(e: &Element, tex: &mut Tex) {
    let rows: Vec<Vec<Tex>> = e
        .find_all(ns::M, "mr")
        .map(|row| row.find_all(ns::M, "e").map(|cell| children_tex(cell, Mode::Math)).collect())
        .collect();
    tex.push_environment("matrix", &rows);
}

fn equation_array(e: &Element, tex: &mut Tex) {
    let rows: Vec<Vec<Tex>> =
        e.find_all(ns::M, "e").map(|row| vec![children_tex(row, Mode::Array)]).collect();
    let aligned = rows.iter().flatten().any(|cell| cell.contains('&'));
    tex.push_environment(if aligned { "aligned" } else { "gathered" }, &rows);
}
