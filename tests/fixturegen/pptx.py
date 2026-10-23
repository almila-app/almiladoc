from collections.abc import Iterable
from pathlib import Path

from fixturegen.common import (
    DECL,
    DOT_PNG,
    M_NS,
    OFFICE_CT,
    OMML_QUADRATIC,
    OMML_SUM,
    Part,
    Rel,
    content_types,
    rels,
    write_zip,
)

PPTX_NS = (
    'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
    'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
)

MC_NS = 'xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"'
A14_NS = 'xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"'

STRICT_PPTX_NS = (
    'xmlns:a="http://purl.oclc.org/ooxml/drawingml/main" '
    'xmlns:p="http://purl.oclc.org/ooxml/presentationml/main" '
    'xmlns:r="http://purl.oclc.org/ooxml/officeDocument/relationships"'
)
STRICT_REL = "http://purl.oclc.org/ooxml/officeDocument/relationships/"

ROOT_RELS = rels(("rId1", "officeDocument", "ppt/presentation.xml"))


def presentation(slides: int) -> str:
    ids = "".join(f'<p:sldId id="{255 + n}" r:id="rId{n}"/>' for n in range(1, slides + 1))
    return f"{DECL}\n<p:presentation {PPTX_NS}>\n<p:sldIdLst>{ids}</p:sldIdLst>\n</p:presentation>"


def slide(body: str, *namespaces: str) -> str:
    """A slide whose shape tree holds `body` after the root group, under PPTX_NS and
    `namespaces`."""
    declared = " ".join((PPTX_NS, *namespaces))
    return f"""{DECL}
<p:sld {declared}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
{body}
</p:spTree></p:cSld></p:sld>"""


def slide1_rels(*rows: Rel) -> Part:
    return ("ppt/slides/_rels/slide1.xml.rels", rels(*rows))


def pptx_types(
    slides: int = 1,
    *overrides: tuple[str, str],
    defaults: Iterable[tuple[str, str]] = (),
) -> str:
    return content_types(
        ("/ppt/presentation.xml", "presentationml.presentation.main"),
        *((f"/ppt/slides/slide{n}.xml", "presentationml.slide") for n in range(1, slides + 1)),
        *overrides,
        defaults=defaults,
    )


def write_pptx(path: Path, *parts: Part, slides: int = 1, ct: str | None = None) -> None:
    slide_rows = [(f"rId{n}", "slide", f"slides/slide{n}.xml") for n in range(1, slides + 1)]
    write_zip(
        path,
        [
            ("[Content_Types].xml", ct or pptx_types(slides)),
            ("_rels/.rels", ROOT_RELS),
            ("ppt/presentation.xml", presentation(slides)),
            ("ppt/_rels/presentation.xml.rels", rels(*slide_rows)),
            *parts,
        ],
    )


def inherit_pptx(out: Path) -> None:
    """Placeholder inheritance through layout and master."""
    # Master: body text is bold with a character bullet at level 1.
    master = f"""{DECL}
<p:sldMaster {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
</p:spTree></p:cSld>
<p:txStyles>
<p:titleStyle><a:lvl1pPr><a:buNone/><a:defRPr/></a:lvl1pPr></p:titleStyle>
<p:bodyStyle><a:lvl1pPr><a:buChar char="•"/><a:defRPr b="1"/></a:lvl1pPr>
<a:lvl2pPr><a:buChar char="-"/><a:defRPr i="1"/></a:lvl2pPr></p:bodyStyle>
<p:otherStyle/>
</p:txStyles>
</p:sldMaster>"""
    # Layout: overrides the body placeholder's level-1 bullet to roman
    # numbering starting at 3.
    layout = f"""{DECL}
<p:sldLayout {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>Layout title text</a:t></a:r></a:p>\
</p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>
<a:lstStyle><a:lvl1pPr><a:buAutoNum type="romanLcPeriod" startAt="3"/></a:lvl1pPr></a:lstStyle>
<a:p><a:endParaRPr/></a:p></p:txBody></p:sp>
</p:spTree></p:cSld>
</p:sldLayout>"""
    slide_xml = f"""{DECL}
<p:sld {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="ctrTitle"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Inherited Title Slide</a:t></a:r></a:p></p:txBody>\
</p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Content"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>
<a:p><a:r><a:t>Roman three bold via master</a:t></a:r></a:p>
<a:p><a:r><a:t>Roman four </a:t></a:r><a:r><a:rPr b="0"/><a:t>with bold turned off</a:t></a:r>\
</a:p>
<a:p><a:pPr lvl="1"/><a:r><a:t>Dash bullet italic via master level two</a:t></a:r></a:p>
<a:p><a:pPr><a:buNone/></a:pPr><a:r><a:t>Plain closing line</a:t></a:r></a:p>
</p:txBody></p:sp>
</p:spTree></p:cSld>
</p:sld>"""
    write_pptx(
        out / "pptx" / "handmade-inherit.pptx",
        ("ppt/slides/slide1.xml", slide_xml),
        slide1_rels(("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")),
        ("ppt/slideLayouts/slideLayout1.xml", layout),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            rels(("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")),
        ),
        ("ppt/slideMasters/slideMaster1.xml", master),
        ct=pptx_types(
            1,
            ("/ppt/slideLayouts/slideLayout1.xml", "presentationml.slideLayout"),
            ("/ppt/slideMasters/slideMaster1.xml", "presentationml.slideMaster"),
        ),
    )


def order_pptx(out: Path) -> None:
    """Titles keep their shape-order position; p:oleObj payloads are extracted."""
    body = """<p:sp><p:nvSpPr><p:cNvPr id="2" name="Lead"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Kicker before the title</a:t></a:r></a:p></p:txBody>\
</p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Title placed second</a:t></a:r></a:p></p:txBody>\
</p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="4" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Body after the title</a:t></a:r></a:p></p:txBody>\
</p:sp>
<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="5" name="Sheet"/><p:cNvGraphicFramePr/><p:nvPr/>\
</p:nvGraphicFramePr>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/presentationml/2006/ole">
<p:oleObj name="Quarterly numbers" progId="Excel.Sheet.12" r:id="rId2"/>
</a:graphicData></a:graphic></p:graphicFrame>"""
    write_pptx(
        out / "pptx" / "handmade-order.pptx",
        ("ppt/slides/slide1.xml", slide(body)),
        slide1_rels(("rId2", "oleObject", "../embeddings/oleObject1.bin")),
        ("ppt/embeddings/oleObject1.bin", b"OLE-PAYLOAD-STAND-IN" * 4),
        ct=pptx_types(defaults=[("bin", OFFICE_CT + "oleObject")]),
    )


def strict_alt(out: Path) -> None:
    sld_ids = '<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>'

    # Strict PPTX: Strict namespaces plus Strict relationship types down the
    # presentation -> slide chain.
    strict_slide = (
        f"{DECL}<p:sld {STRICT_PPTX_NS}>"
        "<p:cSld><p:spTree>"
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/>'
        '<p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>'
        "<p:spPr/><p:txBody><a:bodyPr/>"
        "<a:p><a:r><a:t>Strict slide title</a:t></a:r></a:p></p:txBody></p:sp>"
        '<p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/>'
        '<p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>'
        "<p:spPr/><p:txBody><a:bodyPr/>"
        "<a:p><a:r><a:t>Strict body text</a:t></a:r></a:p></p:txBody></p:sp>"
        "</p:spTree></p:cSld></p:sld>"
    )
    write_zip(
        out / "pptx" / "handmade-strict.pptx",
        [
            ("[Content_Types].xml", pptx_types()),
            ("_rels/.rels", rels(("rId1", STRICT_REL + "officeDocument", "ppt/presentation.xml"))),
            (
                "ppt/presentation.xml",
                f"{DECL}<p:presentation {STRICT_PPTX_NS}>{sld_ids}</p:presentation>",
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                rels(("rId1", STRICT_REL + "slide", "slides/slide1.xml")),
            ),
            ("ppt/slides/slide1.xml", strict_slide),
        ],
    )

    # Alternate-location PPTX: presentation and slide at unconventional
    # paths, reachable only through relationships.
    alt_slide = (
        f"{DECL}<p:sld {PPTX_NS}>"
        "<p:cSld><p:spTree>"
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/>'
        '<p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>'
        "<p:spPr/><p:txBody><a:bodyPr/>"
        "<a:p><a:r><a:t>Relocated deck title</a:t></a:r></a:p></p:txBody></p:sp>"
        "</p:spTree></p:cSld></p:sld>"
    )
    write_zip(
        out / "pptx" / "handmade-altpath.pptx",
        [
            (
                "[Content_Types].xml",
                content_types(
                    ("/deck/pres.xml", "presentationml.presentation.main"),
                    ("/deck/s/one.xml", "presentationml.slide"),
                ),
            ),
            ("_rels/.rels", rels(("rId1", "officeDocument", "deck/pres.xml"))),
            ("deck/pres.xml", f"{DECL}<p:presentation {PPTX_NS}>{sld_ids}</p:presentation>"),
            ("deck/_rels/pres.xml.rels", rels(("rId1", "slide", "s/one.xml"))),
            ("deck/s/one.xml", alt_slide),
        ],
    )


def math_pptx(out: Path) -> None:
    """PowerPoint wraps an equation shape in an a14 AlternateContent whose
    fallback is a picture of the equation."""
    body = f"""\
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Quadratic formula</a:t></a:r></a:p></p:txBody></p:sp>
<mc:AlternateContent><mc:Choice Requires="a14">
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Equation"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>
<a:p><a:r><a:t>Solve with </a:t></a:r><a14:m><m:oMathPara>{OMML_QUADRATIC}</m:oMathPara></a14:m>\
</a:p>
<a:p><a14:m><m:oMathPara>{OMML_SUM}</m:oMathPara></a14:m></a:p>
</p:txBody></p:sp>
</mc:Choice><mc:Fallback>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Equation"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr><a:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></a:blipFill>\
</p:spPr>
<p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>
</mc:Fallback></mc:AlternateContent>"""
    write_pptx(
        out / "pptx" / "handmade-math.pptx",
        ("ppt/slides/slide1.xml", slide(body, MC_NS, A14_NS, M_NS)),
        slide1_rels(("rId2", "image", "../media/image1.png")),
        ("ppt/media/image1.png", DOT_PNG),
        ct=pptx_types(defaults=[("png", "image/png")]),
    )


def links_pptx(out: Path) -> None:
    """Links to another slide resolve to the target slide's start anchor instead of a dead
    relative reference to slides/slideN.xml."""
    slide1 = slide(
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        "<p:spPr/><p:txBody><a:bodyPr/>"
        '<a:p><a:r><a:rPr><a:hlinkClick r:id="rId5"/></a:rPr>'
        "<a:t>Jump to the second slide</a:t></a:r></a:p>"
        '<a:p><a:r><a:rPr><a:hlinkClick r:id="rId6"/></a:rPr><a:t>External link</a:t></a:r></a:p>'
        "</p:txBody></p:sp>"
    )
    slide2 = slide(
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        "<p:spPr/><p:txBody><a:bodyPr/>"
        "<a:p><a:r><a:t>Second slide content</a:t></a:r></a:p>"
        "</p:txBody></p:sp>"
    )
    write_pptx(
        out / "pptx" / "handmade-links.pptx",
        ("ppt/slides/slide1.xml", slide1),
        slide1_rels(
            ("rId5", "slide", "slide2.xml"),
            ("rId6", "hyperlink", "https://example.com/", True),
        ),
        ("ppt/slides/slide2.xml", slide2),
        slides=2,
    )


def scripts_pptx(out: Path) -> None:
    """Superscripts, subscripts and symbol-font characters, as ppt.py writes the same deck."""

    def run(text: str, props: str = "") -> str:
        return f"<a:r>{props}<a:t>{text}</a:t></a:r>"

    def face(latin: str, sym: str | None = None) -> str:
        sym = f'<a:sym typeface="{sym}"/>' if sym else ""
        return f'<a:rPr><a:latin typeface="{latin}"/>{sym}</a:rPr>'

    paragraphs = [
        [run("Area: 5 m"), run("2", '<a:rPr baseline="30000"/>')],
        [run("H"), run("2", '<a:rPr baseline="-25000"/>'), run("O")],
        [run("Size: 10 "), run("m", face("Symbol")), run("m")],
        [run("Gap: 3 "), run("&#xF06D;", face("Arial", "Symbol")), run("s")],
        [run("Checked "), run("&#xFC;&#xFF;", face("Wingdings")), run(" end")],
    ]
    body = "".join(f"<a:p>{''.join(runs)}</a:p>" for runs in paragraphs)
    slide_xml = f"""{DECL}
<p:sld {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr>\
</p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p>{run("Scripts and symbols")}</a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>{body}</p:txBody></p:sp>
</p:spTree></p:cSld>
</p:sld>"""
    write_pptx(out / "pptx" / "handmade-scripts.pptx", ("ppt/slides/slide1.xml", slide_xml))


def abuse(out: Path) -> None:
    """A DrawingML table cell with an astronomically large span area."""
    body = """\
<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="2" name="T"/><p:cNvGraphicFramePr/><p:nvPr/>\
</p:nvGraphicFramePr>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
<a:tbl><a:tr>
<a:tc gridSpan="2000000000" rowSpan="2000000000"><a:txBody><a:bodyPr/><a:p><a:r><a:t>x</a:t></a:r>\
</a:p></a:txBody></a:tc>
</a:tr></a:tbl>
</a:graphicData></a:graphic></p:graphicFrame>"""
    write_pptx(
        out / "abuse" / "max_expansion--hugespan--errors.pptx",
        ("ppt/slides/slide1.xml", slide(body)),
    )


def generate(out: Path) -> None:
    inherit_pptx(out)
    strict_alt(out)
    order_pptx(out)
    links_pptx(out)
    math_pptx(out)
    scripts_pptx(out)
    abuse(out)
