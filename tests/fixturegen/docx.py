from fixturegen.common import (
    CONTENT_TYPES_BASE,
    DOT_PNG,
    M_NS,
    OMML_QUADRATIC,
    OMML_SUM,
    OMML_WHERE,
    write_zip,
    zip_replace,
    zip_without,
)


W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"'
R = 'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'

ROOT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"""


def para(text, ppr=""):
    return f"<w:p>{ppr}<w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>"


def numpara(text, num_id, ilvl=0, pstyle=None):
    ps = f'<w:pStyle w:val="{pstyle}"/>' if pstyle else ""
    ppr = (f"<w:pPr>{ps}<w:numPr><w:ilvl w:val=\"{ilvl}\"/>"
           f"<w:numId w:val=\"{num_id}\"/></w:numPr></w:pPr>")
    return para(text, ppr)


# ---------------------------------------------------------------------------
# Handmade DOCX: complete numbering matrix + style inheritance (P7.4a-f, H6, M1)

def numbering_docx(out):
    styles = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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

    numbering = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering {W}>
<w:abstractNum w:abstractNumId="0">
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
  <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerRoman"/><w:lvlText w:val="%2)"/><w:lvlRestart w:val="0"/></w:lvl>
  <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#8226;"/></w:lvl>
</w:abstractNum>
<w:abstractNum w:abstractNumId="1">
  <w:numStyleLink w:val="ListNumStyle"/>
</w:abstractNum>
<w:abstractNum w:abstractNumId="2">
  <w:styleLink w:val="ListNumStyle"/>
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperLetter"/><w:lvlText w:val="%1:"/></w:lvl>
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
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperLetter"/><w:lvlText w:val="%1)"/></w:lvl>
  </w:lvlOverride>
</w:num>
<w:abstractNum w:abstractNumId="3">
  <w:lvl w:ilvl="0"><w:pStyle w:val="ListLevelOne"/><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
  <w:lvl w:ilvl="1"><w:pStyle w:val="ListLevelTwo"/><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2."/></w:lvl>
</w:abstractNum>
<w:num w:numId="7"><w:abstractNumId w:val="3"/></w:num>
</w:numbering>"""

    body = "".join([
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
        # S3: style-based numbering resolves its level through the abstract
        # levels' pStyle bindings; the ilvl inside the styles' numPr (5 and
        # 0 above - both wrong on purpose) must be ignored.
        para("pStyle-bound level one", '<w:pPr><w:pStyle w:val="ListLevelOne"/></w:pPr>'),
        para("pStyle-bound level two", '<w:pPr><w:pStyle w:val="ListLevelTwo"/></w:pPr>'),
        para("pStyle-bound level one again", '<w:pPr><w:pStyle w:val="ListLevelOne"/></w:pPr>'),
        # heading level via basedOn-inherited outlineLvl
        para("Inherited subheading", '<w:pPr><w:pStyle w:val="SubHead"/></w:pPr>'),
        # Toggle semantics (ECMA-376 17.7.3):
        # (a) style-level false is a no-op -> stays bold
        ('<w:p><w:pPr><w:pStyle w:val="BoldPara"/></w:pPr>'
         '<w:r><w:t xml:space="preserve">Bold here, </w:t></w:r>'
         '<w:r><w:rPr><w:rStyle w:val="PlainChar"/></w:rPr>'
         '<w:t xml:space="preserve">and style-false keeps it bold</w:t></w:r></w:p>'),
        # (b) character-style true toggles inherited bold off
        ('<w:p><w:pPr><w:pStyle w:val="BoldPara"/></w:pPr>'
         '<w:r><w:rPr><w:rStyle w:val="ToggleChar"/></w:rPr>'
         '<w:t xml:space="preserve">toggled off by a true in the character style</w:t>'
         '</w:r></w:p>'),
        # (c) two trues along one basedOn chain cancel out
        ('<w:p><w:pPr><w:pStyle w:val="DoubleBold"/></w:pPr>'
         '<w:r><w:t xml:space="preserve">double toggle cancels to plain</w:t></w:r></w:p>'),
        # (d) direct formatting is absolute; "off" is a valid ST_OnOff value
        ('<w:p><w:pPr><w:pStyle w:val="BoldPara"/></w:pPr>'
         '<w:r><w:rPr><w:b w:val="off"/></w:rPr>'
         '<w:t xml:space="preserve">direct off wins absolutely</w:t></w:r></w:p>'),
        # (e) direct "on" is a valid ST_OnOff value
        ('<w:p><w:r><w:rPr><w:i w:val="on"/></w:rPr>'
         '<w:t xml:space="preserve">direct on makes this italic</w:t></w:r></w:p>'),
    ])
    document = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f'<w:document {W} {R}><w:body>{body}</w:body></w:document>')
    doc_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>"""
    ct = CONTENT_TYPES_BASE.format(extra=(
        '<Override PartName="/word/numbering.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>'))
    write_zip(out / "docx" / "handmade-numbering.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
    ])


# ---------------------------------------------------------------------------
# Handmade DOCX: chart + SmartArt + image rel (P7.5, P7.10, M4)

def rich_docx(out):
    chart = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<c:chart>
<c:title><c:tx><c:rich><a:p><a:r><a:t>Quarterly Widgets</a:t></a:r></a:p></c:rich></c:tx></c:title>
<c:plotArea>
<c:barChart>
<c:ser>
<c:tx><c:strRef><c:f>S</c:f><c:strCache><c:pt c:idx="0"><c:v>Widgets</c:v></c:pt></c:strCache></c:strRef></c:tx>
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

    diagram_data = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
        '<wp:extent cx="3000000" cy="2000000"/><wp:docPr id="1" name="Chart 1" descr="widget chart"/>'
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
        '<dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId20" r:lo="" r:qs="" r:cs=""/>'
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
    document = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f'<w:document {W} {R}><w:body>{body}</w:body></w:document>')
    doc_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
<Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="charts/chart1.xml"/>
<Relationship Id="rId20" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData" Target="diagrams/data1.xml"/>
<Relationship Id="rId30" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/dot.png"/>
<Relationship Id="rId40" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject" Target="embeddings/oleObject1.bin"/>
</Relationships>"""
    styles = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
              f'<w:styles {W}><w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults></w:styles>')
    ct = CONTENT_TYPES_BASE.format(extra=(
        '<Default Extension="png" ContentType="image/png"/>'
        '<Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.oleObject"/>'
        '<Override PartName="/word/charts/chart1.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>'
        '<Override PartName="/word/diagrams/data1.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.drawingml.diagramData+xml"/>'))
    write_zip(out / "docx" / "handmade-rich.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/charts/chart1.xml", chart),
        ("word/diagrams/data1.xml", diagram_data),
        ("word/media/dot.png", DOT_PNG),
        ("word/embeddings/oleObject1.bin", b"OLE-PAYLOAD-STAND-IN" * 4),
    ])


# ---------------------------------------------------------------------------
# R1: ISO 29500 Strict namespaces and rels-driven part discovery

STRICT_W = 'xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main"'


def strict_alt(out):
    ct = CONTENT_TYPES_BASE.format(extra="")

    # Strict DOCX: every namespace and relationship type from the Strict
    # family; styles resolved through the document's typed relationship.
    strict_doc = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                  f'<w:document {STRICT_W}><w:body>'
                  '<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>'
                  '<w:r><w:t>Strict heading</w:t></w:r></w:p>'
                  '<w:p><w:r><w:t xml:space="preserve">Strict body with </w:t></w:r>'
                  '<w:r><w:rPr><w:rStyle w:val="Bold"/></w:rPr>'
                  '<w:t>toggled bold</w:t></w:r></w:p>'
                  '</w:body></w:document>')
    strict_styles = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                     f'<w:styles {STRICT_W}>'
                     '<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>'
                     '<w:style w:type="paragraph" w:styleId="Heading1">'
                     '<w:name w:val="heading 1"/></w:style>'
                     '<w:style w:type="character" w:styleId="Bold">'
                     '<w:name w:val="Bold"/><w:rPr><w:b/></w:rPr></w:style>'
                     '</w:styles>')
    strict_root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"""
    strict_doc_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/styles" Target="styles.xml"/>
</Relationships>"""
    write_zip(out / "docx" / "handmade-strict.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", strict_root_rels),
        ("word/document.xml", strict_doc),
        ("word/_rels/document.xml.rels", strict_doc_rels),
        ("word/styles.xml", strict_styles),
    ])

    # Alternate-location DOCX: no part lives at its conventional path; the
    # main part, styles, and footnotes are all found through relationships.
    alt_doc = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
               f'<w:document {W}><w:body>'
               '<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>'
               '<w:r><w:t>Relocated heading</w:t></w:r></w:p>'
               '<w:p><w:r><w:t xml:space="preserve">Body found via rels</w:t></w:r>'
               '<w:r><w:rPr><w:rStyle w:val="FootnoteRef"/></w:rPr>'
               '<w:footnoteReference w:id="2"/></w:r></w:p>'
               '</w:body></w:document>')
    alt_styles = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                  f'<w:styles {W}>'
                  '<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>'
                  '<w:style w:type="paragraph" w:styleId="Heading1">'
                  '<w:name w:val="heading 1"/></w:style>'
                  '<w:style w:type="character" w:styleId="FootnoteRef">'
                  '<w:name w:val="footnote reference"/></w:style>'
                  '</w:styles>')
    alt_footnotes = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                     f'<w:footnotes {W}>'
                     '<w:footnote w:type="separator" w:id="0"><w:p/></w:footnote>'
                     '<w:footnote w:id="2"><w:p><w:r>'
                     '<w:t>A relocated footnote</w:t></w:r></w:p></w:footnote>'
                     '</w:footnotes>')
    alt_root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="content/main.xml"/>
</Relationships>"""
    alt_doc_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="wordstyles.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="n/footnotes.xml"/>
</Relationships>"""
    write_zip(out / "docx" / "handmade-altpath.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", alt_root_rels),
        ("content/main.xml", alt_doc),
        ("content/_rels/main.xml.rels", alt_doc_rels),
        ("content/wordstyles.xml", alt_styles),
        ("content/n/footnotes.xml", alt_footnotes),
    ])


def math_docx(out):
    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W} {M_NS}><w:body>
<w:p><w:r><w:t xml:space="preserve">The roots are </w:t></w:r>{OMML_QUADRATIC}<w:r><w:t xml:space="preserve"> for any a.</w:t></w:r></w:p>
<w:p><m:oMathPara>{OMML_SUM}{OMML_WHERE}</m:oMathPara></w:p>
<w:p><w:r><w:t>A price of $5 or $6 is not math.</w:t></w:r></w:p>
</w:body></w:document>"""
    styles = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
              f'<w:styles {W}>'
              '<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults></w:styles>')
    write_zip(out / "docx" / "handmade-math.docx", [
        ("[Content_Types].xml", CONTENT_TYPES_BASE.format(extra="")),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/styles.xml", styles),
    ])


# ---------------------------------------------------------------------------
# R15: DOCX gridBefore/gridAfter, legacy hMerge, ST_OnOff tblHeader

def tables_docx(out):
    def tc(text, extra=""):
        return (f'<w:tc><w:tcPr>{extra}</w:tcPr>'
                f'<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p></w:tc>')

    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W}><w:body>
<w:tbl>
<w:tr><w:trPr><w:tblHeader/></w:trPr>
{tc("Head A")}{tc("Head B")}{tc("Head C", '<w:vMerge w:val="restart"/>')}</w:tr>
<w:tr><w:trPr><w:tblHeader w:val="0"/><w:gridBefore w:val="2"/></w:trPr>
{tc("under C", '<w:vMerge/>')}</w:tr>
<w:tr><w:trPr><w:gridAfter w:val="1"/></w:trPr>
{tc("legacy start", '<w:hMerge w:val="restart"/>')}{tc("legacy cont", '<w:hMerge/>')}</w:tr>
</w:tbl>
<w:p><w:r><w:t>after the table</w:t></w:r></w:p>
</w:body></w:document>"""
    styles = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
              f'<w:styles {W}>'
              '<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults></w:styles>')
    ct = CONTENT_TYPES_BASE.format(extra="")
    write_zip(out / "docx" / "handmade-tables.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/styles.xml", styles),
    ])


# ---------------------------------------------------------------------------
# S12: a valid document referencing one part many times must convert with a
# single retained asset; the part is cached and the asset deduplicated. (The
# budget re-charging itself is covered by unit tests - a fixture big enough
# to cross the 512 MiB budget would slow the mutation harness down.)

def manyrefs_docx(out):
    drawing = ('<w:p><w:r><w:drawing>'
               '<a:blip xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
               ' r:embed="rId10"/>'
               '</w:drawing></w:r></w:p>')
    document = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f'<w:document {W} {R}><w:body>'
                '<w:p><w:r><w:t>Seventy references to one image follow.</w:t></w:r></w:p>'
                + drawing * 70 +
                '</w:body></w:document>')
    doc_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/logo.png"/>
</Relationships>"""
    ct = CONTENT_TYPES_BASE.format(extra='<Default Extension="png" ContentType="image/png"/>')
    write_zip(out / "docx" / "handmade-manyrefs.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/media/logo.png", b"\x89PNG\r\n\x1a\n" + b"\x00" * (64 * 1024)),
    ])


# ---------------------------------------------------------------------------
# S5: standard Word OLE markup - a VML preview image next to the
# o:OLEObject; the object's identity and payload must win over the preview.

def ole_docx(out):
    V = 'xmlns:v="urn:schemas-microsoft-com:vml"'
    O = 'xmlns:o="urn:schemas-microsoft-com:office:office"'
    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W} {R} {V} {O}><w:body>
<w:p><w:r><w:t>Before the object</w:t></w:r></w:p>
<w:p><w:r><w:object>
<v:shape id="_x0000_i1025" style="width:96pt;height:48pt">
<v:imagedata r:id="rId3" o:title="preview"/>
</v:shape>
<o:OLEObject Type="Embed" ProgID="Excel.Sheet.12" ShapeID="_x0000_i1025" r:id="rId2"/>
</w:object></w:r></w:p>
</w:body></w:document>"""
    doc_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject" Target="embeddings/oleObject1.bin"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/preview.png"/>
</Relationships>"""
    styles = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
              f'<w:styles {W}>'
              '<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults></w:styles>')
    ct = CONTENT_TYPES_BASE.format(extra=(
        '<Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.oleObject"/>'
        '<Default Extension="png" ContentType="image/png"/>'))
    write_zip(out / "docx" / "handmade-ole.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/embeddings/oleObject1.bin", b"DOCX-OLE-PAYLOAD" * 4),
        ("word/media/preview.png", b"\x89PNG\r\n\x1a\nfakepreview"),
    ])


# ---------------------------------------------------------------------------
# S2: outline level is a tri-state property; direct paragraph properties
# override the style chain, and outlineLvl 9 is the explicit off state.

def outline_docx(out):
    def p(text, ppr=""):
        pr = f"<w:pPr>{ppr}</w:pPr>" if ppr else ""
        return f'<w:p>{pr}<w:r><w:t>{text}</w:t></w:r></w:p>'

    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W}><w:body>
{p("Style heading stays a heading", '<w:pStyle w:val="Outlined"/>')}
{p("Direct level overrides the style", '<w:pStyle w:val="Outlined"/><w:outlineLvl w:val="2"/>')}
{p("Direct nine turns the style heading off", '<w:pStyle w:val="Outlined"/><w:outlineLvl w:val="9"/>')}
{p("Direct outline without a style", '<w:outlineLvl w:val="0"/>')}
{p("Child style nine stops inheritance", '<w:pStyle w:val="NotOutlined"/>')}
</w:body></w:document>"""
    styles = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="Outlined"><w:name w:val="Outlined"/>
<w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="NotOutlined"><w:name w:val="NotOutlined"/>
<w:basedOn w:val="Outlined"/><w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:style>
</w:styles>"""
    ct = CONTENT_TYPES_BASE.format(extra="")
    write_zip(out / "docx" / "handmade-outline.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/styles.xml", styles),
    ])


def blockstyle_docx(out):
    def p(text, style, runs=None):
        body = runs or f'<w:r><w:t xml:space="preserve">{text}</w:t></w:r>'
        return f'<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>{body}</w:p>'

    # Syntax highlighting: Pandoc splits a code line into styled runs.
    highlighted = ('<w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">fn</w:t></w:r>'
                   '<w:r><w:t xml:space="preserve"> main() {</w:t></w:r>')
    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W}><w:body>
{para("Body before.")}
{p("", "SourceCode", highlighted)}
{p("    println!(\"ok\");", "SourceCode")}
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
    styles = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W}>
<w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="SourceCode"><w:name w:val="Source Code"/></w:style>
<w:style w:type="paragraph" w:styleId="SourceCodeChild"><w:name w:val="Listing"/>
<w:basedOn w:val="SourceCode"/></w:style>
<w:style w:type="paragraph" w:styleId="BlockText"><w:name w:val="Block Text"/></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="IntenseQuote"><w:name w:val="Intense Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="HTMLPreformatted"><w:name w:val="HTML Preformatted"/></w:style>
</w:styles>"""
    ct = CONTENT_TYPES_BASE.format(extra="")
    write_zip(out / "docx" / "handmade-blockstyle.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/styles.xml", styles),
    ])


def run_marks_docx(out):
    def box(font, code):
        return f'<w:sym w:font="{font}" w:char="{code}"/>'

    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W}><w:body>
<w:p><w:r>{box("Wingdings 2", "F052")}<w:t>Signed by both parties</w:t></w:r></w:p>
<w:p><w:r>{box("Wingdings 2", "F0A3")}<w:t>Witnessed</w:t></w:r></w:p>
<w:p><w:r>{box("Wingdings", "F0FE")}<w:t>Copy sent</w:t></w:r></w:p>
<w:p><w:r>{box("Wingdings", "F06F")}<w:t>Copy filed</w:t></w:r></w:p>
<w:p><w:r><w:t>Glyph</w:t>{box("Symbol", "F061")}<w:t>dropped</w:t></w:r></w:p>
<w:p><w:r><w:t>Call 555</w:t><w:noBreakHyphen/><w:t>0123 by e</w:t><w:noBreakHyphen/><w:t>mail.</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Self</w:t><w:noBreakHyphen/><w:t>signed</w:t></w:r></w:p>
<w:p><w:r><w:t>Hyphen</w:t><w:softHyphen/><w:t>ation stays joined.</w:t></w:r></w:p>
<w:tbl><w:tr>
<w:tc><w:p><w:r>{box("Wingdings 2", "0052")}<w:t>Required</w:t></w:r></w:p></w:tc>
<w:tc><w:p><w:r>{box("Wingdings 2", "00A3")}<w:t>Not required</w:t></w:r></w:p></w:tc>
</w:tr></w:tbl>
</w:body></w:document>"""
    write_zip(out / "docx" / "handmade-run-marks.docx", [
        ("[Content_Types].xml", CONTENT_TYPES_BASE.format(extra="")),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
    ])


def malformed(out):
    m = out / "malformed"
    m.mkdir(parents=True, exist_ok=True)
    docx = out / "docx" / "text.docx"
    if docx.exists():
        raw = docx.read_bytes()
        (m / "truncated--errors.docx").write_bytes(raw[: len(raw) * 3 // 5])
        zip_without(docx, m / "missing-styles--skips.docx", "word/styles.xml")
        zip_replace(docx, m / "corrupt-styles--skips.docx", "word/styles.xml",
                    b"\x00\x01garbage <notxml")
    # hand-built recovery-class XML shapes
    unclosed_doc = (f'<?xml version="1.0"?><w:document {W}><w:body>'
                    f'<w:p><w:r><w:t>Unclosed paragraph text')
    mismatched_doc = (f'<?xml version="1.0"?><w:document {W}><w:body>'
                      f'<w:p><w:r><w:t>Mismatched tags</w:t></w:r></w:q>'
                      f'</w:body></w:document>')
    styles_min = (f'<?xml version="1.0"?><w:styles {W}/>')
    ct = CONTENT_TYPES_BASE.format(extra="")
    for name, doc in [("unclosed--recovers.docx", unclosed_doc),
                      ("mismatched--recovers.docx", mismatched_doc)]:
        write_zip(m / name, [
            ("[Content_Types].xml", ct),
            ("_rels/.rels", ROOT_RELS),
            ("word/document.xml", doc),
            ("word/styles.xml", styles_min),
        ])
    (m / "empty--errors.docx").write_bytes(b"")


def abuse(out):
    a = out / "abuse"
    a.mkdir(parents=True, exist_ok=True)
    ct = CONTENT_TYPES_BASE.format(extra="")
    # The bomb must live in a part the parser actually reads: a document.xml
    # that decompresses to ~192 MiB crosses the 128 MiB per-entry cap.
    bomb_doc = (f'<?xml version="1.0"?><w:document {W}><w:body><w:p><w:r><w:t>'
                + "A" * (192 * 1024 * 1024)
                + "</w:t></w:r></w:p></w:body></w:document>")
    write_zip(a / "zipbomb--errors.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", bomb_doc),
    ])
    # R2: the bomb lives in an *optional* part (an image behind a
    # relationship). Resource limits must propagate out of recovery paths
    # instead of being skipped like an ordinary broken image.
    image_doc = (f'<?xml version="1.0"?><w:document {W} {R}><w:body>'
                 '<w:p><w:r><w:t>before the image</w:t></w:r></w:p>'
                 '<w:p><w:r><w:drawing>'
                 '<a:blip xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
                 ' r:embed="rId10"/>'
                 '</w:drawing></w:r></w:p>'
                 '</w:body></w:document>')
    image_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>"""
    write_zip(a / "imagebomb--errors.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", image_doc),
        ("word/_rels/document.xml.rels", image_rels),
        ("word/media/image1.png", b"\x00" * (192 * 1024 * 1024)),
    ])
    depth = 40_000
    deep_doc = (f'<?xml version="1.0"?><w:document {W}><w:body><w:p><w:r><w:t>'
                + "<x>" * depth + "deep" + "</x>" * depth
                + "</w:t></w:r></w:p></w:body></w:document>")
    write_zip(a / "deepxml--errors.docx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", deep_doc),
    ])


def generate(out):
    numbering_docx(out)
    rich_docx(out)
    strict_alt(out)
    tables_docx(out)
    outline_docx(out)
    blockstyle_docx(out)
    ole_docx(out)
    manyrefs_docx(out)
    math_docx(out)
    run_marks_docx(out)
    malformed(out)
    abuse(out)
