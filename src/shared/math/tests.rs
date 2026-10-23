use super::*;
use crate::package::archive::Role;
use crate::package::xml::{Element, parse_xml};

fn root(xml: &str) -> Element {
    parse_xml(&crate::ctx::ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap()
}

fn omml(body: &str) -> String {
    let xml = format!(
        r#"<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">{body}</m:oMath>"#
    );
    omath_to_tex(root(&xml).child_elems().next().unwrap())
}

fn convert(body: &str) -> MathTex {
    let xml = format!(r#"<math xmlns="http://www.w3.org/1998/Math/MathML">{body}</math>"#);
    mathml_to_tex(root(&xml).child_elems().next().unwrap())
}

fn mathml(body: &str) -> String {
    let formula = convert(body);
    assert_eq!(formula.glyphs, 0, "{body}");
    formula.tex
}

fn check(to_tex: fn(&str) -> String, cases: &[(&str, &str, &str)]) {
    for &(name, body, tex) in cases {
        assert_eq!(to_tex(body), tex, "{name}");
    }
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
    assert!(mathml_is_display(root(xml).child_elems().next().unwrap()));
}

const GLYPH_CASES: &[(&str, &str, &str, u64)] = &[
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
    ("a glyph in an annotation in a token", "<mi>x<annotation><mglyph/></annotation></mi>", "x", 0),
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
        "<semantics><mrow><mglyph/></mrow><annotation-xml><mglyph/></annotation-xml></semantics>",
        "",
        1,
    ),
    (
        "glyphs in several cells and rows",
        "<mtable><mtr><mtd><mglyph/></mtd><mtd><mglyph/></mtd></mtr><mtr><mtd><mglyph/></mtd></mtr></mtable>",
        "\\begin{matrix} & \\\\ \\end{matrix}",
        3,
    ),
];

#[test]
fn mathml_counts_the_image_glyphs_it_drops_where_they_would_show() {
    for &(name, body, tex, glyphs) in GLYPH_CASES {
        assert_eq!(convert(body), MathTex { tex: tex.to_string(), glyphs }, "{name}");
    }
}

const OMML_FORMS: &[(&str, &str, &str)] = &[
    ("a subscript", "<m:sSub><m:e><m:r>x</m:r></m:e><m:sub><m:r>i</m:r></m:sub></m:sSub>", "x_{i}"),
    (
        "a subscript and superscript",
        "<m:sSubSup><m:e><m:r>x</m:r></m:e><m:sub><m:r>i</m:r></m:sub><m:sup><m:r>2</m:r></m:sup></m:sSubSup>",
        "x_{i}^{2}",
    ),
    (
        "a prescript",
        "<m:sPre><m:sub><m:r>i</m:r></m:sub><m:sup><m:r>2</m:r></m:sup><m:e><m:r>x</m:r></m:e></m:sPre>",
        "{}_{i}^{2}{x}",
    ),
    (
        "a linear fraction",
        r#"<m:f><m:fPr><m:type m:val="lin"/></m:fPr><m:num><m:r>a</m:r></m:num><m:den><m:r>b</m:r></m:den></m:f>"#,
        "{a}/{b}",
    ),
    (
        "a skewed fraction",
        r#"<m:f><m:fPr><m:type m:val="skw"/></m:fPr><m:num><m:r>a</m:r></m:num><m:den><m:r>b</m:r></m:den></m:f>"#,
        "{}^{a}/_{b}",
    ),
    (
        "a fraction without a bar",
        r#"<m:f><m:fPr><m:type m:val="noBar"/></m:fPr><m:num><m:r>a</m:r></m:num><m:den><m:r>b</m:r></m:den></m:f>"#,
        "{a\\atop b}",
    ),
    (
        "a fraction of an unknown type",
        r#"<m:f><m:fPr><m:type m:val="x"/></m:fPr><m:num><m:r>a</m:r></m:num><m:den><m:r>b</m:r></m:den></m:f>"#,
        "\\frac{a}{b}",
    ),
    (
        "a matrix",
        "<m:m><m:mr><m:e><m:r>a</m:r></m:e><m:e><m:r>b</m:r></m:e></m:mr><m:mr><m:e><m:r>c</m:r></m:e><m:e><m:r>d</m:r></m:e></m:mr></m:m>",
        "\\begin{matrix} a & b \\\\ c & d \\end{matrix}",
    ),
    (
        "an equation array without alignment points",
        "<m:eqArr><m:e><m:r>x</m:r></m:e><m:e><m:r>y</m:r></m:e></m:eqArr>",
        "\\begin{gathered} x \\\\ y \\end{gathered}",
    ),
    (
        "an equation array with an alignment point",
        "<m:eqArr><m:e><m:r>x&amp;y</m:r></m:e><m:e><m:r>z</m:r></m:e></m:eqArr>",
        "\\begin{aligned} x & y \\\\ z \\end{aligned}",
    ),
    (
        "an n-ary operator with its upper limit hidden",
        r#"<m:nary><m:naryPr><m:chr m:val="∑"/><m:supHide m:val="1"/></m:naryPr><m:sub><m:r>i</m:r></m:sub><m:sup><m:r>n</m:r></m:sup><m:e><m:r>x</m:r></m:e></m:nary>"#,
        "\\sum_{i}{x}",
    ),
    (
        "an n-ary operator with no lower limit",
        r#"<m:nary><m:naryPr><m:chr m:val="∑"/></m:naryPr><m:sub/><m:sup><m:r>n</m:r></m:sup><m:e><m:r>x</m:r></m:e></m:nary>"#,
        "\\sum^{n}{x}",
    ),
    (
        "a lower limit on a function name",
        "<m:func><m:fName><m:limLow><m:e><m:r>lim</m:r></m:e><m:lim><m:r>x</m:r></m:lim></m:limLow></m:fName><m:e><m:r>f</m:r></m:e></m:func>",
        "\\lim_{x}{f}",
    ),
    (
        "an upper limit on a function name",
        "<m:func><m:fName><m:limUpp><m:e><m:r>lim</m:r></m:e><m:lim><m:r>x</m:r></m:lim></m:limUpp></m:fName><m:e><m:r>f</m:r></m:e></m:func>",
        "\\lim^{x}{f}",
    ),
];

#[test]
fn omml_scripts_fractions_and_environments_have_one_form_each() {
    check(omml, OMML_FORMS);
}

const OMML_VALUES: &[(&str, &str, &str)] = &[
    ("normal text off", r#"<m:r><m:rPr><m:nor m:val="off"/></m:rPr>a</m:r>"#, "a"),
    ("normal text false", r#"<m:r><m:rPr><m:nor m:val="false"/></m:rPr>a</m:r>"#, "a"),
    ("normal text zero", r#"<m:r><m:rPr><m:nor m:val="0"/></m:rPr>a</m:r>"#, "a"),
    ("normal text one", r#"<m:r><m:rPr><m:nor m:val="1"/></m:rPr>a</m:r>"#, "\\text{a}"),
    ("normal text without a value", "<m:r><m:rPr><m:nor/></m:rPr>a</m:r>", "\\text{a}"),
    ("normal text off as element text", "<m:r><m:nor>off</m:nor>a</m:r>", "a"),
    ("normal text on as element text", "<m:r><m:nor>on</m:nor>a</m:r>", "\\text{a}"),
    ("element text is trimmed", "<m:r><m:nor> off </m:nor>a</m:r>", "a"),
    ("one variant makes one run", "<m:r>𝐱𝐲</m:r>", "\\mathbf{xy}"),
    ("variants side by side", "<m:r>𝐱ℝ</m:r>", "\\mathbf{x}\\mathbb{R}"),
    ("an italic letter ends a run", "<m:r>𝐱𝑦𝐳</m:r>", "\\mathbf{x}y\\mathbf{z}"),
    ("several text children", "<m:r><m:t>a</m:t><m:t>b</m:t></m:r>", "ab"),
    ("direct text kept untrimmed", "<m:r>x</m:r><m:r> y </m:r><m:r>z</m:r>", "x y z"),
    ("blank direct text is nothing", "<m:r>x</m:r><m:r>   </m:r><m:r>y</m:r>", "xy"),
    (
        "a degree hidden off",
        r#"<m:rad><m:radPr><m:degHide m:val="off"/></m:radPr><m:deg><m:r>3</m:r></m:deg><m:e><m:r>x</m:r></m:e></m:rad>"#,
        "\\sqrt[3]{x}",
    ),
    (
        "a degree hidden on",
        r#"<m:rad><m:radPr><m:degHide m:val="on"/></m:radPr><m:deg><m:r>3</m:r></m:deg><m:e><m:r>x</m:r></m:e></m:rad>"#,
        "\\sqrt{x}",
    ),
];

#[test]
fn omml_flags_and_run_text_follow_their_values() {
    check(omml, OMML_VALUES);
}

const MATHML_FORMS: &[(&str, &str, &str)] = &[
    ("a subscript", "<msub><mi>x</mi><mi>i</mi></msub>", "x_{i}"),
    (
        "a subscript and superscript",
        "<msubsup><mi>x</mi><mi>i</mi><mn>2</mn></msubsup>",
        "x_{i}^{2}",
    ),
    (
        "prescripts and postscripts",
        "<mmultiscripts><mi>X</mi><mi>a</mi><mi>b</mi><mprescripts/><mi>c</mi><none/></mmultiscripts>",
        "{}_{c}{X}_{a}^{b}",
    ),
    ("a bevelled fraction", r#"<mfrac bevelled="true"><mi>a</mi><mi>b</mi></mfrac>"#, "{a}/{b}"),
    (
        "a fraction with no line",
        r#"<mfrac linethickness="0"><mi>a</mi><mi>b</mi></mfrac>"#,
        "{a\\atop b}",
    ),
    (
        "a fraction with no line in pixels",
        r#"<mfrac linethickness=" 0px "><mi>a</mi><mi>b</mi></mfrac>"#,
        "{a\\atop b}",
    ),
    (
        "a fraction with a line",
        r#"<mfrac linethickness="2"><mi>a</mi><mi>b</mi></mfrac>"#,
        "\\frac{a}{b}",
    ),
    (
        "bevelled beats no line",
        r#"<mfrac bevelled="true" linethickness="0"><mi>a</mi><mi>b</mi></mfrac>"#,
        "{a}/{b}",
    ),
    (
        "not bevelled with no line",
        r#"<mfrac bevelled="false" linethickness="0"><mi>a</mi><mi>b</mi></mfrac>"#,
        "{a\\atop b}",
    ),
    ("a fraction with one part", "<mfrac><mi>a</mi></mfrac>", "a"),
    (
        "a table with a labelled row",
        "<mtable><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr><mlabeledtr><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mlabeledtr></mtable>",
        "\\begin{matrix} a & b \\\\ c & d \\end{matrix}",
    ),
    ("an empty table", "<mtable/>", "\\begin{matrix} \\end{matrix}"),
];

#[test]
fn mathml_scripts_fractions_and_tables_have_one_form_each() {
    check(mathml, MATHML_FORMS);
}

#[test]
fn mathml_space_emits_nothing_at_zero_and_a_backspace_when_negative() {
    const CASES: &[(&str, &str, &str)] = &[
        ("a unit-less width", r#"width="1""#, "a\\ b"),
        ("a width with a unit", r#"width="1em""#, "a\\ b"),
        ("a fractional width", r#"width=".5em""#, "a\\ b"),
        ("a width with a leading zero", r#"width="0.5em""#, "a\\ b"),
        ("a malformed width", r#"width="1.2.3em""#, "a\\ b"),
        ("a named width", r#"width="thickmathspace""#, "a\\ b"),
        ("a negative width", r#"width="-0.5em""#, "a\\!b"),
        ("a named negative width", r#"width="negativethinmathspace""#, "a\\!b"),
        ("a zero width", r#"width="0""#, "ab"),
        ("a zero width with a unit", r#"width="0.0em""#, "ab"),
        ("a negative zero width", r#"width="-0""#, "ab"),
        ("no width", "", "ab"),
    ];
    for &(name, attrs, tex) in CASES {
        assert_eq!(mathml(&format!("<mi>a</mi><mspace {attrs}/><mi>b</mi>")), tex, "{name}");
    }
}

#[test]
fn mathml_fences_and_enclosures_follow_their_attributes() {
    const FENCES: &[(&str, &str, &str)] = &[
        (
            "one separator repeated",
            r#"open="[" close="]" separators=";""#,
            "\\left[a;b;c;d\\right]",
        ),
        ("the last separator repeated", r#"separators=", ;""#, "\\left(a,b;c;d\\right)"),
        ("no separators", r#"separators="""#, "\\left(abcd\\right)"),
        ("empty delimiters", r#"open="" close="""#, "\\left.a,b,c,d\\right."),
        (
            "letter-named delimiters",
            r#"open="⟨" close="⟩""#,
            "\\left\\langle a,b,c,d\\right\\rangle",
        ),
    ];
    for &(name, attrs, tex) in FENCES {
        let body = format!("<mfenced {attrs}><mi>a</mi><mi>b</mi><mi>c</mi><mi>d</mi></mfenced>");
        assert_eq!(mathml(&body), tex, "{name}");
    }
    const ENCLOSURES: &[(&str, &str, &str)] = &[
        ("a box", r#"notation="box""#, "\\boxed{x}"),
        ("a top line", r#"notation="top right""#, "\\overline{x}"),
        ("an underline", r#"notation="underline""#, "\\underline{x}"),
        ("a strike", r#"notation="updiagonalstrike""#, "\\cancel{x}"),
        ("the default", "", "{x}"),
    ];
    for &(name, attrs, tex) in ENCLOSURES {
        assert_eq!(mathml(&format!("<menclose {attrs}><mi>x</mi></menclose>")), tex, "{name}");
    }
}

#[test]
fn mathml_limits_on_big_operators_and_functions_take_script_form() {
    const BIG: &[(char, &str)] = &[
        ('∑', "\\sum"),
        ('∏', "\\prod"),
        ('∐', "\\coprod"),
        ('∫', "\\int"),
        ('∬', "\\iint"),
        ('∭', "\\iiint"),
        ('⨌', "\\iiiint"),
        ('∮', "\\oint"),
        ('∯', "\\oiint"),
        ('∰', "\\oiiint"),
        ('⋃', "\\bigcup"),
        ('⋂', "\\bigcap"),
        ('⋁', "\\bigvee"),
        ('⋀', "\\bigwedge"),
        ('⨁', "\\bigoplus"),
        ('⨂', "\\bigotimes"),
        ('⨀', "\\bigodot"),
        ('⨄', "\\biguplus"),
        ('⨆', "\\bigsqcup"),
    ];
    for &(c, name) in BIG {
        assert_eq!(
            mathml(&format!("<munder><mo>{c}</mo><mi>i</mi></munder>")),
            format!("{name}_{{i}}")
        );
        assert_eq!(
            mathml(&format!("<mover><mo> {c} </mo><mi>n</mi></mover>")),
            format!("{name}^{{n}}")
        );
    }
    const OTHERS: &[(&str, &str, &str)] = &[
        ("a binary operator", "<munder><mo>∪</mo><mi>i</mi></munder>", "\\underset{i}{\\cup}"),
        (
            "two operator characters",
            "<munder><mo>∑∑</mo><mi>i</mi></munder>",
            "\\underset{i}{\\sum\\sum}",
        ),
        ("an identifier", "<munder><mi>∑</mi><mi>i</mi></munder>", "\\underset{i}{\\sum}"),
        ("a named operator", "<munder><mo>lim</mo><mi>i</mi></munder>", "\\lim_{i}"),
        ("a one-letter operator", "<mo>d</mo>", "d"),
        ("a predefined function operator", "<mo>sin</mo>", "\\sin"),
        (
            "a named identifier",
            "<munderover><mi>max</mi><mi>i</mi><mi>j</mi></munderover>",
            "\\max_{i}^{j}",
        ),
    ];
    check(mathml, OTHERS);
}
