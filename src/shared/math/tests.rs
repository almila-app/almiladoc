use super::*;
use crate::package::archive::Role;
use crate::package::xml::{Element, parse_xml};

fn omml(body: &str) -> String {
    let xml = format!(
        r#"<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">{body}</m:oMath>"#
    );
    let root = parse_xml(&crate::ctx::ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
    omath_to_tex(root.child_elems().next().unwrap())
}

fn mathml(body: &str) -> String {
    let xml = format!(r#"<math xmlns="http://www.w3.org/1998/Math/MathML">{body}</math>"#);
    let root: Element =
        parse_xml(&crate::ctx::ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
    let formula = mathml_to_tex(root.child_elems().next().unwrap());
    assert_eq!(formula.glyphs, 0, "{body}");
    formula.tex
}

#[test]
fn omml_structures_map_to_latex_commands() {
    let tex = omml(concat!(
        "<m:f><m:num><m:r><m:t>a</m:t></m:r></m:num><m:den><m:r><m:t>b</m:t></m:r></m:den></m:f>",
        "<m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e><m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup>",
        "<m:rad><m:radPr><m:degHide m:val=\"1\"/></m:radPr><m:deg/><m:e><m:r><m:t>y</m:t></m:r></m:e></m:rad>",
        "<m:nary><m:naryPr><m:chr m:val=\"∑\"/><m:limLoc m:val=\"undOvr\"/></m:naryPr>",
        "<m:sub><m:r><m:t>i=1</m:t></m:r></m:sub><m:sup><m:r><m:t>n</m:t></m:r></m:sup>",
        "<m:e><m:r><m:t>i</m:t></m:r></m:e></m:nary>",
        "<m:d><m:dPr><m:begChr m:val=\"⟨\"/><m:endChr m:val=\"⟩\"/></m:dPr><m:e><m:r><m:t>α</m:t></m:r></m:e></m:d>",
        "<m:func><m:fName><m:r><m:rPr><m:sty m:val=\"p\"/></m:rPr><m:t>sin</m:t></m:r></m:fName>",
        "<m:e><m:r><m:t>θ</m:t></m:r></m:e></m:func>",
    ));
    assert_eq!(
        tex,
        "\\frac{a}{b}x^{2}\\sqrt{y}\\sum_{i=1}^{n}{i}\\left\\langle\\alpha\\right\\rangle\\sin{\\theta}"
    );
}

#[test]
fn omml_normal_text_and_styled_alphanumerics() {
    let tex = omml(concat!(
        "<m:r><m:rPr><m:nor/></m:rPr><m:t>for all </m:t></m:r>",
        "<m:r><m:t>𝐱 ∈ ℝ</m:t></m:r>",
    ));
    assert_eq!(tex, "\\text{for all }\\mathbf{x} \\in \\mathbb{R}");
}

#[test]
fn omml_rtf_shaped_tree_reads_values_from_text() {
    // rtf math destinations carry a property value as element text, a run's
    // text directly in the run, and run properties as numbered children.
    let tex = omml(concat!(
        "<m:f><m:fPr><m:type>lin</m:type></m:fPr>",
        "<m:num><m:r>a</m:r></m:num><m:den><m:r><m:sty>0</m:sty>b</m:r></m:den></m:f>",
    ));
    assert_eq!(tex, "{a}/{\\mathrm{b}}");
}

#[test]
fn mathml_prefers_a_tex_annotation_and_drops_the_rest() {
    let tex = mathml(concat!(
        "<semantics><mrow><mi>x</mi><mo>=</mo><mn>1</mn></mrow>",
        "<annotation encoding=\"application/x-tex\">x = 1</annotation>",
        "<annotation encoding=\"StarMath 5.0\">x = 1</annotation></semantics>",
    ));
    assert_eq!(tex, "x = 1");
}

#[test]
fn mathml_presentation_tree_converts() {
    let tex = mathml(concat!(
        "<mrow><mi>f</mi><mo>⁡</mo><mfenced><mi>x</mi></mfenced><mo>=</mo>",
        "<munderover><mo>∑</mo><mrow><mi>k</mi><mo>=</mo><mn>0</mn></mrow><mi>∞</mi></munderover>",
        "<mfrac><msup><mi>x</mi><mi>k</mi></msup><mrow><mi>k</mi><mo>!</mo></mrow></mfrac>",
        "<mo>+</mo><msqrt><mi>π</mi></msqrt>",
        "<mover><mi>v</mi><mo>→</mo></mover><mi>sin</mi><mtext>for</mtext></mrow>",
    ));
    assert_eq!(
        tex,
        "f\\left(x\\right)=\\sum_{k=0}^{\\infty}\\frac{x^{k}}{k!}+\\sqrt{\\pi}\\vec{v}\\sin\\text{for}"
    );
}

#[test]
fn mathml_display_attribute_selects_block_layout() {
    let xml =
        r#"<math xmlns="http://www.w3.org/1998/Math/MathML" display="block"><mi>x</mi></math>"#;
    let root = parse_xml(&crate::ctx::ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
    assert!(mathml_is_display(root.child_elems().next().unwrap()));
}

#[test]
fn mathml_counts_the_image_glyphs_it_drops_where_they_would_show() {
    let cases: &[(&str, &str, &str, u64)] = &[
        ("a glyph", r#"<mglyph src="a.png" alt="star"/>"#, "", 1),
        ("a glyph beside text", "<mi>x</mi><mglyph/>", "x", 1),
        ("a glyph in an identifier", "<mi><mglyph/></mi>", "", 1),
        ("a glyph in a number", "<mn>1<mglyph/></mn>", "1", 1),
        ("a glyph in an operator", "<mo>+<mglyph/></mo>", "+", 1),
        ("a glyph in text", "<mtext>a<mglyph/></mtext>", "\\text{a}", 1),
        ("a glyph in a string", "<ms><mglyph/></ms>", "\\text{\"\"}", 1),
        ("glyphs in a fraction", "<mfrac><mglyph/><mrow><mglyph/></mrow></mfrac>", "\\frac{}{}", 2),
        ("a glyph in a script", "<msup><mi>x</mi><mglyph/></msup>", "x^{}", 1),
        (
            "a glyph in a cell",
            "<mtable><mtr><mtd><mglyph/></mtd></mtr></mtable>",
            "\\begin{matrix} \\end{matrix}",
            1,
        ),
        ("a glyph in an accent", "<mover><mi>v</mi><mo>→<mglyph/></mo></mover>", "\\vec{v}", 1),
        ("a glyph over a base", "<mover><mi>v</mi><mglyph/></mover>", "\\overset{}{v}", 1),
        (
            "a glyph in a big operator",
            "<munderover><mo>∑<mglyph/></mo><mi>a</mi><mi>b</mi></munderover>",
            "\\sum_{a}^{b}",
            1,
        ),
        ("a phantom glyph", "<mphantom><mglyph/></mphantom>", "\\phantom{}", 0),
        ("a phantom glyph in a token", "<mi>x<mphantom><mglyph/></mphantom></mi>", "x", 0),
        (
            "a phantom glyph in an accent",
            "<mover><mi>v</mi><mo>→<mphantom><mglyph/></mphantom></mo></mover>",
            "\\vec{v}",
            0,
        ),
        (
            "a glyph in an annotation",
            "<semantics><mi>x</mi><annotation-xml><mglyph/></annotation-xml></semantics>",
            "x",
            0,
        ),
        (
            "a glyph under a TeX annotation",
            r#"<semantics><mglyph/><annotation encoding="application/x-tex">y</annotation></semantics>"#,
            "y",
            0,
        ),
        (
            "a glyph in a token under a TeX annotation",
            r#"<mi><semantics><mglyph/><annotation encoding="application/x-tex">y</annotation></semantics></mi>"#,
            "y",
            0,
        ),
        (
            "a glyph in the presentation of a token",
            r#"<mi><semantics><mglyph/><annotation encoding="StarMath 5.0">y</annotation></semantics></mi>"#,
            "y",
            1,
        ),
        (
            "a glyph after the presentation of a token",
            "<mi><semantics><mglyph/><mglyph/></semantics></mi>",
            "",
            1,
        ),
        (
            "a glyph in an annotation in a token",
            "<mi>x<annotation><mglyph/></annotation></mi>",
            "x",
            0,
        ),
        (
            "a glyph in an annotation-xml in a token",
            "<mi>x<annotation-xml><mglyph/></annotation-xml></mi>",
            "x",
            0,
        ),
        ("a glyph in a stray annotation", "<annotation-xml><mglyph/></annotation-xml>", "", 0),
        (
            "a glyph after a leading annotation-xml",
            "<semantics><annotation-xml><mglyph/></annotation-xml><mglyph/></semantics>",
            "",
            1,
        ),
        (
            "a glyph in the presentation",
            r#"<semantics><mrow><mglyph/></mrow><annotation-xml><mglyph/></annotation-xml></semantics>"#,
            "",
            1,
        ),
    ];
    for &(name, body, tex, glyphs) in cases {
        let xml = format!(r#"<math xmlns="http://www.w3.org/1998/Math/MathML">{body}</math>"#);
        let root =
            parse_xml(&crate::ctx::ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
        let formula = mathml_to_tex(root.child_elems().next().unwrap());
        assert_eq!(formula, MathTex { tex: tex.to_string(), glyphs }, "{name}");
    }
}
