import base64
import struct
import zipfile

ZIP_DATE = (2026, 1, 1, 0, 0, 0)

DOT_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8"
    "z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)


def write_cfb(path, streams):
    """Minimal CFB v3 writer (512-byte sectors) for handmade OLE2 fixtures.

    Streams are padded to the 4096-byte mini-stream cutoff so everything
    lives in regular FAT sectors; no mini-FAT is written. `streams` is a
    list of (name, bytes) in CFB name order (shorter names first, then
    case-insensitive).
    """
    SECT = 512
    ENDOFCHAIN = 0xFFFFFFFE
    FATSECT = 0xFFFFFFFD
    FREESECT = 0xFFFFFFFF

    fat = []          # sector id -> next sector id
    sectors = []      # sector payloads, id == index

    def add_chain(data):
        if not data:
            return ENDOFCHAIN
        padded = data + b"\x00" * (-len(data) % SECT)
        first = len(sectors)
        count = len(padded) // SECT
        for i in range(count):
            sectors.append(padded[i * SECT:(i + 1) * SECT])
            fat.append(first + i + 1 if i + 1 < count else ENDOFCHAIN)
        return first

    def dir_entry(name, typ, color, left, right, child, start, size):
        raw = name.encode("utf-16-le") + b"\x00\x00"
        return (raw + b"\x00" * (64 - len(raw))
                + struct.pack("<HBBIII", len(raw), typ, color, left, right, child)
                + b"\x00" * 16          # clsid
                + b"\x00" * 4           # state bits
                + b"\x00" * 16          # ctime/mtime
                + struct.pack("<IQ", start, size))

    NONE = 0xFFFFFFFF
    entries = [None]  # placeholder for root
    starts = []
    for name, data in streams:
        padded = data + b"\x00" * (4096 - len(data)) if 0 < len(data) < 4096 else data
        starts.append((add_chain(padded), len(padded)))
    for i, (name, _) in enumerate(streams):
        sid = i + 1
        right = sid + 1 if sid < len(streams) else NONE
        start, size = starts[i]
        entries.append(dir_entry(name, 2, 1, NONE, right, NONE, start, size))
    child = 1 if streams else NONE
    entries[0] = dir_entry("Root Entry", 5, 1, NONE, NONE, child, ENDOFCHAIN, 0)

    dir_data = b"".join(entries)
    dir_data += b"\x00" * (-len(dir_data) % SECT)
    dir_start = add_chain(dir_data)

    # FAT sectors (each holds 128 entries), marked FATSECT in the FAT itself.
    fat_count = 1
    while (len(fat) + fat_count + 127) // 128 > fat_count:
        fat_count += 1
    fat_start = len(sectors)
    fat_ids = list(range(fat_start, fat_start + fat_count))
    full_fat = fat + [FATSECT] * fat_count
    full_fat += [FREESECT] * (fat_count * 128 - len(full_fat))
    for i in range(fat_count):
        sectors.append(struct.pack("<128I", *full_fat[i * 128:(i + 1) * 128]))

    difat = fat_ids + [FREESECT] * (109 - len(fat_ids))
    header = (b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1" + b"\x00" * 16
              + struct.pack("<HHHHHHIIIIIIIII",
                            0x003E, 0x0003, 0xFFFE, 9, 6, 0, 0, 0,
                            fat_count, dir_start, 0, 4096,
                            ENDOFCHAIN, 0, ENDOFCHAIN)
              + struct.pack("<I", 0)
              + struct.pack("<109I", *difat))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(header + b"".join(sectors))


def write_zip(path, entries, mimetype_first=None):
    """entries: list of (name, bytes). Deterministic timestamps."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        if mimetype_first is not None:
            info = zipfile.ZipInfo("mimetype", date_time=ZIP_DATE)
            z.writestr(info, mimetype_first, compress_type=zipfile.ZIP_STORED)
        for name, data in entries:
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)


CONTENT_TYPES_BASE = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
{extra}</Types>"""


# ---------------------------------------------------------------------------
# Math: OMML (docx, pptx, rtf) and MathML (odt, epub) convert to LaTeX

M_NS = 'xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"'


def omml_run(text):
    return f'<m:r><m:t xml:space="preserve">{text}</m:t></m:r>'


# x = (-b ± √(b² - 4ac)) / 2a
OMML_QUADRATIC = (
    "<m:oMath>" + omml_run("x") + omml_run("=")
    + "<m:f><m:num>" + omml_run("-b±")
    + '<m:rad><m:radPr><m:degHide m:val="1"/></m:radPr><m:deg/><m:e>'
    + "<m:sSup><m:e>" + omml_run("b") + "</m:e><m:sup>" + omml_run("2") + "</m:sup></m:sSup>"
    + omml_run("-4ac") + "</m:e></m:rad></m:num>"
    + "<m:den>" + omml_run("2a") + "</m:den></m:f></m:oMath>"
)
# ∑_{i=1}^{n} x_i = sin(θ)
OMML_SUM = (
    '<m:oMath><m:nary><m:naryPr><m:chr m:val="∑"/><m:limLoc m:val="undOvr"/></m:naryPr>'
    + "<m:sub>" + omml_run("i=1") + "</m:sub><m:sup>" + omml_run("n") + "</m:sup>"
    + "<m:e><m:sSub><m:e>" + omml_run("x") + "</m:e><m:sub>" + omml_run("i") + "</m:sub></m:sSub></m:e></m:nary>"
    + omml_run("=")
    + '<m:func><m:fName><m:r><m:rPr><m:sty m:val="p"/></m:rPr><m:t>sin</m:t></m:r></m:fName>'
    + "<m:e><m:d><m:e>" + omml_run("θ") + "</m:e></m:d></m:e></m:func></m:oMath>"
)
# where 𝐱 ∈ ℝ (normal text run plus styled alphanumerics)
OMML_WHERE = (
    '<m:oMath><m:r><m:rPr><m:nor/></m:rPr><m:t xml:space="preserve">where </m:t></m:r>'
    + omml_run("𝐱 ∈ ℝ") + "</m:oMath>"
)


def zip_without(src, dst, drop):
    with zipfile.ZipFile(src) as zin:
        entries = [(i.filename, zin.read(i.filename))
                   for i in zin.infolist() if i.filename != drop]
    write_zip(dst, entries)


def zip_replace(src, dst, name, data):
    with zipfile.ZipFile(src) as zin:
        entries = [(i.filename, data if i.filename == name else zin.read(i.filename))
                   for i in zin.infolist()]
    write_zip(dst, entries)
