from fixturegen.common import (
    CONTENT_TYPES_BASE,
    DOT_PNG,
    M_NS,
    OMML_QUADRATIC,
    OMML_SUM,
    write_zip,
)


# ---------------------------------------------------------------------------
# Handmade PPTX: placeholder inheritance through layout and master (H7)

PPTX_NS = ('xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
           'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
           'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"')

MC_NS = 'xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"'
A14_NS = 'xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"'


def inherit_pptx(out):
    presentation = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation {PPTX_NS}>
<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
</p:presentation>"""
    pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"""
    # Master: body text is bold with a character bullet at level 1.
    master = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
    layout = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>Layout title text</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>
<a:lstStyle><a:lvl1pPr><a:buAutoNum type="romanLcPeriod" startAt="3"/></a:lvl1pPr></a:lstStyle>
<a:p><a:endParaRPr/></a:p></p:txBody></p:sp>
</p:spTree></p:cSld>
</p:sldLayout>"""
    slide = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Inherited Title Slide</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Content"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>
<a:p><a:r><a:t>Roman three bold via master</a:t></a:r></a:p>
<a:p><a:r><a:t>Roman four </a:t></a:r><a:r><a:rPr b="0"/><a:t>with bold turned off</a:t></a:r></a:p>
<a:p><a:pPr lvl="1"/><a:r><a:t>Dash bullet italic via master level two</a:t></a:r></a:p>
<a:p><a:pPr><a:buNone/></a:pPr><a:r><a:t>Plain closing line</a:t></a:r></a:p>
</p:txBody></p:sp>
</p:spTree></p:cSld>
</p:sld>"""
    slide_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"""
    layout_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"""
    ct = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
</Types>"""
    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""
    write_zip(out / "pptx" / "handmade-inherit.pptx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", root_rels),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
        ("ppt/slides/slide1.xml", slide),
        ("ppt/slides/_rels/slide1.xml.rels", slide_rels),
        ("ppt/slideLayouts/slideLayout1.xml", layout),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", layout_rels),
        ("ppt/slideMasters/slideMaster1.xml", master),
    ])


# ---------------------------------------------------------------------------
# R8: titles keep their shape-order position; p:oleObj payloads extracted

def order_pptx(out):
    presentation = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation {PPTX_NS}>
<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
</p:presentation>"""
    pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"""
    slide = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Lead"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Kicker before the title</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Title placed second</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="4" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Body after the title</a:t></a:r></a:p></p:txBody></p:sp>
<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="5" name="Sheet"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/presentationml/2006/ole">
<p:oleObj name="Quarterly numbers" progId="Excel.Sheet.12" r:id="rId2"/>
</a:graphicData></a:graphic></p:graphicFrame>
</p:spTree></p:cSld></p:sld>"""
    slide_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject" Target="../embeddings/oleObject1.bin"/>
</Relationships>"""
    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""
    ct = CONTENT_TYPES_BASE.format(extra=(
        '<Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.oleObject"/>'))
    write_zip(out / "pptx" / "handmade-order.pptx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", root_rels),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
        ("ppt/slides/slide1.xml", slide),
        ("ppt/slides/_rels/slide1.xml.rels", slide_rels),
        ("ppt/embeddings/oleObject1.bin", b"OLE-PAYLOAD-STAND-IN" * 4),
    ])


STRICT_PPTX_NS = ('xmlns:a="http://purl.oclc.org/ooxml/drawingml/main" '
                  'xmlns:p="http://purl.oclc.org/ooxml/presentationml/main" '
                  'xmlns:r="http://purl.oclc.org/ooxml/officeDocument/relationships"')


def strict_alt(out):
    ct = CONTENT_TYPES_BASE.format(extra="")

    # Strict PPTX: Strict namespaces plus Strict relationship types down the
    # presentation -> slide chain.
    strict_pres = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                   f'<p:presentation {STRICT_PPTX_NS}>'
                   '<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>'
                   '</p:presentation>')
    strict_slide = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                    f'<p:sld {STRICT_PPTX_NS}>'
                    '<p:cSld><p:spTree>'
                    '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
                    '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/>'
                    '<p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>'
                    '<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Strict slide title</a:t></a:r></a:p></p:txBody></p:sp>'
                    '<p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/>'
                    '<p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>'
                    '<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Strict body text</a:t></a:r></a:p></p:txBody></p:sp>'
                    '</p:spTree></p:cSld></p:sld>')
    strict_pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"""
    strict_pptx_root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""
    write_zip(out / "pptx" / "handmade-strict.pptx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", strict_pptx_root_rels),
        ("ppt/presentation.xml", strict_pres),
        ("ppt/_rels/presentation.xml.rels", strict_pres_rels),
        ("ppt/slides/slide1.xml", strict_slide),
    ])

    # Alternate-location PPTX: presentation and slide at unconventional
    # paths, reachable only through relationships.
    alt_pres = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f'<p:presentation {PPTX_NS}>'
                '<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>'
                '</p:presentation>')
    alt_slide = (f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                 f'<p:sld {PPTX_NS}>'
                 '<p:cSld><p:spTree>'
                 '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
                 '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/>'
                 '<p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>'
                 '<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Relocated deck title</a:t></a:r></a:p></p:txBody></p:sp>'
                 '</p:spTree></p:cSld></p:sld>')
    alt_pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="s/one.xml"/>
</Relationships>"""
    alt_pptx_root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="deck/pres.xml"/>
</Relationships>"""
    write_zip(out / "pptx" / "handmade-altpath.pptx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", alt_pptx_root_rels),
        ("deck/pres.xml", alt_pres),
        ("deck/_rels/pres.xml.rels", alt_pres_rels),
        ("deck/s/one.xml", alt_slide),
    ])


def math_pptx(out):
    """PowerPoint wraps an equation shape in an a14 AlternateContent whose
    fallback is a picture of the equation."""
    presentation = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation {PPTX_NS}>
<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
</p:presentation>"""
    pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"""
    slide = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld {PPTX_NS} {MC_NS} {A14_NS} {M_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Quadratic formula</a:t></a:r></a:p></p:txBody></p:sp>
<mc:AlternateContent><mc:Choice Requires="a14">
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Equation"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr/><p:txBody><a:bodyPr/>
<a:p><a:r><a:t>Solve with </a:t></a:r><a14:m><m:oMathPara>{OMML_QUADRATIC}</m:oMathPara></a14:m></a:p>
<a:p><a14:m><m:oMathPara>{OMML_SUM}</m:oMathPara></a14:m></a:p>
</p:txBody></p:sp>
</mc:Choice><mc:Fallback>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Equation"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
<p:spPr><a:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></a:blipFill></p:spPr>
<p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>
</mc:Fallback></mc:AlternateContent>
</p:spTree></p:cSld></p:sld>"""
    slide_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"""
    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""
    ct = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"""
    write_zip(out / "pptx" / "handmade-math.pptx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", root_rels),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
        ("ppt/slides/slide1.xml", slide),
        ("ppt/slides/_rels/slide1.xml.rels", slide_rels),
        ("ppt/media/image1.png", DOT_PNG),
    ])


# ---------------------------------------------------------------------------
# S9: links to another slide resolve to the target slide's start anchor
# instead of a dead relative reference to slides/slideN.xml.

def links_pptx(out):
    presentation = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation {PPTX_NS}>
<p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst>
</p:presentation>"""
    pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>
</Relationships>"""

    def slide(body):
        return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
{body}
</p:spTree></p:cSld></p:sld>"""

    slide1 = slide(
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        '<p:spPr/><p:txBody><a:bodyPr/>'
        '<a:p><a:r><a:rPr><a:hlinkClick r:id="rId5"/></a:rPr><a:t>Jump to the second slide</a:t></a:r></a:p>'
        '<a:p><a:r><a:rPr><a:hlinkClick r:id="rId6"/></a:rPr><a:t>External link</a:t></a:r></a:p>'
        '</p:txBody></p:sp>')
    slide1_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slide2.xml"/>
<Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/>
</Relationships>"""
    slide2 = slide(
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        '<p:spPr/><p:txBody><a:bodyPr/>'
        '<a:p><a:r><a:t>Second slide content</a:t></a:r></a:p>'
        '</p:txBody></p:sp>')
    ct = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"""
    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""
    write_zip(out / "pptx" / "handmade-links.pptx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", root_rels),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
        ("ppt/slides/slide1.xml", slide1),
        ("ppt/slides/_rels/slide1.xml.rels", slide1_rels),
        ("ppt/slides/slide2.xml", slide2),
    ])


def abuse(out):
    a = out / "abuse"
    a.mkdir(parents=True, exist_ok=True)
    # S1: a DrawingML table cell with an astronomically large span area.
    span_pres = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation {PPTX_NS}>
<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
</p:presentation>"""
    span_pres_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"""
    span_slide = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld {PPTX_NS}>
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="2" name="T"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
<a:tbl><a:tr>
<a:tc gridSpan="2000000000" rowSpan="2000000000"><a:txBody><a:bodyPr/><a:p><a:r><a:t>x</a:t></a:r></a:p></a:txBody></a:tc>
</a:tr></a:tbl>
</a:graphicData></a:graphic></p:graphicFrame>
</p:spTree></p:cSld></p:sld>"""
    span_ct = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"""
    span_root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""
    write_zip(a / "hugespan--errors.pptx", [
        ("[Content_Types].xml", span_ct),
        ("_rels/.rels", span_root_rels),
        ("ppt/presentation.xml", span_pres),
        ("ppt/_rels/presentation.xml.rels", span_pres_rels),
        ("ppt/slides/slide1.xml", span_slide),
    ])


def generate(out):
    inherit_pptx(out)
    strict_alt(out)
    order_pptx(out)
    links_pptx(out)
    math_pptx(out)
    abuse(out)
