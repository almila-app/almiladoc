from pathlib import Path

from fixturegen.common import DOT_PNG, Part, write_zip

CONTAINER = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>\
</rootfiles>
</container>"""


def write_epub(path: Path, *entries: Part) -> None:
    write_zip(
        path,
        [("META-INF/container.xml", CONTAINER), *entries],
        mimetype_first="application/epub+zip",
    )


def features_epub(out: Path) -> None:
    """Rowspan, ol attributes, CSS display:none and non-heading anchors."""
    css = "p.hidden { display: none; }\n.crossed { text-decoration: line-through; }\n"
    ch1 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title>
<link rel="stylesheet" type="text/css" href="style.css"/></head><body>
<h1>Feature Chapter</h1>
<p style="font-weight: bold">Inline-styled bold paragraph.</p>
<p class="hidden">This hidden paragraph must not appear.</p>
<p><span class="crossed">Struck via class.</span></p>
<table>
<tr><td rowspan="2">Tall</td><td>B1</td></tr>
<tr><td>B2</td></tr>
<tr><td>A3</td><td>B3</td></tr>
</table>
<ol reversed="reversed" start="3"><li>three</li><li>two</li><li>one</li></ol>
<ol type="a"><li>alpha one</li><li value="5">jumps to five</li><li>six</li></ol>
<ol start="-1"><li>minus one</li><li>zero</li><li>one</li></ol>
<ol reversed="reversed" start="1"><li>one</li><li>zero</li><li>minus one</li></ol>
<p>See <a href="ch2.xhtml#target-span">the marked span</a> in chapter two,
or <a href="https://example.com/x">an external page</a>,
or <a href="notes.txt">a relative resource</a>.</p>
</body></html>"""
    ch2 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Two</title></head><body>
<h1>Second Chapter</h1>
<p>Leading text. <span id="target-span">This span is the anchor target.</span></p>
<p>Back to <a href="ch1.xhtml">chapter one's start</a>.</p>
</body></html>"""
    opf = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="uid">urn:uuid:00000000-0000-0000-0000-00000000f1x7</dc:identifier>
<dc:title>Feature Book</dc:title><dc:language>en</dc:language>
</metadata>
<manifest>
<item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
<item id="css" href="style.css" media-type="text/css"/>
</manifest>
<spine><itemref idref="c1"/><itemref idref="c2"/></spine>
</package>"""
    write_epub(
        out / "epub" / "handmade-features.epub",
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch1),
        ("OEBPS/ch2.xhtml", ch2),
        ("OEBPS/style.css", css),
    )


def math_epub(out: Path) -> None:
    ch = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Math</title></head><body>
<h1>Formulas</h1>
<p>Euler: <math xmlns="http://www.w3.org/1998/Math/MathML"><semantics>
<mrow><msup><mi>e</mi><mrow><mi>i</mi><mi>π</mi></mrow></msup><mo>+</mo><mn>1</mn><mo>=</mo><mn>0</mn></mrow>
<annotation encoding="application/x-tex">e^{i\\pi} + 1 = 0</annotation>
</semantics></math> holds.</p>
<math xmlns="http://www.w3.org/1998/Math/MathML" display="block">
<mrow><munderover><mo>∑</mo><mrow><mi>k</mi><mo>=</mo><mn>0</mn></mrow><mi>∞</mi></munderover>
<mfrac><msup><mi>x</mi><mi>k</mi></msup><mrow><mi>k</mi><mo>!</mo></mrow></mfrac>
<mo>=</mo><mi>exp</mi><mo>⁡</mo><mfenced><mi>x</mi></mfenced></mrow>
</math>
<p>Vectors: <math xmlns="http://www.w3.org/1998/Math/MathML"><mover><mi>v</mi><mo>→</mo></mover><mo>·</mo><mover><mi>w</mi><mo>→</mo></mover></math>.</p>
</body></html>"""
    opf = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="uid">urn:uuid:00000000-0000-0000-0000-0000000math0</dc:identifier>
<dc:title>Math Book</dc:title><dc:language>en</dc:language>
</metadata>
<manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml" properties="mathml"/>\
</manifest>
<spine><itemref idref="c1"/></spine>
</package>"""
    write_epub(
        out / "epub" / "handmade-math.epub",
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch),
    )


def position_epub(out: Path) -> None:
    """Superscript and subscript set directly by tags and an inline style, through classes,
    and cancelled by an inline baseline over a raising class; DOCX and ODF build the same
    text. The package names no title, which would render as a heading of its own."""
    css = ".raised { vertical-align: super }\n.lowered { vertical-align: sub }\n"
    ch = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Position</title>
<link rel="stylesheet" type="text/css" href="style.css"/></head><body>
<p>Direct: 10<sup>-3</sup> M and H<span style="vertical-align: sub">2</span>O.</p>
<p>Through a style: x<span class="raised">2</span> and CO<span class="lowered">2</span>.</p>
<p>Baseline over a superscript style: <span class="raised" \
style="vertical-align: baseline">level</span>.</p>
</body></html>"""
    opf = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="uid">urn:uuid:00000000-0000-0000-0000-00000000p051</dc:identifier>
<dc:language>en</dc:language>
</metadata>
<manifest>
<item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="css" href="style.css" media-type="text/css"/>
</manifest>
<spine><itemref idref="c1"/></spine>
</package>"""
    write_epub(
        out / "epub" / "handmade-position.epub",
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch),
        ("OEBPS/style.css", css),
    )


def css_links_epub(out: Path) -> None:
    """Per-chapter CSS cascades, at-rules and spine-only anchor classification."""
    css_a = ".mark { font-weight: bold; }\n"
    css_b = ".mark { font-style: italic; font-weight: normal; }\n"
    ch1 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title>
<link rel="stylesheet" type="text/css" href="style-a.css"/></head><body>
<h1>Sheet A Chapter</h1>
<p><span class="mark">Bold via sheet A.</span></p>
<p>See <a href="pic.png">the picture</a>,
<a href="data.bin">the download</a>,
<a href="appendix.xhtml">the non-linear appendix</a>,
or <a href="ch2.xhtml">chapter two</a>.</p>
</body></html>"""
    ch2 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Two</title>
<link rel="stylesheet" type="text/css" href="style-b.css"/>
<style>p.local { text-decoration: line-through; }</style></head><body>
<h1>Sheet B Chapter</h1>
<p><span class="mark">Italic via sheet B only.</span></p>
<p class="local">Struck via the chapter style block.</p>
</body></html>"""
    ch3 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Three</title>
<link rel="stylesheet" type="text/css" href="style-a.css"/>
<link rel="stylesheet" type="text/css" href="style-b.css"/></head><body>
<h1>Cascade Chapter</h1>
<p><span class="mark">Sheet B wins the conflict.</span></p>
</body></html>"""
    ch4 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Four</title>
<style>
span.secret { display: none; }
.secret { display: block; }
span { display: none; }
.visible { display: block; }
p.strong { font-weight: bold; }
.strong { font-weight: normal; }
.gone { display: none; }
</style></head><body>
<h1>Specificity Chapter</h1>
<p><span class="secret">Hidden: tag.class outweighs the restoring class.</span>after the span</p>
<p><span class="visible">Restored: the class rule outweighs the hiding tag rule.</span></p>
<p class="strong">Bold: tag.class beats the later class rule.</p>
<p class="gone">Never shown.</p>
</body></html>"""
    ch5 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Five</title>
<link rel="stylesheet" type="text/css" href="style-c.css"/>
<link rel="stylesheet" type="text/css" href="style-print.css" media="print"/>
<style media="print">p.styled { display: none; }</style></head><body>
<h1>At-rule Chapter</h1>
<p class="gone">Hidden: a rule after the statement at-rules.</p>
<p class="printed">Shown: its rule sits in a print group.</p>
<p class="onscreen">Hidden: its rule sits in a screen group.</p>
<p class="wide">Shown: its query holds on some screens only.</p>
<p class="grid">Shown: its condition is not settled.</p>
<p class="late">Hidden: a rule after a font-face.</p>
<p class="linked">Shown: its sheet is for print.</p>
<p class="styled">Shown: its style block is for print.</p>
</body></html>"""
    css_c = """@charset "utf-8";
@namespace epub "http://www.idpf.org/2007/ops";
@import url(unused.css);
.gone { display: none; }
@media print { h1 { font-weight: bold; } .printed { display: none; } }
@media screen { .onscreen { display: none; } }
@media (min-width: 40em) { .wide { display: none; } }
@supports (display: grid) { .grid { display: none; } }
@font-face { font-family: f; src: url(f.woff); }
.late { display: none; }
"""
    css_print = ".linked { display: none; }\n"
    appendix = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Appendix</title></head><body>
<p>Non-linear content, not converted.</p>
</body></html>"""
    opf = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="uid">urn:uuid:00000000-0000-0000-0000-00000000c55l</dc:identifier>
<dc:title>Cascade Book</dc:title><dc:language>en</dc:language>
</metadata>
<manifest>
<item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
<item id="c3" href="ch3.xhtml" media-type="application/xhtml+xml"/>
<item id="c4" href="ch4.xhtml" media-type="application/xhtml+xml"/>
<item id="app" href="appendix.xhtml" media-type="application/xhtml+xml"/>
<item id="c5" href="ch5.xhtml" media-type="application/xhtml+xml"/>
<item id="cssa" href="style-a.css" media-type="text/css"/>
<item id="cssb" href="style-b.css" media-type="text/css"/>
<item id="cssc" href="style-c.css" media-type="text/css"/>
<item id="cssp" href="style-print.css" media-type="text/css"/>
<item id="pic" href="pic.png" media-type="image/png"/>
<item id="bin" href="data.bin" media-type="application/octet-stream"/>
</manifest>
<spine><itemref idref="c1"/><itemref idref="c2"/><itemref idref="c3"/><itemref idref="c4"/>
<itemref idref="app" linear="no"/><itemref idref="c5"/></spine>
</package>"""
    write_epub(
        out / "epub" / "handmade-css-links.epub",
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch1),
        ("OEBPS/ch2.xhtml", ch2),
        ("OEBPS/ch3.xhtml", ch3),
        ("OEBPS/ch4.xhtml", ch4),
        ("OEBPS/ch5.xhtml", ch5),
        ("OEBPS/appendix.xhtml", appendix),
        ("OEBPS/style-a.css", css_a),
        ("OEBPS/style-b.css", css_b),
        ("OEBPS/style-c.css", css_c),
        ("OEBPS/style-print.css", css_print),
        ("OEBPS/pic.png", DOT_PNG),
        ("OEBPS/data.bin", b"NOT-A-DOCUMENT"),
    )


def generate(out: Path) -> None:
    css_links_epub(out)
    features_epub(out)
    math_epub(out)
    position_epub(out)
