from collections.abc import Sequence
from pathlib import Path

from fixturegen.common import write_file

HELVETICA = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"


def stream(content: bytes) -> bytes:
    return b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream"


def handmade_pdf(
    pages: Sequence[bytes], font: bytes = HELVETICA, extra: Sequence[bytes] = ()
) -> bytes:
    """A minimal PDF: a font as /F1 and a 1x1 gray image as /Im1 on every page, one
    content stream per page, then the `extra` objects the font refers to, numbered
    from 5 + 2 * len(pages). Object numbers are fixed by position, so the xref
    table is exact rather than reconstructed by the reader."""
    n = len(pages)
    objs = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids ["
        + b" ".join(b"%d 0 R" % (5 + 2 * i) for i in range(n))
        + b"] /Count %d >>" % n,
        3: font,
        4: b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray"
        b" /BitsPerComponent 8 /Length 1 >>\nstream\n\x80\nendstream",
    }
    for i, content in enumerate(pages):
        page, contents = 5 + 2 * i, 6 + 2 * i
        objs[page] = (
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]"
            b" /Resources << /Font << /F1 3 0 R >> /XObject << /Im1 4 0 R >> >>"
            b" /Contents %d 0 R >>" % contents
        )
        objs[contents] = stream(content)
    for i, obj in enumerate(extra):
        objs[5 + 2 * n + i] = obj
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for num in sorted(objs):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % num + objs[num] + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    return bytes(out)


TEXT_PAGE = b"BT /F1 24 Tf 72 700 Td (Text on the first page) Tj ET"
IMAGE_PAGE = b"q 468 0 0 648 72 72 cm /Im1 Do Q"


# Ligature glyphs named by /Differences and no ToUnicode map: an underscore
# name (T_h, f_t, s_t) and a uni name of two code points (uni00660069).
# One Tj per word: pdf-inspector takes a page with an image resource and fewer
# than ten text operators for a scan.
LIGATURE_FONT = (
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding"
    b" /BaseEncoding /WinAnsiEncoding /Differences [128 /T_h /uni00660069 /f_t /s_t] >> >>"
)
LIGATURE_WORDS = [
    b"\\200e",
    b" model",
    b" classi\\201es",
    b" the",
    b" input",
    b" a\\202er",
    b" it",
    b" is",
    b" \\203ated",
    b".",
]
LIGATURE_PAGE = (
    b"BT /F1 18 Tf 72 700 Td "
    + b" ".join(b"(" + word + b") Tj" for word in LIGATURE_WORDS)
    + b" ET"
)

# An inline image with no /CS, which needs OCR rather than aborting.
INLINE_IMAGE_PAGE = b"q 8 0 0 8 0 0 cm BI /W 8 /H 8 /BPC 8 /G true ID " + bytes(64) + b" EI Q"


# A right-to-left line painted in visual order, as a producer that applied the
# bidi algorithm draws it: the words last to first and each spelled backwards,
# the number left to right, the brackets mirrored. Arabic glyphs are uniXXXX
# names in /Differences, with no ToUnicode map.
ARABIC_VISUAL = [
    "موي",
    " لك",
    " ةبتكملا",
    " يف",
    " (",
    "12",
    " لصفلا",
    ")",
    " باتكلا",
    " بلاطلا",
    " أرقي",
]


def arabic_page() -> bytes:
    letters = sorted({c for item in ARABIC_VISUAL for c in item if ord(c) > 127})
    code = {c: 128 + i for i, c in enumerate(letters)}
    names = b" ".join(b"/uni%04X" % ord(c) for c in letters)
    font = (
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding"
        b" /BaseEncoding /WinAnsiEncoding /Differences [128 " + names + b"] >> >>"
    )
    escaped = (
        bytes(code.get(c, ord(c)) for c in item).replace(b"(", b"\\(").replace(b")", b"\\)")
        for item in ARABIC_VISUAL
    )
    shows = b" ".join(b"(" + item + b") Tj" for item in escaped)
    return handmade_pdf([b"BT /F1 18 Tf 72 700 Td " + shows + b" ET"], font=font)


# A Type0 font on the predefined CMap Identity-H with no ToUnicode map and no program:
# its codes are Adobe-Korea1 CIDs, which decode only through the bundled CID-to-Unicode
# table of that collection. The descendant is indirect and has a font descriptor, as
# pdf-inspector keys the table by the descendant's object only then. Ten hex-string Tj
# operators keep the page from reading as a scan.
KOREA1_FONT = (
    b"<< /Type /Font /Subtype /Type0 /BaseFont /HYSMyeongJo-Medium /Encoding /Identity-H"
    b" /DescendantFonts [7 0 R] >>"
)
KOREA1_CID_FONT = (
    b"<< /Type /Font /Subtype /CIDFontType0 /BaseFont /HYSMyeongJo-Medium"
    b" /CIDSystemInfo << /Registry (Adobe) /Ordering (Korea1) /Supplement 2 >> /DW 1000"
    b" /FontDescriptor 8 0 R >>"
)
KOREA1_DESCRIPTOR = (
    b"<< /Type /FontDescriptor /FontName /HYSMyeongJo-Medium /Flags 4"
    b" /FontBBox [0 -120 1000 880] /ItalicAngle 0 /Ascent 880 /Descent -120 /CapHeight 700"
    b" /StemV 80 >>"
)
KOREA1_CIDS = {
    " ": 1,
    ".": 15,
    "국": 1204,
    "글": 1238,
    "꼴": 1308,
    "는": 1495,
    "니": 1508,
    "다": 1518,
    "드": 1624,
    "로": 1789,
    "를": 1843,
    "문": 1945,
    "서": 2226,
    "시": 2342,
    "어": 2479,
    "유": 2604,
    "의": 2628,
    "이": 2633,
    "읽": 2637,
    "입": 2641,
    "자": 2647,
    "코": 3023,
    "한": 3296,
    "험": 3319,
}
KOREA1_WORDS = [
    "이 ",
    "문서는 ",
    "한국어 ",
    "글꼴의 ",
    "문자 ",
    "코드를 ",
    "유니코드로 ",
    "읽는 ",
    "시험",
    "입니다.",
]


def korea1_page() -> bytes:
    shows = b" ".join(
        b"<" + b"".join(b"%04X" % KOREA1_CIDS[c] for c in word) + b"> Tj" for word in KOREA1_WORDS
    )
    return b"BT /F1 18 Tf 72 700 Td " + shows + b" ET"


def generate(out: Path) -> None:
    pdf = out / "pdf"
    write_file(pdf / "handmade-scanned.pdf", handmade_pdf([IMAGE_PAGE, IMAGE_PAGE]))
    write_file(pdf / "handmade-mixed.pdf", handmade_pdf([TEXT_PAGE, IMAGE_PAGE]))
    write_file(pdf / "handmade-ligatures.pdf", handmade_pdf([LIGATURE_PAGE], font=LIGATURE_FONT))
    write_file(pdf / "handmade-arabic.pdf", arabic_page())
    write_file(pdf / "handmade-inline-image-no-colorspace.pdf", handmade_pdf([INLINE_IMAGE_PAGE]))
    write_file(
        pdf / "handmade-predefined-cmap.pdf",
        handmade_pdf(
            [korea1_page()], font=KOREA1_FONT, extra=[KOREA1_CID_FONT, KOREA1_DESCRIPTOR]
        ),
    )
