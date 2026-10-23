from pathlib import Path

from fixturegen.common import Part, write_zip

MIME = {
    ".odt": "application/vnd.oasis.opendocument.text",
    ".ods": "application/vnd.oasis.opendocument.spreadsheet",
    ".odp": "application/vnd.oasis.opendocument.presentation",
}


def write_odf(path: Path, content: str | bytes, *parts: Part) -> None:
    write_zip(path, [("content.xml", content), *parts], mimetype_first=MIME[path.suffix])


def ods_content(body: str) -> str:
    return f"""<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet>{body}</office:spreadsheet></office:body></office:document-content>"""


def gaps_ods(out: Path) -> None:
    """Internal gaps keep their coordinates; only trailing filler is elided."""
    body = """
<table:table table:name="RowGap">
<table:table-row><table:table-cell office:value-type="string"><text:p>top</text:p>\
</table:table-cell></table:table-row>
<table:table-row table:number-rows-repeated="120"><table:table-cell/></table:table-row>
<table:table-row><table:table-cell office:value-type="string">\
<text:p>row 122 after the gap</text:p></table:table-cell></table:table-row>
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
"""
    write_odf(out / "ods" / "handmade-gaps.ods", ods_content(body))


def hidden_ods(out: Path) -> None:
    """A workbook that hides: a sheet through a child of a hiding table style,
    rows through collapse, filter and a hidden group, columns
    through collapse, filter and a hidden group (a shown group inside it stays
    hidden), the content a hidden cell holds (a note, a missing object), and
    merges whose origin or middle a hidden row or column cuts."""
    cell = '<table:table-cell office:value-type="string"><text:p>{}</text:p></table:table-cell>'
    covered = "<table:covered-table-cell/>"
    span = (
        '<table:table-cell office:value-type="string" table:number-columns-spanned="{}" '
        'table:number-rows-spanned="{}"><text:p>{}</text:p></table:table-cell>'
    )

    def row(*cells: str, attrs: str = "") -> str:
        return "<table:table-row{}>{}</table:table-row>".format(attrs, "".join(cells))

    def text(*values: str) -> list[str]:
        return [cell.format(value) for value in values]

    collapse = ' table:visibility="collapse"'
    hidden_note = (
        '<table:table-cell office:value-type="string"><text:p>never read'
        '<text:note text:id="ftn1" text:note-class="footnote">'
        "<text:note-citation>1</text:note-citation>"
        "<text:note-body><text:p>never parsed</text:p></text:note-body></text:note></text:p>"
        '<draw:frame><draw:object xlink:href="./Missing"/></draw:frame></table:table-cell>'
    )
    report = "\n".join(
        [
            '<table:table table:name="Report">',
            "<table:table-column/>",
            '<table:table-column table:visibility="collapse"/>',
            '<table:table-column-group table:display="false"><table:table-column/>'
            '<table:table-column-group table:display="true"><table:table-column/>'
            "</table:table-column-group>"
            "</table:table-column-group>",
            "<table:table-column/>",
            '<table:table-column table:visibility="filter"/>',
            "<table:table-column/>",
            row(*text("Item", "gone b", "gone c", "gone d", "Count", "gone f", "Note")),
            row(*text("apple", "gone", "gone", "gone", "3", "gone", "fresh")),
            row(
                *text("filtered", "gone", "gone", "gone", "9", "gone", "gone"),
                attrs=' table:visibility="filter"',
            ),
            row(*text("pear", "gone", "gone", "gone", "4", "gone", "ripe")),
            '<table:table-row-group table:display="false">',
            row(*text("group one", "", "", "", "1", "", "gone")),
            '<table:table-row-group table:display="true">',
            row(*text("group two", "", "", "", "2", "", "gone")),
            "</table:table-row-group></table:table-row-group>",
            row(*text("plum", "gone", "gone", "gone", "5", "gone", "dark")),
            row(hidden_note, attrs=collapse),
            row(*text("fig", "", "", "", "6", "", "dry")),
            "</table:table>",
        ]
    )
    merged = "\n".join(
        [
            '<table:table table:name="Merged">',
            '<table:table-column/><table:table-column table:visibility="collapse"/>'
            "<table:table-column/><table:table-column/>",
            row(span.format(3, 1, "across"), covered, covered, *text("end")),
            row(
                span.format(1, 2, "moved down"),
                *text("gone 1", "gone 2", "gone 3"),
                attrs=collapse,
            ),
            row(covered, *text("k", "l", "m")),
            row(*text("n"), span.format(2, 1, "moved right"), covered, *text("o")),
            row(
                span.format(1, 2, "vanishes"), *text("gone 4", "gone 5", "gone 6"), attrs=collapse
            ),
            row(covered, *text("gone 7", "gone 8", "gone 9"), attrs=collapse),
            row(*text("last", "p", "q", "r")),
            "</table:table>",
        ]
    )
    secret = (
        '<table:table table:name="Secret" table:style-name="Archive">'
        + row(*text("a hidden sheet"))
        + "</table:table>"
    )
    content = f"""<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
 xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
 xmlns:xlink="http://www.w3.org/1999/xlink" office:version="1.3">
<office:automatic-styles>
<style:style style:name="Hidden" style:family="table"><style:table-properties \
table:display="false"/></style:style>
<style:style style:name="Archive" style:family="table" style:parent-style-name="Hidden"/>
</office:automatic-styles>
<office:body><office:spreadsheet>
{report}
{secret}
{merged}
</office:spreadsheet></office:body></office:document-content>"""
    write_odf(out / "ods" / "handmade-hidden.ods", content)


def math_odt(out: Path) -> None:
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
<draw:object xlink:href="./Object 1" xlink:type="simple" xlink:show="embed" \
xlink:actuate="onLoad"/>
<draw:image xlink:href="./ObjectReplacements/Object 1" xlink:type="simple" xlink:show="embed" \
xlink:actuate="onLoad"/>
</draw:frame> on the board.</text:p>
<text:p>Inline object: <draw:frame draw:name="Object2" text:anchor-type="as-char"><draw:object>
<math:math><math:semantics><math:mrow><math:mfrac><math:mi>a</math:mi><math:mi>b</math:mi></math:mfrac>
<math:mo>+</math:mo><math:msqrt><math:mi>x</math:mi></math:msqrt></math:mrow></math:semantics></math:math>
</draw:object></draw:frame> done.</text:p>
</office:text></office:body></office:document-content>"""
    manifest = """<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" \
manifest:version="1.3">
<manifest:file-entry manifest:full-path="/" \
manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="Object 1/content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="Object 1/" \
manifest:media-type="application/vnd.oasis.opendocument.formula"/>
<manifest:file-entry manifest:full-path="ObjectReplacements/Object 1" manifest:media-type=""/>
</manifest:manifest>"""
    write_odf(
        out / "odt" / "handmade-math.odt",
        content,
        ("META-INF/manifest.xml", manifest),
        ("Object 1/content.xml", formula),
        ("ObjectReplacements/Object 1", b"VCLMTF\x00replacement-stand-in"),
    )


def defaults_odt(out: Path) -> None:
    """style:default-style beneath named chains."""
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
    write_odf(out / "odt" / "handmade-defaults.odt", content_xml, ("styles.xml", styles_xml))


def durations_ods(out: Path) -> None:
    """Full ISO durations."""
    durations = """<table:table table:name="Durations">
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
</table:table>"""
    write_odf(out / "ods" / "handmade-durations.ods", ods_content(durations))


def lists_odt(out: Path) -> None:
    """List headers render without markers, start-value restarts split the run, and
    continue-list resolves by the target's xml:id."""
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
    write_odf(out / "odt" / "handmade-lists.odt", lists_xml)


def manifest_odt(out: Path) -> None:
    """Encryption is detected from the actual manifest:encryption-data element: a comment
    mentioning it must not classify the document as encrypted, while a real element must."""
    plain_content = """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:text><text:p>Not encrypted at all.</text:p></office:text></office:body>
</office:document-content>"""
    manifest_comment = """<?xml version="1.0"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
<!-- this comment mentions encryption-data but the package is plain -->
<manifest:file-entry manifest:full-path="/" \
manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"""
    write_odf(
        out / "odt" / "handmade-manifestcomment.odt",
        plain_content,
        ("META-INF/manifest.xml", manifest_comment),
    )
    manifest_encrypted = """<?xml version="1.0"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml">
<manifest:encryption-data manifest:checksum-type="SHA1/1K" manifest:checksum="AAAA">
<manifest:algorithm manifest:algorithm-name="Blowfish CFB" manifest:initialisation-vector="BBBB"/>
<manifest:key-derivation manifest:key-derivation-name="PBKDF2" manifest:iteration-count="1024" \
manifest:salt="CCCC"/>
</manifest:encryption-data>
</manifest:file-entry>
</manifest:manifest>"""
    write_odf(
        out / "malformed" / "encrypted--errors.odt",
        b"\x00\x01ciphertext-stand-in",
        ("META-INF/manifest.xml", manifest_encrypted),
    )


def blockstyle_odt(out: Path) -> None:
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
    write_odf(out / "odt" / "handmade-blockstyle.odt", content_xml, ("styles.xml", styles_xml))


def position_odt(out: Path) -> None:
    """Superscript and subscript set directly by automatic styles in both spellings of
    `style:text-position`, through named text styles, and cancelled by `0%` over a raised
    parent; DOCX and EPUB build the same text."""
    styles_xml = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" office:version="1.3">
<office:styles>
<style:style style:name="Raised" style:family="text">
<style:text-properties style:text-position="super 58%"/></style:style>
<style:style style:name="Lowered" style:family="text">
<style:text-properties style:text-position="-33% 58%"/></style:style>
</office:styles></office:document-styles>"""
    content_xml = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3">
<office:automatic-styles>
<style:style style:name="T1" style:family="text"><style:text-properties \
style:text-position="33% 58%"/></style:style>
<style:style style:name="T2" style:family="text"><style:text-properties \
style:text-position="sub 58%"/></style:style>
<style:style style:name="T3" style:family="text" style:parent-style-name="Raised">\
<style:text-properties style:text-position="0% 100%"/></style:style>
</office:automatic-styles>
<office:body><office:text>
<text:p>Direct: 10<text:span text:style-name="T1">-3</text:span> M and H<text:span \
text:style-name="T2">2</text:span>O.</text:p>
<text:p>Through a style: x<text:span text:style-name="Raised">2</text:span> and CO<text:span \
text:style-name="Lowered">2</text:span>.</text:p>
<text:p>Baseline over a superscript style: <text:span \
text:style-name="T3">level</text:span>.</text:p>
</office:text></office:body></office:document-content>"""
    write_odf(out / "odt" / "handmade-position.odt", content_xml, ("styles.xml", styles_xml))


def hidden_odt(out: Path) -> None:
    """Text hidden by a span style, by a paragraph style a span overrides, by
    a hidden-paragraph field, hidden-text and a hidden section; a hidden note
    reference whose note never parses, and a hidden list item that takes no
    number. ODF has no paragraph mark, so nothing joins."""
    styles_xml = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3">
<office:styles>
<style:style style:name="Hidden_20_Paragraph" style:display-name="Hidden Paragraph" \
style:family="paragraph">
<style:text-properties text:display="none"/>
</style:style>
</office:styles></office:document-styles>"""
    content_xml = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3">
<office:automatic-styles>
<style:style style:name="T1" style:family="text"><style:text-properties text:display="none"/>\
</style:style>
<style:style style:name="T2" style:family="text"><style:text-properties text:display="true"/>\
</style:style>
<style:style style:name="P1" style:family="paragraph" \
style:parent-style-name="Hidden_20_Paragraph"/>
<text:list-style style:name="L1">
<text:list-level-style-number text:level="1" style:num-suffix="." style:num-format="1"/>
</text:list-style>
</office:automatic-styles>
<office:body><office:text>
<text:p>Visible text<text:span text:style-name="T1"> with a hidden span</text:span> \
continues.</text:p>
<text:p text:style-name="P1">A hidden paragraph style hides this, <text:span \
text:style-name="T2">but this span shows</text:span>.</text:p>
<text:p>A hidden-paragraph field hides this paragraph.<text:hidden-paragraph \
text:condition="ooow:1" text:is-hidden="true"/></text:p>
<text:p>Hidden text <text:hidden-text text:condition="ooow:1" text:string-value="vanishes " \
text:is-hidden="true">vanishes </text:hidden-text>here; shown text <text:hidden-text \
text:condition="ooow:0" text:string-value="stays" \
text:is-hidden="false">stays</text:hidden-text>.</text:p>
<text:section text:name="Hidden section" text:display="none"><text:p>A hidden section.</text:p>\
</text:section>
<text:p>A hidden note reference<text:span text:style-name="T1">\
<text:note text:id="ftn1" text:note-class="footnote"><text:note-citation>1</text:note-citation>\
<text:note-body><text:p>Never parsed.</text:p></text:note-body></text:note>\
</text:span> and a shown one<text:note text:id="ftn2" text:note-class="footnote">\
<text:note-citation>1</text:note-citation><text:note-body>\
<text:p>Shown note<text:span text:style-name="T1"> with hidden words</text:span>.</text:p>\
</text:note-body></text:note>.</text:p>
<text:list text:style-name="L1">
<text:list-item><text:p>First item</text:p></text:list-item>
<text:list-item><text:p text:style-name="P1">Hidden item</text:p></text:list-item>
<text:list-item><text:p>Second item</text:p></text:list-item>
</text:list>
</office:text></office:body></office:document-content>"""
    write_odf(out / "odt" / "handmade-hidden.odt", content_xml, ("styles.xml", styles_xml))


def splitlist_odp(out: Path) -> None:
    """LibreOffice writes a level that follows a shallower one as an item
    holding only a nested list: it shows no label, takes no number and
    nests under the item before it, or stands alone when none precedes."""
    content_xml = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
 xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
 xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" office:version="1.3">
<office:automatic-styles>
<text:list-style style:name="L1">
<text:list-level-style-number text:level="1" style:num-suffix="." style:num-format="1"/>
<text:list-level-style-number text:level="2" style:num-format="1" text:display-levels="2"/>
</text:list-style>
</office:automatic-styles>
<office:body><office:presentation>
<draw:page draw:name="Split">
<draw:frame presentation:class="title"><draw:text-box><text:p>Split list levels</text:p>\
</draw:text-box></draw:frame>
<draw:frame presentation:class="outline"><draw:text-box>
<text:list text:style-name="L1">
<text:list-item><text:p>First point</text:p></text:list-item>
<text:list-item><text:list><text:list-item><text:p>Detail under the first point</text:p>\
</text:list-item></text:list></text:list-item>
<text:list-item><text:p>Second point</text:p></text:list-item>
</text:list>
</draw:text-box></draw:frame>
<draw:frame presentation:class="outline"><draw:text-box>
<text:list text:style-name="L1">
<text:list-item><text:list><text:list-item><text:p>Detail with no point before it</text:p>\
</text:list-item></text:list></text:list-item>
<text:list-item><text:p>Point after the detail</text:p></text:list-item>
</text:list>
</draw:text-box></draw:frame>
</draw:page>
</office:presentation></office:body></office:document-content>"""
    write_odf(out / "odp" / "handmade-splitlist.odp", content_xml)


def abuse(out: Path) -> None:
    sheets = [
        (
            "max_expansion--hugerepeat--errors.ods",
            """<table:table table:name="S">
<table:table-row table:number-rows-repeated="9000000">
<table:table-cell office:value-type="string" table:number-columns-repeated="1000">\
<text:p>x</text:p></table:table-cell>
</table:table-row>
</table:table>""",
        ),
        # A single cell whose declared span area is astronomically large; the
        # complete area must be charged against the expansion budget *before* any
        # per-position work happens.
        (
            "max_expansion--hugespan--errors.ods",
            """<table:table table:name="S">
<table:table-row>
<table:table-cell table:number-rows-spanned="2000000000" \
table:number-columns-spanned="2000000000"><text:p>x</text:p></table:table-cell>
</table:table-row>
</table:table>""",
        ),
        # A repeated row of present but empty cells is charged against the expansion budget
        # for every repeat.
        (
            "max_expansion--emptyrowrepeat--errors.ods",
            """<table:table table:name="S">
<table:table-row table:number-rows-repeated="9000000000000000"><table:table-cell><text:p/>\
</table:table-cell></table:table-row>
<table:table-row><table:table-cell><text:p>tail</text:p></table:table-cell></table:table-row>
</table:table>""",
        ),
        # One 64 KiB cell repeated 1026 times: its duplicated text passes the
        # 64 MiB expansion-text limit while the cell count stays far under its own.
        (
            "max_expansion_text_bytes--longrepeat--errors.ods",
            f"""<table:table table:name="S">
<table:table-row>
<table:table-cell office:value-type="string" table:number-columns-repeated="1026">\
<text:p>{"x" * 65536}</text:p></table:table-cell>
</table:table-row>
</table:table>""",
        ),
    ]
    for name, body in sheets:
        write_odf(out / "abuse" / name, ods_content(body))
    # 100 001 empty slides, one past the default max_pages.
    pageflood_odp = (
        """<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0">
<office:body><office:presentation>"""
        + "<draw:page/>" * 100_001
        + """</office:presentation>
</office:body></office:document-content>"""
    )
    write_odf(out / "abuse" / "max_pages--pageflood--errors.odp", pageflood_odp)


def generate(out: Path) -> None:
    gaps_ods(out)
    hidden_ods(out)
    blockstyle_odt(out)
    defaults_odt(out)
    durations_ods(out)
    lists_odt(out)
    manifest_odt(out)
    math_odt(out)
    position_odt(out)
    hidden_odt(out)
    splitlist_odp(out)
    abuse(out)
