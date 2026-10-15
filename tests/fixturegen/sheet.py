import struct

from fixturegen.common import write_zip


# ---------------------------------------------------------------------------
# R17a: merged ranges in spreadsheets become spanning grid cells

def merged_xlsx(out):
    ct = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"""
    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"""
    workbook = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Merged" sheetId="1" r:id="rId1"/></sheets></workbook>"""
    wb_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"""
    sheet = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1">
<c r="A1" t="inlineStr"><is><t>Merged across</t></is></c>
<c r="C1" t="inlineStr"><is><t xml:space="preserve">  padded  </t></is></c>
</row>
<row r="2">
<c r="A2" t="inlineStr"><is><t>tall</t></is></c>
<c r="B2" t="inlineStr"><is><t>b2</t></is></c>
<c r="C2"><v>3.5</v></c>
</row>
<row r="3"><c r="B3" t="inlineStr"><is><t>b3</t></is></c></row>
</sheetData>
<mergeCells count="2"><mergeCell ref="A1:B1"/><mergeCell ref="A2:A3"/></mergeCells>
</worksheet>"""
    write_zip(out / "xlsx" / "handmade-merged.xlsx", [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", root_rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", wb_rels),
        ("xl/worksheets/sheet1.xml", sheet),
    ])


# ---------------------------------------------------------------------------
# Handmade XLSB: binary SpreadsheetML records (MS-XLSB). Exercises the
# variable-length record framing (two-byte ids, multi-byte sizes via a long
# shared string), RK integer/float cells with and without the /100 bit,
# number formats, a hidden row, a hidden column, a hidden sheet, and a merge
# extending past the populated range.

def xlsb_rec(rec_id, payload=b""):
    out = bytearray()
    if rec_id < 0x80:
        out.append(rec_id)
    else:
        out.append((rec_id & 0x7F) | 0x80)
        out.append(rec_id >> 7)
    size = len(payload)
    while True:
        low = size & 0x7F
        size >>= 7
        if size == 0:
            out.append(low)
            break
        out.append(low | 0x80)
    return bytes(out) + payload


def xlsb_str(s):
    data = s.encode("utf-16-le")
    return struct.pack("<I", len(data) // 2) + data


def xlsb_cell(col, style=0):
    return struct.pack("<II", col, style)


def xlsb_row(r, hidden=False):
    flags = 0x10 if hidden else 0
    return xlsb_rec(0, struct.pack("<IIHBBBI", r, 0, 0, 0, flags, 0, 0))


def sheet_xlsb(out):
    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.bin"/>
</Relationships>"""
    wb_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.bin"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.bin"/>
<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.bin"/>
</Relationships>"""
    workbook = (
        xlsb_rec(153, struct.pack("<II", 0, 0) + xlsb_str(""))  # BrtWbProp, 1900 dates
        + xlsb_rec(156, struct.pack("<II", 0, 1) + xlsb_str("rId1") + xlsb_str("Data"))
        + xlsb_rec(156, struct.pack("<II", 1, 2) + xlsb_str("rId2") + xlsb_str("Secret"))
    )
    # cellXfs: 0 General, 1 percent, 2 currency, 3 builtin date (ifmt 14).
    xf = lambda ifmt: xlsb_rec(47, struct.pack("<HH", 0, ifmt) + bytes(12))
    styles = (
        xlsb_rec(44, struct.pack("<H", 164) + xlsb_str("0.0%"))
        + xlsb_rec(44, struct.pack("<H", 165) + xlsb_str('"$"#,##0.00'))
        + xlsb_rec(617, struct.pack("<I", 4))
        + xf(0) + xf(164) + xf(165) + xf(14)
        + xlsb_rec(618)
    )
    # The long entry forces a record size beyond one byte.
    sst_items = ["Region", "north " * 40]
    shared = b"".join(xlsb_rec(19, b"\x00" + xlsb_str(s)) for s in sst_items)
    isst = lambda col, i: xlsb_rec(7, xlsb_cell(col) + struct.pack("<I", i))
    real = lambda col, style, v: xlsb_rec(5, xlsb_cell(col, style) + struct.pack("<d", v))
    rk = lambda col, style, v: xlsb_rec(2, xlsb_cell(col, style) + struct.pack("<I", v))
    st = lambda col, s: xlsb_rec(6, xlsb_cell(col) + xlsb_str(s))
    data = (
        xlsb_rec(60, struct.pack("<IIIIH", 3, 3, 0, 0, 1))  # column D hidden
        + xlsb_row(0)
        + isst(0, 0) + isst(1, 1) + st(2, "Notes") + st(3, "hidden column")
        + xlsb_row(1)
        + rk(0, 0, (42 << 2) | 2)                       # RK integer 42
        + rk(1, 1, (65 << 2) | 2 | 1)                   # RK integer/100: 65% via 0.65
        + real(2, 2, 1234.5)                            # currency
        + xlsb_row(2, hidden=True)
        + st(0, "hidden row")
        + xlsb_row(3)
        + rk(0, 0, struct.unpack("<Q", struct.pack("<d", 1.5))[0] >> 32)  # RK float
        + rk(1, 0, (struct.unpack("<Q", struct.pack("<d", 1.5))[0] >> 32) | 1)  # RK float/100
        + real(2, 3, 46096.0)                           # builtin date format
        + xlsb_row(4)
        + st(0, "wide merge")
        + xlsb_rec(4, xlsb_cell(1) + b"\x01")           # TRUE
        + xlsb_rec(3, xlsb_cell(2) + b"\x07")           # #DIV/0!
        + xlsb_rec(177, struct.pack("<I", 1))           # BrtBeginMergeCells
        + xlsb_rec(176, struct.pack("<IIII", 4, 5, 0, 2))  # A5:C6, past the populated rows
        + xlsb_rec(178)                                 # BrtEndMergeCells
    )
    secret = xlsb_row(0) + st(0, "must not appear")
    write_zip(out / "xlsb" / "handmade-sheet.xlsb", [
        ("_rels/.rels", root_rels),
        ("xl/workbook.bin", workbook),
        ("xl/_rels/workbook.bin.rels", wb_rels),
        ("xl/styles.bin", styles),
        ("xl/sharedStrings.bin", shared),
        ("xl/worksheets/sheet1.bin", data),
        ("xl/worksheets/sheet2.bin", secret),
    ])


def generate(out):
    merged_xlsx(out)
    sheet_xlsb(out)
