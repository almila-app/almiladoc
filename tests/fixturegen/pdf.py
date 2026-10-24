import zlib
from collections.abc import Sequence
from pathlib import Path

from fixturegen.common import write_file

HELVETICA = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"


def stream(content: bytes) -> bytes:
    return b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream"


def assemble(objs: dict[int, bytes]) -> bytes:
    """Number `objs` as given and write the exact xref table, so no reader reconstructs it."""
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


def handmade_pdf(
    pages: Sequence[bytes], font: bytes = HELVETICA, extra: Sequence[bytes] = ()
) -> bytes:
    """A minimal PDF: a font as /F1 and a 1x1 gray image as /Im1 on every page, one
    content stream per page, then the `extra` objects the font refers to, numbered
    from 5 + 2 * len(pages)."""
    n = len(pages)
    objs = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids ["
        + b" ".join(b"%d 0 R" % (5 + 2 * i) for i in range(n))
        + b"] /Count %d >>" % n,
        3: font,
        4: DOT,
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
    return assemble(objs)


def gray_image(w: int, h: int, lines: bool) -> bytes:
    """A gray picture, blank or ruled with text-like ink lines as a scanned page is."""
    rows = bytearray()
    for y in range(h):
        for x in range(w):
            ink = lines and (y % 12) in (4, 5, 6) and (x * 7 + y) % 9 < 6 and 20 < x < w - 20
            rows.append(30 if ink else 235)
    data = zlib.compress(bytes(rows), 9)
    return (
        b"<< /Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceGray"
        b" /BitsPerComponent 8 /Filter /FlateDecode /Length %d >>\nstream\n"
        % (w, h, len(data))
        + data
        + b"\nendstream"
    )


DOT = (
    b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray"
    b" /BitsPerComponent 8 /Length 1 >>\nstream\n\x80\nendstream"
)
TILES = 8
XOBJECTS = {
    b"Im1": lambda: DOT,
    b"Scan": lambda: gray_image(306, 396, True),
    b"Fig": lambda: gray_image(240, 200, False),
    b"ScanHi": lambda: gray_image(1224, 1584, True),
    b"FigHi": lambda: gray_image(1224, 1584, False),
    **{b"T%d" % i: lambda: gray_image(306, 396 // TILES, True) for i in range(TILES)},
}

# A composite font on Identity-H with no ToUnicode map, whose codes no reader can resolve.
UNRESOLVED_FONT = (
    b"<< /Type /Font /Subtype /Type0 /BaseFont /Unresolved /Encoding /Identity-H"
    b" /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Unresolved"
    b" /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 500 >>] >>"
)

Case = Sequence[tuple[bytes, Sequence[bytes]]]


def case_pdf(pages: Case) -> bytes:
    """Pages of a content stream each, painting the named XObjects; /F1 is Helvetica and
    /F2 a font whose codes give no characters."""
    names = [name for name in XOBJECTS if any(name in used for _, used in pages)]
    n = len(pages)
    first_page = 5 + len(names)
    objs = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids ["
        + b" ".join(b"%d 0 R" % (first_page + 2 * i) for i in range(n))
        + b"] /Count %d >>" % n,
        3: HELVETICA,
        4: UNRESOLVED_FONT,
    }
    for i, name in enumerate(names):
        objs[5 + i] = XOBJECTS[name]()
    for i, (content, used) in enumerate(pages):
        page, contents = first_page + 2 * i, first_page + 2 * i + 1
        xobjects = b" ".join(b"/%s %d 0 R" % (x, 5 + names.index(x)) for x in used)
        objs[page] = (
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]"
            b" /Resources << /Font << /F1 3 0 R /F2 4 0 R >> /XObject << " + xobjects + b" >> >>"
            b" /Contents %d 0 R >>" % contents
        )
        objs[contents] = stream(content)
    return assemble(objs)


TEXT_PAGE = b"BT /F1 24 Tf 72 700 Td (Text on the first page) Tj ET"
IMAGE_PAGE = b"q 468 0 0 648 72 72 cm /Im1 Do Q"


# Ligature glyphs named by /Differences and no ToUnicode map: an underscore
# name (T_h, f_t, s_t) and a uni name of two code points (uni00660069).
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
# table of that collection.
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
    return b"BT /F1 10 Tf 72 700 Td " + shows + b" ET"


FULL_SCAN = b"q 612 0 0 792 0 0 cm /Scan Do Q\n"
FULL_SCAN_HI = b"q 612 0 0 792 0 0 cm /ScanHi Do Q\n"
BODY = [
    b"Interviews were transcribed and coded by two raters working",
    b"independently, and disagreements were settled by discussion.",
    b"The coding frame followed the categories of the pilot study,",
    b"with two additions that the pilot had not anticipated at all.",
    b"Agreement before discussion was substantial across the sample.",
    b"Each transcript was then summarised in a short structured memo.",
]
TITLE = [b"Annual Report on Coding Practice: 2023", b"Issued September 2024"]


def text_lines(lines: Sequence[bytes], top: int = 690, mode: int = 0, size: int = 11) -> bytes:
    shows = b" ".join(b"(" + line + b") Tj T*" for line in lines)
    return b"BT /F1 %d Tf %d Tr 14 TL 72 %d Td " % (size, mode, top) + shows + b" ET\n"


def running_head(n: int) -> bytes:
    return (
        b"BT /F1 9 Tf 72 765 Td (Journal of Verdict Tests, Vol. 7) Tj ET\n"
        b"BT /F1 9 Tf 300 30 Td (%d) Tj ET\n" % n
    )


def stamp(n: int) -> bytes:
    return (
        b"BT /F1 10 Tf 160 770 Td (PROCEEDINGS OF THE ROYAL SOCIETY, 1923) Tj ET\n"
        b"BT /F1 10 Tf 520 770 Td (%d) Tj ET\n" % (16 + n)
    )


def tiles() -> bytes:
    h = 792 / TILES
    return b"".join(
        b"q 612 0 0 %.2f 0 %.2f cm /T%d Do Q\n" % (h, 792 - (i + 1) * h, i) for i in range(TILES)
    )


def vector_glyphs() -> bytes:
    return b"0 g\n" + b"".join(
        b"%d %d m %d %d l %d %d l %d %d l h f\n" % (x, y, x + 3, y + 8, x + 6, y, x + 3, y + 3)
        for y in range(700, 560, -14)
        for x in range(72, 540, 9)
    )


TILE_NAMES = [b"T%d" % i for i in range(TILES)]
CLIP_AWAY = b"q 0 0 10 10 re W n\n"
DIGITISED = b"BT /F1 8 Tf 220 20 Td (Digitized by the Internet Archive) Tj ET"
SPACES = b"BT /F1 12 Tf 72 700 Td (          ) Tj ET"
UNRESOLVED_RUN = b"BT /F2 12 Tf 72 600 Td <0102030405060708> Tj ET"
COVER = text_lines(TITLE, top=420, size=20)

# Each page's verdict is pinned in tests/it/pdf.rs.
VERDICT_CASES: dict[str, Case] = {
    "blank": [(b"", [])],
    "blank-state": [(b"q 1 0 0 1 0 0 cm BT /F1 12 Tf 14 TL ET 0 g Q", [])],
    "header-only": [
        (running_head(1) + text_lines(BODY), []),
        (running_head(2), []),
        (running_head(3) + text_lines(BODY), []),
    ],
    "image-only": [(FULL_SCAN, [b"Scan"])],
    "invisible-mode3-scan": [(FULL_SCAN + text_lines(BODY, mode=3), [b"Scan"])],
    "invisible-mode3-alone": [(text_lines(BODY, mode=3), [])],
    "invisible-clipped": [(CLIP_AWAY + text_lines(BODY) + b"Q\n", [])],
    "invisible-clipped-scan": [(FULL_SCAN + CLIP_AWAY + text_lines(BODY) + b"Q\n", [b"Scan"])],
    "inkless-spaces": [(SPACES, [])],
    "inkless-spaces-scan": [(FULL_SCAN + SPACES, [b"Scan"])],
    "inkless-mode3-scan": [
        (FULL_SCAN + b"BT /F1 12 Tf 3 Tr 72 700 Td (Scan 1923.) Tj ET", [b"Scan"])
    ],
    "sparse-visible": [(b"BT /F1 12 Tf 72 700 Td (Page note.) Tj ET", [b"Im1"])],
    "stamped-scan": [(FULL_SCAN + stamp(i), [b"Scan"]) for i in range(1, 4)],
    "stamped-scan-single": [(FULL_SCAN + DIGITISED, [b"Scan"])],
    "stamped-scan-hires": [(FULL_SCAN_HI + stamp(i), [b"ScanHi"]) for i in range(1, 4)],
    "stamped-scan-single-hires": [(FULL_SCAN_HI + DIGITISED, [b"ScanHi"])],
    "tiled-scan": [(tiles(), TILE_NAMES)],
    "tiled-scan-layer": [(tiles() + text_lines(BODY, mode=3), TILE_NAMES)],
    "five-page": [
        (TEXT_PAGE, [b"Im1"]),
        (IMAGE_PAGE, [b"Im1"]),
        (TEXT_PAGE, [b"Im1"]),
        (TEXT_PAGE, [b"Im1"]),
        (IMAGE_PAGE, [b"Im1"]),
    ],
    "five-page-garbled": [
        (TEXT_PAGE, [b"Im1"]),
        (IMAGE_PAGE, [b"Im1"]),
        (TEXT_PAGE + b"\n" + UNRESOLVED_RUN, [b"Im1"]),
        (TEXT_PAGE, [b"Im1"]),
        (IMAGE_PAGE, [b"Im1"]),
    ],
    "vector-glyphs": [(vector_glyphs(), [])],
    "figure-cover-hires": [(b"q 612 0 0 792 0 0 cm /FigHi Do Q\n" + COVER, [b"FigHi"])],
    "figure-large": [
        (
            text_lines(BODY, top=740)
            + b"q 468 0 0 480 72 120 cm /Fig Do Q\n"
            + text_lines([b"Figure 1. Coding agreement by round and rater."], top=100),
            [b"Fig"],
        )
    ],
    "figure-cover": [(b"q 612 0 0 792 0 0 cm /Fig Do Q\n" + COVER, [b"Fig"])],
    "census-cover": [
        (
            b"q 612 0 0 792 0 0 cm /Fig Do Q\n"
            + text_lines([b"Income in the United States: 2023"], top=520, size=24)
            + text_lines([b"Current Population Reports", b"Issued September 2024"], top=460)
            + text_lines([b"Series 60, Number 282"], top=420, size=14)
            + b"BT /F1 8 Tf 72 20 Td (U.S. Department of Commerce) Tj ET\n",
            [b"Fig"],
        )
    ],
}


# A Type0 font on the legacy predefined CMap KSC-EUC-H with no ToUnicode map: its strings
# are EUC-KR bytes, which decode only through that CMap and the Adobe-Korea1 collection.
KSC_FONT = (
    b"<< /Type /Font /Subtype /Type0 /BaseFont /HYSMyeongJo-Medium /Encoding /KSC-EUC-H"
    b" /DescendantFonts [7 0 R] >>"
)
KSC_SENTENCE = "이 문서는 한국어 글꼴의 옛 코드를 읽는 시험입니다."


def ksc_page() -> bytes:
    words = KSC_SENTENCE.split(" ")
    shown = [word + " " for word in words[:-1]] + words[-1:]
    shows = b" ".join(
        b"<" + word.encode("euc-kr").hex().upper().encode() + b"> Tj" for word in shown
    )
    return b"BT /F1 10 Tf 72 700 Td " + shows + b" ET"


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
    write_file(
        pdf / "handmade-legacy-cmap.pdf",
        handmade_pdf([ksc_page()], font=KSC_FONT, extra=[KOREA1_CID_FONT, KOREA1_DESCRIPTOR]),
    )
    for name, pages in VERDICT_CASES.items():
        write_file(pdf / f"handmade-{name}.pdf", case_pdf(pages))
