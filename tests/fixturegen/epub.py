from fixturegen.common import DOT_PNG, write_zip


# ---------------------------------------------------------------------------
# Handmade EPUB: rowspan, ol attrs, CSS display:none, non-heading anchors

def features_epub(out):
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
    container = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""
    write_zip(out / "epub" / "handmade-features.epub", [
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch1),
        ("OEBPS/ch2.xhtml", ch2),
        ("OEBPS/style.css", css),
    ], mimetype_first="application/epub+zip")


def math_epub(out):
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
<manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml" properties="mathml"/></manifest>
<spine><itemref idref="c1"/></spine>
</package>"""
    container = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""
    write_zip(out / "epub" / "handmade-math.epub", [
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch),
    ], mimetype_first="application/epub+zip")


# ---------------------------------------------------------------------------
# Per-chapter CSS cascades and spine-only anchor classification

def css_links_epub(out):
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
<item id="cssa" href="style-a.css" media-type="text/css"/>
<item id="cssb" href="style-b.css" media-type="text/css"/>
<item id="pic" href="pic.png" media-type="image/png"/>
<item id="bin" href="data.bin" media-type="application/octet-stream"/>
</manifest>
<spine><itemref idref="c1"/><itemref idref="c2"/><itemref idref="c3"/><itemref idref="c4"/>
<itemref idref="app" linear="no"/></spine>
</package>"""
    container = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""
    write_zip(out / "epub" / "handmade-css-links.epub", [
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf),
        ("OEBPS/ch1.xhtml", ch1),
        ("OEBPS/ch2.xhtml", ch2),
        ("OEBPS/ch3.xhtml", ch3),
        ("OEBPS/ch4.xhtml", ch4),
        ("OEBPS/appendix.xhtml", appendix),
        ("OEBPS/style-a.css", css_a),
        ("OEBPS/style-b.css", css_b),
        ("OEBPS/pic.png", DOT_PNG),
        ("OEBPS/data.bin", b"NOT-A-DOCUMENT"),
    ], mimetype_first="application/epub+zip")


def generate(out):
    css_links_epub(out)
    features_epub(out)
    math_epub(out)
