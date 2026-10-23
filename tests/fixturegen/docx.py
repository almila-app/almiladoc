import base64
import random
import struct
from itertools import chain
from pathlib import Path

from fixturegen.common import (
    BIN_DEFAULT,
    DECL,
    DOT_PNG,
    M_NS,
    OMML_QUADRATIC,
    OMML_SUM,
    OMML_WHERE,
    PNG_DEFAULT,
    WORD_PARTS,
    Part,
    Rel,
    content_types,
    override,
    rels,
    write_zip,
    zip_edit,
)

W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"'
R = 'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
STRICT_REL = "http://purl.oclc.org/ooxml/officeDocument/relationships/"

ROOT_RELS = rels(("rId1", "officeDocument", "word/document.xml"))
NUMBERING_OVERRIDE = override("/word/numbering.xml", "wordprocessingml.numbering")
PLAIN_STYLES = (
    "word/styles.xml",
    f"{DECL}<w:styles {W}>"
    "<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults></w:styles>",
)


def write_docx(path: Path, document: str, *parts: Part, ct_extra: str = "") -> None:
    write_zip(
        path,
        [
            ("[Content_Types].xml", content_types(*WORD_PARTS, extra=ct_extra)),
            ("_rels/.rels", ROOT_RELS),
            ("word/document.xml", document),
            *parts,
        ],
    )


def doc_rels(*rows: Rel) -> Part:
    return ("word/_rels/document.xml.rels", rels(*rows))


def para(text: str, ppr: str = "") -> str:
    return f'<w:p>{ppr}<w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>'


def image_ref(rid: str) -> str:
    return (
        "<w:p><w:r><w:drawing>"
        '<a:blip xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        f' r:embed="{rid}"/>'
        "</w:drawing></w:r></w:p>"
    )


def numpara(text: str, num_id: int, ilvl: int = 0, pstyle: str | None = None) -> str:
    ps = f'<w:pStyle w:val="{pstyle}"/>' if pstyle else ""
    ppr = (
        f'<w:pPr>{ps}<w:numPr><w:ilvl w:val="{ilvl}"/>'
        f'<w:numId w:val="{num_id}"/></w:numPr></w:pPr>'
    )
    return para(text, ppr)


def numbering_docx(out: Path) -> None:
    """Complete numbering matrix and style inheritance."""
    styles = f"""{DECL}
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="HeadBase">
  <w:name w:val="Head Base"/><w:pPr><w:outlineLvl w:val="1"/></w:pPr>
</w:style>
<w:style w:type="paragraph" w:styleId="SubHead">
  <w:name w:val="Sub Head"/><w:basedOn w:val="HeadBase"/>
  <w:rPr><w:b/></w:rPr>
</w:style>
<w:style w:type="paragraph" w:styleId="NumberedPara">
  <w:name w:val="Numbered Para"/>
  <w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>
</w:style>
<w:style w:type="paragraph" w:styleId="BoldPara">
  <w:name w:val="Bold Para"/><w:rPr><w:b/></w:rPr>
</w:style>
<w:style w:type="character" w:styleId="PlainChar">
  <w:name w:val="Plain Char"/><w:rPr><w:b w:val="0"/></w:rPr>
</w:style>
<w:style w:type="character" w:styleId="ToggleChar">
  <w:name w:val="Toggle Char"/><w:rPr><w:b/></w:rPr>
</w:style>
<w:style w:type="paragraph" w:styleId="BoldBase">
  <w:name w:val="Bold Base"/><w:rPr><w:b/></w:rPr>
</w:style>
<w:style w:type="paragraph" w:styleId="DoubleBold">
  <w:name w:val="Double Bold"/><w:basedOn w:val="BoldBase"/>
  <w:rPr><w:b w:val="true"/></w:rPr>
</w:style>
<w:style w:type="numbering" w:styleId="ListNumStyle">
  <w:name w:val="List Num Style"/>
  <w:pPr><w:numPr><w:numId w:val="4"/></w:numPr></w:pPr>
</w:style>
<w:style w:type="paragraph" w:styleId="ListLevelOne">
  <w:name w:val="List Level One"/>
  <w:pPr><w:numPr><w:ilvl w:val="5"/><w:numId w:val="7"/></w:numPr></w:pPr>
</w:style>
<w:style w:type="paragraph" w:styleId="ListLevelTwo">
  <w:name w:val="List Level Two"/>
  <w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr>
</w:style>
</w:styles>"""

    numbering = f"""{DECL}
<w:numbering {W}>
<w:abstractNum w:abstractNumId="0">
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
  <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerRoman"/><w:lvlText w:val="%2)"/>\
<w:lvlRestart w:val="0"/></w:lvl>
  <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#8226;"/>\
</w:lvl>
</w:abstractNum>
<w:abstractNum w:abstractNumId="1">
  <w:numStyleLink w:val="ListNumStyle"/>
</w:abstractNum>
<w:abstractNum w:abstractNumId="2">
  <w:styleLink w:val="ListNumStyle"/>
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperLetter"/><w:lvlText w:val="%1:"/>\
</w:lvl>
</w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="3"><w:abstractNumId w:val="0"/>
  <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
</w:num>
<w:num w:numId="4"><w:abstractNumId w:val="2"/></w:num>
<w:num w:numId="5"><w:abstractNumId w:val="1"/></w:num>
<w:num w:numId="6"><w:abstractNumId w:val="0"/>
  <w:lvlOverride w:ilvl="0">
    <w:startOverride w:val="7"/>
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperLetter"/><w:lvlText w:val="%1)"/>\
</w:lvl>
  </w:lvlOverride>
</w:num>
<w:abstractNum w:abstractNumId="3">
  <w:lvl w:ilvl="0"><w:pStyle w:val="ListLevelOne"/><w:start w:val="1"/>\
<w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
  <w:lvl w:ilvl="1"><w:pStyle w:val="ListLevelTwo"/><w:start w:val="1"/>\
<w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2."/></w:lvl>
</w:abstractNum>
<w:num w:numId="7"><w:abstractNumId w:val="3"/></w:num>
</w:numbering>"""

    body = "".join(
        [
            numpara("One-one", 1),
            numpara("One-two", 1),
            numpara("One-two-a roman", 1, 1),
            numpara("One-two-b roman", 1, 1),
            numpara("Deep bullet", 1, 2),
            numpara("One-three", 1),
            para("Interruption paragraph."),
            numpara("One-four continues the count", 1),
            numpara("Two-one continues the shared abstract", 2),
            numpara("Ten-start via override", 3),
            numpara("Letter-A via numStyleLink", 5),
            numpara("Level replaced and start overridden together", 6),
            # suppressed numbering: style brings numId 1, direct numPr zeroes it
            numpara("Suppressed numbering paragraph", 0, 0, pstyle="NumberedPara"),
            # style-provided numbering only (no direct numPr)
            para("Style-numbered paragraph", '<w:pPr><w:pStyle w:val="NumberedPara"/></w:pPr>'),
            # Style-based numbering resolves its level through the abstract
            # levels' pStyle bindings; the ilvl inside the styles' numPr (5 and
            # 0 above - both wrong on purpose) must be ignored.
            para("pStyle-bound level one", '<w:pPr><w:pStyle w:val="ListLevelOne"/></w:pPr>'),
            para("pStyle-bound level two", '<w:pPr><w:pStyle w:val="ListLevelTwo"/></w:pPr>'),
            para(
                "pStyle-bound level one again", '<w:pPr><w:pStyle w:val="ListLevelOne"/></w:pPr>'
            ),
            # heading level via basedOn-inherited outlineLvl
            para("Inherited subheading", '<w:pPr><w:pStyle w:val="SubHead"/></w:pPr>'),
            # Toggle semantics (ECMA-376 17.7.3):
            # (a) style-level false is a no-op -> stays bold
            (
                '<w:p><w:pPr><w:pStyle w:val="BoldPara"/></w:pPr>'
                '<w:r><w:t xml:space="preserve">Bold here, </w:t></w:r>'
                '<w:r><w:rPr><w:rStyle w:val="PlainChar"/></w:rPr>'
                '<w:t xml:space="preserve">and style-false keeps it bold</w:t></w:r></w:p>'
            ),
            # (b) character-style true toggles inherited bold off
            (
                '<w:p><w:pPr><w:pStyle w:val="BoldPara"/></w:pPr>'
                '<w:r><w:rPr><w:rStyle w:val="ToggleChar"/></w:rPr>'
                '<w:t xml:space="preserve">toggled off by a true in the character style</w:t>'
                "</w:r></w:p>"
            ),
            # (c) two trues along one basedOn chain cancel out
            (
                '<w:p><w:pPr><w:pStyle w:val="DoubleBold"/></w:pPr>'
                '<w:r><w:t xml:space="preserve">double toggle cancels to plain</w:t></w:r></w:p>'
            ),
            # (d) direct formatting is absolute; "off" is a valid ST_OnOff value
            (
                '<w:p><w:pPr><w:pStyle w:val="BoldPara"/></w:pPr>'
                '<w:r><w:rPr><w:b w:val="off"/></w:rPr>'
                '<w:t xml:space="preserve">direct off wins absolutely</w:t></w:r></w:p>'
            ),
            # (e) direct "on" is a valid ST_OnOff value
            (
                '<w:p><w:r><w:rPr><w:i w:val="on"/></w:rPr>'
                '<w:t xml:space="preserve">direct on makes this italic</w:t></w:r></w:p>'
            ),
        ]
    )
    document = f"{DECL}<w:document {W} {R}><w:body>{body}</w:body></w:document>"
    write_docx(
        out / "docx" / "handmade-numbering.docx",
        document,
        doc_rels(("rId1", "styles", "styles.xml"), ("rId2", "numbering", "numbering.xml")),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        ct_extra=NUMBERING_OVERRIDE,
    )


def rich_docx(out: Path) -> None:
    """A chart, a SmartArt diagram, an image and an OLE object behind relationships."""
    chart = f"""{DECL}
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<c:chart>
<c:title><c:tx><c:rich><a:p><a:r><a:t>Quarterly Widgets</a:t></a:r></a:p></c:rich></c:tx></c:title>
<c:plotArea>
<c:barChart>
<c:ser>
<c:tx><c:strRef><c:f>S</c:f><c:strCache><c:pt c:idx="0"><c:v>Widgets</c:v></c:pt></c:strCache>\
</c:strRef></c:tx>
<c:cat><c:strRef><c:f>C</c:f><c:strCache>
<c:pt c:idx="0"><c:v>Q1</c:v></c:pt><c:pt c:idx="1"><c:v>Q2</c:v></c:pt>
</c:strCache></c:strRef></c:cat>
<c:val><c:numRef><c:f>V</c:f><c:numCache>
<c:pt c:idx="0"><c:v>10</c:v></c:pt><c:pt c:idx="1"><c:v>14</c:v></c:pt>
</c:numCache></c:numRef></c:val>
</c:ser>
</c:barChart>
<c:catAx><c:title><c:tx><c:rich><a:p><a:r><a:t>Quarter</a:t></a:r></a:p></c:rich></c:tx></c:title></c:catAx>
<c:valAx><c:title><c:tx><c:rich><a:p><a:r><a:t>Units</a:t></a:r></a:p></c:rich></c:tx></c:title></c:valAx>
</c:plotArea>
</c:chart>
</c:chartSpace>"""

    diagram_data = f"""{DECL}
<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<dgm:ptLst>
<dgm:pt dgm:modelId="1" dgm:type="doc"/>
<dgm:pt dgm:modelId="2"><dgm:t><a:bodyPr/><a:p><a:r><a:t>Plan</a:t></a:r></a:p></dgm:t></dgm:pt>
<dgm:pt dgm:modelId="3"><dgm:t><a:bodyPr/><a:p><a:r><a:t>Build</a:t></a:r></a:p></dgm:t></dgm:pt>
<dgm:pt dgm:modelId="4"><dgm:t><a:bodyPr/><a:p><a:r><a:t>Ship</a:t></a:r></a:p></dgm:t></dgm:pt>
</dgm:ptLst>
</dgm:dataModel>"""

    drawing_chart = (
        '<w:drawing><wp:inline xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing">'
        '<wp:extent cx="3000000" cy="2000000"/>'
        '<wp:docPr id="1" name="Chart 1" descr="widget chart"/>'
        '<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
        '<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">'
        '<c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId10"/>'
        "</a:graphicData></a:graphic></wp:inline></w:drawing>"
    )
    drawing_diagram = (
        '<w:drawing><wp:inline xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing">'
        '<wp:extent cx="3000000" cy="2000000"/><wp:docPr id="2" name="Diagram 1"/>'
        '<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
        '<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram">'
        '<dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" '
        'r:dm="rId20" r:lo="" r:qs="" r:cs=""/>'
        "</a:graphicData></a:graphic></wp:inline></w:drawing>"
    )
    drawing_image = (
        '<w:drawing><wp:inline xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing">'
        '<wp:extent cx="100000" cy="100000"/><wp:docPr id="3" name="Dot" descr="tiny dot image"/>'
        '<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
        '<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">'
        '<pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">'
        '<pic:blipFill><a:blip r:embed="rId30"/></pic:blipFill></pic:pic>'
        "</a:graphicData></a:graphic></wp:inline></w:drawing>"
    )

    drawing_ole = (
        '<w:object xmlns:o="urn:schemas-microsoft-com:office:office" '
        'xmlns:v="urn:schemas-microsoft-com:vml">'
        '<v:shape id="ole1" style="width:100pt;height:50pt"/>'
        '<o:OLEObject Type="Embed" ProgID="Excel.Sheet.12" ShapeID="ole1" r:id="rId40"/>'
        "</w:object>"
    )

    body = (
        para("Rich objects follow.")
        + f"<w:p><w:r>{drawing_chart}</w:r></w:p>"
        + f"<w:p><w:r>{drawing_diagram}</w:r></w:p>"
        + f"<w:p><w:r>{drawing_image}</w:r></w:p>"
        + f"<w:p><w:r>{drawing_ole}</w:r></w:p>"
        + para("After the objects.")
    )
    document = f"{DECL}<w:document {W} {R}><w:body>{body}</w:body></w:document>"
    write_docx(
        out / "docx" / "handmade-rich.docx",
        document,
        doc_rels(
            ("rId1", "styles", "styles.xml"),
            ("rId10", "chart", "charts/chart1.xml"),
            ("rId20", "diagramData", "diagrams/data1.xml"),
            ("rId30", "image", "media/dot.png"),
            ("rId40", "oleObject", "embeddings/oleObject1.bin"),
        ),
        PLAIN_STYLES,
        ("word/charts/chart1.xml", chart),
        ("word/diagrams/data1.xml", diagram_data),
        ("word/media/dot.png", DOT_PNG),
        ("word/embeddings/oleObject1.bin", b"OLE-PAYLOAD-STAND-IN" * 4),
        ct_extra=PNG_DEFAULT
        + BIN_DEFAULT
        + override("/word/charts/chart1.xml", "drawingml.chart")
        + override("/word/diagrams/data1.xml", "drawingml.diagramData"),
    )


STRICT_W = 'xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main"'


def strict_alt(out: Path) -> None:
    """ISO 29500 Strict namespaces and rels-driven part discovery."""
    # Strict DOCX: every namespace and relationship type from the Strict
    # family; styles resolved through the document's typed relationship.
    strict_doc = (
        f"{DECL}<w:document {STRICT_W}><w:body>"
        '<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>'
        "<w:r><w:t>Strict heading</w:t></w:r></w:p>"
        '<w:p><w:r><w:t xml:space="preserve">Strict body with </w:t></w:r>'
        '<w:r><w:rPr><w:rStyle w:val="Bold"/></w:rPr>'
        "<w:t>toggled bold</w:t></w:r></w:p>"
        "</w:body></w:document>"
    )
    strict_styles = (
        f"{DECL}<w:styles {STRICT_W}>"
        "<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>"
        '<w:style w:type="paragraph" w:styleId="Heading1">'
        '<w:name w:val="heading 1"/></w:style>'
        '<w:style w:type="character" w:styleId="Bold">'
        '<w:name w:val="Bold"/><w:rPr><w:b/></w:rPr></w:style>'
        "</w:styles>"
    )
    write_zip(
        out / "docx" / "handmade-strict.docx",
        [
            ("[Content_Types].xml", content_types(*WORD_PARTS)),
            ("_rels/.rels", rels(("rId1", STRICT_REL + "officeDocument", "word/document.xml"))),
            ("word/document.xml", strict_doc),
            doc_rels(("rId1", STRICT_REL + "styles", "styles.xml")),
            ("word/styles.xml", strict_styles),
        ],
    )

    # Alternate-location DOCX: no part lives at its conventional path; the
    # main part, styles, and footnotes are all found through relationships.
    alt_doc = (
        f"{DECL}<w:document {W}><w:body>"
        '<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>'
        "<w:r><w:t>Relocated heading</w:t></w:r></w:p>"
        '<w:p><w:r><w:t xml:space="preserve">Body found via rels</w:t></w:r>'
        '<w:r><w:rPr><w:rStyle w:val="FootnoteRef"/></w:rPr>'
        '<w:footnoteReference w:id="2"/></w:r></w:p>'
        "</w:body></w:document>"
    )
    alt_styles = (
        f"{DECL}<w:styles {W}>"
        "<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>"
        '<w:style w:type="paragraph" w:styleId="Heading1">'
        '<w:name w:val="heading 1"/></w:style>'
        '<w:style w:type="character" w:styleId="FootnoteRef">'
        '<w:name w:val="footnote reference"/></w:style>'
        "</w:styles>"
    )
    alt_footnotes = (
        f"{DECL}<w:footnotes {W}>"
        '<w:footnote w:type="separator" w:id="0"><w:p/></w:footnote>'
        '<w:footnote w:id="2"><w:p><w:r>'
        "<w:t>A relocated footnote</w:t></w:r></w:p></w:footnote>"
        "</w:footnotes>"
    )
    alt_doc_rels = rels(
        ("rId1", "styles", "wordstyles.xml"), ("rId2", "footnotes", "n/footnotes.xml")
    )
    write_zip(
        out / "docx" / "handmade-altpath.docx",
        [
            (
                "[Content_Types].xml",
                content_types(
                    ("/content/main.xml", "wordprocessingml.document.main"),
                    ("/content/wordstyles.xml", "wordprocessingml.styles"),
                    ("/content/n/footnotes.xml", "wordprocessingml.footnotes"),
                ),
            ),
            ("_rels/.rels", rels(("rId1", "officeDocument", "content/main.xml"))),
            ("content/main.xml", alt_doc),
            ("content/_rels/main.xml.rels", alt_doc_rels),
            ("content/wordstyles.xml", alt_styles),
            ("content/n/footnotes.xml", alt_footnotes),
        ],
    )


def math_docx(out: Path) -> None:
    document = f"""{DECL}
<w:document {W} {M_NS}><w:body>
<w:p><w:r><w:t xml:space="preserve">The roots are </w:t></w:r>{OMML_QUADRATIC}<w:r>\
<w:t xml:space="preserve"> for any a.</w:t></w:r></w:p>
<w:p><m:oMathPara>{OMML_SUM}{OMML_WHERE}</m:oMathPara></w:p>
<w:p><w:r><w:t>A price of $5 or $6 is not math.</w:t></w:r></w:p>
</w:body></w:document>"""
    write_docx(out / "docx" / "handmade-math.docx", document, PLAIN_STYLES)


def tables_docx(out: Path) -> None:
    """gridBefore and gridAfter, legacy hMerge, and the ST_OnOff tblHeader."""

    def tc(text: str, extra: str = "") -> str:
        return (
            f"<w:tc><w:tcPr>{extra}</w:tcPr>"
            f'<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p></w:tc>'
        )

    document = f"""{DECL}
<w:document {W}><w:body>
<w:tbl>
<w:tr><w:trPr><w:tblHeader/></w:trPr>
{tc("Head A")}{tc("Head B")}{tc("Head C", '<w:vMerge w:val="restart"/>')}</w:tr>
<w:tr><w:trPr><w:tblHeader w:val="0"/><w:gridBefore w:val="2"/></w:trPr>
{tc("under C", "<w:vMerge/>")}</w:tr>
<w:tr><w:trPr><w:gridAfter w:val="1"/></w:trPr>
{tc("legacy start", '<w:hMerge w:val="restart"/>')}{tc("legacy cont", "<w:hMerge/>")}</w:tr>
</w:tbl>
<w:p><w:r><w:t>after the table</w:t></w:r></w:p>
</w:body></w:document>"""
    write_docx(out / "docx" / "handmade-tables.docx", document, PLAIN_STYLES)


def bare_number_docx(out: Path) -> None:
    """Bare `%1` and `%1)` decimal levels render as ordered lists."""
    numbering = f"""{DECL}
<w:numbering {W}>
<w:abstractNum w:abstractNumId="0">
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1"/></w:lvl>
</w:abstractNum>
<w:abstractNum w:abstractNumId="1">
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1)"/></w:lvl>
</w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="2"><w:abstractNumId w:val="0"/>
  <w:lvlOverride w:ilvl="0"><w:startOverride w:val="9"/></w:lvlOverride>
</w:num>
<w:num w:numId="3"><w:abstractNumId w:val="1"/></w:num>
</w:numbering>"""
    body = "".join(
        [
            numpara("First numbered paragraph.", 1),
            numpara("Second numbered paragraph.", 1),
            para("Interruption paragraph."),
            numpara("Ninth numbered paragraph.", 2),
            numpara("Tenth numbered paragraph.", 2),
            numpara("First with a parenthesis.", 3),
            numpara("Second with a parenthesis.", 3),
        ]
    )
    document = f"{DECL}<w:document {W} {R}><w:body>{body}</w:body></w:document>"
    write_docx(
        out / "docx" / "handmade-bare-number.docx",
        document,
        doc_rels(("rId1", "styles", "styles.xml"), ("rId2", "numbering", "numbering.xml")),
        PLAIN_STYLES,
        ("word/numbering.xml", numbering),
        ct_extra=NUMBERING_OVERRIDE,
    )


def nested_docx(out: Path) -> None:
    """One-cell frames around a table give way to it; a frame with text of its own, and a
    table nested in a wider grid, stay flattened."""

    def p(text: str = "") -> str:
        run = f'<w:r><w:t xml:space="preserve">{text}</w:t></w:r>' if text else ""
        return f"<w:p>{run}</w:p>"

    def tc(*content: str, span: int = 1) -> str:
        props = f'<w:gridSpan w:val="{span}"/>' if span > 1 else ""
        return f"<w:tc><w:tcPr>{props}</w:tcPr>{''.join(content)}</w:tc>"

    def tbl(*rows: list[str]) -> str:
        return "<w:tbl>" + "".join(f"<w:tr>{''.join(row)}</w:tr>" for row in rows) + "</w:tbl>"

    inner = tbl(
        [tc(p("Metric")), tc(p("Value"))],
        [tc(p("Height")), tc(p("120"))],
        [tc(p("Weight")), tc(p("34"))],
    )
    framed = tbl([tc(inner, p())])
    twice = tbl([tc(p(), tbl([tc(tbl([tc(p("deep")), tc(p("cell"))]), p())]), p())])
    spanned = tbl([tc(tbl([tc(p("left")), tc(p("right"))]), p(), span=2)])
    labelled = tbl([tc(p("Section A"), tbl([tc(p("a")), tc(p("b"))]), p("End of section"))])
    wide = tbl([tc(tbl([tc(p("a")), tc(p("b"))]), p()), tc(p("right"))])
    document = (
        f"{DECL}<w:document {W}><w:body>"
        + p("Before")
        + framed
        + p("Twice framed")
        + twice
        + p("Spanned frame")
        + spanned
        + p("Frame with text")
        + labelled
        + p("Wider grid")
        + wide
        + p("After")
        + "</w:body></w:document>"
    )
    write_docx(out / "docx" / "handmade-nested.docx", document, PLAIN_STYLES)


def manyrefs_docx(out: Path) -> None:
    """A valid document referencing one part many times must convert with a single retained
    asset; the part is cached and the asset deduplicated. (The budget re-charging itself is
    covered by unit tests - a fixture big enough to cross the 512 MiB budget would slow the
    mutation harness down.)"""
    drawing = image_ref("rId10")
    document = (
        f"{DECL}<w:document {W} {R}><w:body>"
        "<w:p><w:r><w:t>Seventy references to one image follow.</w:t></w:r></w:p>"
        + drawing * 70
        + "</w:body></w:document>"
    )
    write_docx(
        out / "docx" / "handmade-manyrefs.docx",
        document,
        doc_rels(("rId10", "image", "media/logo.png")),
        ("word/media/logo.png", b"\x89PNG\r\n\x1a\n" + b"\x00" * (64 * 1024)),
        ct_extra=PNG_DEFAULT,
    )


def ole_docx(out: Path) -> None:
    """Standard Word OLE markup - a VML preview image next to the o:OLEObject; the object's
    identity and payload must win over the preview."""
    VML = 'xmlns:v="urn:schemas-microsoft-com:vml"'
    OFFICE = 'xmlns:o="urn:schemas-microsoft-com:office:office"'
    document = f"""{DECL}
<w:document {W} {R} {VML} {OFFICE}><w:body>
<w:p><w:r><w:t>Before the object</w:t></w:r></w:p>
<w:p><w:r><w:object>
<v:shape id="_x0000_i1025" style="width:96pt;height:48pt">
<v:imagedata r:id="rId3" o:title="preview"/>
</v:shape>
<o:OLEObject Type="Embed" ProgID="Excel.Sheet.12" ShapeID="_x0000_i1025" r:id="rId2"/>
</w:object></w:r></w:p>
</w:body></w:document>"""
    write_docx(
        out / "docx" / "handmade-ole.docx",
        document,
        doc_rels(
            ("rId2", "oleObject", "embeddings/oleObject1.bin"),
            ("rId3", "image", "media/preview.png"),
        ),
        PLAIN_STYLES,
        ("word/embeddings/oleObject1.bin", b"DOCX-OLE-PAYLOAD" * 4),
        ("word/media/preview.png", b"\x89PNG\r\n\x1a\nfakepreview"),
        ct_extra=BIN_DEFAULT + PNG_DEFAULT,
    )


def ole_iso_docx(out: Path) -> None:
    """The ISO objects other producers write - a DrawingML preview then w:objectEmbed or
    w:objectLink, no VML - Word's ActiveX control, a VML preview then w:control, and a movie
    beside its drawing; each is shown by its own preview, and only an embedded payload is kept."""
    namespaces = (
        'xmlns:v="urn:schemas-microsoft-com:vml" '
        'xmlns:o="urn:schemas-microsoft-com:office:office" '
        'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
        'xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" '
        'xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"'
    )
    picture_uri = "http://schemas.openxmlformats.org/drawingml/2006/picture"
    activex_type = "application/vnd.ms-office.activeX+xml"
    activex = (
        f'{DECL}\n<ax:ocx xmlns:ax="http://schemas.microsoft.com/office/2006/activeX" '
        'ax:classid="{8BD21D40-EC42-11CE-9E0D-00AA006002F3}" ax:persistence="persistPropertyBag"/>'
    )

    def drawing(docpr: int, name: str, rid: str) -> str:
        return (
            '<w:drawing><wp:inline><wp:extent cx="1219200" cy="609600"/>'
            f'<wp:docPr id="{docpr}" name="{name}" descr="{name}"/>'
            f'<a:graphic><a:graphicData uri="{picture_uri}"><pic:pic>'
            f'<pic:nvPicPr><pic:cNvPr id="0" name="{name}"/><pic:cNvPicPr/></pic:nvPicPr>'
            f'<pic:blipFill><a:blip r:embed="{rid}"/></pic:blipFill><pic:spPr/>'
            "</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing>"
        )

    def object_paragraph(content: str) -> str:
        return f"<w:p><w:r><w:object>{content}</w:object></w:r></w:p>"

    embedded = object_paragraph(
        drawing(1, "Embedded sheet", "rId3")
        + '<w:objectEmbed r:id="rId2" w:progId="Excel.Sheet.12"/>'
    )
    linked = object_paragraph(
        drawing(2, "Linked sheet", "rId7")
        + '<w:objectLink r:id="rId6" w:progId="Excel.Sheet.12" w:updateMode="always"/>'
    )
    control = object_paragraph(
        '<v:shape id="_x0000_i1026" type="#_x0000_t201" style="width:108pt;height:18pt">'
        '<v:imagedata r:id="rId5" o:title=""/></v:shape>'
        '<w:control r:id="rId4" w:name="CheckBox1" w:shapeid="_x0000_i1026"/>'
    )
    movie = object_paragraph(drawing(3, "Movie poster", "rId8") + '<w:movie r:id="rId9"/>')
    objects = embedded + linked + control + movie
    body = para("Before the objects") + objects + para("After the objects")
    document = f"{DECL}\n<w:document {W} {R} {namespaces}><w:body>{body}</w:body></w:document>"
    write_docx(
        out / "docx" / "handmade-ole-iso.docx",
        document,
        doc_rels(
            ("rId2", "oleObject", "embeddings/oleObject1.bin"),
            ("rId3", "image", "media/embedded.png"),
            ("rId4", "control", "activeX/activeX1.xml"),
            ("rId5", "image", "media/control.png"),
            ("rId6", "oleObject", "file:///C:/Reports/sheet.xlsx", True),
            ("rId7", "image", "media/linked.png"),
            ("rId8", "image", "media/poster.png"),
            ("rId9", "video", "file:///C:/Media/clip.mp4", True),
        ),
        PLAIN_STYLES,
        ("word/embeddings/oleObject1.bin", b"DOCX-ISO-OLE-PAYLOAD" * 4),
        ("word/media/embedded.png", b"\x89PNG\r\n\x1a\nembeddedpreview"),
        ("word/media/control.png", b"\x89PNG\r\n\x1a\ncontrolpreview"),
        ("word/media/linked.png", b"\x89PNG\r\n\x1a\nlinkedpreview"),
        ("word/media/poster.png", b"\x89PNG\r\n\x1a\nmovieposter"),
        ("word/activeX/activeX1.xml", activex),
        ct_extra=BIN_DEFAULT
        + PNG_DEFAULT
        + f'<Override PartName="/word/activeX/activeX1.xml" ContentType="{activex_type}"/>',
    )


def outline_docx(out: Path) -> None:
    """Outline level is a tri-state property; direct paragraph properties override the style
    chain, and outlineLvl 9 is the explicit off state."""

    def p(text: str, ppr: str = "") -> str:
        pr = f"<w:pPr>{ppr}</w:pPr>" if ppr else ""
        return f"<w:p>{pr}<w:r><w:t>{text}</w:t></w:r></w:p>"

    nine = p(
        "Direct nine turns the style heading off",
        '<w:pStyle w:val="Outlined"/><w:outlineLvl w:val="9"/>',
    )
    document = f"""{DECL}
<w:document {W}><w:body>
{p("Style heading stays a heading", '<w:pStyle w:val="Outlined"/>')}
{p("Direct level overrides the style", '<w:pStyle w:val="Outlined"/><w:outlineLvl w:val="2"/>')}
{nine}
{p("Direct outline without a style", '<w:outlineLvl w:val="0"/>')}
{p("Child style nine stops inheritance", '<w:pStyle w:val="NotOutlined"/>')}
</w:body></w:document>"""
    styles = f"""{DECL}
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="Outlined"><w:name w:val="Outlined"/>
<w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="NotOutlined"><w:name w:val="NotOutlined"/>
<w:basedOn w:val="Outlined"/><w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:style>
</w:styles>"""
    write_docx(out / "docx" / "handmade-outline.docx", document, ("word/styles.xml", styles))


def blockstyle_docx(out: Path) -> None:
    def p(text: str, style: str, runs: str | None = None) -> str:
        body = runs or f'<w:r><w:t xml:space="preserve">{text}</w:t></w:r>'
        return f'<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>{body}</w:p>'

    # Syntax highlighting: Pandoc splits a code line into styled runs.
    highlighted = (
        '<w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">fn</w:t></w:r>'
        '<w:r><w:t xml:space="preserve"> main() {</w:t></w:r>'
    )
    println = p('    println!("ok");', "SourceCode")
    document = f"""{DECL}
<w:document {W}><w:body>
{para("Body before.")}
{p("", "SourceCode", highlighted)}
{println}
{p("}", "SourceCode")}
{para("Body between.")}
{p("A quotation.", "BlockText")}
{p("Its second paragraph.", "BlockText")}
{para("Body between.")}
{p("Word quote.", "Quote")}
{p("Word intense quote.", "IntenseQuote")}
{para("Body between.")}
{p("HTML preformatted line.", "HTMLPreformatted")}
{p("Inherits Source Code.", "SourceCodeChild")}
{para("Body after.")}
</w:body></w:document>"""
    styles = f"""{DECL}
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="SourceCode"><w:name w:val="Source Code"/></w:style>
<w:style w:type="paragraph" w:styleId="SourceCodeChild"><w:name w:val="Listing"/>
<w:basedOn w:val="SourceCode"/></w:style>
<w:style w:type="paragraph" w:styleId="BlockText"><w:name w:val="Block Text"/></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="IntenseQuote"><w:name w:val="Intense Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="HTMLPreformatted"><w:name w:val="HTML Preformatted"/>\
</w:style>
</w:styles>"""
    write_docx(out / "docx" / "handmade-blockstyle.docx", document, ("word/styles.xml", styles))


def run_marks_docx(out: Path) -> None:
    def box(font: str, code: str) -> str:
        return f'<w:sym w:font="{font}" w:char="{code}"/>'

    document = f"""{DECL}
<w:document {W}><w:body>
<w:p><w:r>{box("Wingdings 2", "F052")}<w:t>Signed by both parties</w:t></w:r></w:p>
<w:p><w:r>{box("Wingdings 2", "F0A3")}<w:t>Witnessed</w:t></w:r></w:p>
<w:p><w:r>{box("Wingdings", "F0FE")}<w:t>Copy sent</w:t></w:r></w:p>
<w:p><w:r>{box("Wingdings", "F06F")}<w:t>Copy filed</w:t></w:r></w:p>
<w:p><w:r><w:t>Glyph</w:t>{box("Symbol", "F060")}<w:t>dropped</w:t></w:r></w:p>
<w:p><w:r><w:t>Call 555</w:t><w:noBreakHyphen/><w:t>0123 by e</w:t><w:noBreakHyphen/>\
<w:t>mail.</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Self</w:t><w:noBreakHyphen/><w:t>signed</w:t></w:r></w:p>
<w:p><w:r><w:t>Hyphen</w:t><w:softHyphen/><w:t>ation stays joined.</w:t></w:r></w:p>
<w:tbl><w:tr>
<w:tc><w:p><w:r>{box("Wingdings 2", "0052")}<w:t>Required</w:t></w:r></w:p></w:tc>
<w:tc><w:p><w:r>{box("Wingdings 2", "00A3")}<w:t>Not required</w:t></w:r></w:p></w:tc>
</w:tr></w:tbl>
</w:body></w:document>"""
    write_docx(out / "docx" / "handmade-run-marks.docx", document)


def symbols_docx(out: Path) -> None:
    styles = f"""{DECL}
<w:styles {W}>
<w:style w:type="character" w:styleId="Greek">
  <w:name w:val="Greek"/><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>
</w:style>
</w:styles>"""
    symbol = '<w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>'
    document = f"""{DECL}
<w:document {W}><w:body>
<w:p><w:r><w:t xml:space="preserve">10 </w:t></w:r><w:r>{symbol}<w:t>m</w:t></w:r><w:r>\
<w:t xml:space="preserve">m </w:t></w:r><w:r><w:sym w:font="Symbol" w:char="F0B1"/></w:r><w:r>\
<w:t xml:space="preserve"> 2</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:rStyle w:val="Greek"/></w:rPr><w:t>abg</w:t></w:r><w:r>\
<w:t xml:space="preserve"> and </w:t></w:r><w:r>{symbol}<w:t>\uf0a3\uf0b3</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:rFonts w:ascii="Times New Roman" w:hAnsi="Symbol"/></w:rPr><w:t>m\uf06d</w:t>\
</w:r></w:p>
</w:body></w:document>"""
    write_docx(
        out / "docx" / "handmade-symbols.docx",
        document,
        doc_rels(("rId1", "styles", "styles.xml")),
        ("word/styles.xml", styles),
    )


def position_docx(out: Path) -> None:
    """Superscript and subscript set directly, through a character style, and cancelled by a
    direct baseline over a superscript style; ODF and EPUB build the same text."""

    def run(text: str, rpr: str = "") -> str:
        return f'<w:r><w:rPr>{rpr}</w:rPr><w:t xml:space="preserve">{text}</w:t></w:r>'

    def shift(value: str) -> str:
        return f'<w:vertAlign w:val="{value}"/>'

    def char_style(style_id: str) -> str:
        return f'<w:rStyle w:val="{style_id}"/>'

    styles = f"""{DECL}
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="character" w:styleId="Raised"><w:name w:val="Raised"/>
<w:rPr>{shift("superscript")}</w:rPr></w:style>
<w:style w:type="character" w:styleId="Lowered"><w:name w:val="Lowered"/>
<w:rPr>{shift("subscript")}</w:rPr></w:style>
</w:styles>"""
    document = f"""{DECL}
<w:document {W}><w:body>
<w:p>{run("Direct: 10")}{run("-3", shift("superscript"))}{run(" M and H")}\
{run("2", shift("subscript"))}{run("O.")}</w:p>
<w:p>{run("Through a style: x")}{run("2", char_style("Raised"))}{run(" and CO")}\
{run("2", char_style("Lowered"))}{run(".")}</w:p>
<w:p>{run("Baseline over a superscript style: ")}\
{run("level", char_style("Raised") + shift("baseline"))}{run(".")}</w:p>
</w:body></w:document>"""
    write_docx(
        out / "docx" / "handmade-position.docx",
        document,
        doc_rels(("rId1", "styles", "styles.xml")),
        ("word/styles.xml", styles),
    )


def hidden_docx(out: Path) -> None:
    """Hidden text and tracked changes: what drops, what joins, and what each counts."""
    hide = "<w:rPr><w:vanish/></w:rPr>"

    def run(text: str, rpr: str = "") -> str:
        return f'<w:r>{rpr}<w:t xml:space="preserve">{text}</w:t></w:r>'

    def styled_run(style: str, text: str) -> str:
        return run(text, f'<w:rPr><w:rStyle w:val="{style}"/></w:rPr>')

    item = '<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr>'
    styles = f"""{DECL}
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style>
<w:style w:type="paragraph" w:styleId="HiddenNote"><w:name w:val="Hidden Note"/>{hide}</w:style>
<w:style w:type="character" w:styleId="Secret"><w:name w:val="Secret"/>{hide}</w:style>
<w:style w:type="character" w:styleId="Unsecret"><w:name w:val="Unsecret"/>
<w:basedOn w:val="Secret"/>{hide}</w:style>
</w:styles>"""
    numbering = f"""{DECL}
<w:numbering {W}>
<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/>
<w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
</w:numbering>"""
    ruby = (
        "<w:r><w:ruby><w:rt><w:r><w:t>unread</w:t></w:r></w:rt>"
        "<w:rubyBase><w:r><w:t>ruby</w:t></w:r></w:rubyBase></w:ruby></w:r>"
    )
    footnotes = f"""{DECL}
<w:footnotes {W}>
<w:footnote w:type="separator" w:id="0"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
<w:footnote w:id="1"><w:p>{run("A note whose reference shows.")}</w:p></w:footnote>
<w:footnote w:id="2"><w:p>{run("A note whose only reference is hidden, ")}{ruby}</w:p></w:footnote>
</w:footnotes>"""
    shown_mark = '<w:rPr><w:vanish w:val="0"/></w:rPr>'
    document = f"""{DECL}
<w:document {W}><w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{run("Hidden text and tracked changes")}</w:p>
<w:p>{run("Shown, ")}{run("hidden directly, ", hide)}{run("and shown again.")}</w:p>
<w:p>{run("A character style hides ")}{styled_run("Secret", "this, ")}\
{run("and a style based on it flips ")}{styled_run("Unsecret", "back.")}</w:p>
<w:p><w:pPr><w:pStyle w:val="HiddenNote"/></w:pPr>\
{run("A hidden paragraph style hides this text and its mark, ")}\
{run("all but this, which joins ", shown_mark)}</w:p>
<w:p>{run("the next paragraph.")}</w:p>
<w:p><w:pPr><w:rPr><w:vanish/></w:rPr></w:pPr>{run("A hidden mark joins ")}</w:p>
<w:p>{run("this paragraph.")}</w:p>
<w:p><w:pPr><w:rPr><w:specVanish/></w:rPr></w:pPr>{run("A style separator joins ")}</w:p>
<w:p>{run("as well.")}</w:p>
<w:p>{run("A note shows")}<w:r><w:footnoteReference w:id="1"/></w:r>\
{run(" and a hidden one does not")}<w:r>{hide}<w:footnoteReference w:id="2"/></w:r>{run(".")}</w:p>
<w:p>{run("Web-only hiding ")}{run("still shows.", "<w:rPr><w:webHidden/></w:rPr>")}</w:p>
<w:p>{run("Accepted revisions drop ")}<w:del w:id="10" w:author="A"><w:r>\
<w:delText xml:space="preserve">deleted words </w:delText></w:r></w:del>\
<w:ins w:id="11" w:author="A">{run("and keep insertions.")}</w:ins></w:p>
<w:moveFromRangeStart w:id="20" w:author="A" w:name="move1"/>
<w:p><w:pPr><w:rPr><w:moveFrom w:id="21" w:author="A"/></w:rPr></w:pPr>\
<w:moveFrom w:id="22" w:author="A">{run("A moved sentence.")}</w:moveFrom></w:p>
<w:moveFromRangeEnd w:id="20"/>
<w:p>{run("It moved below, ")}<w:moveFrom w:id="23" w:author="A">\
{run("while this half of a move names no destination, ")}</w:moveFrom>{run("which counts.")}</w:p>
<w:moveToRangeStart w:id="30" w:author="A" w:name="move1"/>
<w:p><w:pPr><w:rPr><w:moveTo w:id="31" w:author="A"/></w:rPr></w:pPr>\
<w:moveTo w:id="32" w:author="A">{run("A moved sentence.")}</w:moveTo></w:p>
<w:moveToRangeEnd w:id="30"/>
<w:p>{run("Two comments")}<w:r><w:commentReference w:id="1"/></w:r>{run(" each count")}\
<w:r><w:commentReference w:id="2"/></w:r>{run(".")}</w:p>
<w:p><w:pPr>{item}</w:pPr>{run("First item")}</w:p>
<w:p><w:pPr>{item}<w:rPr><w:del w:id="40" w:author="A"/></w:rPr></w:pPr>\
{run("Second item, its mark deleted before a table")}</w:p>
<w:tbl><w:tblGrid><w:gridCol w:w="4000"/></w:tblGrid>
<w:tr><w:tc><w:p>{run("Cell text")}{run(" hidden in the cell", hide)}</w:p></w:tc></w:tr>
</w:tbl>
<w:sectPr/>
</w:body></w:document>"""
    write_docx(
        out / "docx" / "handmade-hidden.docx",
        document,
        doc_rels(
            ("rId1", "styles", "styles.xml"),
            ("rId2", "numbering", "numbering.xml"),
            ("rId3", "footnotes", "footnotes.xml"),
        ),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        ("word/footnotes.xml", footnotes),
        ct_extra=NUMBERING_OVERRIDE
        + override("/word/footnotes.xml", "wordprocessingml.footnotes"),
    )


def malformed(out: Path) -> None:
    m = out / "malformed"
    m.mkdir(parents=True, exist_ok=True)
    docx = out / "docx" / "text.docx"
    if docx.exists():
        raw = docx.read_bytes()
        (m / "truncated--errors.docx").write_bytes(raw[: len(raw) * 3 // 5])
        zip_edit(docx, m / "missing-styles--skips.docx", "word/styles.xml")
        zip_edit(
            docx, m / "corrupt-styles--skips.docx", "word/styles.xml", b"\x00\x01garbage <notxml"
        )
    # hand-built ill-formed XML shapes
    unclosed_doc = (
        f'<?xml version="1.0"?><w:document {W}><w:body><w:p><w:r><w:t>Unclosed paragraph text'
    )
    mismatched_doc = (
        f'<?xml version="1.0"?><w:document {W}><w:body>'
        f"<w:p><w:r><w:t>Mismatched tags</w:t></w:r></w:q>"
        f"</w:body></w:document>"
    )
    styles_min = f'<?xml version="1.0"?><w:styles {W}/>'
    for name, doc in [
        ("unclosed--errors.docx", unclosed_doc),
        ("mismatched--errors.docx", mismatched_doc),
    ]:
        write_docx(m / name, doc, ("word/styles.xml", styles_min))
    # an unclosed optional part is unreadable, the required part converts
    whole_doc = (
        f'<?xml version="1.0"?><w:document {W}><w:body>'
        f"<w:p><w:r><w:t>Body text</w:t></w:r></w:p></w:body></w:document>"
    )
    unclosed_notes = (
        f'<?xml version="1.0"?><w:footnotes {W}><w:footnote w:id="2"><w:p><w:r><w:t>Unclosed note'
    )
    write_docx(m / "unclosed-notes--skips.docx", whole_doc, ("word/footnotes.xml", unclosed_notes))
    (m / "empty--errors.docx").write_bytes(b"")


def noise(size: int, seed: int) -> bytes:
    """Seeded incompressible bytes, which hold a part under the compression
    ratio limit while another limit is the one it trips."""
    return random.Random(seed).randbytes(size)


def noise_comment(size: int, seed: int) -> str:
    return "<!--" + base64.b64encode(noise(size, seed)).decode() + "-->"


def zip64_end(data: bytes, entries: int) -> bytes:
    """Declares `entries` entries in ZIP64 end records, with no matching
    central headers."""
    end = data.rindex(b"PK\x05\x06")
    size, start = struct.unpack_from("<II", data, end + 12)
    record = struct.pack(
        "<4sQHHIIQQQQ", b"PK\x06\x06", 44, 45, 45, 0, 0, entries, entries, size, start
    )
    locator = struct.pack("<4sIQI", b"PK\x06\x07", 0, end, 1)
    classic = struct.pack(
        "<4sHHHHIIH", b"PK\x05\x06", 0, 0, 0xFFFF, 0xFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0
    )
    return data[:end] + record + locator + classic


def eocd_abuse(out: Path) -> None:
    path = out / "abuse" / "max_entry_count--eocd--errors.docx"
    write_docx(
        path,
        f'<?xml version="1.0"?><w:document {W}><w:body><w:p><w:r>'
        "<w:t>text</w:t></w:r></w:p></w:body></w:document>",
    )
    path.write_bytes(zip64_end(path.read_bytes(), 100_001))


def ratiobomb_abuse(out: Path) -> None:
    ratio_doc = (
        '<?xml version="1.0"?><!--'
        + "A" * (16 * 1024 * 1024)
        + f"--><w:document {W}><w:body/></w:document>"
    )
    write_docx(out / "abuse" / "max_compression_ratio--bomb--errors.docx", ratio_doc)


def zipbomb_abuse(out: Path) -> None:
    """The bomb must live in a part the parser actually reads: a document.xml
    that decompresses to ~192 MiB crosses the 128 MiB per-entry cap."""
    bomb_doc = (
        f'<?xml version="1.0"?>{noise_comment(32 * 1024, 1)}'
        f"<w:document {W}><w:body><w:p><w:r><w:t>"
        + "A" * (192 * 1024 * 1024)
        + "</w:t></w:r></w:p></w:body></w:document>"
    )
    write_docx(out / "abuse" / "max_entry_bytes--zipbomb--errors.docx", bomb_doc)


def imagebomb_abuse(out: Path) -> None:
    """The bomb lives in an *optional* part (an image behind a relationship). Resource limits
    must propagate out of recovery paths instead of being skipped like an ordinary broken
    image."""
    image_doc = (
        f'<?xml version="1.0"?><w:document {W} {R}><w:body>'
        "<w:p><w:r><w:t>before the image</w:t></w:r></w:p>"
        + image_ref("rId10")
        + "</w:body></w:document>"
    )
    write_docx(
        out / "abuse" / "max_entry_bytes--imagebomb--errors.docx",
        image_doc,
        doc_rels(("rId10", "image", "media/image1.png")),
        ("word/media/image1.png", b"\x00" * (192 * 1024 * 1024) + noise(32 * 1024, 2)),
    )


def deepxml_abuse(out: Path) -> None:
    depth = 40_000
    deep_doc = (
        f'<?xml version="1.0"?><w:document {W}><w:body><w:p><w:r><w:t>'
        + "<x>" * depth
        + "deep"
        + "</x>" * depth
        + "</w:t></w:r></w:p></w:body></w:document>"
    )
    write_docx(out / "abuse" / "max_nesting_depth--deepxml--errors.docx", deep_doc)


def flatflood_abuse(out: Path) -> None:
    """Past the default node limit, each node an empty element."""
    flat_doc = (
        f'<?xml version="1.0"?>{noise_comment(2 * 1024, 3)}'
        f"<w:document {W}><w:body>" + "<x/>" * 2_000_001 + "</w:body></w:document>"
    )
    write_docx(out / "abuse" / "max_xml_nodes--flatflood--errors.docx", flat_doc)


def paddedparts_abuse(out: Path) -> None:
    """Five parts the parser reads, each padded by a 110 MiB comment that no node counts: under
    the per-entry cap, past the whole-archive total."""
    pad = "<!--" + "A" * (110 * 1024 * 1024) + "-->" + noise_comment(16 * 1024, 4)
    parts = (
        ("word/styles.xml", f"<w:styles {W}/>"),
        ("word/numbering.xml", f"<w:numbering {W}/>"),
        ("word/document.xml", f"<w:document {W}><w:body/></w:document>"),
        ("word/footnotes.xml", f"<w:footnotes {W}/>"),
        ("word/endnotes.xml", f"<w:endnotes {W}/>"),
    )
    head = [("[Content_Types].xml", content_types(*WORD_PARTS)), ("_rels/.rels", ROOT_RELS)]
    write_zip(
        out / "abuse" / "max_total_bytes--paddedparts--errors.docx",
        chain(head, ((name, f'<?xml version="1.0"?>{pad}{root}') for name, root in parts)),
    )


def twoimages_abuse(out: Path) -> None:
    """Two embedded images, each under the per-entry cap, together past the retained-asset
    total."""
    two_images_doc = (
        f'<?xml version="1.0"?><w:document {W} {R}><w:body>'
        + image_ref("rId10")
        + image_ref("rId11")
        + "</w:body></w:document>"
    )
    write_docx(
        out / "abuse" / "max_asset_total_bytes--twoimages--errors.docx",
        two_images_doc,
        doc_rels(("rId10", "image", "media/image1.png"), ("rId11", "image", "media/image2.png")),
        ("word/media/image1.png", b"\x00" * (100 * 1024 * 1024) + noise(16 * 1024, 5)),
        ("word/media/image2.png", b"\x00" * (100 * 1024 * 1024) + noise(16 * 1024, 6)),
    )


def generate(out: Path) -> None:
    numbering_docx(out)
    bare_number_docx(out)
    rich_docx(out)
    strict_alt(out)
    tables_docx(out)
    nested_docx(out)
    outline_docx(out)
    blockstyle_docx(out)
    ole_docx(out)
    ole_iso_docx(out)
    manyrefs_docx(out)
    math_docx(out)
    run_marks_docx(out)
    symbols_docx(out)
    position_docx(out)
    hidden_docx(out)
    malformed(out)
    eocd_abuse(out)
    ratiobomb_abuse(out)
    zipbomb_abuse(out)
    imagebomb_abuse(out)
    deepxml_abuse(out)
    flatflood_abuse(out)
    paddedparts_abuse(out)
    twoimages_abuse(out)
