import struct
from collections.abc import Mapping, Sequence
from itertools import accumulate
from pathlib import Path

from fixturegen.common import write_cfb, write_file

FIB_SIZE = 0x400
FIB_END = 0x384  # [MS-DOC] 2.5.1 Fib through cswNew, for nFib 0x00C1
F_EXT_CHAR = 0x1000
W_IDENT = 0xA5EC
FLAGS = 0x0A
CB_MAC = 0x40
CCP_TEXT = 0x4C
FC_STSHF = 0xA2
FC_PLCF_BTE_CHPX = 0xFA
FC_PLCF_BTE_PAPX = 0x102
FC_STTBF_FFN = 0x112
FC_CLX = 0x1A2

SPRM_CF_BOLD = 0x0835
SPRM_CF_ITALIC = 0x0836
SPRM_C_ISS = 0x2A48
SPRM_C_ISTD = 0x4A30
SPRM_CF_VANISH = 0x083C
SPRM_CF_SPEC = 0x0855
SPRM_C_RG_FTC0 = 0x4A4F
SPRM_C_RG_FTC1 = 0x4A50
SPRM_C_RG_FTC2 = 0x4A51
SPRM_C_SYMBOL = 0x6A09

FFN_CHS = 3  # [MS-DOC] FFN: the charset byte
FFN_NAME = 39  # [MS-DOC] FFN: xszFfn, past prq through fs


def clx(pieces: Sequence[tuple[int, int, bool]]) -> bytes:
    """A Clx whose PlcPcd covers `pieces`, each (cp_end, fc, compressed)."""
    cps = [0] + [cp_end for cp_end, _, _ in pieces]
    plc = struct.pack(f"<{len(cps)}I", *cps)
    for _, fc, compressed in pieces:
        fc_compressed = (fc * 2) | 0x40000000 if compressed else fc
        plc += struct.pack("<HIH", 0, fc_compressed, 0)  # Pcd: flags, FcCompressed, prm
    return b"\x02" + struct.pack("<I", len(plc)) + plc


def word_document(
    pieces: Sequence[bytes | str],
    lid: int = 0x0409,
    tables: Sequence[tuple[int, bytes]] = (),
    pages: Sequence[tuple[int, bytes]] = (),
) -> tuple[bytes, bytes]:
    """The WordDocument and 0Table streams of a Word 97 document: a FIB, each text piece named
    by a Clx appended to the table stream, then each 512-byte page `(pn, bytes)`. A `bytes`
    piece is compressed (one byte per CP, [MS-DOC] FcCompressed), a `str` piece UTF-16LE. Each
    table `(fib_offset, part)` is laid out in order and its (fc, lcb) pair packed at
    `fib_offset`."""
    stream = bytearray(FIB_SIZE)
    struct.pack_into("<HH", stream, 0x00, W_IDENT, 0x00C1)  # wIdent, nFib (Word 97)
    struct.pack_into("<H", stream, 0x06, lid)  # FibBase.lid
    struct.pack_into("<HH", stream, FLAGS, F_EXT_CHAR, 0x00BF)  # flags, nFibBack
    struct.pack_into("<H", stream, 0x20, 0x000E)  # csw
    struct.pack_into("<H", stream, 0x3E, 0x0016)  # cslw
    struct.pack_into("<H", stream, 0x98, 0x005D)  # cbRgFcLcb
    placed, cp = [], 0
    for piece in pieces:
        compressed = isinstance(piece, bytes)
        data = piece if compressed else piece.encode("utf-16-le")
        cp += len(data) if compressed else len(data) // 2
        placed.append((cp, len(stream), compressed))
        stream += data
    for pn, page in pages:
        assert len(stream) <= pn * 512 and len(page) == 512
        stream += bytes(pn * 512 - len(stream)) + page
    table = b""
    for fib_offset, part in tables:
        struct.pack_into("<II", stream, fib_offset, len(table), len(part))
        table += part
    pcdt = clx(placed)
    struct.pack_into("<I", stream, CB_MAC, len(stream))
    struct.pack_into("<I", stream, CCP_TEXT, cp)
    struct.pack_into("<II", stream, FC_CLX, len(table), len(pcdt))  # fcClx, lcbClx
    return bytes(stream), table + pcdt


def write_doc(path: Path, word: bytes, table: bytes) -> None:
    """Write a Word 97 document after checking what its FIB declares: the section counts nFib
    0x00C1 fixes ([MS-DOC] 2.5.1 Fib), FibBase's fExtChar and nFibBack, cbMac as the bytes
    written, every FibRgFcLcb97 range inside the table stream, and each piece's text between
    the FIB and cbMac."""

    def u16(at: int) -> int:
        return struct.unpack_from("<H", word, at)[0]

    def u32(at: int) -> int:
        return struct.unpack_from("<I", word, at)[0]

    declared = [u16(at) for at in (0x00, 0x02, 0x0C, 0x20, 0x3E, 0x98, FIB_END - 2)]
    assert declared == [W_IDENT, 0x00C1, 0x00BF, 0x000E, 0x0016, 0x005D, 0], declared
    assert u16(FLAGS) & F_EXT_CHAR, "fExtChar"
    assert u32(CB_MAC) == len(word), "cbMac"
    for at in range(0x9A, FIB_END - 2, 8):
        assert u32(at + 4) == 0 or u32(at) + u32(at + 4) <= len(table), hex(at)
    plc = table[u32(FC_CLX) + 5 : u32(FC_CLX) + u32(FC_CLX + 4)]
    n = (len(plc) - 4) // 12
    cps = struct.unpack_from(f"<{n + 1}I", plc)
    for i in range(n):
        fc = struct.unpack_from("<I", plc, (n + 1) * 4 + i * 8 + 2)[0]
        compressed = fc & 0x40000000
        start = (fc & 0x3FFFFFFF) // 2 if compressed else fc
        end = start + (cps[i + 1] - cps[i]) * (1 if compressed else 2)
        assert start >= FIB_END and end <= len(word), f"piece {i}"
    write_cfb(path, [("0Table", table), ("WordDocument", word)])


def pieces_doc(out: Path) -> None:
    """Compressed text reads by the fixed FcCompressed map under any language id; a
    Unicode piece carries the Japanese and Cyrillic that map cannot."""
    unicode_text = "こんにちは世界。\r日本語の段落です。\rПривет, мир!\rВторой абзац по-русски.\r"
    word, table = word_document([b"\x93x\x94\xe0\r", unicode_text, b"d\xe9j\xe0 vu\r"], lid=0x0419)
    write_doc(out / "doc" / "handmade-pieces.doc", word, table)


def std_record(
    name: str, sgc: int, upx: Sequence[bytes], istd_base: int = 0x0FFF, sti: int = 0x0FFE
) -> bytes:
    """An LPStd ([MS-DOC] 2.9.135), its STD of an even length: StdfBase, the Xstz name, then each
    UPX length-prefixed on a word boundary."""
    stdf = bytearray(10)
    struct.pack_into("<HHH", stdf, 0, sti, (istd_base << 4) | sgc, len(upx))
    record = bytes(stdf) + struct.pack("<H", len(name)) + name.encode("utf-16-le") + b"\x00\x00"
    for payload in upx:
        record += bytes(len(record) % 2) + struct.pack("<H", len(payload)) + payload
    record += bytes(len(record) % 2)
    return struct.pack("<H", len(record)) + record


def stsh_stream(cstd: int, stds: Mapping[int, bytes]) -> bytes:
    """An STSH of `cstd` slots holding the STDs `{istd: record}` and empty slots elsewhere."""
    stshi = bytearray(18)
    struct.pack_into("<HH", stshi, 0, cstd, 10)  # cstd, cbSTDBaseInFile
    empty = struct.pack("<H", 0)
    return (
        struct.pack("<H", len(stshi))
        + bytes(stshi)
        + b"".join(stds.get(istd, empty) for istd in range(cstd))
    )


def fkp_page(bounds: Sequence[int], entries: Sequence[bytes], rgb: int) -> bytes:
    """An FKP page over the FC `bounds`, each run's `entries` blob (empty for none) written once,
    on a word boundary after the `rgb`-byte offsets that point at it."""
    page = bytearray(512)
    for i, bound in enumerate(bounds):
        struct.pack_into("<I", page, i * 4, bound)
    count = len(entries)
    at, off = {}, (count + 1) * 4 + count * rgb
    off += off % 2
    for entry in dict.fromkeys(filter(None, entries)):
        page[off : off + len(entry)] = entry
        at[entry] = off // 2
        off += len(entry) + len(entry) % 2
    for k, entry in enumerate(entries):
        if entry:
            page[(count + 1) * 4 + k * rgb] = at[entry]
    page[511] = count
    return bytes(page)


def bin_table(first_fc: int, last_fc: int, pn: int) -> bytes:
    """A PlcBte whose one FKP page, `pn`, covers the FCs from `first_fc` to `last_fc`."""
    return struct.pack("<III", first_fc, last_fc, pn)


def blockstyle_doc(out: Path) -> None:
    """Binary .doc whose STSH names a Source Code and a Quote style, bound to
    paragraphs through a PAPX FKP page."""
    CODE_ISTD, QUOTE_ISTD = 15, 16

    def std(name: str, istd: int) -> bytes:
        # cupx 2: a PAPX holding the istd, and an empty CHPX.
        return std_record(name, 1, [struct.pack("<H", istd), b""])

    stsh = stsh_stream(
        QUOTE_ISTD + 1,
        {CODE_ISTD: std("Source Code", CODE_ISTD), QUOTE_ISTD: std("Quote", QUOTE_ISTD)},
    )

    paras = ["Body before.\r", "fn main() {\r", "}\r", "A quotation.\r", "Body after.\r"]
    istds = [0, CODE_ISTD, CODE_ISTD, QUOTE_ISTD, 0]
    text = "".join(paras).encode("ascii")

    bounds = list(accumulate(map(len, paras), initial=FIB_SIZE))
    PN = 3
    # A PAPX of the "cb 0" form: byte 0 zero, byte 1 its length in words.
    fkp = fkp_page(bounds, [bytes([0, 1]) + struct.pack("<H", istd) for istd in istds], 13)
    plcf = bin_table(FIB_SIZE, bounds[-1], PN)
    word, table = word_document(
        [text], tables=[(FC_STSHF, stsh), (FC_PLCF_BTE_PAPX, plcf)], pages=[(PN, fkp)]
    )
    write_doc(out / "doc" / "handmade-blockstyle.doc", word, table)


def charstyle_doc(out: Path) -> None:
    """Binary .doc whose Callout paragraph style is bold and superscript and whose Emphasis
    character style is italic alone, applied by `sprmCIstd` in the CHPX runs: Emphasis keeps what
    Callout sets, a `sprmCIstd` after direct bold clears it, and bold after one adds to it."""
    CALLOUT, EMPHASIS, DEFAULT_FONT = 15, 16, 10
    bold = struct.pack("<HB", SPRM_CF_BOLD, 1)
    italic = struct.pack("<HB", SPRM_CF_ITALIC, 1)
    superscript = struct.pack("<HB", SPRM_C_ISS, 1)

    def restyle(istd: int) -> bytes:
        return struct.pack("<HH", SPRM_C_ISTD, istd)

    # cupx 2 for a paragraph style (PAPX holding its istd, then CHPX), 1 for a character style.
    stsh = stsh_stream(
        EMPHASIS + 1,
        {
            0: std_record("Normal", 1, [struct.pack("<H", 0), b""], sti=0),
            DEFAULT_FONT: std_record("Default Paragraph Font", 2, [b""], sti=65),
            CALLOUT: std_record(
                "Callout", 1, [struct.pack("<H", CALLOUT), bold + superscript], istd_base=0
            ),
            EMPHASIS: std_record("Emphasis", 2, [italic]),
        },
    )

    paragraphs = [
        (CALLOUT, [("Note ", b""), ("key term", restyle(EMPHASIS)), (" here.\r", b"")]),
        (
            0,
            [
                ("Plain ", b""),
                ("reset", bold + restyle(EMPHASIS)),
                (" ", b""),
                ("added", restyle(EMPHASIS) + bold),
                (".\r", b""),
            ],
        ),
    ]
    runs = [run for _, parts in paragraphs for run in parts]
    text = "".join(chars for chars, _ in runs).encode("ascii")
    run_bounds = list(accumulate((len(chars) for chars, _ in runs), initial=FIB_SIZE))
    lengths = [sum(len(chars) for chars, _ in parts) for _, parts in paragraphs]
    para_bounds = list(accumulate(lengths, initial=FIB_SIZE))

    # Entries: a PAPX of the "cb 0" form (byte 0 zero, byte 1 its length in words), a CHPX with its
    # byte length first.
    papx = [bytes([0, 1]) + struct.pack("<H", istd) for istd, _ in paragraphs]
    chpx = [bytes([len(props)]) + props if props else b"" for _, props in runs]
    PAPX_PN, CHPX_PN = 3, 4
    plcf_papx = bin_table(FIB_SIZE, para_bounds[-1], PAPX_PN)
    plcf_chpx = bin_table(FIB_SIZE, run_bounds[-1], CHPX_PN)
    pages = [(PAPX_PN, fkp_page(para_bounds, papx, 13)), (CHPX_PN, fkp_page(run_bounds, chpx, 1))]
    word, table = word_document(
        [text],
        tables=[(FC_STSHF, stsh), (FC_PLCF_BTE_PAPX, plcf_papx), (FC_PLCF_BTE_CHPX, plcf_chpx)],
        pages=pages,
    )
    write_doc(out / "doc" / "handmade-charstyle.doc", word, table)


def sttbf_ffn(fonts: Sequence[tuple[str, int]]) -> bytes:
    """A SttbfFfn ([MS-DOC]) of the fonts `(name, charset)`: cData, cbExtra 0, then each
    FFN behind its byte length, its other properties zero."""
    table = struct.pack("<HH", len(fonts), 0)
    for name, charset in fonts:
        ffn = bytearray(FFN_NAME)
        ffn[FFN_CHS] = charset
        ffn += name.encode("utf-16-le") + b"\x00\x00"
        table += bytes([len(ffn)]) + ffn
    return table


def scientific_doc(out: Path) -> None:
    """The text of `rtf/handmade-scientific.rtf` in a Unicode piece: Symbol and Wingdings runs
    named by font index, an inserted symbol, and a hidden Wingdings code."""
    TIMES, SYMBOL, WINGDINGS = 0, 1, 2
    fonts = sttbf_ffn([("Times New Roman", 0), ("Symbol", 2), ("Wingdings", 2)])

    def font(ftc: int) -> bytes:
        return b"".join(
            struct.pack("<HH", sprm, ftc)
            for sprm in (SPRM_C_RG_FTC0, SPRM_C_RG_FTC1, SPRM_C_RG_FTC2)
        )

    symbol, wingdings = font(SYMBOL), font(WINGDINGS)
    inserted = struct.pack("<HB", SPRM_CF_SPEC, 1) + struct.pack(
        "<HHH", SPRM_C_SYMBOL, SYMBOL, 0xF06D
    )
    hidden = struct.pack("<HB", SPRM_CF_VANISH, 1)
    runs = [
        ("Particles of 10 ", b""),
        ("m", symbol),
        ("m ", b""),
        ("\uf0b1", symbol),
        (" 2 ", b""),
        ("m", symbol),
        ("m settle at a rate ", b""),
        ("\uf0b5", symbol),
        (" ", b""),
        ("\uf062", symbol),
        (".\rA field pasted as Unicode keeps the last symbol font: 5", b""),
        ("(", inserted),
        ("m.\r", b""),
        ("\uf0fe", wingdings),
        (" Calibrated ", b""),
        ("\uf06f", wingdings),
        (" Replicated ", b""),
        ("\uf0fc", wingdings),
        (" Reviewed\rStep ", b""),
        ("\uf081", wingdings),
        (" precedes step ", b""),
        ("\uf082", wingdings),
        (" and ", hidden + font(TIMES)),
        ("\uf083", hidden + wingdings),
        (".\r", b""),
    ]
    text = "".join(chars for chars, _ in runs)
    bounds = list(accumulate((2 * len(chars) for chars, _ in runs), initial=FIB_SIZE))
    chpx = [bytes([len(props)]) + props if props else b"" for _, props in runs]
    stsh = stsh_stream(1, {0: std_record("Normal", 1, [struct.pack("<H", 0), b""], sti=0)})
    CHPX_PN = 4
    word, table = word_document(
        [text],
        tables=[
            (FC_STSHF, stsh),
            (FC_PLCF_BTE_CHPX, bin_table(FIB_SIZE, bounds[-1], CHPX_PN)),
            (FC_STTBF_FFN, fonts),
        ],
        pages=[(CHPX_PN, fkp_page(bounds, chpx, 1))],
    )
    write_doc(out / "doc" / "handmade-scientific.doc", word, table)


def malformed(out: Path) -> None:
    doc = out / "doc" / "text.doc"
    if doc.exists():
        write_file(out / "malformed" / "truncated--errors.doc", doc.read_bytes()[:4096])


def abuse(out: Path) -> None:
    """HYPERLINK fields nested 1024 deep, the most a raised nesting limit
    admits: past the default limit of 256."""
    depth = 1024
    field = '\x13 HYPERLINK "https://example.com" \x14'
    text = (field * depth + "x" + "\x15" * depth + "\r").encode("ascii")
    word, table = word_document([text])
    write_doc(out / "abuse" / "max_nesting_depth--fieldnest--errors.doc", word, table)


def generate(out: Path) -> None:
    pieces_doc(out)
    blockstyle_doc(out)
    charstyle_doc(out)
    scientific_doc(out)
    malformed(out)
    abuse(out)
