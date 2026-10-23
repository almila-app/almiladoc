import struct
from collections.abc import Sequence
from itertools import accumulate
from pathlib import Path

from fixturegen.common import write_cfb, write_file

Paragraphs = Sequence[Sequence[tuple[str, dict[str, int]]]]

DOCUMENT = 0x03E8
SLIDE = 0x03EE
SLIDE_ATOM = 0x03EF
NOTES = 0x03F0
NOTES_ATOM = 0x03F1
ENVIRONMENT = 0x03F2
SLIDE_PERSIST_ATOM = 0x03F3
MAIN_MASTER = 0x03F8
DRAWING = 0x040C
FONT_COLLECTION = 0x07D5
PLACEHOLDER_ATOM = 0x0BC3
TEXT_HEADER_ATOM = 0x0F9F
TEXT_CHARS_ATOM = 0x0FA0
STYLE_TEXT_PROP_ATOM = 0x0FA1
TEXT_MASTER_STYLE_ATOM = 0x0FA3
TEXT_BYTES_ATOM = 0x0FA8
FONT_ENTITY_ATOM = 0x0FB7
CSTRING = 0x0FBA
HEADERS_FOOTERS = 0x0FD9
HEADERS_FOOTERS_ATOM = 0x0FDA
SLIDE_LIST_WITH_TEXT = 0x0FF0
USER_EDIT_ATOM = 0x0FF5
CURRENT_USER_ATOM = 0x0FF6
PROG_TAGS = 0x1388
PROG_BINARY_TAG = 0x138A
BINARY_TAG_DATA_BLOB = 0x138B
PERSIST_DIRECTORY_ATOM = 0x1772
COMMENT10 = 0x2EE0
COMMENT10_ATOM = 0x2EE1
DG_CONTAINER = 0xF002
SPGR_CONTAINER = 0xF003
SP_CONTAINER = 0xF004
FSP = 0xF00A
CLIENT_TEXTBOX = 0xF00D
CLIENT_DATA = 0xF011

SLIDE_LIST_INSTANCE = 0
MASTER_LIST_INSTANCE = 1
NOTES_LIST_INSTANCE = 2

PAGE_HEADERS_FOOTERS_INSTANCE = 0
SLIDE_HEADERS_FOOTERS_INSTANCE = 3
NOTES_HEADERS_FOOTERS_INSTANCE = 4
HEADER_INSTANCE = 1
FOOTER_INSTANCE = 2


def ppt_rec(ver_inst: int, rec_type: int, body: bytes) -> bytes:
    return struct.pack("<HHI", ver_inst, rec_type, len(body)) + body


def ppt_container(instance: int, rec_type: int, body: bytes) -> bytes:
    return ppt_rec((instance << 4) | 0xF, rec_type, body)


def persist_atom(persist_ref: int, sid: int) -> bytes:
    return ppt_rec(0, SLIDE_PERSIST_ATOM, struct.pack("<IIIII", persist_ref, 0, 0, sid, 0))


def slide_atom(master_id: int = 0) -> bytes:
    return ppt_rec(
        2,
        SLIDE_ATOM,
        struct.pack("<I", 0) + b"\x00" * 8 + struct.pack("<IIHH", master_id, 0, 0, 0),
    )


def text_header(kind: int) -> bytes:
    return ppt_rec(0, TEXT_HEADER_ATOM, struct.pack("<I", kind))


def text_slide(text: str, master_id: int = 0) -> bytes:
    body = ppt_rec(0, TEXT_BYTES_ATOM, text.encode("ascii"))
    return ppt_container(0, SLIDE, slide_atom(master_id) + text_header(1) + body)


def notes(slide_ref: int, text: str) -> bytes:
    notes_atom = ppt_rec(1, NOTES_ATOM, struct.pack("<IHH", slide_ref, 0, 0))
    body = ppt_rec(0, TEXT_BYTES_ATOM, text.encode("ascii"))
    return ppt_container(0, NOTES, notes_atom + text_header(2) + body)


def write_ppt(path: Path, *blobs: bytes) -> None:
    """A presentation whose persist objects 1..n are `blobs` in order, followed by the persist
    directory, the UserEditAtom and the Current User stream that points at it."""
    *offsets, directory_at = accumulate(map(len, blobs), initial=0)
    count = len(blobs)
    directory = ppt_rec(
        0, PERSIST_DIRECTORY_ATOM, struct.pack(f"<I{count}I", 1 | (count << 20), *offsets)
    )
    edit_at = directory_at + len(directory)
    edit = ppt_rec(
        0, USER_EDIT_ATOM, struct.pack("<IIIIIIHH", 0, 0, 0, directory_at, 1, count + 1, 0, 0)
    )
    current_user = ppt_rec(0, CURRENT_USER_ATOM, struct.pack("<III", 20, 0xE391C05F, edit_at))
    write_cfb(
        path,
        [
            ("Current User", current_user),
            ("PowerPoint Document", b"".join(blobs) + directory + edit),
        ],
    )


def multimaster_ppt(out: Path) -> None:
    """Per-slide master selection with tri-state level defaults."""

    # TxMasterStyleAtom instance 1 (body text), one level: PF carries
    # hasBullet, CF carries exactly one specified style bit (tri-state).
    def tx_master(bullet_on: bool, cf_mask: int, cf_style: int) -> bytes:
        pf = struct.pack("<IH", 0x0001, 0x0001 if bullet_on else 0x0000)
        cf = struct.pack("<IH", cf_mask, cf_style)
        return ppt_rec(1 << 4, TEXT_MASTER_STYLE_ATOM, struct.pack("<H", 1) + pf + cf)

    slide_list = ppt_container(
        SLIDE_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(4, 256) + persist_atom(5, 257)
    )
    master_list = ppt_container(
        MASTER_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(2, 1001) + persist_atom(3, 1002)
    )
    doc = ppt_container(0, DOCUMENT, slide_list + master_list)
    master_a = ppt_container(0, MAIN_MASTER, tx_master(True, 0x0001, 0x0001))  # bullet+bold
    master_b = ppt_container(0, MAIN_MASTER, tx_master(False, 0x0002, 0x0002))  # no bullet, italic
    slide1 = text_slide("Alpha master body text\r", 1001)
    slide2 = text_slide("Beta master body text\r", 1002)
    write_ppt(out / "ppt" / "handmade-multimaster.ppt", doc, master_a, master_b, slide1, slide2)


def sparsenotes_ppt(out: Path) -> None:
    """Notes pages pair to slides via NotesAtom.slideIdRef, not list order: this deck has notes
    only on the second slide, which order-based zipping would misattribute to the first."""
    slide_list = ppt_container(
        SLIDE_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(2, 256) + persist_atom(3, 257)
    )
    notes_list = ppt_container(NOTES_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(4, 1301))
    doc = ppt_container(0, DOCUMENT, slide_list + notes_list)
    s1 = text_slide("First slide text\r")
    s2 = text_slide("Second slide text\r")
    n2 = notes(257, "Notes for the second slide\r")
    write_ppt(out / "ppt" / "handmade-sparsenotes.ppt", doc, s1, s2, n2)


def scripts_ppt(out: Path) -> None:
    """Superscripts, subscripts and symbol-font characters, as pptx.py writes the same deck."""

    def font_entity(index: int, name: str, charset: int) -> bytes:
        face = name.encode("utf-16-le").ljust(64, b"\x00")
        return ppt_rec(
            index << 4, FONT_ENTITY_ATOM, face + struct.pack("<BBBB", charset, 0, 0x04, 0x22)
        )

    def char_run(
        count: int,
        font: int | None = None,
        symbol: int | None = None,
        position: int | None = None,
    ) -> bytes:
        mask, fields = 0, b""
        if font is not None:
            mask |= 0x10000
            fields += struct.pack("<H", font)
        if symbol is not None:
            mask |= 0x800000
            fields += struct.pack("<H", symbol)
        if position is not None:
            mask |= 0x80000
            fields += struct.pack("<h", position)
        return struct.pack("<II", count, mask) + fields

    def text_of(paragraphs: Paragraphs) -> str:
        return "".join("".join(text for text, _ in runs) + "\r" for runs in paragraphs)

    def styles_of(paragraphs: Paragraphs) -> bytes:
        # Both run lists cover the text and its implicit final mark.
        counts = [sum(len(t) for t, _ in runs) + 1 for runs in paragraphs]
        counts[-1] += 1
        para_runs = b"".join(struct.pack("<IHI", count, 0, 0) for count in counts)
        char_runs = []
        for runs in paragraphs:
            for i, (text, props) in enumerate(runs):
                char_runs.append([len(text) + (i == len(runs) - 1), props])
        char_runs[-1][0] += 1
        return para_runs + b"".join(char_run(count, **props) for count, props in char_runs)

    body = [
        [("Area: 5 m", {}), ("2", {"position": 30})],
        [("H", {}), ("2", {"position": -25}), ("O", {})],
        [("Size: 10 ", {}), ("m", {"font": 1}), ("m", {})],
        [("Gap: 3 ", {}), ("\uf06d", {"font": 0, "symbol": 1}), ("s", {})],
        [("Checked ", {}), ("\xfc\xff", {"font": 2}), (" end", {})],
    ]
    title = text_header(0) + ppt_rec(0, TEXT_CHARS_ATOM, "Scripts and symbols".encode("utf-16-le"))
    text_body = (
        text_header(1)
        + ppt_rec(0, TEXT_CHARS_ATOM, text_of(body).encode("utf-16-le"))
        + ppt_rec(0, STYLE_TEXT_PROP_ATOM, styles_of(body))
    )
    slide = ppt_container(0, SLIDE, slide_atom() + title + text_body)

    fonts = ppt_container(
        0,
        FONT_COLLECTION,
        font_entity(0, "Arial", 0) + font_entity(1, "Symbol", 2) + font_entity(2, "Wingdings", 2),
    )
    doc = ppt_container(
        0,
        DOCUMENT,
        ppt_container(0, ENVIRONMENT, fonts)
        + ppt_container(SLIDE_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(2, 256)),
    )
    write_ppt(out / "ppt" / "handmade-scripts.ppt", doc, slide)


def side_ppt(out: Path) -> None:
    """Headers, footers and comments the slides show and no output carries: a per-slide
    container, the document's defaults for slides and notes, a footer placeholder drawn on a
    slide and a `Comment10` comment."""

    def headers_footers(
        instance: int, flags: int, strings: Sequence[tuple[int, str]] = ()
    ) -> bytes:
        atom = ppt_rec(0, HEADERS_FOOTERS_ATOM, struct.pack("<hH", 0, flags))
        texts = b"".join(ppt_rec(i << 4, CSTRING, t.encode("utf-16-le")) for i, t in strings)
        return ppt_container(instance, HEADERS_FOOTERS, atom + texts)

    def comment_tag() -> bytes:
        comment = ppt_container(0, COMMENT10, ppt_rec(0, COMMENT10_ATOM, b"\x00" * 36))
        tag = ppt_rec(0, CSTRING, "___PPT10".encode("utf-16-le"))
        tag += ppt_rec(0, BINARY_TAG_DATA_BLOB, comment)
        return ppt_container(0, PROG_TAGS, ppt_container(0, PROG_BINARY_TAG, tag))

    def drawn_footer(text: str) -> bytes:
        fsp = ppt_rec(2, FSP, struct.pack("<II", 1, 0x0A00))
        placement = struct.pack("<IBBH", 0, 9, 0, 0)
        client = ppt_container(0, CLIENT_DATA, ppt_rec(0, PLACEHOLDER_ATOM, placement))
        textbox = ppt_container(
            0,
            CLIENT_TEXTBOX,
            text_header(4) + ppt_rec(0, TEXT_CHARS_ATOM, text.encode("utf-16-le")),
        )
        shape = ppt_container(0, SP_CONTAINER, fsp + client + textbox)
        group = ppt_container(0, SPGR_CONTAINER, shape)
        return ppt_container(0, DRAWING, ppt_container(0, DG_CONTAINER, group))

    def slide(title: str, body: str, extra: bytes = b"") -> bytes:
        heading = text_header(0) + ppt_rec(0, TEXT_CHARS_ATOM, title.encode("utf-16-le"))
        text = text_header(1) + ppt_rec(0, TEXT_BYTES_ATOM, body.encode("ascii"))
        return ppt_container(0, SLIDE, slide_atom() + heading + text + extra)

    today, slide_number, header, footer = 0x1 | 0x2, 0x8, 0x10, 0x20
    first = slide(
        "Quarterly results",
        "First slide text\r",
        headers_footers(
            PAGE_HEADERS_FOOTERS_INSTANCE,
            today | slide_number | footer,
            [(FOOTER_INSTANCE, "Quarterly review")],
        )
        + comment_tag(),
    )
    second = slide("Outlook", "Second slide text\r", drawn_footer("Draft"))
    first_notes = notes(256, "Notes for the first slide\r")

    doc = ppt_container(
        0,
        DOCUMENT,
        headers_footers(SLIDE_HEADERS_FOOTERS_INSTANCE, footer, [(FOOTER_INSTANCE, "Draft")])
        + headers_footers(NOTES_HEADERS_FOOTERS_INSTANCE, header, [(HEADER_INSTANCE, "Handout")])
        + ppt_container(
            SLIDE_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(2, 256) + persist_atom(3, 257)
        )
        + ppt_container(NOTES_LIST_INSTANCE, SLIDE_LIST_WITH_TEXT, persist_atom(4, 1301)),
    )
    write_ppt(out / "ppt" / "handmade-side.ppt", doc, first, second, first_notes)


def malformed(out: Path) -> None:
    ppt = out / "ppt" / "pres.ppt"
    if ppt.exists():
        # Corrupt every UserEditAtom record type so the persist directory is
        # structurally unusable -> the labelled raw-recovery path must run.
        intact = struct.pack("<H", USER_EDIT_ATOM)
        corrupt = struct.pack("<H", USER_EDIT_ATOM | 0xF000)
        raw = ppt.read_bytes().replace(intact, corrupt)
        write_file(out / "malformed" / "brokenpersist--recovers.ppt", raw)


def abuse(out: Path) -> None:
    """Pathologically nested binary PPT containers; the bounded record walk must fail with
    ResourceLimit instead of exhausting the stack."""
    nested = ppt_rec(0, TEXT_BYTES_ATOM, b"deep")
    for _ in range(200):
        nested = ppt_container(0, SLIDE, nested)
    write_cfb(
        out / "abuse" / "max_record_depth--deepnest--errors.ppt",
        [("Current User", b""), ("PowerPoint Document", nested)],
    )


def generate(out: Path) -> None:
    multimaster_ppt(out)
    sparsenotes_ppt(out)
    scripts_ppt(out)
    side_ppt(out)
    malformed(out)
    abuse(out)
