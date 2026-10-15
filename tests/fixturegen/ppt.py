import struct

from fixturegen.common import write_cfb


# ---------------------------------------------------------------------------
# R14: binary PPT per-slide master selection with tri-state level defaults

def ppt_rec(ver_inst, rec_type, body):
    return struct.pack("<HHI", ver_inst, rec_type, len(body)) + body


def ppt_container(instance, rec_type, body):
    return ppt_rec((instance << 4) | 0xF, rec_type, body)


def multimaster_ppt(out):
    # TxMasterStyleAtom instance 1 (body text), one level: PF carries
    # hasBullet, CF carries exactly one specified style bit (tri-state).
    def tx_master(bullet_on, cf_mask, cf_style):
        pf = struct.pack("<IH", 0x0001, 0x0001 if bullet_on else 0x0000)
        cf = struct.pack("<IH", cf_mask, cf_style)
        return ppt_rec(1 << 4, 0x0FA3, struct.pack("<H", 1) + pf + cf)

    def slide(master_id, text):
        slide_atom = ppt_rec(2, 0x03EF, struct.pack("<I", 0) + b"\x00" * 8
                             + struct.pack("<IIHH", master_id, 0, 0, 0))
        th = ppt_rec(0, 0x0F9F, struct.pack("<I", 1))
        tb = ppt_rec(0, 0x0FA8, text.encode("ascii"))
        return ppt_container(0, 0x03EE, slide_atom + th + tb)

    def persist_atom(persist_ref, sid):
        return ppt_rec(0, 0x03F3, struct.pack("<IIIII", persist_ref, 0, 0, sid, 0))

    slide_list = ppt_container(0, 0x0FF0, persist_atom(4, 256) + persist_atom(5, 257))
    master_list = ppt_container(1, 0x0FF0, persist_atom(2, 1001) + persist_atom(3, 1002))
    doc = ppt_container(0, 0x03E8, slide_list + master_list)
    master_a = ppt_container(0, 0x03F8, tx_master(True, 0x0001, 0x0001))   # bullet+bold
    master_b = ppt_container(0, 0x03F8, tx_master(False, 0x0002, 0x0002))  # no bullet, italic
    slide1 = slide(1001, "Alpha master body text\r")
    slide2 = slide(1002, "Beta master body text\r")

    stream = b""
    offsets = {}
    for pid, blob in [(1, doc), (2, master_a), (3, master_b), (4, slide1), (5, slide2)]:
        offsets[pid] = len(stream)
        stream += blob
    entries = struct.pack("<I", 1 | (5 << 20)) + struct.pack(
        "<5I", *(offsets[i] for i in range(1, 6)))
    off_dir = len(stream)
    stream += ppt_rec(0, 0x1772, entries)
    off_edit = len(stream)
    stream += ppt_rec(0, 0x0FF5, struct.pack(
        "<IIIIIIHH", 0, 0, 0, off_dir, 1, 6, 0, 0))
    current_user = ppt_rec(0, 0x0FF6, struct.pack("<III", 20, 0xE391C05F, off_edit))
    write_cfb(out / "ppt" / "handmade-multimaster.ppt", [
        ("Current User", current_user),
        ("PowerPoint Document", stream),
    ])


# ---------------------------------------------------------------------------
# S20: notes pages pair to slides via NotesAtom.slideIdRef, not list order -
# this deck has notes only on the SECOND slide, which order-based zipping
# would misattribute to the first.

def sparsenotes_ppt(out):
    def slide(text):
        slide_atom = ppt_rec(2, 0x03EF, struct.pack("<I", 0) + b"\x00" * 8
                             + struct.pack("<IIHH", 0, 0, 0, 0))
        th = ppt_rec(0, 0x0F9F, struct.pack("<I", 1))
        tb = ppt_rec(0, 0x0FA8, text.encode("ascii"))
        return ppt_container(0, 0x03EE, slide_atom + th + tb)

    def notes(slide_ref, text):
        notes_atom = ppt_rec(1, 0x03F1, struct.pack("<IHH", slide_ref, 0, 0))
        th = ppt_rec(0, 0x0F9F, struct.pack("<I", 2))
        tb = ppt_rec(0, 0x0FA8, text.encode("ascii"))
        return ppt_container(0, 0x03F0, notes_atom + th + tb)

    def persist_atom(persist_ref, sid):
        return ppt_rec(0, 0x03F3, struct.pack("<IIIII", persist_ref, 0, 0, sid, 0))

    slide_list = ppt_container(0, 0x0FF0, persist_atom(2, 256) + persist_atom(3, 257))
    notes_list = ppt_container(2, 0x0FF0, persist_atom(4, 1301))
    doc = ppt_container(0, 0x03E8, slide_list + notes_list)
    s1 = slide("First slide text\r")
    s2 = slide("Second slide text\r")
    n2 = notes(257, "Notes for the second slide\r")

    stream = b""
    offsets = {}
    for pid, blob in [(1, doc), (2, s1), (3, s2), (4, n2)]:
        offsets[pid] = len(stream)
        stream += blob
    entries = struct.pack("<I", 1 | (4 << 20)) + struct.pack(
        "<4I", *(offsets[i] for i in range(1, 5)))
    off_dir = len(stream)
    stream += ppt_rec(0, 0x1772, entries)
    off_edit = len(stream)
    stream += ppt_rec(0, 0x0FF5, struct.pack(
        "<IIIIIIHH", 0, 0, 0, off_dir, 1, 5, 0, 0))
    current_user = ppt_rec(0, 0x0FF6, struct.pack("<III", 20, 0xE391C05F, off_edit))
    write_cfb(out / "ppt" / "handmade-sparsenotes.ppt", [
        ("Current User", current_user),
        ("PowerPoint Document", stream),
    ])


def malformed(out):
    m = out / "malformed"
    m.mkdir(parents=True, exist_ok=True)
    ppt = out / "ppt" / "pres.ppt"
    if ppt.exists():
        # Corrupt every UserEditAtom record type so the persist directory is
        # structurally unusable -> the labelled raw-recovery path must run.
        raw = ppt.read_bytes().replace(b"\xf5\x0f", b"\xf5\xff")
        (m / "brokenpersist--recovers.ppt").write_bytes(raw)


def abuse(out):
    a = out / "abuse"
    a.mkdir(parents=True, exist_ok=True)
    # R6: pathologically nested binary PPT containers; the bounded record
    # walk must fail with ResourceLimit instead of exhausting the stack.
    nested = struct.pack("<HHI", 0x0000, 0x0FA8, 4) + b"deep"
    for _ in range(200):
        nested = struct.pack("<HHI", 0x000F, 0x03EE, len(nested)) + nested
    write_cfb(a / "max_record_depth--deepnest--errors.ppt", [
        ("Current User", b""),
        ("PowerPoint Document", nested),
    ])


def generate(out):
    multimaster_ppt(out)
    sparsenotes_ppt(out)
    malformed(out)
    abuse(out)
