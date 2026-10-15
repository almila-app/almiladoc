from fixturegen.common import write_zip


# ---------------------------------------------------------------------------
# R5: internal ODF gaps keep their coordinates; only trailing filler elided

def gaps_ods(out):
    content = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet>
<table:table table:name="RowGap">
<table:table-row><table:table-cell office:value-type="string"><text:p>top</text:p></table:table-cell></table:table-row>
<table:table-row table:number-rows-repeated="120"><table:table-cell/></table:table-row>
<table:table-row><table:table-cell office:value-type="string"><text:p>row 122 after the gap</text:p></table:table-cell></table:table-row>
<table:table-row table:number-rows-repeated="500"><table:table-cell/></table:table-row>
</table:table>
<table:table table:name="CellGap">
<table:table-row>
<table:table-cell office:value-type="string"><text:p>left</text:p></table:table-cell>
<table:table-cell table:number-columns-repeated="1010"/>
<table:table-cell office:value-type="string"><text:p>column 1012</text:p></table:table-cell>
<table:table-cell table:number-columns-repeated="200"/>
</table:table-row>
</table:table>
</office:spreadsheet></office:body></office:document-content>"""
    write_zip(out / "ods" / "handmade-gaps.ods",
              [("content.xml", content)],
              mimetype_first="application/vnd.oasis.opendocument.spreadsheet")


def math_odt(out):
    """LibreOffice stores a formula as an object directory holding MathML
    (with a StarMath annotation) next to a replacement metafile; ODF also
    allows the MathML inline in the draw:object."""
    formula = """<?xml version="1.0" encoding="UTF-8"?>
<math xmlns="http://www.w3.org/1998/Math/MathML" display="block">
<semantics>
<mrow><mi>E</mi><mo stretchy="false">=</mo><mi>m</mi><msup><mi>c</mi><mn>2</mn></msup></mrow>
<annotation encoding="StarMath 5.0">E = m c^2</annotation>
</semantics>
</math>"""
    content = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
 xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
 xmlns:xlink="http://www.w3.org/1999/xlink"
 xmlns:math="http://www.w3.org/1998/Math/MathML" office:version="1.3">
<office:body><office:text>
<text:p>Einstein wrote <draw:frame draw:name="Object1" text:anchor-type="as-char">
<draw:object xlink:href="./Object 1" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>
<draw:image xlink:href="./ObjectReplacements/Object 1" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>
</draw:frame> on the board.</text:p>
<text:p>Inline object: <draw:frame draw:name="Object2" text:anchor-type="as-char"><draw:object>
<math:math><math:semantics><math:mrow><math:mfrac><math:mi>a</math:mi><math:mi>b</math:mi></math:mfrac>
<math:mo>+</math:mo><math:msqrt><math:mi>x</math:mi></math:msqrt></math:mrow></math:semantics></math:math>
</draw:object></draw:frame> done.</text:p>
</office:text></office:body></office:document-content>"""
    manifest = """<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="Object 1/content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="Object 1/" manifest:media-type="application/vnd.oasis.opendocument.formula"/>
<manifest:file-entry manifest:full-path="ObjectReplacements/Object 1" manifest:media-type=""/>
</manifest:manifest>"""
    write_zip(out / "odt" / "handmade-math.odt", [
        ("content.xml", content),
        ("META-INF/manifest.xml", manifest),
        ("Object 1/content.xml", formula),
        ("ObjectReplacements/Object 1", b"VCLMTF\x00replacement-stand-in"),
    ], mimetype_first="application/vnd.oasis.opendocument.text")


# ---------------------------------------------------------------------------
# R16: ODF style:default-style beneath named chains; full ISO durations

def defaults_odf(out):
    styles_xml = """<?xml version="1.0"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0">
<office:styles>
<style:default-style style:family="paragraph">
<style:text-properties fo:font-weight="bold"/>
</style:default-style>
<style:style style:name="Norm" style:family="paragraph"/>
<style:style style:name="Off" style:family="paragraph">
<style:text-properties fo:font-weight="normal"/>
</style:style>
</office:styles>
</office:document-styles>"""
    content_xml = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:text>
<text:p text:style-name="Norm">Bold via the family default style</text:p>
<text:p text:style-name="Off">Explicitly back to normal weight</text:p>
<text:p>Unstyled paragraph also inherits the default</text:p>
</office:text></office:body></office:document-content>"""
    write_zip(out / "odt" / "handmade-defaults.odt", [
        ("content.xml", content_xml),
        ("styles.xml", styles_xml),
    ], mimetype_first="application/vnd.oasis.opendocument.text")

    durations = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="Durations">
<table:table-row>
<table:table-cell office:value-type="time" office:time-value="P1DT2H"/>
<table:table-cell office:value-type="time" office:time-value="PT26H30M15S"/>
<table:table-cell office:value-type="time" office:time-value="P2W"/>
<table:table-cell office:value-type="time" office:time-value="-PT1H5M"/>
<table:table-cell office:value-type="time" office:time-value="P1M"/>
</table:table-row>
<table:table-row>
<table:table-cell office:value-type="time" office:time-value="P0.5D"/>
<table:table-cell office:value-type="time" office:time-value="PT1.5H"/>
<table:table-cell office:value-type="time" office:time-value="PT1.5M"/>
<table:table-cell office:value-type="time" office:time-value="PT1.25S"/>
<table:table-cell office:value-type="time" office:time-value="PT99999999999999999999H"/>
</table:table-row>
</table:table></office:spreadsheet></office:body></office:document-content>"""
    write_zip(out / "ods" / "handmade-durations.ods",
              [("content.xml", durations)],
              mimetype_first="application/vnd.oasis.opendocument.spreadsheet")

    # S7: list headers render without markers, start-value restarts split
    # the run, and continue-list resolves by the target's xml:id.
    lists_xml = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:xml="http://www.w3.org/XML/1998/namespace">
<office:automatic-styles>
<text:list-style style:name="Num">
<text:list-level-style-number text:level="1" style:num-format="1"/>
</text:list-style>
</office:automatic-styles>
<office:body><office:text>
<text:list xml:id="lstA" text:style-name="Num">
<text:list-header><text:p>Header without a marker</text:p></text:list-header>
<text:list-item><text:p>one</text:p></text:list-item>
<text:list-item><text:p>two</text:p></text:list-item>
<text:list-item text:start-value="10"><text:p>ten via restart</text:p></text:list-item>
<text:list-item><text:p>eleven</text:p></text:list-item>
</text:list>
<text:p>Interruption paragraph.</text:p>
<text:list text:continue-list="lstA" text:style-name="Num">
<text:list-item><text:p>twelve continues lstA</text:p></text:list-item>
</text:list>
</office:text></office:body></office:document-content>"""
    write_zip(out / "odt" / "handmade-lists.odt",
              [("content.xml", lists_xml)],
              mimetype_first="application/vnd.oasis.opendocument.text")

    # S18: encryption is detected from the actual manifest:encryption-data
    # element - a comment mentioning it must not classify the document as
    # encrypted, while a real element must.
    plain_content = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:text><text:p>Not encrypted at all.</text:p></office:text></office:body>
</office:document-content>"""
    manifest_comment = """<?xml version="1.0"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
<!-- this comment mentions encryption-data but the package is plain -->
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"""
    write_zip(out / "odt" / "handmade-manifestcomment.odt", [
        ("content.xml", plain_content),
        ("META-INF/manifest.xml", manifest_comment),
    ], mimetype_first="application/vnd.oasis.opendocument.text")
    manifest_encrypted = """<?xml version="1.0"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml">
<manifest:encryption-data manifest:checksum-type="SHA1/1K" manifest:checksum="AAAA">
<manifest:algorithm manifest:algorithm-name="Blowfish CFB" manifest:initialisation-vector="BBBB"/>
<manifest:key-derivation manifest:key-derivation-name="PBKDF2" manifest:iteration-count="1024" manifest:salt="CCCC"/>
</manifest:encryption-data>
</manifest:file-entry>
</manifest:manifest>"""
    write_zip(out / "malformed" / "encrypted--errors.odt", [
        ("content.xml", b"\x00\x01ciphertext-stand-in"),
        ("META-INF/manifest.xml", manifest_encrypted),
    ], mimetype_first="application/vnd.oasis.opendocument.text")


def blockstyle_odt(out):
    """LibreOffice names its quote and code containers Quotations and
    Preformatted Text; automatic styles reach them through parent-style-name."""
    styles_xml = """<?xml version="1.0"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0">
<office:styles>
<style:style style:name="Quotations" style:family="paragraph" style:parent-style-name="Standard"/>
<style:style style:name="Preformatted_20_Text" style:display-name="Preformatted Text"
 style:family="paragraph" style:parent-style-name="Standard"/>
</office:styles></office:document-styles>"""
    content_xml = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:automatic-styles>
<style:style style:name="P1" style:family="paragraph"
 style:parent-style-name="Preformatted_20_Text"/>
</office:automatic-styles>
<office:body><office:text>
<text:p>Body before.</text:p>
<text:p text:style-name="Preformatted_20_Text">fn main() {</text:p>
<text:p text:style-name="P1"><text:s text:c="4"/>println!("ok");</text:p>
<text:p text:style-name="Preformatted_20_Text">}</text:p>
<text:p>Body between.</text:p>
<text:p text:style-name="Quotations">A quotation.</text:p>
<text:p text:style-name="Quotations">Its second paragraph.</text:p>
<text:p>Body after.</text:p>
</office:text></office:body></office:document-content>"""
    write_zip(out / "odt" / "handmade-blockstyle.odt",
              [("content.xml", content_xml), ("styles.xml", styles_xml)],
              mimetype_first="application/vnd.oasis.opendocument.text")


def abuse(out):
    a = out / "abuse"
    a.mkdir(parents=True, exist_ok=True)
    repeat_ods = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">
<table:table-row table:number-rows-repeated="9000000">
<table:table-cell office:value-type="string" table:number-columns-repeated="1000"><text:p>x</text:p></table:table-cell>
</table:table-row>
</table:table></office:spreadsheet></office:body></office:document-content>"""
    write_zip(a / "hugerepeat--errors.ods",
              [("content.xml", repeat_ods)],
              mimetype_first="application/vnd.oasis.opendocument.spreadsheet")
    # S1: a single cell whose declared span area is astronomically large; the
    # complete area must be charged against the expansion budget *before* any
    # per-position work happens.
    span_ods = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">
<table:table-row>
<table:table-cell table:number-rows-spanned="2000000000" table:number-columns-spanned="2000000000"><text:p>x</text:p></table:table-cell>
</table:table-row>
</table:table></office:spreadsheet></office:body></office:document-content>"""
    write_zip(a / "hugespan--errors.ods",
              [("content.xml", span_ods)],
              mimetype_first="application/vnd.oasis.opendocument.spreadsheet")
    # S1: a repeated row whose cells are structurally present but empty used
    # to bypass the budget entirely (nothing content-bearing was ever
    # charged) while still looping over the repeat count.
    emptyrepeat_ods = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">
<table:table-row table:number-rows-repeated="9000000000000000"><table:table-cell><text:p/></table:table-cell></table:table-row>
<table:table-row><table:table-cell><text:p>tail</text:p></table:table-cell></table:table-row>
</table:table></office:spreadsheet></office:body></office:document-content>"""
    write_zip(a / "emptyrowrepeat--errors.ods",
              [("content.xml", emptyrepeat_ods)],
              mimetype_first="application/vnd.oasis.opendocument.spreadsheet")


def generate(out):
    gaps_ods(out)
    blockstyle_odt(out)
    defaults_odf(out)
    math_odt(out)
    abuse(out)
