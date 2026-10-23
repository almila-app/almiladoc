import struct
from collections.abc import Iterable
from pathlib import Path

from fixturegen.common import DECL, Part, Rel, content_types, rels, write_cfb, write_zip


def write_one_sheet_xlsx(path: Path, name: str, sheet: str) -> None:
    workbook = f"""{DECL}
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="{name}" sheetId="1" r:id="rId1"/></sheets></workbook>"""
    write_zip(
        path,
        [
            (
                "[Content_Types].xml",
                content_types(
                    ("/xl/workbook.xml", "spreadsheetml.sheet.main"),
                    ("/xl/worksheets/sheet1.xml", "spreadsheetml.worksheet"),
                ),
            ),
            ("_rels/.rels", rels(("rId1", "officeDocument", "xl/workbook.xml"))),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels(("rId1", "worksheet", "worksheets/sheet1.xml"))),
            ("xl/worksheets/sheet1.xml", sheet),
        ],
    )


def write_xlsb(
    path: Path, workbook: bytes, *parts: Part, sheets: int = 1, extra_rels: Iterable[Rel] = ()
) -> None:
    sheet_rows = [
        (f"rId{n}", "worksheet", f"worksheets/sheet{n}.bin") for n in range(1, sheets + 1)
    ]
    write_zip(
        path,
        [
            ("_rels/.rels", rels(("rId1", "officeDocument", "xl/workbook.bin"))),
            ("xl/workbook.bin", workbook),
            ("xl/_rels/workbook.bin.rels", rels(*sheet_rows, *extra_rels)),
            *parts,
        ],
    )


def merged_xlsx(out: Path) -> None:
    """Merged ranges in spreadsheets become spanning grid cells."""
    sheet = f"""{DECL}
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
    write_one_sheet_xlsx(out / "xlsx" / "handmade-merged.xlsx", "Merged", sheet)


BRT_ROW_HDR = 0
BRT_CELL_RK = 2
BRT_CELL_ERROR = 3
BRT_CELL_BOOL = 4
BRT_CELL_REAL = 5
BRT_CELL_ST = 6
BRT_CELL_ISST = 7
BRT_SST_ITEM = 19
BRT_FMT = 44
BRT_XF = 47
BRT_COL_INFO = 60
BRT_WB_PROP = 153
BRT_BUNDLE_SH = 156
BRT_MERGE_CELL = 176
BRT_BEGIN_MERGE_CELLS = 177
BRT_END_MERGE_CELLS = 178
BRT_BEGIN_CELL_XFS = 617
BRT_END_CELL_XFS = 618


def xlsb_rec(rec_id: int, payload: bytes = b"") -> bytes:
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


def xlsb_str(s: str) -> bytes:
    data = s.encode("utf-16-le")
    return struct.pack("<I", len(data) // 2) + data


def xlsb_cell(col: int, style: int = 0) -> bytes:
    return struct.pack("<II", col, style)


def xlsb_row(r: int, hidden: bool = False) -> bytes:
    flags = 0x10 if hidden else 0
    return xlsb_rec(BRT_ROW_HDR, struct.pack("<IIHBBBI", r, 0, 0, 0, flags, 0, 0))


def xlsb_xf(ifmt: int) -> bytes:
    return xlsb_rec(BRT_XF, struct.pack("<HH", 0, ifmt) + bytes(12))


def xlsb_isst(col: int, i: int) -> bytes:
    return xlsb_rec(BRT_CELL_ISST, xlsb_cell(col) + struct.pack("<I", i))


def xlsb_real(col: int, style: int, v: float) -> bytes:
    return xlsb_rec(BRT_CELL_REAL, xlsb_cell(col, style) + struct.pack("<d", v))


def xlsb_rk(col: int, style: int, v: int) -> bytes:
    return xlsb_rec(BRT_CELL_RK, xlsb_cell(col, style) + struct.pack("<I", v))


def xlsb_st(col: int, s: str) -> bytes:
    return xlsb_rec(BRT_CELL_ST, xlsb_cell(col) + xlsb_str(s))


def sheet_xlsb(out: Path) -> None:
    """Binary SpreadsheetML records ([MS-XLSB]): variable-length record framing (two-byte ids,
    multi-byte sizes via a long shared string), RK integer and float cells with and without the
    /100 bit, number formats, a hidden row, a hidden column, a hidden sheet, and a merge
    extending past the populated range."""
    workbook = (
        xlsb_rec(BRT_WB_PROP, struct.pack("<II", 0, 0) + xlsb_str(""))  # 1900 dates
        + xlsb_rec(BRT_BUNDLE_SH, struct.pack("<II", 0, 1) + xlsb_str("rId1") + xlsb_str("Data"))
        + xlsb_rec(BRT_BUNDLE_SH, struct.pack("<II", 1, 2) + xlsb_str("rId2") + xlsb_str("Secret"))
    )
    # cellXfs: 0 General, 1 percent, 2 currency, 3 builtin date (ifmt 14).
    styles = (
        xlsb_rec(BRT_FMT, struct.pack("<H", 164) + xlsb_str("0.0%"))
        + xlsb_rec(BRT_FMT, struct.pack("<H", 165) + xlsb_str('"$"#,##0.00'))
        + xlsb_rec(BRT_BEGIN_CELL_XFS, struct.pack("<I", 4))
        + xlsb_xf(0)
        + xlsb_xf(164)
        + xlsb_xf(165)
        + xlsb_xf(14)
        + xlsb_rec(BRT_END_CELL_XFS)
    )
    # The long entry forces a record size beyond one byte.
    sst_items = ["Region", "north " * 40]
    shared = b"".join(xlsb_rec(BRT_SST_ITEM, b"\x00" + xlsb_str(s)) for s in sst_items)
    merge_a5_c6 = struct.pack("<IIII", 4, 5, 0, 2)  # past the populated rows
    data = (
        xlsb_rec(BRT_COL_INFO, struct.pack("<IIIIH", 3, 3, 0, 0, 1))  # column D hidden
        + xlsb_row(0)
        + xlsb_isst(0, 0)
        + xlsb_isst(1, 1)
        + xlsb_st(2, "Notes")
        + xlsb_st(3, "hidden column")
        + xlsb_row(1)
        + xlsb_rk(0, 0, (42 << 2) | 2)  # RK integer 42
        + xlsb_rk(1, 1, (65 << 2) | 2 | 1)  # RK integer/100: 65% via 0.65
        + xlsb_real(2, 2, 1234.5)  # currency
        + xlsb_row(2, hidden=True)
        + xlsb_st(0, "hidden row")
        + xlsb_row(3)
        + xlsb_rk(0, 0, struct.unpack("<Q", struct.pack("<d", 1.5))[0] >> 32)  # RK float
        + xlsb_rk(1, 0, (struct.unpack("<Q", struct.pack("<d", 1.5))[0] >> 32) | 1)  # RK float/100
        + xlsb_real(2, 3, 46096.0)  # builtin date format
        + xlsb_row(4)
        + xlsb_st(0, "wide merge")
        + xlsb_rec(BRT_CELL_BOOL, xlsb_cell(1) + b"\x01")  # TRUE
        + xlsb_rec(BRT_CELL_ERROR, xlsb_cell(2) + b"\x07")  # #DIV/0!
        + xlsb_rec(BRT_BEGIN_MERGE_CELLS, struct.pack("<I", 1))
        + xlsb_rec(BRT_MERGE_CELL, merge_a5_c6)
        + xlsb_rec(BRT_END_MERGE_CELLS)
    )
    secret = xlsb_row(0) + xlsb_st(0, "must not appear")
    write_xlsb(
        out / "xlsb" / "handmade-sheet.xlsb",
        workbook,
        ("xl/styles.bin", styles),
        ("xl/sharedStrings.bin", shared),
        ("xl/worksheets/sheet1.bin", data),
        ("xl/worksheets/sheet2.bin", secret),
        sheets=2,
        extra_rels=[
            ("rId3", "styles", "styles.bin"),
            ("rId4", "sharedStrings", "sharedStrings.bin"),
        ],
    )


TABLE_10X5 = [[f"{chr(65 + c)}{r + 1}" for c in range(5)] for r in range(10)]


def trailing_space_xlsx(out: Path) -> None:
    """A whitespace cell at the grid's last row must not widen a 10-row table past its content;
    the XLSB and XLS fixtures hold the same table."""
    rows = "".join(
        f'<row r="{r + 1}">'
        + "".join(f'<c r="{v}" t="inlineStr"><is><t>{v}</t></is></c>' for v in row)
        + "</row>"
        for r, row in enumerate(TABLE_10X5)
    )
    sheet = f"""{DECL}
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>{rows}<row r="1048576"><c r="E1048576" t="inlineStr"><is><t xml:space="preserve"> </t>\
</is></c></row></sheetData>
</worksheet>"""
    write_one_sheet_xlsx(out / "xlsx" / "handmade-trailing-space.xlsx", "Filled", sheet)


def trailing_space_xlsb(out: Path) -> None:
    workbook = xlsb_rec(BRT_WB_PROP, struct.pack("<II", 0, 0) + xlsb_str("")) + xlsb_rec(
        BRT_BUNDLE_SH, struct.pack("<II", 0, 1) + xlsb_str("rId1") + xlsb_str("Filled")
    )
    data = (
        b"".join(
            xlsb_row(r) + b"".join(xlsb_st(c, v) for c, v in enumerate(row))
            for r, row in enumerate(TABLE_10X5)
        )
        + xlsb_row(1048575)
        + xlsb_st(4, " ")
    )
    write_xlsb(
        out / "xlsb" / "handmade-trailing-space.xlsb", workbook, ("xl/worksheets/sheet1.bin", data)
    )


BOF = 0x0809
EOF_REC = 0x000A
BOUNDSHEET = 0x0085
XF = 0x00E0
LABEL = 0x0204
USES_ELFS = 0x0160

GLOBALS_SUBSTREAM = 0x0005
WORKSHEET_SUBSTREAM = 0x0010


def biff_rec(rec_type: int, body: bytes = b"") -> bytes:
    return struct.pack("<HH", rec_type, len(body)) + body


def biff_bof(substream: int) -> bytes:
    return biff_rec(BOF, struct.pack("<HH", 0x0600, substream) + bytes(12))


def biff_label(row: int, col: int, text: str) -> bytes:
    data = text.encode("ascii")
    return biff_rec(LABEL, struct.pack("<HHHHB", row, col, 0, len(data), 0) + data)


def biff_boundsheet(offset: int) -> bytes:
    return biff_rec(BOUNDSHEET, struct.pack("<IBBBB", offset, 0, 0, 6, 0) + b"Filled")


def write_xls(
    path: Path, sheet_records: bytes, globals_eof: bool = True, sheets: bool = True
) -> None:
    """One BIFF8 worksheet named "Filled" (or none), padded to the CFB
    mini-stream cutoff with a skipped record so no stray bytes follow."""
    globals_head = biff_bof(GLOBALS_SUBSTREAM) + biff_rec(XF, bytes(20))
    directory_len = len(biff_boundsheet(0)) if sheets else 0
    eof = biff_rec(EOF_REC)
    sheet = biff_bof(WORKSHEET_SUBSTREAM) + sheet_records + eof if sheets else b""
    body_len = len(globals_head) + directory_len + (len(eof) if globals_eof else 0) + len(sheet)
    filler = biff_rec(USES_ELFS, bytes(max(0, 4096 - body_len - 4)))
    offset = len(globals_head) + len(filler) + directory_len + (len(eof) if globals_eof else 0)
    stream = (
        globals_head
        + filler
        + (biff_boundsheet(offset) if sheets else b"")
        + (eof if globals_eof else b"")
        + sheet
    )
    write_cfb(path, [("Workbook", stream)])


def trailing_space_xls(out: Path) -> None:
    cells = b"".join(
        biff_label(r, c, v) for r, row in enumerate(TABLE_10X5) for c, v in enumerate(row)
    )
    write_xls(out / "xls" / "handmade-trailing-space.xls", cells + biff_label(65535, 255, " "))


def malformed(out: Path) -> None:
    write_xls(out / "malformed" / "nosheet--errors.xls", b"", sheets=False)
    write_xls(
        out / "malformed" / "unended-globals--recovers.xls",
        biff_label(0, 0, "kept"),
        globals_eof=False,
    )


def abuse(out: Path) -> None:
    sheet = f"""{DECL}
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row>
<row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>b</t></is></c></row>
</sheetData>
</worksheet>"""
    write_one_sheet_xlsx(out / "abuse" / "max_grid_slots--corners--errors.xlsx", "Corners", sheet)


def generate(out: Path) -> None:
    merged_xlsx(out)
    sheet_xlsb(out)
    trailing_space_xlsx(out)
    trailing_space_xlsb(out)
    trailing_space_xls(out)
    malformed(out)
    abuse(out)
