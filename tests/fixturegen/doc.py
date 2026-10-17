import struct

from fixturegen.common import write_cfb


# ---------------------------------------------------------------------------
# Compressed .doc text decodes via the FIB language id's code page

def word_doc_stream(lid, text_bytes, far_east=False):
    """A minimal WordDocument stream: FIB base + legacy single-piece text
    (fcMin/fcMac, no Clx), with the text stored fc-compressed (one byte per
    CP) at offset 0x400."""
    fib = bytearray(0x400)
    struct.pack_into("<H", fib, 0x00, 0xA5EC)          # wIdent
    struct.pack_into("<H", fib, 0x02, 0x00C1)          # nFib (Word 97)
    flags = 0x4000 if far_east else 0                  # fFarEast
    struct.pack_into("<H", fib, 0x0A, flags)
    if far_east:
        struct.pack_into("<H", fib, 0x3C, lid)         # FibRgW97.lidFE
    else:
        struct.pack_into("<H", fib, 0x06, lid)         # FibBase.lid
    struct.pack_into("<I", fib, 0x18, 0x400)           # fcMin
    struct.pack_into("<I", fib, 0x1C, 0x400 + len(text_bytes))  # fcMac
    struct.pack_into("<I", fib, 0x4C, len(text_bytes))  # ccpText (byte CPs)
    return bytes(fib) + text_bytes


def encoded_docs(out):
    d = out / "doc"
    sj = "こんにちは世界。\r日本語の段落です。\r".encode("shift_jis")
    write_cfb(d / "handmade-shiftjis.doc",
              [("WordDocument", word_doc_stream(0x0411, sj, far_east=True))])
    ru = "Привет, мир!\rВторой абзац по-русски.\r".encode("cp1251")
    write_cfb(d / "handmade-cyrillic.doc",
              [("WordDocument", word_doc_stream(0x0419, ru))])


def blockstyle_doc(out):
    """Binary .doc whose STSH names a Source Code and a Quote style, bound to
    paragraphs through a PAPX FKP page."""
    CODE_ISTD, QUOTE_ISTD = 15, 16

    def std(name, istd):
        stdf = bytearray(10)
        struct.pack_into("<H", stdf, 0, 0x0FFE)              # sti: not built-in
        struct.pack_into("<H", stdf, 2, (0x0FFF << 4) | 1)   # istdBase nil, paragraph
        struct.pack_into("<H", stdf, 4, 2)                   # cupx: papx + chpx
        chars = name.encode("utf-16-le")
        xstz = struct.pack("<H", len(name)) + chars + b"\x00\x00"
        # UPX payloads follow the name, each 2-byte aligned in the record.
        upx = struct.pack("<H", 2) + struct.pack("<H", istd) + struct.pack("<H", 0)
        record = bytes(stdf) + xstz + upx
        assert len(record) % 2 == 0
        return struct.pack("<H", len(record)) + record

    cstd = QUOTE_ISTD + 1
    stshi = bytearray(18)
    struct.pack_into("<H", stshi, 0, cstd)      # cstd
    struct.pack_into("<H", stshi, 2, 10)        # cbSTDBaseInFile
    stsh = struct.pack("<H", len(stshi)) + bytes(stshi)
    for istd in range(cstd):
        if istd == CODE_ISTD:
            stsh += std("Source Code", istd)
        elif istd == QUOTE_ISTD:
            stsh += std("Quote", istd)
        else:
            stsh += struct.pack("<H", 0)

    paras = ["Body before.\r", "fn main() {\r", "}\r", "A quotation.\r", "Body after.\r"]
    istds = [0, CODE_ISTD, CODE_ISTD, QUOTE_ISTD, 0]
    text = "".join(paras).encode("cp1252")

    # PAPX FKP page: rgfc boundaries, then one BX per paragraph pointing at a
    # PAPX blob (cb 0 form: byte 0 zero, byte 1 length in words).
    fkp = bytearray(512)
    fc = 0x400
    bounds = [fc]
    for p in paras:
        fc += len(p)
        bounds.append(fc)
    for i, b in enumerate(bounds):
        struct.pack_into("<I", fkp, i * 4, b)
    count = len(paras)
    blob_at = {}
    off = (count + 1) * 4 + count * 13
    off += off % 2
    for istd in dict.fromkeys(istds):
        fkp[off] = 0
        fkp[off + 1] = 1
        struct.pack_into("<H", fkp, off + 2, istd)
        blob_at[istd] = off // 2
        off += 4
    for k, istd in enumerate(istds):
        fkp[(count + 1) * 4 + k * 13] = blob_at[istd]
    fkp[511] = count

    PN = 3
    word_doc = bytearray(word_doc_stream(0x0409, text))
    word_doc += b"\x00" * (PN * 512 - len(word_doc))
    word_doc += fkp
    # PlcfBtePapx: one FKP page covering the whole text.
    plcf = struct.pack("<II", 0x400, bounds[-1]) + struct.pack("<I", PN)
    struct.pack_into("<II", word_doc, 0xA2, 0, len(stsh))
    struct.pack_into("<II", word_doc, 0x102, len(stsh), len(plcf))

    write_cfb(out / "doc" / "handmade-blockstyle.doc",
              [("0Table", stsh + plcf), ("WordDocument", bytes(word_doc))])


def malformed(out):
    m = out / "malformed"
    m.mkdir(parents=True, exist_ok=True)
    doc = out / "doc" / "text.doc"
    if doc.exists():
        (m / "truncated--errors.doc").write_bytes(doc.read_bytes()[:4096])


def abuse(out):
    """HYPERLINK fields nested 1024 deep, the most a raised nesting limit
    admits: past the default limit of 256."""
    depth = 1024
    field = '\x13 HYPERLINK "https://example.com" \x14'
    text = (field * depth + "x" + "\x15" * depth + "\r").encode("cp1252")
    write_cfb(out / "abuse" / "max_nesting_depth--fieldnest--errors.doc",
              [("WordDocument", word_doc_stream(0x0409, text))])


def generate(out):
    encoded_docs(out)
    blockstyle_doc(out)
    malformed(out)
    abuse(out)
